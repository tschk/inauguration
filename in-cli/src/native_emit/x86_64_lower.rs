//! Core IR → x86_64 lowering for the owned native subset.
//!
//! Lowers a subset of Core IR functions into x86_64 machine code.
//! Supports scalar function bodies with:
//!   - let bindings (Int, Bool, Float, String)
//!   - return
//!   - if/else
//!   - while loops
//!   - integer and scalar double arithmetic
//!   - direct function calls
//!   - struct init/field access (scalar fields only)

use crate::core_ir::{Decl, Expr, LoopKind, MatchArm, MatchPattern, Stmt, Typ, UnifiedModule};
use crate::native_emit::antidecomp;
/// Layout of emitted string payloads; retained under the existing public x86 API name.
pub use crate::native_emit::global_data::StringLayout as X86StringLayout;
use crate::native_emit::global_data::{
    append_string_payload, apply_data_relocations, build_data_section, collect_globals,
};
use crate::native_emit::x86_64::{self, CodeEmitter, RAX, RBP, RBX, RCX, RDI, RDX, RSI};
use crate::native_emit::{AbsoluteReference, NativeSection};
use std::collections::HashMap;

use crate::native_emit::stdlib_wrappers::wrapper as jit_stdlib_wrapper;

pub const X86_64_TRIPLE: &str = "x86_64-unknown-none";

fn emit_profile_prologue() -> Vec<u8> {
    if antidecomp::harden_active() {
        antidecomp::harden_prologue()
    } else {
        x86_64::prologue()
    }
}

fn emit_profile_epilogue() -> Vec<u8> {
    if antidecomp::harden_active() {
        antidecomp::harden_epilogue()
    } else {
        x86_64::epilogue()
    }
}

// Restoring SP from the frame pointer also discards transient operand spills.
fn emit_function_epilogue(emitter: &mut CodeEmitter, is_interrupt: bool) {
    if is_interrupt {
        emitter.emit_insns(&x86_64::mov_rr(x86_64::REG_SP, x86_64::REG_FP));
        emitter.emit_insns(&x86_64::pop_r(x86_64::REG_FP));
        for &reg in &[
            15u8, 14, 13, 12, 11, 10, 9, 8, RDI, RSI, RBP, RBX, RDX, RCX, RAX,
        ] {
            emitter.emit_insns(&x86_64::pop_r(reg));
        }
        emitter.emit_width(&[0xCF], &[0x48, 0xCF]);
    } else {
        emitter.emit_insns(&emit_profile_epilogue());
    }
}

fn emit_failure_return(emitter: &mut CodeEmitter, ctx: &mut LowerCtx<'_>) -> Result<(), String> {
    emitter.emit_insns(&x86_64::load_i64(RAX, 1));
    if matches!(
        ctx.ret_typ.canonical(),
        Typ::String | Typ::Array(_) | Typ::Vector(_) | Typ::Named(_)
    ) {
        store_error_global(emitter, ctx, "__inrt_err_val")?;
        store_error_global(emitter, ctx, "__inrt_err_type")?;
        store_error_global(emitter, ctx, "__inrt_err_flag")?;
        emit_error_exit(emitter, ctx)?;
    } else {
        emit_function_epilogue(emitter, ctx.is_interrupt);
    }
    Ok(())
}

fn patch_absolute_reference(
    code: &mut [u8],
    reference: AbsoluteReference,
    base: u64,
    is_32bit: bool,
) -> Result<AbsoluteReference, String> {
    let expected_width = if is_32bit { 4 } else { 8 };
    if reference.width != expected_width {
        return Err(
            "x86_64-lower: absolute reference width does not match target pointer ABI".into(),
        );
    }
    let address = base
        .checked_add(reference.target_offset)
        .ok_or("x86_64-lower: absolute reference address overflow")?;
    if is_32bit && address > u64::from(u32::MAX) {
        return Err("x86_64-lower: absolute reference exceeds target pointer address space".into());
    }
    let start = usize::try_from(reference.site)
        .map_err(|_| "x86_64-lower: absolute reference site exceeds host address space")?;
    let end = start
        .checked_add(usize::from(reference.width))
        .ok_or("x86_64-lower: absolute reference site overflow")?;
    let slot = code
        .get_mut(start..end)
        .ok_or("x86_64-lower: absolute reference patch outside code")?;
    let bytes = address.to_le_bytes();
    slot.copy_from_slice(&bytes[..usize::from(reference.width)]);
    Ok(reference)
}

pub struct X86_64CompileResult {
    pub code: Vec<u8>,
    pub entry_offset: u32,
    pub exports: Vec<(String, u32)>,
    /// Byte offsets in `code` where target-width absolute addresses were written.
    /// At load time, patch each by adding (actual_base - codegen_base).
    pub relocations: Vec<u32>,
    /// Validated absolute sites with section-relative targets for object writers.
    pub absolute_refs: Vec<AbsoluteReference>,
    /// (site, data offset) for every absolute reference to a global slot.
    /// Native linkers rebase these against `.data`; the JIT maps the data
    /// section and rebases to the mapped address. Sites here are excluded
    /// from the code-relative `relocations` pass at JIT load time.
    pub global_relocs: Vec<(u32, u64)>,
    /// The base address used during codegen (KERNEL_BASE = 0x101100).
    pub codegen_base: u64,
    /// Initialised data section bytes (global variable initial values).
    pub data: Vec<u8>,
    /// Data-relative pointer slots and their data-relative payload targets.
    pub data_relocations: Vec<(u64, u64)>,
    /// Undefined symbols (externs) — names of functions called but not
    /// defined in this module. Resolved at link time.
    pub externs: Vec<String>,
    /// (extern symbol name, code offset of the `E8` opcode byte) for every
    /// call to a function defined outside this module. The dynamic SCI
    /// emitter turns these into import records; the loader patches the
    /// disp32 at `code + site + 1` to the provider's virtual address.
    pub extern_calls: Vec<(String, u32)>,
}

impl X86_64CompileResult {
    pub(crate) fn require_resolved_imports(&self) -> Result<(), String> {
        if self.externs.is_empty() {
            return Ok(());
        }
        let mut names = self.externs.clone();
        names.sort();
        names.dedup();
        Err(format!(
            "x86_64-lower: unresolved runtime imports: {}",
            names.join(", ")
        ))
    }
}

#[derive(Debug, Clone)]
struct FunctionInfo {
    name: String,
    params: Vec<(String, Typ)>,
    ret: Typ,
    body: Vec<Stmt>,
    may_throw: bool,
}

struct LowerCtx<'a> {
    string_layout: X86StringLayout,
    /// Map local name → stack offset (negative from RBP)
    locals: HashMap<String, StackSlot>,
    scalar_types: HashMap<String, Typ>,
    empty_struct_bases: HashMap<String, u32>,
    receiver_pointers: HashMap<String, u32>,
    /// Current stack frame size (negative, grows down)
    frame_size: u32,
    /// Set when a return statement has been emitted
    emitted_return: bool,
    error_branches: Vec<Vec<u32>>,
    error_types: HashMap<String, u32>,
    frame_size_sites: Vec<u32>,
    /// True if this function is an interrupt handler
    is_interrupt: bool,
    /// Struct field definitions
    structs: &'a HashMap<String, Vec<(String, Typ)>>,
    /// All functions by name (for call resolution)
    functions: &'a HashMap<String, FunctionInfo>,
    /// Current function name (for error messages)
    fn_name: String,
    /// Global variable offsets: name → offset within the data section.
    /// Absolute addresses are patched after code and data layout are known.
    globals: HashMap<String, u64>,
    /// Sites in the emitted code that reference a global variable.
    pending_globals: Vec<PendingGlobal>,
    /// JIT ABI: R15 supplies a writable error page and stdlib imports resolve in process.
    runtime_error_slot: bool,
    /// Error handling: offset for error flag byte (Throw/Try)
    error_flag_offset: u32,
    /// Error handling: offset for error value (Throw/Try)
    error_value_offset: u32,
    /// Return type of the current function
    ret_typ: Typ,
    /// Unpatched `jmp rel32` sites for `break` in the current loop nest.
    break_sites: Vec<u32>,
    /// Unpatched `jmp rel32` sites for `continue` in the current loop nest.
    continue_sites: Vec<u32>,
}

#[derive(Debug, Clone)]
enum StackSlot {
    Scalar(u32), // offset from RBP (negative)
    Struct { fields: HashMap<String, u32> },
    InlineArray { offsets: Vec<u32> },
}

#[derive(Debug, Clone)]
struct PendingCall {
    site: u32,
    target: String,
}

#[derive(Debug, Clone)]
struct PendingGlobal {
    site: u32,
    width: u8,
    offset: u64,
}

impl<'a> LowerCtx<'a> {
    fn new(
        fn_name: &str,
        params: &[(String, Typ)],
        structs: &'a HashMap<String, Vec<(String, Typ)>>,
        functions: &'a HashMap<String, FunctionInfo>,
        globals: HashMap<String, u64>,
    ) -> Self {
        let mut ctx = Self {
            string_layout: X86StringLayout::Instring,
            locals: HashMap::new(),
            scalar_types: HashMap::new(),
            empty_struct_bases: HashMap::new(),
            receiver_pointers: HashMap::new(),
            frame_size: 0,
            emitted_return: false,
            error_branches: Vec::new(),
            error_types: HashMap::new(),
            frame_size_sites: Vec::new(),
            is_interrupt: false,
            structs,
            functions,
            fn_name: fn_name.to_string(),
            globals,
            pending_globals: Vec::new(),
            runtime_error_slot: false,
            error_flag_offset: 0,
            error_value_offset: 0,
            ret_typ: Typ::Int,
            break_sites: Vec::new(),
            continue_sites: Vec::new(),
        };
        // Allocate stack slots for parameters
        // On x86_64 (System V), first 6 integer args go in RDI, RSI, RDX, RCX, R8, R9.
        // All parameters get a local stack slot so the body can reload them the same
        // way. Register params are stored to their slots in the prologue; stack params
        // are loaded from the caller's argument area and stored to their slots there.
        for (name, typ) in params.iter() {
            ctx.scalar_types.insert(name.clone(), typ.clone());
            // Preserve known struct parameters as field-addressable locals.
            // Unknown named types retain the scalar fallback used by the
            // partial native ABI.
            if let Typ::Named(struct_name) = typ.canonical()
                && ctx.structs.contains_key(&struct_name)
            {
                ctx.alloc_local(name, typ)
                    .expect("known struct parameter must allocate");
            } else {
                let offset = ctx.alloc_slot();
                ctx.locals.insert(name.clone(), StackSlot::Scalar(offset));
            }
        }
        ctx
    }

    fn alloc_slot(&mut self) -> u32 {
        let offset = self.frame_size;
        self.frame_size += 8;
        offset
    }

    fn alloc_local(&mut self, name: &str, typ: &Typ) -> Result<(), String> {
        super::lower::ensure_unit_sequence_layout(typ)?;
        record_array_fields(self.structs, typ)?;
        self.scalar_types.insert(name.to_string(), typ.clone());
        if self.locals.contains_key(name) {
            return Ok(());
        }
        match typ {
            Typ::Int
            | Typ::Bool
            | Typ::String
            | Typ::Float
            | Typ::Void
            | Typ::Array(_)
            | Typ::Vector(_) => {
                let offset = self.alloc_slot();
                self.locals
                    .insert(name.to_string(), StackSlot::Scalar(offset));
                Ok(())
            }
            Typ::Named(struct_name) => {
                let Some(fields) = self.structs.get(struct_name).cloned() else {
                    // Frontends preserve opaque/external aggregate names even
                    // when no layout is available in the merged module. Keep
                    // those values in the scalar fallback used by the owned
                    // ABI instead of rejecting an otherwise lowerable body.
                    let offset = self.alloc_slot();
                    self.locals
                        .insert(name.to_string(), StackSlot::Scalar(offset));
                    return Ok(());
                };
                let mut slots = HashMap::new();
                if fields.is_empty() {
                    let base = self.alloc_slot();
                    self.empty_struct_bases.insert(name.to_string(), base);
                }
                // Frame offsets grow downward; reverse allocation keeps declared
                // fields in increasing memory order from the struct address.
                let fields =
                    crate::native_emit::struct_layout::x86_fields(self.structs, struct_name)
                        .unwrap_or(fields);
                for (field, field_ty) in fields.into_iter().rev() {
                    if field_ty.canonical() == Typ::Void {
                        return Err(
                            "x86_64-lower: unit aggregate fields require a zero-width layout"
                                .into(),
                        );
                    }
                    super::lower::ensure_unit_sequence_layout(&field_ty)?;
                    // ponytail: all struct fields map to scalar slots
                    slots.insert(field.clone(), self.alloc_slot());
                }
                self.locals
                    .insert(name.to_string(), StackSlot::Struct { fields: slots });
                Ok(())
            }
            Typ::Generic(_) => {
                let offset = self.alloc_slot();
                self.locals
                    .insert(name.to_string(), StackSlot::Scalar(offset));
                Ok(())
            }
        }
    }

    fn receiver_field(&self, name: &str, field: &str) -> Option<(u32, i32)> {
        let pointer = *self.receiver_pointers.get(name)?;
        let StackSlot::Struct { fields } = self.locals.get(name)? else {
            return None;
        };
        let offset = *fields.get(field)?;
        let base = *fields.values().max()?;
        Some((pointer, (base - offset) as i32))
    }

    fn frame_reserve(&self) -> u32 {
        // Round up to 16-byte alignment
        (self.frame_size + 15) & !15
    }

    fn slot_offset(&self, name: &str) -> Result<u32, String> {
        match self.locals.get(name) {
            Some(StackSlot::Scalar(offset)) => Ok(*offset),
            _ => Err(format!(
                "x86_64-lower: expected scalar local `{name}` in `{}`",
                self.fn_name
            )),
        }
    }
}

/// Lower a Core IR module for the JIT ABI, whose invocation supplies R15.
pub fn lower_module(module: &UnifiedModule, entry: &str) -> Result<X86_64CompileResult, String> {
    let result = lower_module_impl(
        module,
        entry,
        0x101100,
        0x200000,
        X86StringLayout::Instring,
        true,
    )?;
    result.require_resolved_imports()?;
    Ok(result)
}

/// Lower a native artifact using the default load layout, without a JIT runtime.
pub fn lower_native_module(
    module: &UnifiedModule,
    entry: &str,
) -> Result<X86_64CompileResult, String> {
    lower_module_with_bases(module, entry, 0x101100, 0x200000)
}

/// Lower a Core IR module to x86_64 machine code for a specific load layout.
///
/// `code_base` is the virtual address of the first instruction in `code`.
/// `data_base` is the virtual address where the global data section starts.
/// String literals are placed immediately after the code section.
pub fn lower_module_with_bases(
    module: &UnifiedModule,
    entry: &str,
    code_base: u64,
    data_base: u64,
) -> Result<X86_64CompileResult, String> {
    lower_module_with_bases_layout(
        module,
        entry,
        code_base,
        data_base,
        X86StringLayout::Instring,
    )
}

/// Like [`lower_module_with_bases`] with an explicit string-literal layout.
pub fn lower_module_with_bases_layout(
    module: &UnifiedModule,
    entry: &str,
    code_base: u64,
    data_base: u64,
    layout: X86StringLayout,
) -> Result<X86_64CompileResult, String> {
    lower_module_impl(module, entry, code_base, data_base, layout, false)
}

fn lower_module_impl(
    module: &UnifiedModule,
    entry: &str,
    code_base: u64,
    data_base: u64,
    layout: X86StringLayout,
    runtime_error_slot: bool,
) -> Result<X86_64CompileResult, String> {
    crate::core_ir_verifier::check_execution_contracts(module)?;
    let semantics = if x86_64::is_32bit() {
        crate::integer_semantics::IntegerSemantics::Word32
    } else {
        crate::integer_semantics::IntegerSemantics::Word64
    };
    let prepared = crate::lower_core::prepared_native_module_with_semantics(module, semantics);
    let module = prepared.as_ref();
    crate::core_ir_verifier::check_execution_contracts(module)?;
    crate::core_ir_verifier::check_nonvoid_completion(module)?;
    let functions = collect_functions(module)?;
    let structs = collect_structs(module);
    for decl in &module.decls {
        if let Decl::Global { typ, .. } = decl {
            record_array_fields(&structs, typ)?;
        }
    }
    let globals = collect_globals(module, &structs)?;
    let global_types: HashMap<String, Typ> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.canonical())),
            _ => None,
        })
        .collect();

    let all_strings = collect_string_literals(module);

    let mut emitter = CodeEmitter::new();
    let mut function_offsets: HashMap<String, u32> = HashMap::new();
    let mut all_pending_calls: Vec<PendingCall> = Vec::new();
    let mut all_pending_globals: Vec<PendingGlobal> = Vec::new();
    let mut extern_calls: Vec<(String, u32)> = Vec::new();

    // Sort functions so the entry function is always first (so the trampoline
    // can jump to a known offset 0 in the compiled code section).
    let mut names: Vec<String> = functions.keys().cloned().collect();
    names.sort_by(|a, b| {
        if a == entry {
            std::cmp::Ordering::Less
        } else if b == entry {
            std::cmp::Ordering::Greater
        } else {
            a.cmp(b)
        }
    });

    for name in &names {
        let func = &functions[name];
        // Skip extern functions (empty body) — they become undefined syms
        if func.body.is_empty() {
            continue;
        }
        if x86_64::is_32bit() {
            super::validate_word32_scalar_signature(&func.params, &func.ret)?;
        }
        let offset = emitter.len();
        function_offsets.insert(name.clone(), offset);
        let is_interrupt = crate::core_ir::is_interrupt_fn(&func.name);
        let pending = lower_function(
            &mut emitter,
            func,
            &structs,
            &functions,
            &globals,
            &global_types,
            &mut all_pending_calls,
            is_interrupt,
            runtime_error_slot,
            layout,
        )?;
        all_pending_globals.extend(pending);
    }

    // Resolve calls — collect string refs, resolve function addresses and calls
    let mut str_refs: Vec<(u32, String)> = Vec::new();
    let mut relocations: Vec<u32> = Vec::new();
    let mut absolute_refs = Vec::new();
    for call in &all_pending_calls {
        if call.target.starts_with("@addr_") {
            // Function address reference: write absolute address at site
            let fn_name = &call.target[6..];
            if let Some(&func_offset) = function_offsets.get(fn_name) {
                absolute_refs.push(patch_absolute_reference(
                    &mut emitter.bytes,
                    AbsoluteReference {
                        site: call.site,
                        width: if x86_64::is_32bit() { 4 } else { 8 },
                        target_section: NativeSection::Code,
                        target_offset: u64::from(func_offset),
                    },
                    code_base,
                    x86_64::is_32bit(),
                )?);
                relocations.push(call.site);
            } else {
                return Err(format!(
                    "x86_64-lower: unresolved function address `{fn_name}`"
                ));
            }
        } else if call.target.starts_with("@str_") {
            str_refs.push((call.site, call.target[5..].to_string()));
        } else if let Some(&target_offset) = function_offsets.get(&call.target) {
            let rel_offset = target_offset as i32 - call.site as i32 - 5; // call is 5 bytes
            emitter.patch_u32(call.site + 1, rel_offset as u32);
        } else {
            // Extern call: keep the rel32 placeholder at zero. The dynamic
            // SCI emitter records (symbol, site) as an import record and the
            // loader patches the disp32 at site+1 to the provider's address.
            extern_calls.push((call.target.clone(), call.site));
        }
    }

    // Append string data section and patch string literal references
    if !str_refs.is_empty() || !all_strings.is_empty() {
        while !emitter.len().is_multiple_of(8) {
            emitter.bytes.push(0x90);
        }
        for s in &all_strings {
            let offset = append_string_payload(&mut emitter.bytes, s, layout)?;
            for &(site, ref content) in &str_refs {
                if content == s {
                    absolute_refs.push(patch_absolute_reference(
                        &mut emitter.bytes,
                        AbsoluteReference {
                            site,
                            width: if x86_64::is_32bit() { 4 } else { 8 },
                            target_section: NativeSection::Code,
                            target_offset: offset,
                        },
                        code_base,
                        x86_64::is_32bit(),
                    )?);
                    relocations.push(site);
                }
            }
        }
    }

    // Patch global-variable references now that the data section address is known.
    let mut global_relocs: Vec<(u32, u64)> = Vec::new();
    for pg in &all_pending_globals {
        absolute_refs.push(patch_absolute_reference(
            &mut emitter.bytes,
            AbsoluteReference {
                site: pg.site,
                width: pg.width,
                target_section: NativeSection::Data,
                target_offset: pg.offset,
            },
            data_base,
            x86_64::is_32bit(),
        )?);
        global_relocs.push((pg.site, pg.offset));
        relocations.push(pg.site);
    }

    let entry_offset = function_offsets.get(entry).copied().unwrap_or(0);
    let mut exports: Vec<(String, u32)> = function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset))
        .collect();
    exports.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    let crate::native_emit::global_data::DataSection {
        bytes: mut data,
        pointer_fixups: data_relocations,
    } = build_data_section(module, &globals, layout)?;
    for reference in &absolute_refs {
        let target_len = match reference.target_section {
            NativeSection::Code => emitter.bytes.len(),
            NativeSection::Data => data.len(),
        };
        if reference.target_offset >= target_len as u64 {
            return Err("x86_64-lower: absolute reference target outside section".into());
        }
    }
    apply_data_relocations(
        &mut data,
        &data_relocations,
        data_base,
        if x86_64::is_32bit() {
            u32::MAX as u64
        } else {
            u64::MAX
        },
    )?;

    // Build externs list: calls to functions not defined in this module
    let mut externs: Vec<String> = Vec::new();
    for call in &all_pending_calls {
        if !call.target.starts_with("@addr_")
            && !call.target.starts_with("@str_")
            && !function_offsets.contains_key(&call.target)
            && !externs.contains(&call.target)
        {
            externs.push(call.target.clone());
        }
    }

    Ok(X86_64CompileResult {
        code: emitter.bytes,
        entry_offset,
        exports,
        relocations,
        absolute_refs,
        global_relocs,
        codegen_base: code_base,
        data,
        data_relocations,
        externs,
        extern_calls,
    })
}

fn collect_functions(module: &UnifiedModule) -> Result<HashMap<String, FunctionInfo>, String> {
    let mut functions = HashMap::new();
    let mut name_counts: HashMap<String, u32> = HashMap::new();
    for decl in &module.decls {
        let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        else {
            continue;
        };
        // ponytail: single allocation for the unique name string
        let unique_name = if functions.contains_key(name) {
            let count = if let Some(c) = name_counts.get_mut(name) {
                *c += 1;
                *c
            } else {
                name_counts.insert(name.clone(), 2);
                2
            };
            format!("{name}__dup{count}")
        } else {
            name_counts.insert(name.clone(), 1);
            name.clone()
        };
        functions.insert(
            unique_name.clone(),
            FunctionInfo {
                name: unique_name,
                params: params.clone(),
                ret: ret.clone(),
                body: body.clone(),
                may_throw: false,
            },
        );
    }
    // Build disambiguation map and update call targets
    let mut name_map: HashMap<String, String> = HashMap::new();
    for unique in functions.keys() {
        let orig = unique.split("__dup").next().unwrap_or(unique).to_string();
        name_map.insert(orig, unique.clone());
    }
    let module_names = crate::core_bindings::module_names(&module.decls);
    let references = crate::core_bindings::function_references(&module.decls);
    for func in functions.values_mut() {
        crate::core_bindings::normalize_function_with_references(
            &mut func.params,
            &mut func.body,
            &module_names,
            &references,
        );
        rename_calls(&mut func.body, &name_map);
    }
    let fallible = crate::core_effects::fallible_functions(functions.values().map(|info| {
        (
            info.name.as_str(),
            info.body.as_slice(),
            &info.ret,
            info.params.as_slice(),
        )
    }));
    for info in functions.values_mut() {
        info.may_throw = fallible.contains(&info.name);
    }
    if functions.is_empty() {
        return Err("x86_64-lower: module has no functions".to_string());
    }
    Ok(functions)
}

fn rename_calls(stmts: &mut [Stmt], name_map: &HashMap<String, String>) {
    for stmt in stmts.iter_mut() {
        rename_calls_in_stmt(stmt, name_map);
    }
}

fn rename_calls_in_stmt(stmt: &mut Stmt, name_map: &HashMap<String, String>) {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Expr(expr)
        | Stmt::Return(Some(expr)) => rename_calls_in_expr(expr, name_map),
        Stmt::Declare(_, _) | Stmt::Return(None) => {}
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            rename_calls_in_expr(cond, name_map);
            rename_calls(then_body, name_map);
            rename_calls(else_body, name_map);
        }
        Stmt::Loop { body, cond, .. } => {
            if let Some(cond) = cond {
                rename_calls_in_expr(cond, name_map);
            }
            rename_calls(body, name_map);
        }
        Stmt::FieldAssign { base, value, .. } => {
            rename_calls_in_expr(base, name_map);
            rename_calls_in_expr(value, name_map);
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            rename_calls_in_expr(base, name_map);
            rename_calls_in_expr(index, name_map);
            rename_calls_in_expr(value, name_map);
        }
        Stmt::Match { scrutinee, arms } => {
            rename_calls_in_expr(scrutinee, name_map);
            for arm in arms {
                rename_calls(&mut arm.body, name_map);
            }
        }
        Stmt::Throw(expr) => rename_calls_in_expr(expr, name_map),
        Stmt::Try { body, catches } => {
            rename_calls(body, name_map);
            for catch in catches {
                rename_calls(&mut catch.body, name_map);
            }
        }
        Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
    }
}

fn rename_calls_in_expr(expr: &mut Expr, name_map: &HashMap<String, String>) {
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            rename_calls_in_expr(scrutinee, name_map);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rename_calls_in_expr(guard, name_map);
                }
                rename_calls_in_expr(&mut arm.value, name_map);
            }
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            for value in [cond, then_expr, else_expr] {
                rename_calls_in_expr(value, name_map);
            }
        }
        Expr::Block { body, result } => {
            rename_calls(body, name_map);
            if let Some(value) = result {
                rename_calls_in_expr(value, name_map);
            }
        }

        Expr::Call { callee, args } => {
            if let Expr::Ident(name) = callee.as_ref() {
                if let Some(mapped) = name_map.get(name.as_str()) {
                    **callee = Expr::Ident(mapped.clone());
                }
            }
            for arg in args {
                rename_calls_in_expr(arg, name_map);
            }
        }
        Expr::Ident(name) => {
            if let Some(mapped) = name_map.get(name.as_str()) {
                *name = mapped.clone();
            }
        }
        Expr::Unary { expr: inner, .. } => rename_calls_in_expr(inner, name_map),
        Expr::Binary { lhs, rhs, .. } => {
            rename_calls_in_expr(lhs, name_map);
            rename_calls_in_expr(rhs, name_map);
        }
        Expr::StructInit { fields, .. } => {
            for (_, field_expr) in fields {
                rename_calls_in_expr(field_expr, name_map);
            }
        }
        Expr::Field { base, .. } => rename_calls_in_expr(base, name_map),
        Expr::Index { base, index, .. } => {
            rename_calls_in_expr(base, name_map);
            rename_calls_in_expr(index, name_map);
        }
        Expr::ArrayLit(items) => {
            for item in items {
                rename_calls_in_expr(item, name_map);
            }
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Closure { .. } => {}
    }
}

fn collect_structs(module: &UnifiedModule) -> HashMap<String, Vec<(String, Typ)>> {
    let mut structs: HashMap<String, Vec<(String, Typ)>> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Struct { name, fields, .. } => Some((name.clone(), fields.clone())),
            Decl::Class { name, fields, .. } => Some((name.clone(), fields.clone())),
            _ => None,
        })
        .collect();
    // Synthetic struct defs for common Rust std types
    if !structs.contains_key("Vec") {
        structs.insert(
            "Vec".into(),
            vec![
                ("ptr".into(), Typ::Int),
                ("len".into(), Typ::Int),
                ("cap".into(), Typ::Int),
            ],
        );
    }
    if !structs.contains_key("String") {
        structs.insert(
            "String".into(),
            vec![("vec".into(), Typ::Named("Vec".into()))],
        );
    }
    if !structs.contains_key("Box") {
        structs.insert("Box".into(), vec![("ptr".into(), Typ::Int)]);
    }
    if !structs.contains_key("Option") {
        structs.insert(
            "Option".into(),
            vec![("tag".into(), Typ::Int), ("value".into(), Typ::Int)],
        );
    }
    if !structs.contains_key("Result") {
        structs.insert(
            "Result".into(),
            vec![
                ("tag".into(), Typ::Int),
                ("ok".into(), Typ::Int),
                ("err".into(), Typ::Int),
            ],
        );
    }
    if !structs.contains_key("HashMap") {
        structs.insert("HashMap".into(), vec![("ptr".into(), Typ::Int)]);
    }
    if !structs.contains_key("PathBuf") {
        structs.insert(
            "PathBuf".into(),
            vec![("vec".into(), Typ::Named("Vec".into()))],
        );
    }
    structs
}

fn collect_strings_from_expr(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            collect_strings_from_expr(scrutinee, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_strings_from_expr(guard, out);
                }
                collect_strings_from_expr(&arm.value, out);
            }
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            for value in [cond, then_expr, else_expr] {
                collect_strings_from_expr(value, out);
            }
        }
        Expr::Block { body, result } => {
            for stmt in body {
                collect_strings_from_stmt(stmt, out);
            }
            if let Some(value) = result {
                collect_strings_from_expr(value, out);
            }
        }

        Expr::StringLit(s) => out.push(s.clone()),
        Expr::Unary { expr, .. } => collect_strings_from_expr(expr, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_strings_from_expr(lhs, out);
            collect_strings_from_expr(rhs, out);
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                collect_strings_from_expr(e, out);
            }
        }
        Expr::Field { base, .. } => collect_strings_from_expr(base, out),
        Expr::ArrayLit(elts) => {
            for e in elts {
                collect_strings_from_expr(e, out);
            }
        }
        Expr::Index { base, index, .. } => {
            collect_strings_from_expr(base, out);
            collect_strings_from_expr(index, out);
        }
        Expr::Call { callee, args, .. } => {
            collect_strings_from_expr(callee, out);
            for a in args {
                collect_strings_from_expr(a, out);
            }
        }
        Expr::Closure { body, .. } => {
            for s in body {
                collect_strings_from_stmt(s, out);
            }
        }
        _ => {}
    }
}

fn collect_strings_from_stmt(stmt: &Stmt, out: &mut Vec<String>) {
    match stmt {
        Stmt::Let(_, _, expr) => collect_strings_from_expr(expr, out),
        Stmt::Assign(_, expr) => collect_strings_from_expr(expr, out),
        Stmt::FieldAssign { value: expr, .. } => collect_strings_from_expr(expr, out),
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            collect_strings_from_expr(base, out);
            collect_strings_from_expr(index, out);
            collect_strings_from_expr(value, out);
        }
        Stmt::Return(Some(expr)) => collect_strings_from_expr(expr, out),
        Stmt::Declare(_, _) | Stmt::Return(None) => {}
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            collect_strings_from_expr(cond, out);
            for s in then_body {
                collect_strings_from_stmt(s, out);
            }
            for s in else_body {
                collect_strings_from_stmt(s, out);
            }
        }
        Stmt::Loop { body, .. } => {
            for s in body {
                collect_strings_from_stmt(s, out);
            }
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            collect_strings_from_expr(scrutinee, out);
            for arm in arms {
                if let Some(value) = parse_string_match_pattern(&arm.pattern) {
                    out.push(value);
                }
                for s in &arm.body {
                    collect_strings_from_stmt(s, out);
                }
            }
        }
        Stmt::Throw(expr) => collect_strings_from_expr(expr, out),
        Stmt::Try { body, catches, .. } => {
            for s in body {
                collect_strings_from_stmt(s, out);
            }
            for c in catches {
                if let Ok(MatchPattern::StringPat(value)) = MatchPattern::parse(&c.pattern) {
                    out.push(value);
                }
                for s in &c.body {
                    collect_strings_from_stmt(s, out);
                }
            }
        }
        Stmt::Expr(expr) => collect_strings_from_expr(expr, out),
        Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
    }
}

/// Collect all unique string literal contents from the module.
pub(crate) fn collect_string_literals(module: &UnifiedModule) -> Vec<String> {
    let mut strings = Vec::new();
    for decl in &module.decls {
        if let Decl::Function { body, .. } = decl {
            for stmt in body {
                collect_strings_from_stmt(stmt, &mut strings);
            }
        }
        if let Decl::Global {
            init: Some(expr), ..
        } = decl
        {
            collect_strings_from_expr(expr, &mut strings);
        }
    }
    strings.sort();
    strings.dedup();
    strings
}

fn lower_function(
    emitter: &mut CodeEmitter,
    func: &FunctionInfo,
    structs: &HashMap<String, Vec<(String, Typ)>>,
    functions: &HashMap<String, FunctionInfo>,
    globals: &HashMap<String, u64>,
    global_types: &HashMap<String, Typ>,
    pending_calls: &mut Vec<PendingCall>,
    is_interrupt: bool,
    runtime_error_slot: bool,
    string_layout: X86StringLayout,
) -> Result<Vec<PendingGlobal>, String> {
    super::lower::ensure_unit_sequence_layout(&func.ret)?;
    record_array_fields(structs, &func.ret)?;
    for (_, typ) in &func.params {
        record_array_fields(structs, typ)?;
    }

    for (_, typ) in &func.params {
        super::lower::ensure_unit_sequence_layout(typ)?;
    }
    // Validate return type and store for use in Return handling
    let _ret_is_struct = matches!(&func.ret, Typ::Named(_) | Typ::Array(_));
    match &func.ret {
        Typ::Int
        | Typ::Bool
        | Typ::Float
        | Typ::String
        | Typ::Void
        | Typ::Named(_)
        | Typ::Array(_)
        | Typ::Vector(_)
        | Typ::Generic(_) => {}
    }

    let mut ctx = LowerCtx::new(
        &func.name,
        &func.params,
        structs,
        functions,
        globals.clone(),
    );
    for (name, typ) in global_types {
        ctx.scalar_types
            .entry(name.clone())
            .or_insert_with(|| typ.clone());
    }
    ctx.string_layout = string_layout;
    ctx.is_interrupt = is_interrupt;
    ctx.runtime_error_slot = runtime_error_slot;
    ctx.ret_typ = func.ret.clone();
    // Pre-allocate locals for let bindings
    alloc_declared_locals(&mut ctx, &func.body)?;

    for name in crate::core_effects::error_bindings(&func.body) {
        if ctx.locals.contains_key(&name) {
            let offset = ctx.alloc_slot();
            ctx.error_types.insert(name, offset);
        }
    }
    ctx.error_flag_offset = ctx.frame_size;
    ctx.error_value_offset = ctx.frame_size + 8;
    ctx.frame_size += 24;

    if is_interrupt {
        // Interrupt prologue: save all GPRs, then standard frame.
        // The CPU already pushed SS/RSP/RFLAGS/CS/RIP (+error code for some).
        for &reg in &[
            RAX, RCX, RDX, RBX, RBP, RSI, RDI, 8u8, 9, 10, 11, 12, 13, 14, 15,
        ] {
            emitter.emit_insns(&x86_64::push_r(reg));
        }
        emitter.emit_insns(&emit_profile_prologue());
    } else {
        emitter.emit_insns(&emit_profile_prologue());
    }

    // Temporaries are allocated during lowering; patch every stack reset afterward.
    let frame_instruction = x86_64::sub_rsp_i32(0);
    ctx.frame_size_sites
        .push(emitter.len() + frame_instruction.len() as u32 - 4);
    emitter.emit_insns(&frame_instruction);

    let param_regs: &[u8] = if x86_64::is_32bit() {
        &[RDI, RSI, RDX, RCX]
    } else {
        &[RDI, RSI, RDX, RCX, 8, 9]
    };
    let stack_word = if x86_64::is_32bit() { 4 } else { 8 };
    // Harden saves RBX before establishing FP, shifting incoming stack arguments.
    let stack_arg_base = stack_word * (2 + usize::from(antidecomp::harden_active()));
    let mut struct_params = Vec::new();
    if !is_interrupt {
        for (i, (name, typ)) in func.params.iter().enumerate() {
            if let Some(StackSlot::Struct { fields }) = ctx.locals.get(name).cloned() {
                let Typ::Named(struct_name) = typ.canonical() else {
                    unreachable!()
                };
                if x86_64::is_32bit()
                    || crate::native_emit::struct_layout::x86_fields(ctx.structs, &struct_name)
                        .is_err()
                {
                    return Err("x86_64-lower: struct parameter needs a supported scalar-field 64-bit layout".into());
                }
                let pointer_slot = ctx.alloc_slot();
                if name == "self" {
                    ctx.receiver_pointers.insert(name.clone(), pointer_slot);
                }
                if i < param_regs.len() {
                    emitter.emit_insns(&x86_64::str64(param_regs[i], pointer_slot as u16));
                } else {
                    emitter.emit_insns(&x86_64::mov_r_m(
                        RAX,
                        RBP,
                        (stack_arg_base + (i - param_regs.len()) * stack_word) as i32,
                    ));
                    emitter.emit_insns(&x86_64::str64(RAX, pointer_slot as u16));
                }
                struct_params.push((pointer_slot, fields));
            } else if i < param_regs.len() {
                if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(name) {
                    emitter.emit_insns(&x86_64::str64(param_regs[i], *offset as u16));
                }
            } else if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(name) {
                // Stack params: load from caller's argument area and store to the
                // local slot so the body can reload via ldr64.
                let stack_offset = (stack_arg_base + (i - param_regs.len()) * stack_word) as i32;
                emitter.emit_insns(&x86_64::mov_r_m(RAX, RBP, stack_offset));
                emitter.emit_insns(&x86_64::str64(RAX, *offset as u16));
            }
        }
    } else {
        // Interrupt functions: do not write into [rbp+16+] (saved R15).
        // Store params to their stack slots so the body can reload via ldr64.
        for (i, (name, _)) in func.params.iter().enumerate() {
            if i < param_regs.len() {
                if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(name) {
                    emitter.emit_insns(&x86_64::str64(param_regs[i], *offset as u16));
                }
            }
        }
    }

    // All incoming registers have been saved before copying aggregate fields.
    for (pointer_slot, fields) in struct_params {
        emitter.emit_insns(&x86_64::ldr64(RAX, pointer_slot as u16));
        emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 1));
        let valid = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(0x07, 0));
        emit_failure_return(emitter, &mut ctx)?;
        emitter.patch_u32(valid + 2, emitter.len() - valid - 6);
        let mut offsets: Vec<_> = fields.values().copied().collect();
        offsets.sort_unstable_by(|lhs, rhs| rhs.cmp(lhs));
        for (index, offset) in offsets.into_iter().enumerate() {
            emitter.emit_insns(&x86_64::mov_r_m(RCX, RAX, (index * 8) as i32));
            emitter.emit_insns(&x86_64::str64(RCX, offset as u16));
        }
    }

    // Lower function body
    for stmt in &func.body {
        lower_stmt(emitter, &mut ctx, stmt, pending_calls)?;
    }

    if !ctx.break_sites.is_empty() || !ctx.continue_sites.is_empty() {
        return Err(format!(
            "x86_64-lower: loop jump outside a loop in `{}`",
            func.name
        ));
    }
    if ctx.frame_size > u16::MAX as u32 {
        return Err("x86_64-lower: stack frame exceeds supported slot offsets".into());
    }
    for site in &ctx.frame_size_sites {
        emitter.patch_u32(*site, ctx.frame_reserve());
    }
    let pending_globals = ctx.pending_globals;

    // A return in one branch does not terminate every fallthrough path.
    if func.ret.canonical() == Typ::Void || !ctx.emitted_return {
        if matches!(&func.ret, Typ::Named(_) | Typ::Array(_)) {
            // ponytail: missing aggregate returns require control-flow validation.
            emitter.emit_insns(&x86_64::load_i64(RAX, 0));
        } else if func.ret.canonical() == Typ::Void {
            emitter.emit_insns(&x86_64::zero_reg(RAX));
        }
        emit_function_epilogue(emitter, is_interrupt);
    }

    Ok(pending_globals)
}

fn alloc_declared_locals(ctx: &mut LowerCtx<'_>, body: &[Stmt]) -> Result<(), String> {
    for stmt in body {
        match stmt {
            Stmt::Declare(name, typ) => {
                if matches!(typ.canonical(), Typ::Void | Typ::Generic(_)) {
                    return Err(format!(
                        "x86_64-lower: declaration `{name}` needs a supported value layout"
                    ));
                }
                ctx.alloc_local(name, &typ.canonical())?;
                ctx.scalar_types.insert(name.clone(), typ.clone());
            }
            Stmt::Let(name, typ, expr) => {
                if x86_64::is_32bit()
                    && let Expr::ArrayLit(items) = expr
                {
                    let array_type = typ
                        .clone()
                        .or_else(|| scalar_expr_type(expr, ctx))
                        .unwrap_or(Typ::Array(Box::new(Typ::Void)));
                    record_array_fields(ctx.structs, &array_type)?;
                    ctx.scalar_types.insert(name.clone(), array_type);
                    if !ctx.locals.contains_key(name) {
                        let offsets = (0..items.len()).map(|_| ctx.alloc_slot()).collect();
                        ctx.locals
                            .insert(name.clone(), StackSlot::InlineArray { offsets });
                    }
                } else if let Some(typ) = typ {
                    ctx.alloc_local(name, typ)?;
                } else {
                    let typ = scalar_expr_type(expr, ctx).unwrap_or(Typ::Int);
                    ctx.alloc_local(name, &typ)?;
                }
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                alloc_declared_locals(ctx, then_body)?;
                alloc_declared_locals(ctx, else_body)?;
            }
            Stmt::Loop { body, .. } => {
                alloc_declared_locals(ctx, body)?;
            }
            Stmt::Match { scrutinee, arms } => {
                for arm in arms {
                    if let MatchPattern::IdentPat(name) = MatchPattern::parse(&arm.pattern)? {
                        let typ = scalar_expr_type(scrutinee, ctx)
                            .map(|typ| typ.canonical())
                            .ok_or("native-lower: unknown match binding type")?;
                        if !matches!(typ, Typ::Int | Typ::Bool | Typ::Float | Typ::String) {
                            return Err("native-lower: unsupported aggregate match binding".into());
                        }
                        ctx.alloc_local(&name, &typ)?;
                    }
                    alloc_declared_locals(ctx, &arm.body)?;
                }
            }
            Stmt::Throw(_) => {}
            Stmt::Try { body, catches, .. } => {
                alloc_declared_locals(ctx, body)?;
                for catch in catches {
                    if let MatchPattern::IdentPat(name) = MatchPattern::parse(&catch.pattern)? {
                        ctx.alloc_local(&name, &Typ::Int)?;
                    }
                    alloc_declared_locals(ctx, &catch.body)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn emit_error_exit(emitter: &mut CodeEmitter, ctx: &mut LowerCtx<'_>) -> Result<(), String> {
    if let Some(branches) = ctx.error_branches.last_mut() {
        let site = emitter.len();
        emitter.emit_insns(&x86_64::jmp_rel32(0));
        branches.push(site);
    } else {
        if matches!(
            ctx.ret_typ.canonical(),
            Typ::String | Typ::Named(_) | Typ::Array(_) | Typ::Vector(_) | Typ::Void
        ) {
            emitter.emit_insns(&x86_64::load_i64(RAX, 0));
        } else {
            load_error_global(emitter, ctx, "__inrt_err_val")?;
        }
        emit_function_epilogue(emitter, ctx.is_interrupt);
    }
    Ok(())
}

fn emit_error_check(emitter: &mut CodeEmitter, ctx: &mut LowerCtx<'_>) -> Result<(), String> {
    error_slot_offset(ctx, "__inrt_err_flag")?;
    emitter.emit_insns(&[0x41, 0x80, 0x3F, 0x00]); // cmp byte [r15], 0
    let continue_branch = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
    emit_error_exit(emitter, ctx)?;
    let delta = emitter.len() as i32 - continue_branch as i32 - 6;
    emitter.patch_u32(continue_branch + 2, delta as u32);
    Ok(())
}

fn lower_stmt(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    stmt: &Stmt,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    #[allow(unreachable_patterns)]
    match stmt {
        Stmt::Return(expr) => {
            if let Some(expr) = expr {
                let ret_typ = ctx.ret_typ.canonical();
                if ret_typ == Typ::Void
                    && !crate::core_ir::expr_diverges(expr)
                    && scalar_expr_type(expr, ctx).map(|typ| typ.canonical()) != Some(Typ::Void)
                {
                    return Err("x86_64-lower: void return requires a unit value".into());
                }

                if let Typ::Named(name) = &ret_typ
                    && ctx.structs.contains_key(name)
                {
                    if crate::native_emit::struct_layout::x86_fields(ctx.structs, name).is_err() {
                        return Err(
                            "x86_64-lower: struct return needs a supported scalar-field layout"
                                .into(),
                        );
                    }
                    let schema = crate::native_emit::struct_layout::x86_fields(ctx.structs, name)?;
                    let fields: HashMap<_, _> = schema
                        .iter()
                        .rev()
                        .map(|(field, _)| (field.clone(), ctx.alloc_slot()))
                        .collect();
                    store_struct_value(emitter, ctx, expr, &fields, pending_calls)?;
                    let mut offsets: Vec<_> = fields.values().copied().collect();
                    offsets.sort_unstable_by(|a, b| b.cmp(a));
                    lower_heap_array(emitter, ctx, &offsets, pending_calls)?;
                    emitter.emit_insns(&x86_64::add_rmi8(RAX, 16));
                } else if matches!(ret_typ, Typ::Named(_)) {
                    emitter.emit_insns(&x86_64::load_i64(RAX, 0));
                } else {
                    lower_typed_sequence_value(emitter, ctx, expr, &ret_typ, RAX, pending_calls)?;
                }
            } else {
                emitter.emit_insns(&x86_64::zero_reg(RAX));
            }
            emit_function_epilogue(emitter, ctx.is_interrupt);
            ctx.emitted_return = true;
            Ok(())
        }
        Stmt::Declare(name, typ) => {
            if !ctx.locals.contains_key(name) {
                return Err(format!(
                    "x86_64-lower: declaration `{name}` has no planned storage"
                ));
            }
            ctx.scalar_types.insert(name.clone(), typ.clone());
            Ok(())
        }
        Stmt::Let(name, typ, expr) => {
            if let Expr::Ident(source) = expr
                && let Some(slot @ StackSlot::InlineArray { .. }) = ctx.locals.get(source).cloned()
            {
                ctx.locals.insert(name.clone(), slot);
                if let Some(array_type) = ctx.scalar_types.get(source).cloned() {
                    ctx.scalar_types.insert(name.clone(), array_type);
                }
                return Ok(());
            }
            if let Expr::ArrayLit(items) = expr
                && let Some(StackSlot::InlineArray { offsets }) = ctx.locals.get(name).cloned()
            {
                for (item, offset) in items.iter().zip(offsets) {
                    lower_expr_into(emitter, ctx, item, RAX, pending_calls)?;
                    emitter.emit_insns(&x86_64::str64(RAX, offset as u16));
                }
                return Ok(());
            }
            if let Expr::StructInit {
                name: ty_name,
                fields,
            } = expr
            {
                if !ctx.locals.contains_key(name) {
                    let resolved = typ.clone().unwrap_or_else(|| Typ::Named(ty_name.clone()));
                    ctx.alloc_local(name, &resolved)?;
                }
                lower_struct_init(emitter, ctx, name, fields, pending_calls)?;
                return Ok(());
            }
            if !ctx.locals.contains_key(name) {
                let resolved = typ.clone().unwrap_or(Typ::Int);
                ctx.alloc_local(name, &resolved)?;
            }
            if let Some(StackSlot::Struct { fields }) = ctx.locals.get(name).cloned() {
                return store_struct_value(emitter, ctx, expr, &fields, pending_calls);
            }
            let destination = ctx.scalar_types.get(name).cloned().unwrap_or(Typ::Int);
            lower_typed_sequence_value(emitter, ctx, expr, &destination, RAX, pending_calls)?;
            if ctx.scalar_types.get(name).map(Typ::canonical) == Some(Typ::Void) {
                emitter.emit_insns(&x86_64::zero_reg(RAX));
            }
            store_local_error_type(emitter, ctx, name, expr)?;
            if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(name) {
                emitter.emit_insns(&x86_64::str64(RAX, *offset as u16));
            }
            Ok(())
        }
        Stmt::Assign(name, expr) => {
            // Local bindings and parameters shadow globals.
            if !ctx.locals.contains_key(name)
                && let Some(&offset) = ctx.globals.get(name)
            {
                if matches!(
                    ctx.scalar_types.get(name).map(Typ::canonical),
                    Some(Typ::Array(_))
                ) {
                    return Err("x86_64-lower: whole global array assignment is unsupported".into());
                }
                if let Some(Typ::Named(typ)) = ctx.scalar_types.get(name).map(Typ::canonical) {
                    let schema = ctx.structs.get(&typ).cloned().ok_or_else(||
                        "x86_64-lower: global struct assignment needs a known scalar-field layout".to_string())?;
                    if x86_64::is_32bit()
                        || schema.iter().any(|(_, typ)| {
                            !matches!(
                                typ.canonical(),
                                Typ::Int | Typ::Bool | Typ::Float | Typ::String
                            )
                        })
                    {
                        return Err("x86_64-lower: global struct assignment needs a scalar-field 64-bit layout".into());
                    }
                    let fields: HashMap<_, _> = schema
                        .iter()
                        .rev()
                        .map(|(field, _)| (field.clone(), ctx.alloc_slot()))
                        .collect();
                    // Stage the complete source before mutating the global.
                    store_struct_value(emitter, ctx, expr, &fields, pending_calls)?;
                    for (field, slot) in fields {
                        let key = format!("{name}.{field}");
                        let offset = ctx.globals.get(&key).copied().ok_or_else(|| {
                            format!("x86_64-lower: missing global struct field `{key}`")
                        })?;
                        emitter.emit_insns(&x86_64::ldr64(RAX, slot as u16));
                        let site = emitter.len() + 2;
                        emitter.emit_insns(&x86_64::mov_abs_from_rax(0));
                        ctx.pending_globals.push(PendingGlobal {
                            site,
                            width: 8,
                            offset,
                        });
                    }
                    return Ok(());
                }
                lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
                let addr_offset = if x86_64::is_32bit() { 1 } else { 2 };
                let site = emitter.len() + addr_offset;
                emitter.emit_insns(&x86_64::mov_abs_from_rax(0));
                ctx.pending_globals.push(PendingGlobal {
                    site,
                    width: if x86_64::is_32bit() { 4 } else { 8 },
                    offset,
                });
                return Ok(());
            }
            // ponytail: bracket name = array index or struct field not lowered by frontend, skip
            if name.contains('[') || name.contains(' ') {
                lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
                return Ok(());
            }
            if let Some(StackSlot::Struct { fields }) = ctx.locals.get(name).cloned() {
                store_struct_value(emitter, ctx, expr, &fields, pending_calls)?;
                // Receiver replacement commits only after the source copy completes.
                for (field, &slot) in &fields {
                    if let Some((pointer, offset)) = ctx.receiver_field(name, field) {
                        emitter.emit_insns(&x86_64::ldr64(RAX, slot as u16));
                        emitter.emit_insns(&x86_64::ldr64(RCX, pointer as u16));
                        emitter.emit_insns(&x86_64::mov_m_r(RCX, offset, RAX));
                    }
                }
                return Ok(());
            }
            let offset = match ctx.locals.get(name) {
                Some(StackSlot::Scalar(off)) => Some(*off),
                _ => None,
            };
            let destination = ctx.scalar_types.get(name).cloned().unwrap_or(Typ::Int);
            lower_typed_sequence_value(emitter, ctx, expr, &destination, RAX, pending_calls)?;
            if ctx.scalar_types.get(name).map(Typ::canonical) == Some(Typ::Void) {
                emitter.emit_insns(&x86_64::zero_reg(RAX));
            }
            if let Some(off) = offset {
                emitter.emit_insns(&x86_64::str64(RAX, off as u16));
                store_local_error_type(emitter, ctx, name, expr)?;
            }
            Ok(())
        }
        Stmt::FieldAssign {
            base, name, value, ..
        } => {
            if let Expr::Index { base: array, index } = base {
                let typ =
                    scalar_expr_type(array, ctx).ok_or("x86_64-lower: unknown record array")?;
                let FlatRecordArray { fields: schema, .. } =
                    record_array_fields(ctx.structs, &typ)?
                        .ok_or("x86_64-lower: field store requires a flat record array")?;
                let field = schema
                    .iter()
                    .position(|(field, _)| field == name)
                    .ok_or("x86_64-lower: unknown record array field")?;
                if scalar_expr_type(value, ctx).map(|typ| typ.canonical())
                    != Some(schema[field].1.canonical())
                {
                    return Err("x86_64-lower: record array field type mismatch".into());
                }
                let slots = stage_record_index(emitter, ctx, array, index, pending_calls)?;
                let rhs = ctx.alloc_slot();
                lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::str64(RAX, rhs as u16));
                load_record_index_address(emitter, ctx, slots)?;
                emitter.emit_insns(&x86_64::ldr64(RCX, rhs as u16));
                emitter.emit_insns(&x86_64::mov_m_r(RAX, (field * 8) as i32, RCX));
                return Ok(());
            }
            let (base_name, prefix) =
                crate::native_emit::lower::field_path(base).ok_or_else(|| {
                    format!("x86_64-lower: unsupported field assignment base for `{name}`")
                })?;
            let field = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if let Some((pointer, offset)) = ctx.receiver_field(base_name, &field) {
                lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::ldr64(RCX, pointer as u16));
                emitter.emit_insns(&x86_64::mov_m_r(RCX, offset, RAX));
                return Ok(());
            }
            if !ctx.locals.contains_key(base_name) {
                let field_key = format!("{base_name}.{field}");
                let offset = ctx.globals.get(&field_key).copied().ok_or_else(|| {
                    format!("x86_64-lower: unknown field assignment target `{field_key}`")
                })?;
                lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                let addr_offset = if x86_64::is_32bit() { 1 } else { 2 };
                let site = emitter.len() + addr_offset;
                emitter.emit_insns(&x86_64::mov_abs_from_rax(0));
                ctx.pending_globals.push(PendingGlobal {
                    site,
                    width: if x86_64::is_32bit() { 4 } else { 8 },
                    offset,
                });
                return Ok(());
            }
            let Some(StackSlot::Struct { fields }) = ctx.locals.get(base_name) else {
                return Err(format!(
                    "x86_64-lower: field assignment source `{base_name}` has no struct layout"
                ));
            };
            let field_offset = fields.get(&field).copied().ok_or_else(|| {
                format!("x86_64-lower: field `{field}` missing on assignment source `{base_name}`")
            })?;
            lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
            emitter.emit_insns(&x86_64::str64(RAX, field_offset as u16));
            Ok(())
        }
        Stmt::Expr(expr) => {
            lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
            Ok(())
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            if let Some(typ) = scalar_expr_type(base, ctx)
                && let Some(FlatRecordArray {
                    name,
                    fields: schema,
                }) = record_array_fields(ctx.structs, &typ)?
            {
                if scalar_expr_type(value, ctx).map(|typ| typ.canonical()) != Some(Typ::Named(name))
                {
                    return Err("x86_64-lower: record array replacement type mismatch".into());
                }
                let slots = stage_record_index(emitter, ctx, base, index, pending_calls)?;
                let fields: HashMap<_, _> = schema
                    .iter()
                    .rev()
                    .map(|(field, _)| (field.clone(), ctx.alloc_slot()))
                    .collect();
                store_struct_value(emitter, ctx, value, &fields, pending_calls)?;
                load_record_index_address(emitter, ctx, slots)?;
                for (index, (field, _)) in schema.iter().enumerate() {
                    emitter.emit_insns(&x86_64::ldr64(RCX, fields[field] as u16));
                    emitter.emit_insns(&x86_64::mov_m_r(RAX, (index * 8) as i32, RCX));
                }
                return Ok(());
            }
            if matches!(scalar_expr_type(base, ctx), Some(Typ::Array(elem)) if matches!(elem.canonical(), Typ::Array(_)))
            {
                return Err("x86_64-lower: whole nested array assignment is unsupported".into());
            }

            if let Expr::Ident(name) = base
                && let Some(StackSlot::InlineArray { offsets }) = ctx.locals.get(name).cloned()
            {
                let index_slot = ctx.alloc_slot();
                lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::str64(RAX, index_slot as u16));
                lower_expr_into(emitter, ctx, value, RSI, pending_calls)?;
                emitter.emit_insns(&x86_64::ldr64(RAX, index_slot as u16));
                emitter.emit_insns(&x86_64::load_i64(RDX, offsets.len() as i64));
                emitter.emit_insns(&x86_64::cmp_rr(RAX, RDX));
                let out_of_bounds = emitter.len();
                emitter.emit_insns(&x86_64::jcc_near(0x03, 0));
                if let Some(&first) = offsets.first() {
                    emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
                    emitter.emit_insns(&x86_64::load_i64(RDX, first as i64 + 8));
                    emitter.emit_insns(&x86_64::add_rr(RAX, RDX));
                    emitter.emit_insns(&x86_64::mov_rr(RDI, x86_64::REG_FP));
                    emitter.emit_insns(&x86_64::sub_rr(RDI, RAX));
                    emitter.emit_insns(&x86_64::mov_ptr_reg(RDI, RSI));
                }
                let end = emitter.len();
                emitter.emit_insns(&x86_64::jmp_rel32(0));
                emitter.patch_u32(out_of_bounds + 2, emitter.len() - out_of_bounds - 6);
                emit_failure_return(emitter, ctx)?;
                emitter.patch_u32(end + 1, emitter.len() - end - 5);
                return Ok(());
            }
            let base_slot = ctx.alloc_slot();
            let index_slot = ctx.alloc_slot();
            lower_expr_into(emitter, ctx, base, RAX, pending_calls)?;
            emitter.emit_insns(&x86_64::str64(RAX, base_slot as u16));
            lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
            emitter.emit_insns(&x86_64::str64(RAX, index_slot as u16));
            lower_expr_into(emitter, ctx, value, RSI, pending_calls)?;
            emitter.emit_insns(&x86_64::ldr64(RDI, base_slot as u16));
            emitter.emit_insns(&x86_64::ldr64(RAX, index_slot as u16));
            if scalar_expr_type(base, ctx) == Some(Typ::Int) {
                emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
                emitter.emit_insns(&x86_64::add_rr(RDI, RAX));
                emitter.emit_insns(&x86_64::mov_m_r(RDI, 0, RSI));
                return Ok(());
            }
            emitter.emit_insns(&x86_64::test_rr(RDI, RDI));
            let null = emitter.len();
            emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
            emitter.emit_insns(&x86_64::mov_r_m(RDX, RDI, 0));
            emitter.emit_insns(&x86_64::cmp_rr(RAX, RDX));
            let out_of_bounds = emitter.len();
            emitter.emit_insns(&x86_64::jcc_near(0x03, 0));
            emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
            emitter.emit_insns(&x86_64::add_rr(RDI, RAX));
            emitter.emit_insns(&x86_64::mov_m_r(RDI, 16, RSI));
            let end = emitter.len();
            emitter.emit_insns(&x86_64::jmp_rel32(0));
            emitter.patch_u32(null + 2, emitter.len() - null - 6);
            emitter.patch_u32(out_of_bounds + 2, emitter.len() - out_of_bounds - 6);
            emit_failure_return(emitter, ctx)?;
            emitter.patch_u32(end + 1, emitter.len() - end - 5);
            Ok(())
        }
        Stmt::Break => {
            // patched by lower_loop when inside a loop
            let site = emitter.len();
            emitter.emit_insns(&x86_64::jmp_rel32(0));
            ctx.break_sites.push(site);
            Ok(())
        }
        Stmt::Continue => {
            let site = emitter.len();
            emitter.emit_insns(&x86_64::jmp_rel32(0));
            ctx.continue_sites.push(site);
            Ok(())
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => lower_if(emitter, ctx, cond, then_body, else_body, pending_calls),
        Stmt::Loop {
            kind: LoopKind::For { binding },
            cond,
            body,
        } => {
            let mut counter = ctx.frame_size as usize;
            while [
                format!("__for_{counter}"),
                format!("__for_items_{counter}"),
                format!("__for_end_{counter}"),
            ]
            .iter()
            .any(|name| {
                ctx.locals.contains_key(name)
                    || ctx.globals.contains_key(name)
                    || ctx.functions.contains_key(name)
            }) {
                counter += 1;
            }
            let mut iterator = cond.clone().unwrap_or(Expr::ArrayLit(Vec::new()));
            crate::lower_core::normalize_range_iterator(&mut iterator, |name| {
                ctx.functions.contains_key(name)
                    || ctx.locals.contains_key(name)
                    || ctx.globals.contains_key(name)
            });
            let expanded = crate::lower_core::expand_for_loop(
                binding.clone(),
                iterator,
                body.clone(),
                &mut counter,
            );
            alloc_declared_locals(ctx, &expanded)?;
            for stmt in &expanded {
                lower_stmt(emitter, ctx, stmt, pending_calls)?;
            }
            Ok(())
        }
        Stmt::Propagate => emit_error_check(emitter, ctx),
        Stmt::Loop { cond, body, .. } => lower_loop(emitter, ctx, cond, body, pending_calls),
        Stmt::Match {
            scrutinee, arms, ..
        } => lower_match(emitter, ctx, scrutinee, arms, pending_calls),
        Stmt::Throw(expr) => {
            lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
            store_error_global(emitter, ctx, "__inrt_err_val")?;
            emit_error_type(emitter, ctx, expr)?;
            emitter.emit_insns(&x86_64::mov_m_r(
                15,
                error_slot_offset(ctx, "__inrt_err_type")?,
                RCX,
            ));
            emitter.emit_insns(&x86_64::load_i64(RAX, 1));
            store_error_global(emitter, ctx, "__inrt_err_flag")?;
            emit_error_exit(emitter, ctx)
        }
        Stmt::Try { body, catches, .. } => {
            emitter.emit_insns(&x86_64::load_i64(RAX, 0));
            store_error_global(emitter, ctx, "__inrt_err_flag")?;

            ctx.error_branches.push(Vec::new());
            for stmt in body {
                lower_stmt(emitter, ctx, stmt, pending_calls)?;
            }
            let error_branches = ctx.error_branches.pop().unwrap();

            load_error_global(emitter, ctx, "__inrt_err_flag")?;
            emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
            let handler_branch = emitter.len();
            emitter.emit_bytes(&[0x0F, 0x85, 0, 0, 0, 0]); // jne rel32 placeholder
            let end_branch = emitter.len();
            emitter.emit_insns(&x86_64::jmp_rel32(0)); // jmp end placeholder

            let handler_offset = emitter.len();
            for site in error_branches {
                emitter.patch_u32(site + 1, (handler_offset as i32 - site as i32 - 5) as u32);
            }
            emitter.emit_insns(&x86_64::lea_sp_from_fp());
            let frame_instruction = x86_64::sub_rsp_i32(0);
            ctx.frame_size_sites
                .push(emitter.len() + frame_instruction.len() as u32 - 4);
            emitter.emit_insns(&frame_instruction);
            let mut caught_branches = Vec::new();
            for catch_arm in catches {
                let pattern = MatchPattern::parse(&catch_arm.pattern)?;
                load_error_global(emitter, ctx, "__inrt_err_val")?;
                let mut type_misses = Vec::new();
                let tag = match &pattern {
                    MatchPattern::IntPat(_) => Some(1),
                    MatchPattern::BoolPat(_) => Some(2),
                    MatchPattern::StringPat(_) => Some(4),
                    _ => None,
                };
                if let Some(tag) = tag {
                    emitter.emit_insns(&x86_64::mov_r_m(
                        RCX,
                        15,
                        error_slot_offset(ctx, "__inrt_err_type")?,
                    ));
                    emitter.emit_insns(&x86_64::load_i64(RDX, tag));
                    emitter.emit_insns(&x86_64::cmp_rr(RCX, RDX));
                    let site = emitter.len();
                    emitter.emit_insns(&x86_64::jcc_near(0x05, 0));
                    type_misses.push(site);
                }
                let next_arm = match pattern {
                    MatchPattern::IntPat(value) => {
                        emitter.emit_insns(&x86_64::load_i64(RCX, value));
                        emitter.emit_insns(&x86_64::cmp_rr(RAX, RCX));
                        let site = emitter.len();
                        emitter.emit_insns(&x86_64::jcc_near(0x05, 0));
                        Some(site)
                    }
                    MatchPattern::BoolPat(value) => {
                        emitter.emit_insns(&x86_64::load_i64(RCX, i64::from(value)));
                        emitter.emit_insns(&x86_64::cmp_rr(RAX, RCX));
                        let site = emitter.len();
                        emitter.emit_insns(&x86_64::jcc_near(0x05, 0));
                        Some(site)
                    }
                    MatchPattern::StringPat(value) => {
                        emitter.emit_insns(&x86_64::push_r(RAX));
                        lower_string_lit(emitter, RCX, &value, pending_calls)?;
                        emitter.emit_insns(&x86_64::pop_r(RAX));
                        type_misses.extend(emit_string_match_test(emitter, ctx.string_layout)?);
                        None
                    }
                    MatchPattern::WildPat => None,
                    MatchPattern::IdentPat(name) => {
                        if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(&name) {
                            emitter.emit_insns(&x86_64::str64(RAX, *offset as u16));
                            if let Some(tag_offset) = ctx.error_types.get(&name) {
                                emitter.emit_insns(&x86_64::mov_r_m(
                                    RCX,
                                    15,
                                    error_slot_offset(ctx, "__inrt_err_type")?,
                                ));
                                emitter.emit_insns(&x86_64::str64(RCX, *tag_offset as u16));
                            }
                        }
                        None
                    }
                    // ponytail: aggregate catch matching is not lowered yet.
                    _ => {
                        return Err(format!(
                            "x86_64-lower: unsupported catch pattern `{}` in `{}`",
                            catch_arm.pattern, ctx.fn_name
                        ));
                    }
                };
                emitter.emit_insns(&x86_64::load_i64(RAX, 0));
                store_error_global(emitter, ctx, "__inrt_err_flag")?;
                for catch_stmt in &catch_arm.body {
                    lower_stmt(emitter, ctx, catch_stmt, pending_calls)?;
                }
                let site = emitter.len();
                emitter.emit_insns(&x86_64::jmp_rel32(0));
                caught_branches.push(site);
                type_misses.extend(next_arm);
                for site in type_misses {
                    emitter.patch_u32(site + 2, (emitter.len() as i32 - site as i32 - 6) as u32);
                }
            }
            emit_error_exit(emitter, ctx)?;

            let end_offset = emitter.len();
            for site in caught_branches {
                emitter.patch_u32(site + 1, (end_offset as i32 - site as i32 - 5) as u32);
            }

            // Patch handler branch (jne rel32)
            let handler_delta = handler_offset as i32 - handler_branch as i32 - 6;
            emitter.patch_u32(handler_branch + 2, handler_delta as u32);
            // Patch end branch (jmp rel32)
            let end_delta = end_offset as i32 - end_branch as i32 - 5;
            emitter.patch_u32(end_branch + 1, end_delta as u32);

            Ok(())
        }
        _ => Err(format!(
            "x86_64-lower: unsupported statement in `{}`",
            ctx.fn_name
        )),
    }
}

fn lower_if(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    cond: &Expr,
    then_body: &[Stmt],
    else_body: &[Stmt],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    lower_expr_into(emitter, ctx, cond, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 0));

    // Use near (rel32) jumps so that large if-else chains don't overflow
    // the 8-bit displacement of short jumps.
    let else_branch = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x04, 0)); // je rel32 placeholder

    for stmt in then_body {
        lower_stmt(emitter, ctx, stmt, pending_calls)?;
    }

    let end_branch = emitter.len();
    emitter.emit_insns(&x86_64::jmp_rel32(0)); // jmp rel32 placeholder

    // Patch else branch (je rel32: opcode is 2 bytes, displacement is 4 bytes)
    let else_offset = emitter.len();
    let else_delta = else_offset as i32 - else_branch as i32 - 6;
    emitter.patch_u32(else_branch + 2, else_delta as u32);

    for stmt in else_body {
        lower_stmt(emitter, ctx, stmt, pending_calls)?;
    }

    // Patch end branch (jmp rel32: opcode is 1 byte, displacement is 4 bytes)
    let end_offset = emitter.len();
    let end_delta = end_offset as i32 - end_branch as i32 - 5;
    emitter.patch_u32(end_branch + 1, end_delta as u32);

    Ok(())
}

fn lower_loop(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    cond: &Option<Expr>,
    body: &[Stmt],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let saved_breaks = std::mem::take(&mut ctx.break_sites);
    let saved_continues = std::mem::take(&mut ctx.continue_sites);
    let loop_start = emitter.len();

    let exit_branch = if let Some(cond) = cond {
        lower_expr_into(emitter, ctx, cond, RAX, pending_calls)?;
        emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 0));
        let exit_branch = emitter.len();
        emitter.emit_bytes(&[0x0F, 0x84, 0, 0, 0, 0]); // je rel32
        Some(exit_branch)
    } else {
        None
    };

    for stmt in body {
        lower_stmt(emitter, ctx, stmt, pending_calls)?;
    }

    let continue_target = loop_start;
    for site in std::mem::take(&mut ctx.continue_sites) {
        let delta = continue_target as i32 - site as i32 - 5;
        emitter.patch_u32((site + 1) as u32, delta as u32);
    }

    let loop_end = emitter.len();
    let back_delta = loop_start as i32 - loop_end as i32;
    if back_delta - 2 >= i8::MIN as i32 && back_delta - 2 <= i8::MAX as i32 {
        emitter.emit_insns(&x86_64::jmp_rel8((back_delta - 2) as i8));
    } else {
        emitter.emit_insns(&x86_64::jmp_rel32(back_delta - 5));
    }

    let after = emitter.len();
    if let Some(exit_branch) = exit_branch {
        let exit_delta = after as i32 - exit_branch as i32 - 6;
        emitter.patch_u32(exit_branch + 2, exit_delta as u32);
    }
    for site in std::mem::take(&mut ctx.break_sites) {
        let delta = after as i32 - site as i32 - 5;
        emitter.patch_u32((site + 1) as u32, delta as u32);
    }

    ctx.break_sites = saved_breaks;
    ctx.continue_sites = saved_continues;
    Ok(())
}

fn emit_string_match_test(
    emitter: &mut CodeEmitter,
    layout: X86StringLayout,
) -> Result<Vec<u32>, String> {
    if x86_64::is_32bit() {
        return Err("x86_64-lower: string matching requires 64-bit mode".into());
    }
    // Preserve RAX for later arms; R8/R9 walk the two payloads.
    let mut misses = Vec::with_capacity(3);
    emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
    misses.push(emitter.len());
    emitter.emit_insns(&x86_64::jcc_near(4, 0));
    emitter.emit_insns(&x86_64::test_rr(RCX, RCX));
    misses.push(emitter.len());
    emitter.emit_insns(&x86_64::jcc_near(4, 0));
    emitter.emit_insns(&x86_64::mov_rr(8, RAX));
    emitter.emit_insns(&x86_64::mov_rr(9, RCX));
    if layout == X86StringLayout::Cstring {
        let head = emitter.len();
        emitter.emit_insns(&x86_64::movzx_m8(RDX, 8));
        emitter.emit_insns(&x86_64::movzx_m8(RCX, 9));
        emitter.emit_insns(&x86_64::cmp_rr(RDX, RCX));
        misses.push(emitter.len());
        emitter.emit_insns(&x86_64::jcc_near(5, 0));
        emitter.emit_insns(&x86_64::test_rr(RDX, RDX));
        let end = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(4, 0));
        emitter.emit_insns(&x86_64::add_rmi8(8, 1));
        emitter.emit_insns(&x86_64::add_rmi8(9, 1));
        let back = emitter.len();
        emitter.emit_insns(&x86_64::jmp_rel32(head as i32 - back as i32 - 5));
        emitter.patch_u32(end + 2, (emitter.len() as i32 - end as i32 - 6) as u32);
        return Ok(misses);
    }
    emitter.emit_insns(&x86_64::mov_r_m(10, 8, 0));
    emitter.emit_insns(&x86_64::mov_r_m(11, 9, 0));
    emitter.emit_insns(&x86_64::cmp_rr(10, 11));
    misses.push(emitter.len());
    emitter.emit_insns(&x86_64::jcc_near(5, 0));
    emitter.emit_insns(&x86_64::test_rr(10, 10));
    let empty = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(4, 0));
    emitter.emit_insns(&x86_64::add_rmi8(8, 8));
    emitter.emit_insns(&x86_64::add_rmi8(9, 8));
    let head = emitter.len();
    emitter.emit_insns(&x86_64::movzx_m8(RDX, 8));
    emitter.emit_insns(&x86_64::movzx_m8(RCX, 9));
    emitter.emit_insns(&x86_64::cmp_rr(RDX, RCX));
    misses.push(emitter.len());
    emitter.emit_insns(&x86_64::jcc_near(5, 0));
    emitter.emit_insns(&x86_64::add_rmi8(8, 1));
    emitter.emit_insns(&x86_64::add_rmi8(9, 1));
    emitter.emit_insns(&x86_64::sub_rmi8(10, 1));
    let back = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(5, head as i32 - back as i32 - 6));
    emitter.patch_u32(empty + 2, (emitter.len() as i32 - empty as i32 - 6) as u32);
    Ok(misses)
}

fn lower_match(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    scrutinee: &Expr,
    arms: &[MatchArm],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    lower_expr_into(emitter, ctx, scrutinee, RAX, pending_calls)?;

    let mut end_branches = Vec::new();

    for arm in arms {
        let pattern = MatchPattern::parse(&arm.pattern)?;
        if let MatchPattern::IdentPat(name) = &pattern {
            let Some(StackSlot::Scalar(offset)) = ctx.locals.get(name) else {
                return Err("x86_64-lower: missing scalar match binding".into());
            };
            emitter.emit_insns(&x86_64::str64(RAX, *offset as u16));
            store_local_error_type(emitter, ctx, name, scrutinee)?;
        }
        if matches!(pattern, MatchPattern::WildPat | MatchPattern::IdentPat(_)) {
            for stmt in &arm.body {
                lower_stmt(emitter, ctx, stmt, pending_calls)?;
            }
            break;
        }
        let scalar = match &pattern {
            MatchPattern::IntPat(value) => Some((*value, Typ::Int)),
            MatchPattern::BoolPat(value) => Some((i64::from(*value), Typ::Bool)),
            _ => None,
        };
        if let Some((value, typ)) = scalar {
            let type_miss = if let Expr::Ident(name) = scrutinee
                && ctx.error_types.contains_key(name)
            {
                emit_error_type(emitter, ctx, scrutinee)?;
                emitter.emit_insns(&x86_64::load_i64(
                    RDX,
                    crate::core_effects::error_type_tag(&typ).unwrap(),
                ));
                emitter.emit_insns(&x86_64::cmp_rr(RCX, RDX));
                let site = emitter.len();
                emitter.emit_insns(&x86_64::jcc_near(5, 0));
                Some(site)
            } else {
                if scalar_expr_type(scrutinee, ctx) != Some(typ) {
                    return Err(
                        "x86_64-lower: scalar pattern requires a compatible scrutinee".into(),
                    );
                }
                None
            };
            // cmp rax, value
            emitter.emit_insns(&x86_64::load_i64(RCX, value));
            emitter.emit_insns(&x86_64::cmp_rr(RAX, RCX));
            let next_branch = emitter.len();
            // jne rel32
            emitter.emit_bytes(&[0x0F, 0x85, 0, 0, 0, 0]);

            for stmt in &arm.body {
                lower_stmt(emitter, ctx, stmt, pending_calls)?;
            }

            let end_branch = emitter.len();
            emitter.emit_insns(&x86_64::jmp_rel32(0));

            // Patch next_branch (jne rel32)
            let next_offset = emitter.len() as i32 - next_branch as i32 - 6;
            emitter.patch_u32(next_branch + 2, next_offset as u32);
            if let Some(site) = type_miss {
                emitter.patch_u32(site + 2, (emitter.len() as i32 - site as i32 - 6) as u32);
            }
            end_branches.push(end_branch);
        } else if let MatchPattern::StringPat(pat) = pattern {
            let type_miss = if let Expr::Ident(name) = scrutinee
                && ctx.error_types.contains_key(name)
            {
                emit_error_type(emitter, ctx, scrutinee)?;
                emitter.emit_insns(&x86_64::load_i64(RDX, 4));
                emitter.emit_insns(&x86_64::cmp_rr(RCX, RDX));
                let site = emitter.len();
                emitter.emit_insns(&x86_64::jcc_near(5, 0));
                Some(site)
            } else {
                if scalar_expr_type(scrutinee, ctx) != Some(Typ::String) {
                    return Err("x86_64-lower: string pattern requires a String scrutinee".into());
                }
                None
            };
            emitter.emit_insns(&x86_64::push_r(RAX));
            lower_string_lit(emitter, RCX, &pat, pending_calls)?;
            emitter.emit_insns(&x86_64::pop_r(RAX));
            let mut not_equal = emit_string_match_test(emitter, ctx.string_layout)?;
            not_equal.extend(type_miss);

            for stmt in &arm.body {
                lower_stmt(emitter, ctx, stmt, pending_calls)?;
            }

            let end_branch = emitter.len();
            emitter.emit_insns(&x86_64::jmp_rel32(0));

            for site in not_equal {
                emitter.patch_u32(site + 2, (emitter.len() as i32 - site as i32 - 6) as u32);
            }
            end_branches.push(end_branch);
        } else {
            return Err(format!(
                "x86_64-lower: unsupported match pattern `{}` in `{}`",
                arm.pattern.trim(),
                ctx.fn_name
            ));
        }
    }

    // Patch all end branches to jump here
    let end_offset = emitter.len();
    for branch in &end_branches {
        let delta = end_offset as i32 - *branch as i32 - 5;
        emitter.patch_u32(*branch + 1, delta as u32);
    }

    Ok(())
}

fn parse_string_match_pattern(pattern: &str) -> Option<String> {
    let trimmed = pattern.trim().trim_end_matches(':').trim();
    let trimmed = trimmed.strip_prefix("case ").unwrap_or(trimmed).trim();
    crate::core_ir::decode_string_literal(trimmed)
}

fn lower_int_lit(emitter: &mut CodeEmitter, target_reg: u8, value: i64) -> Result<(), String> {
    if antidecomp::harden_active() && target_reg == RAX {
        emitter.emit_insns(&antidecomp::weird_materialize_rax(value));
    } else {
        emitter.emit_insns(&x86_64::load_i64(target_reg, value));
    }
    Ok(())
}

fn lower_bool_lit(emitter: &mut CodeEmitter, target_reg: u8, value: bool) -> Result<(), String> {
    emitter.emit_insns(&x86_64::load_i64(target_reg, if value { 1 } else { 0 }));
    Ok(())
}

fn emit_error_type(
    emitter: &mut CodeEmitter,
    ctx: &LowerCtx<'_>,
    expr: &Expr,
) -> Result<(), String> {
    if let Expr::Ident(name) = expr
        && let Some(offset) = ctx.error_types.get(name)
    {
        emitter.emit_insns(&x86_64::ldr64(RCX, *offset as u16));
        return Ok(());
    }
    let tag = scalar_expr_type(expr, ctx)
        .as_ref()
        .and_then(crate::core_effects::error_type_tag)
        .ok_or_else(|| format!("x86_64-lower: unsupported throw payload type for {expr:?}"))?;
    emitter.emit_insns(&x86_64::load_i64(RCX, tag));
    Ok(())
}

fn store_local_error_type(
    emitter: &mut CodeEmitter,
    ctx: &LowerCtx<'_>,
    name: &str,
    expr: &Expr,
) -> Result<(), String> {
    if let Some(offset) = ctx.error_types.get(name) {
        emit_error_type(emitter, ctx, expr)?;
        emitter.emit_insns(&x86_64::str64(RCX, *offset as u16));
    }
    Ok(())
}

fn error_slot_offset(ctx: &LowerCtx<'_>, name: &str) -> Result<i32, String> {
    if !ctx.runtime_error_slot || x86_64::is_32bit() || ctx.is_interrupt {
        return Err(format!(
            "x86_64-lower: `{}` uses throw/try/propagate without a runtime-provided writable error slot",
            ctx.fn_name
        ));
    }
    match name {
        "__inrt_err_flag" => Ok(0),
        "__inrt_err_val" => Ok(8),
        "__inrt_err_type" => Ok(16),
        other => Err(format!("x86_64-lower: unknown error slot `{other}`")),
    }
}

fn store_error_global(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    name: &str,
) -> Result<(), String> {
    emitter.emit_insns(&x86_64::mov_m_r(15, error_slot_offset(ctx, name)?, RAX));
    Ok(())
}

fn load_error_global(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    name: &str,
) -> Result<(), String> {
    emitter.emit_insns(&x86_64::mov_r_m(RAX, 15, error_slot_offset(ctx, name)?));
    Ok(())
}

fn lower_ident_ref(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    name: &str,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if ctx.scalar_types.get(name).map(Typ::canonical) == Some(Typ::Void) {
        return lower_int_lit(emitter, target_reg, 0);
    }
    if let Some(&pointer) = ctx.receiver_pointers.get(name) {
        emitter.emit_insns(&x86_64::ldr64(target_reg, pointer as u16));
        return Ok(());
    }
    if matches!(ctx.locals.get(name), Some(StackSlot::InlineArray { .. })) {
        return Err("x86_64-lower: escaping 32-bit arrays require an aggregate ABI".into());
    }
    if let Some(StackSlot::Struct { fields }) = ctx.locals.get(name) {
        let base = fields
            .values()
            .max()
            .copied()
            .or_else(|| ctx.empty_struct_bases.get(name).copied())
            .ok_or("x86_64-lower: struct has no addressable storage")?;
        emitter.emit_insns(&x86_64::mov_rr(target_reg, RBP));
        emitter.emit_insns(&x86_64::sub_rmi32(target_reg, base as i32 + 8));
        return Ok(());
    }
    if !ctx.locals.contains_key(name)
        && matches!(
            ctx.scalar_types.get(name).map(Typ::canonical),
            Some(Typ::Array(_))
        )
        && let Some(&offset) = ctx.globals.get(name)
    {
        if x86_64::is_32bit() {
            return Err("x86_64-lower: global arrays require the 64-bit ABI".into());
        }
        let site = emitter.len() + 2;
        emitter.emit_insns(&x86_64::mov_ri64(target_reg, 0));
        ctx.pending_globals.push(PendingGlobal {
            site,
            width: 8,
            offset,
        });
        return Ok(());
    }
    if !ctx.locals.contains_key(name)
        && matches!(ctx.scalar_types.get(name).map(Typ::canonical), Some(Typ::Named(ref typ)) if ctx.structs.contains_key(typ))
        && let Some(&offset) = ctx.globals.get(name)
    {
        if let Some(Typ::Named(typ)) = ctx.scalar_types.get(name).map(Typ::canonical)
            && ctx.structs[&typ].iter().any(|(_, typ)| {
                !matches!(
                    typ.canonical(),
                    Typ::Int | Typ::Bool | Typ::Float | Typ::String
                )
            })
        {
            return Err("x86_64-lower: nested global struct layout is unsupported".into());
        }
        let site = emitter.len() + if x86_64::is_32bit() { 1 } else { 2 };
        emitter.emit_insns(&x86_64::mov_ri64(target_reg, 0));
        ctx.pending_globals.push(PendingGlobal {
            site,
            width: if x86_64::is_32bit() { 4 } else { 8 },
            offset,
        });
        return Ok(());
    }
    // Check if this is a global variable
    if let Some(&offset) = ctx.globals.get(name) {
        if target_reg == RAX {
            let addr_offset = if x86_64::is_32bit() { 1 } else { 2 };
            let site = emitter.len() + addr_offset;
            emitter.emit_insns(&x86_64::mov_rax_from_abs(0));
            ctx.pending_globals.push(PendingGlobal {
                site,
                width: if x86_64::is_32bit() { 4 } else { 8 },
                offset,
            });
        } else if x86_64::is_32bit() {
            let site = emitter.len() + 3;
            emitter.emit_insns(&x86_64::mov_r_from_abs32(target_reg, 0));
            ctx.pending_globals.push(PendingGlobal {
                site,
                width: 4,
                offset,
            });
        } else {
            let site = emitter.len() + 2;
            emitter.emit_insns(&x86_64::mov_ri64(target_reg, 0));
            emitter.emit_insns(&x86_64::mov_r_m(target_reg, target_reg, 0));
            ctx.pending_globals.push(PendingGlobal {
                site,
                width: 8,
                offset,
            });
        }
        return Ok(());
    }
    // Check if this is a function name (used as address/pointer)
    if ctx.functions.contains_key(name) {
        // Emit placeholder address (will be patched after all functions are laid out).
        let (placeholder, site_offset) = if target_reg == RAX {
            if x86_64::is_32bit() {
                let mut code = vec![0xB8];
                code.extend_from_slice(&[0xEF, 0xBE, 0xAD, 0xDE]);
                (code, emitter.len() + 1)
            } else {
                let mut code = vec![0x48, 0xB8];
                code.extend_from_slice(&[0xEF, 0xBE, 0xAD, 0xDE, 0x00, 0x00, 0x00, 0x00]);
                (code, emitter.len() + 2)
            }
        } else {
            (
                x86_64::mov_ri64(target_reg, 0xDEADBEEF),
                emitter.len() + if x86_64::is_32bit() { 1 } else { 2 },
            )
        };
        emitter.emit_insns(&placeholder);
        pending_calls.push(PendingCall {
            site: site_offset as u32,
            target: format!("@addr_{}", name),
        });
        return Ok(());
    }
    let offset = ctx.slot_offset(name).map_err(|_| {
        format!(
            "x86_64-lower: unresolved identifier `{name}` in `{}`",
            ctx.fn_name
        )
    })?;
    if target_reg == RAX {
        emitter.emit_insns(&x86_64::ldr64(target_reg, offset as u16));
    } else {
        emitter.emit_insns(&x86_64::ldr64(RAX, offset as u16));
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
    }
    Ok(())
}

fn lower_checked_div_or_mod(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    modulo: bool,
    unsigned: bool,
) -> Result<(), String> {
    emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
    emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
    emitter.emit_insns(&x86_64::test_rr(RCX, RCX));
    let nonzero = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(5, 0));
    emit_failure_return(emitter, ctx)?;
    let valid = emitter.len();
    emitter.patch_u32(nonzero + 2, (valid as i32 - nonzero as i32 - 6) as u32);

    if unsigned {
        emitter.emit_insns(&x86_64::zero_reg(RDX));
        emitter.emit_insns(&x86_64::div_reg(RCX));
        if modulo {
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
        }
        return Ok(());
    }

    // Match AArch64's wrapping MIN/-1 result without an x86 hardware fault.
    emitter.emit_insns(&x86_64::cmp_rmi8(RCX, 0xff));
    let ordinary_divisor = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(5, 0));
    let minimum = if x86_64::is_32bit() {
        i32::MIN as i64
    } else {
        i64::MIN
    };
    emitter.emit_insns(&x86_64::load_i64(RDX, minimum));
    emitter.emit_insns(&x86_64::cmp_rr(RAX, RDX));
    let ordinary_dividend = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(5, 0));
    if modulo {
        emitter.emit_insns(&x86_64::zero_reg(RAX));
    }
    let overflow_done = emitter.len();
    emitter.emit_insns(&x86_64::jmp_rel32(0));
    let divide = emitter.len();
    for branch in [ordinary_divisor, ordinary_dividend] {
        emitter.patch_u32(branch + 2, (divide as i32 - branch as i32 - 6) as u32);
    }
    emitter.emit_insns(&x86_64::sign_extend_dividend());
    emitter.emit_insns(&x86_64::idiv_reg(RCX));
    if modulo {
        emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
    }
    let end = emitter.len();
    emitter.patch_u32(
        overflow_done + 1,
        (end as i32 - overflow_done as i32 - 5) as u32,
    );
    Ok(())
}

fn scalar_expr_type(expr: &Expr, ctx: &LowerCtx<'_>) -> Option<Typ> {
    scalar_expr_type_with_bindings(expr, ctx, &HashMap::new())
}

fn scalar_expr_type_with_bindings(
    expr: &Expr,
    ctx: &LowerCtx<'_>,
    bindings: &HashMap<String, Typ>,
) -> Option<Typ> {
    if let Expr::Ident(name) = expr {
        if let Some(typ) = bindings.get(name) {
            return Some(typ.canonical());
        }
    }
    if let Expr::ArrayLit(items) = expr {
        return Some(Typ::Array(Box::new(
            items
                .iter()
                .filter(|item| !crate::core_ir::empty_sequence_literal(item))
                .find_map(|item| scalar_expr_type_with_bindings(item, ctx, bindings))
                .unwrap_or(Typ::Void),
        )));
    }
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            let typ = scalar_expr_type_with_bindings(scrutinee, ctx, bindings)?;
            let mut types = Vec::new();
            for arm in arms {
                let mut scoped = bindings.clone();
                let pattern = crate::core_ir::MatchPattern::parse(&arm.pattern).ok()?;
                for (name, typ) in pattern
                    .typed_bindings(&typ, &|name| ctx.structs.get(name).map(Vec::as_slice))
                    .ok()?
                {
                    scoped.insert(name, typ);
                }
                types.push(scalar_expr_type_with_bindings(&arm.value, ctx, &scoped));
            }
            crate::core_ir::match_value_type(arms, types)
        }
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => crate::core_ir::conditional_type(
            then_expr,
            scalar_expr_type_with_bindings(then_expr, ctx, bindings),
            else_expr,
            scalar_expr_type_with_bindings(else_expr, ctx, bindings),
        ),
        Expr::Block { body, result } => {
            let mut scoped = bindings.clone();
            for stmt in body {
                if let crate::core_ir::Stmt::Declare(name, typ) = stmt {
                    scoped.insert(name.clone(), typ.clone());
                }
                if let crate::core_ir::Stmt::Let(name, typ, value) = stmt {
                    if let Some(typ) = typ
                        .clone()
                        .or_else(|| scalar_expr_type_with_bindings(value, ctx, &scoped))
                    {
                        scoped.insert(name.clone(), typ);
                    }
                }
            }
            match result {
                Some(result) => scalar_expr_type_with_bindings(result, ctx, &scoped),
                None => Some(Typ::Void),
            }
        }

        Expr::Ident(name) => ctx
            .scalar_types
            .get(name)
            .map(Typ::canonical)
            .or_else(|| ctx.functions.contains_key(name).then_some(Typ::Int)),
        Expr::Index { base, .. } => {
            match scalar_expr_type_with_bindings(base, ctx, bindings)?.canonical() {
                Typ::Array(item) | Typ::Vector(item) => Some(*item),
                _ => None,
            }
        }
        Expr::Field { base, name } => {
            let mut typ = scalar_expr_type_with_bindings(base, ctx, bindings)?;
            for field in name.split('.') {
                let Typ::Named(struct_name) = typ else {
                    return None;
                };
                typ = ctx
                    .structs
                    .get(&struct_name)?
                    .iter()
                    .find(|(name, _)| name == field)?
                    .1
                    .canonical();
            }
            Some(typ)
        }
        Expr::Call { callee, .. } => match callee.as_ref() {
            Expr::Ident(name) => ctx
                .functions
                .get(name)
                .map(|f| f.ret.canonical())
                .or_else(|| crate::core_ir_verifier::intrinsic_return_type(name).cloned()),
            _ => None,
        },
        Expr::Unary { op, expr }
            if op == "-"
                || op == "vector-clone"
                || crate::core_ir::array_copy::count(op).is_some() =>
        {
            let typ = scalar_expr_type_with_bindings(expr, ctx, bindings);
            if op == "-" && typ.as_ref().is_some_and(|typ| typ.canonical() == Typ::Bool) {
                Some(Typ::Int)
            } else {
                typ
            }
        }
        Expr::Unary { op, .. } if op == "!" => Some(Typ::Bool),
        Expr::Unary { op, .. }
            if op == "~"
                || op == "bool-to-int"
                || op == "float-to-int"
                || crate::core_ir::float_to_bits_width(op).is_some()
                || crate::core_ir::integer_cast_spec(op).is_some()
                || crate::core_ir::saturating_float_cast_spec(op).is_some() =>
        {
            Some(Typ::Int)
        }
        Expr::Unary { op, .. }
            if crate::core_ir::integer_float_cast_spec(op).is_some()
                || crate::core_ir::bits_to_float_width(op).is_some()
                || matches!(op.as_str(), "float-cast-f32" | "float-cast-f64") =>
        {
            Some(Typ::Float)
        }
        Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "&" | "|" | "^" | "xor") => {
            crate::core_ir::bitwise_result_type(
                scalar_expr_type_with_bindings(lhs, ctx, bindings),
                scalar_expr_type_with_bindings(rhs, ctx, bindings),
            )
        }
        Expr::Binary { op, lhs, rhs }
            if matches!(op.as_str(), "<<" | ">>" | "%" | "lshr" | "udiv" | "urem") =>
        {
            if scalar_expr_type_with_bindings(lhs, ctx, bindings) == Some(Typ::Int)
                && scalar_expr_type_with_bindings(rhs, ctx, bindings) == Some(Typ::Int)
            {
                Some(Typ::Int)
            } else {
                None
            }
        }
        Expr::Binary { op, .. }
            if matches!(
                op.as_str(),
                "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||"
            ) =>
        {
            Some(Typ::Bool)
        }

        Expr::Binary { op, lhs, rhs }
            if op == "+"
                && [lhs, rhs].into_iter().all(|value| {
                    scalar_expr_type_with_bindings(value, ctx, bindings)
                        .is_some_and(|typ| typ.canonical() == Typ::String)
                }) =>
        {
            Some(Typ::String)
        }
        Expr::Binary { op, lhs, rhs }
            if matches!(
                op.as_str(),
                "+" | "-" | "*" | "/" | "+=" | "-=" | "*=" | "/="
            ) =>
        {
            if scalar_expr_type_with_bindings(lhs, ctx, bindings) == Some(Typ::Float)
                || scalar_expr_type_with_bindings(rhs, ctx, bindings) == Some(Typ::Float)
            {
                Some(Typ::Float)
            } else {
                Some(Typ::Int)
            }
        }
        _ => crate::native_emit::lower::expr_type(expr),
    }
}

fn lower_binary_expr(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let string_concat = op == "+"
        && [lhs, rhs].into_iter().all(|value| {
            scalar_expr_type(value, ctx).is_some_and(|typ| typ.canonical() == Typ::String)
        });
    // shortcut: allocating String concat needs the hosted ABI; add a target runtime before freestanding support.
    if string_concat
        && (x86_64::is_32bit()
            || !ctx.runtime_error_slot
            || ctx.string_layout != X86StringLayout::Instring
            || ctx.is_interrupt)
    {
        return Err(
            "x86_64-lower: String concatenation requires the hosted 64-bit Instring JIT runtime"
                .into(),
        );
    }
    if matches!(op, "&&" | "||") {
        lower_expr_into(emitter, ctx, lhs, RAX, pending_calls)?;
        emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
        let skip_rhs = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(if op == "&&" { 4 } else { 5 }, 0));
        lower_expr_into(emitter, ctx, rhs, RAX, pending_calls)?;
        let delta = emitter.len() as i32 - skip_rhs as i32 - 6;
        emitter.patch_u32(skip_rhs + 2, delta as u32);
        emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
        emitter.emit_insns(&x86_64::setcc(0x95, RAX));
        emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
        return Ok(());
    }
    lower_expr_into(emitter, ctx, lhs, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::push_r(RAX));
    lower_expr_into(emitter, ctx, rhs, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::pop_r(RDX));

    if matches!(op, "==" | "!=" | "===" | "!==")
        && [lhs, rhs].into_iter().all(|value| {
            scalar_expr_type(value, ctx).is_some_and(|typ| typ.canonical() == Typ::String)
        })
    {
        // Operands were evaluated once, left-to-right, above. The shared matcher
        // consumes RAX/RCX and leaves mismatch branches for its caller to resolve.
        emitter.emit_insns(&x86_64::mov_rr(RCX, RDX));
        let misses = emit_string_match_test(emitter, ctx.string_layout)?;
        let equal = matches!(op, "==" | "===");
        emitter.emit_insns(&x86_64::load_i64(RAX, i64::from(equal)));
        let end = emitter.len();
        emitter.emit_insns(&x86_64::jmp_rel32(0));
        for site in misses {
            emitter.patch_u32(site + 2, (emitter.len() as i32 - site as i32 - 6) as u32);
        }
        emitter.emit_insns(&x86_64::load_i64(RAX, i64::from(!equal)));
        emitter.patch_u32(end + 1, (emitter.len() as i32 - end as i32 - 5) as u32);
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
        return Ok(());
    }

    if string_concat {
        let wrapper =
            jit_stdlib_wrapper("str-concat").ok_or("missing String concat runtime alias")?;
        let ptr = crate::native_emit::native_link::resolve_native_fn(wrapper)
            .or_else(|| {
                // Public low-level JIT lowering may run before a JitRuntime is constructed.
                crate::native_emit::native_link::bootstrap_jit_native();
                crate::native_emit::native_link::resolve_native_fn(wrapper)
            })
            .ok_or("x86_64-lower: String concat runtime wrapper unavailable")?;
        emitter.emit_insns(&x86_64::mov_rr(RDI, RDX));
        emitter.emit_insns(&x86_64::mov_rr(RSI, RAX));
        emit_aligned_host_call(emitter, ptr);
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
        return Ok(());
    }

    // ponytail: Float uses the owned integer ABI; typed C FFI needs XMM argument/return marshalling.
    let lhs_float = scalar_expr_type(lhs, ctx) == Some(Typ::Float);
    let rhs_float = scalar_expr_type(rhs, ctx) == Some(Typ::Float);
    if lhs_float || rhs_float {
        if x86_64::is_32bit() {
            return Err("x86_64-lower: Float requires 64-bit mode".into());
        }
        emitter.emit_insns(&if lhs_float {
            x86_64::movq_xmm_gpr(0, RDX, true)
        } else {
            x86_64::cvtsi2sd(0, RDX)
        });
        emitter.emit_insns(&if rhs_float {
            x86_64::movq_xmm_gpr(1, RAX, true)
        } else {
            x86_64::cvtsi2sd(1, RAX)
        });
        match op {
            "+" | "+=" | "-" | "-=" | "*" | "*=" | "/" | "/=" => {
                let opcode = match op {
                    "+" | "+=" => 0x58,
                    "-" | "-=" => 0x5c,
                    "*" | "*=" => 0x59,
                    _ => 0x5e,
                };
                emitter.emit_insns(&x86_64::scalar_double(opcode, 0, 1));
                emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, false));
            }
            "==" | "!=" | "<" | "<=" | ">" | ">=" => {
                emitter.emit_insns(&x86_64::ucomisd(0, 1));
                let condition = match op {
                    "==" => 0x94,
                    "!=" => 0x95,
                    "<" => 0x92,
                    "<=" => 0x96,
                    ">" => 0x97,
                    _ => 0x93,
                };
                emitter.emit_insns(&x86_64::setcc(condition, RAX));
                emitter.emit_insns(&x86_64::setcc(if op == "!=" { 0x9a } else { 0x9b }, RDX));
                emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
                emitter.emit_insns(&x86_64::movzx_r8(RDX, RDX));
                emitter.emit_insns(&if op == "!=" {
                    x86_64::or_rr(RAX, RDX)
                } else {
                    x86_64::and_rr(RAX, RDX)
                });
            }
            _ => return Err(format!("x86_64-lower: unsupported Float operator `{op}`")),
        }
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
        return Ok(());
    }

    match op {
        "+" => {
            emitter.emit_insns(&x86_64::add_rr(RAX, RDX));
        }
        "-" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::sub_rr(RAX, RCX));
        }
        "*" => {
            emitter.emit_insns(&x86_64::imul_rr(RAX, RDX));
        }
        ">" => {
            emitter.emit_insns(&x86_64::cmp_rr(RDX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x9F, RAX)); // setg al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        ">=" => {
            emitter.emit_insns(&x86_64::cmp_rr(RDX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x9D, RAX)); // setge al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "<=" => {
            emitter.emit_insns(&x86_64::cmp_rr(RDX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x9E, RAX)); // setle al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "/" | "/=" | "%" | "%=" => {
            lower_checked_div_or_mod(emitter, ctx, matches!(op, "%" | "%="), false)?;
        }
        "udiv" | "urem" => {
            lower_checked_div_or_mod(emitter, ctx, op == "urem", true)?;
        }
        "lshr" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::shr_reg_cl(RAX));
        }
        "<" => {
            emitter.emit_insns(&x86_64::cmp_rr(RDX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x9C, RAX)); // setl al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "==" | "=" => {
            emitter.emit_insns(&x86_64::cmp_rr(RDX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x94, RAX)); // sete al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "!=" => {
            emitter.emit_insns(&x86_64::cmp_rr(RDX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x95, RAX)); // setne al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "^" | "xor" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::xor_rr(RAX, RCX));
        }
        "<<" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::shl_reg_cl(RAX));
        }
        ">>" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::sar_reg_cl(RAX));
        }
        "&" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::and_rr(RAX, RCX));
        }
        "+=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::add_rr(RAX, RCX));
        }
        "-=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::sub_rr(RAX, RCX));
        }
        "|" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::or_rr(RAX, RCX));
        }
        "*=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::imul_rr(RAX, RCX));
        }

        "&=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::and_rr(RAX, RCX));
        }
        "|=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::or_rr(RAX, RCX));
        }
        "^=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::xor_rr(RAX, RCX));
        }
        "<<=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::shl_reg_cl(RAX));
        }
        ">>=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
            emitter.emit_insns(&x86_64::sar_reg_cl(RAX));
        }

        _ => {
            return Err(format!(
                "x86_64-lower: unsupported operator `{op}` in `{}`",
                ctx.fn_name
            ));
        }
    }

    if target_reg != RAX {
        emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
    }
    Ok(())
}

fn lower_builtin_call(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_name: &str,
    target_reg: u8,
    args: &[Expr],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<bool, String> {
    if crate::core_atomic::signature(target_name).is_some()
        && ctx.functions.contains_key(target_name)
    {
        return Err("canonical atomic intrinsic name is redeclared".into());
    }
    if ctx
        .functions
        .get(target_name)
        .is_some_and(|function| !function.body.is_empty())
    {
        return Ok(false);
    }
    if matches!(
        target_name,
        "str-len" | "str_len" | "__inrt_str_len" | "array-len" | "array_len"
    ) {
        crate::native_emit::stdlib_wrappers::validate_length_call(
            target_name,
            ctx.functions
                .get(target_name)
                .map(|function| (function.params.as_slice(), &function.ret)),
            args,
            args.iter().map(|argument| scalar_expr_type(argument, ctx)),
        )
        .map_err(|error| {
            format!(
                "x86_64-lower: incompatible length {} `{target_name}`",
                error.category()
            )
        })?;
    }
    if matches!(
        target_name,
        "load8" | "load16" | "load32" | "load64" | "store8" | "store16" | "store32" | "store64"
    ) {
        crate::native_emit::stdlib_wrappers::validate_intrinsic_call(
            target_name,
            ctx.functions
                .get(target_name)
                .map(|function| (function.params.as_slice(), &function.ret)),
            args.iter().map(|argument| scalar_expr_type(argument, ctx)),
        )
        .map_err(|error| {
            format!(
                "x86_64-lower: incompatible MMIO {} `{target_name}`",
                error.category()
            )
        })?;
    }
    if !ctx.functions.contains_key(target_name) {
        if let Some(order) = crate::core_atomic::ordering(&Expr::Call {
            callee: Box::new(Expr::Ident(target_name.into())),
            args: args.to_vec(),
        }) {
            emitter.emit_insns(&x86_64::load_i64(target_reg, order));
            return Ok(true);
        }
        if let Some((kind, op)) = crate::core_atomic::operation(target_name) {
            if ctx.structs.contains_key(kind.type_name()) {
                return Err(
                    "atomic intrinsics cannot operate on a user-defined struct layout".into(),
                );
            }
            if op == "new" || x86_64::is_32bit() {
                return Err(
                    "x86_64-lower: atomics require static cells and a 64-bit target".into(),
                );
            }
            let expected = if op == "load" { 2 } else { 3 };
            if args.len() != expected {
                return Err("x86_64-lower: invalid atomic argument count".into());
            }
            if let Expr::Call { callee, .. } = args.last().unwrap() {
                if let Expr::Ident(name) = callee.as_ref() {
                    if ctx.functions.contains_key(name) {
                        return Err(
                            "atomic ordering constant is shadowed by a declared function".into(),
                        );
                    }
                }
            }
            crate::core_atomic::check_order(op, args.last().unwrap())?;
            let Expr::Ident(name) = &args[0] else {
                return Err("x86_64-lower: atomic receiver must be a static cell".into());
            };
            if ctx.locals.contains_key(name) || ctx.scalar_types.get(name) != Some(&kind.typ()) {
                return Err("x86_64-lower: atomic receiver must be a matching static cell".into());
            }
            let offset = *ctx
                .globals
                .get(name)
                .ok_or("x86_64-lower: atomic receiver has no static storage")?;
            if op != "load" {
                lower_expr_into(emitter, ctx, &args[1], RAX, pending_calls)?;
            }
            let site = emitter.len() + 2;
            emitter.emit_insns(&x86_64::mov_ri64(RDI, 0));
            ctx.pending_globals.push(PendingGlobal {
                site,
                width: 8,
                offset,
            });
            match op {
                "load" => emitter.emit_insns(if kind.wide() {
                    &[0x48, 0x8b, 0x07]
                } else {
                    &[0x8b, 0x07]
                }),
                "store" => {
                    emitter.emit_insns(if kind.wide() {
                        &[0x48, 0x87, 0x07]
                    } else {
                        &[0x87, 0x07]
                    });
                    emitter.emit_insns(&x86_64::load_i64(RAX, 0));
                }
                "fetch_add" => emitter.emit_insns(if kind.wide() {
                    &[0xf0, 0x48, 0x0f, 0xc1, 0x07]
                } else {
                    &[0xf0, 0x0f, 0xc1, 0x07]
                }),
                _ => unreachable!(),
            }
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
            }
            return Ok(true);
        }
    }
    match target_name {
        "str-len" | "str_len" | "__inrt_str_len" if args.len() == 1 => {
            if x86_64::is_32bit() {
                return Err("x86_64-lower: string length requires 64-bit mode".into());
            }
            lower_expr_into(emitter, ctx, &args[0], target_reg, pending_calls)?;
            emitter.emit_insns(&x86_64::test_rr(target_reg, target_reg));
            let empty = emitter.len();
            emitter.emit_insns(&x86_64::jcc_near(4, 0));
            emitter.emit_insns(&x86_64::mov_r_m(target_reg, target_reg, 0));
            emitter.patch_u32(empty + 2, (emitter.len() as i32 - empty as i32 - 6) as u32);
            Ok(true)
        }
        "vector-with-capacity-int" | "vector-with-capacity-float" | "vector-with-capacity-bool"
            if args.len() == 1 =>
        {
            if scalar_expr_type(&args[0], ctx).map(|typ| typ.canonical()) != Some(Typ::Int) {
                return Err("x86_64-lower: vector allocation capacity must be an integer".into());
            }
            if !ctx.runtime_error_slot || x86_64::is_32bit() || ctx.is_interrupt {
                return Err(
                    "x86_64-lower: vector allocation requires the 64-bit JIT runtime".into(),
                );
            }
            lower_expr_into(emitter, ctx, &args[0], RSI, pending_calls)?;
            emitter.emit_insns(&x86_64::mov_rr(RDI, 15));
            emitter.emit_insns(&x86_64::load_i64(
                RAX,
                crate::jit_runtime::reserve_array as *const () as i64,
            ));
            emitter.emit_insns(&x86_64::mov_rr(11, x86_64::REG_SP));
            emitter.emit_insns(&x86_64::load_i64(10, -16));
            emitter.emit_insns(&x86_64::and_rr(x86_64::REG_SP, 10));
            emitter.emit_insns(&x86_64::sub_rsp_i8(16));
            emitter.emit_insns(&x86_64::mov_m_r(x86_64::REG_SP, 0, 11));
            emitter.emit_insns(&[0xFF, 0xD0]);
            emitter.emit_insns(&x86_64::mov_r_m(x86_64::REG_SP, x86_64::REG_SP, 0));
            emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
            let valid = emitter.len();
            emitter.emit_insns(&x86_64::jcc_near(0x05, 0));
            emit_failure_return(emitter, ctx)?;
            emitter.patch_u32(valid + 2, emitter.len() - valid - 6);
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
            }
            Ok(true)
        }
        "vector-capacity" if args.len() == 1 => {
            if !matches!(scalar_expr_type(&args[0], ctx), Some(Typ::Vector(_))) {
                return Err("x86_64-lower: vector capacity requires vector storage".into());
            }
            lower_expr_into(emitter, ctx, &args[0], target_reg, pending_calls)?;
            emitter.emit_insns(&x86_64::test_rr(target_reg, target_reg));
            let empty = emitter.len();
            emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
            emitter.emit_insns(&x86_64::mov_r_m(target_reg, target_reg, 8));
            emitter.patch_u32(empty + 2, emitter.len() - empty - 6);
            Ok(true)
        }
        "array-len" | "array_len" if args.len() == 1 => {
            if let Expr::Ident(name) = &args[0]
                && let Some(StackSlot::InlineArray { offsets }) = ctx.locals.get(name)
            {
                emitter.emit_insns(&x86_64::load_i64(target_reg, offsets.len() as i64));
                return Ok(true);
            }
            let len = match &args[0] {
                Expr::ArrayLit(items) => {
                    for item in items {
                        lower_expr_into(emitter, ctx, item, target_reg, pending_calls)?;
                    }
                    items.len() as i64
                }
                expr if matches!(
                    scalar_expr_type(expr, ctx),
                    Some(Typ::Array(_) | Typ::Vector(_))
                ) =>
                {
                    lower_expr_into(emitter, ctx, expr, target_reg, pending_calls)?;
                    emitter.emit_insns(&x86_64::test_rr(target_reg, target_reg));
                    let empty = emitter.len();
                    emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
                    emitter.emit_insns(&x86_64::mov_r_m(target_reg, target_reg, 0));
                    emitter.patch_u32(empty + 2, emitter.len() - empty - 6);
                    return Ok(true);
                }
                _ => return Err("x86_64-lower: unsupported array-len argument".into()),
            };
            emitter.emit_insns(&x86_64::load_i64(target_reg, len));
            Ok(true)
        }
        "hlt" => {
            emitter.emit_bytes(&[0xF4]);
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::xor_rr(target_reg, target_reg));
            }
            Ok(true)
        }
        "pause" => {
            emitter.emit_bytes(&[0xF3, 0x90]);
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::xor_rr(target_reg, target_reg));
            }
            Ok(true)
        }
        "cli" => {
            emitter.emit_bytes(&[0xFA]);
            Ok(true)
        }
        "sti" => {
            emitter.emit_bytes(&[0xFB]);
            Ok(true)
        }
        "outb" => {
            if args.len() >= 2 {
                lower_expr_into(emitter, ctx, &args[0], RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::push_r(RAX));
                let val_reg = if x86_64::is_32bit() { RCX } else { RSI };
                lower_expr_into(emitter, ctx, &args[1], val_reg, pending_calls)?;
                emitter.emit_insns(&x86_64::pop_r(RDX));
                if x86_64::is_32bit() {
                    emitter.emit_bytes(&[0x88, 0xC8]); // mov al, cl
                } else {
                    emitter.emit_bytes(&[0x40, 0x88, 0xF0]); // mov al, sil
                }
                emitter.emit_bytes(&[0xEE]); // out dx, al
            } else {
                return Err(format!(
                    "x86_64-lower: `outb` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "inb" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_bytes(&[0x66, 0x89, 0xFA]); // mov dx, di
                emitter.emit_bytes(&[0xEC]); // in al, dx
                emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
            } else {
                return Err(format!(
                    "x86_64-lower: `inb` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "outl" => {
            if args.len() >= 2 {
                lower_expr_into(emitter, ctx, &args[0], RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::push_r(RAX));
                lower_expr_into(emitter, ctx, &args[1], RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::pop_r(RDX));
                emitter.emit_bytes(&[0xEF]); // out dx, eax
            } else {
                return Err(format!(
                    "x86_64-lower: `outl` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "inl" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_bytes(&[0x66, 0x89, 0xFA]); // mov dx, di
                emitter.emit_bytes(&[0xED]); // in eax, dx
            } else {
                return Err(format!(
                    "x86_64-lower: `inl` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "outw" => {
            if args.len() >= 2 {
                lower_expr_into(emitter, ctx, &args[0], RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::push_r(RAX));
                lower_expr_into(emitter, ctx, &args[1], RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::pop_r(RDX));
                emitter.emit_bytes(&[0x66, 0xEF]); // out dx, ax (16-bit)
            } else {
                return Err(format!(
                    "x86_64-lower: `outw` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "inw" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_bytes(&[0x66, 0x89, 0xFA]); // mov dx, di
                emitter.emit_bytes(&[0x66, 0xED]); // in ax, dx (16-bit)
                emitter.emit_insns(&x86_64::movzx_r16(RAX, RAX));
            } else {
                return Err(format!(
                    "x86_64-lower: `inw` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "load8" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RCX, pending_calls)?;
                emitter.emit_insns(&x86_64::movzx_m8(RAX, RCX));
                if target_reg != RAX {
                    emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
                }
            } else {
                return Err(format!(
                    "x86_64-lower: `load8` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "store8" => {
            if args.len() >= 2 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_insns(&x86_64::push_r(RDI));
                let val_reg = if x86_64::is_32bit() { RCX } else { RSI };
                lower_expr_into(emitter, ctx, &args[1], val_reg, pending_calls)?;
                emitter.emit_insns(&x86_64::pop_r(RDI));
                if x86_64::is_32bit() {
                    emitter.emit_bytes(&[0x88, 0x0F]); // mov [edi], cl
                } else {
                    emitter.emit_bytes(&[0x40, 0x88, 0x37]); // mov [rdi], sil
                }
            } else {
                return Err(format!(
                    "x86_64-lower: `store8` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "load16" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RCX, pending_calls)?;
                emitter.emit_insns(&x86_64::movzx_m16(RAX, RCX));
                if target_reg != RAX {
                    emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
                }
            } else {
                return Err(format!(
                    "x86_64-lower: `load16` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "store16" => {
            if args.len() >= 2 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_insns(&x86_64::push_r(RDI));
                lower_expr_into(emitter, ctx, &args[1], RSI, pending_calls)?;
                emitter.emit_insns(&x86_64::pop_r(RDI));
                emitter.emit_bytes(&[0x66, 0x89, 0x37]); // mov [rdi], si
            } else {
                return Err(format!(
                    "x86_64-lower: `store16` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "load32" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RCX, pending_calls)?;
                emitter.emit_bytes(&[0x8B, 0x01]); // mov eax, [rcx]
                if target_reg != RAX {
                    emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
                }
            } else {
                return Err(format!(
                    "x86_64-lower: `load32` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "store32" => {
            if args.len() >= 2 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_insns(&x86_64::push_r(RDI));
                lower_expr_into(emitter, ctx, &args[1], RSI, pending_calls)?;
                emitter.emit_insns(&x86_64::pop_r(RDI));
                emitter.emit_bytes(&[0x89, 0x37]); // mov [rdi], esi
            } else {
                return Err(format!(
                    "x86_64-lower: `store32` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "load64" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RCX, pending_calls)?;
                emitter.emit_insns(&x86_64::mov_reg_ptr(RAX, RCX)); // mov rax, [rcx]
                if target_reg != RAX {
                    emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
                }
            } else {
                return Err(format!(
                    "x86_64-lower: `load64` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "store64" => {
            if args.len() >= 2 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_insns(&x86_64::push_r(RDI));
                lower_expr_into(emitter, ctx, &args[1], RSI, pending_calls)?;
                emitter.emit_insns(&x86_64::pop_r(RDI));
                emitter.emit_insns(&x86_64::mov_ptr_reg(RDI, RSI)); // mov [rdi], rsi
            } else {
                return Err(format!(
                    "x86_64-lower: `store64` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "read-cr2" => {
            emitter.emit_width(&[0x0F, 0x20, 0xD0], &[0x48, 0x0F, 0x20, 0xD0]); // mov eax/rax, cr2
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
            }
            Ok(true)
        }
        "read-cr3" => {
            emitter.emit_width(&[0x0F, 0x20, 0xD8], &[0x48, 0x0F, 0x20, 0xD8]); // mov eax/rax, cr3
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
            }
            Ok(true)
        }
        "write-cr3" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_bytes(&[0x0F, 0x22, 0xC7]); // mov cr3, edi/rdi
            }
            Ok(true)
        }
        "invlpg" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_width(&[0x0F, 0x01, 0x3F], &[0x48, 0x0F, 0x01, 0x3F]); // invlpg [edi/rdi]
            } else {
                return Err(format!(
                    "x86_64-lower: `invlpg` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "lidt" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                emitter.emit_bytes(&[0x0F, 0x01, 0x1F]); // lidt [rdi]
            } else {
                return Err(format!(
                    "x86_64-lower: `lidt` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            Ok(true)
        }
        "invoke" | "invoke1" | "invoke2" | "invoke-native" | "invoke-native1"
        | "invoke-native2" | "invoke_native" | "invoke_native1" | "invoke_native2"
            if !ctx.functions.contains_key(target_name) =>
        {
            if x86_64::is_32bit() {
                return Err("x86_64-lower: indirect Int calls require 64-bit mode".into());
            }
            let raw_native = matches!(
                target_name,
                "invoke-native"
                    | "invoke-native1"
                    | "invoke-native2"
                    | "invoke_native"
                    | "invoke_native1"
                    | "invoke_native2"
            );
            let expected = match target_name {
                "invoke" | "invoke-native" | "invoke_native" => 1,
                "invoke1" | "invoke-native1" | "invoke_native1" => 2,
                _ => 3,
            };
            if args.len() != expected {
                return Err(format!(
                    "x86_64-lower: `{target_name}` requires {expected} arguments"
                ));
            }
            if let Expr::Ident(name) = &args[0]
                && !ctx.locals.contains_key(name)
                && !ctx.globals.contains_key(name)
                && let Some(function) = ctx.functions.get(name)
            {
                if raw_native && function.may_throw {
                    return Err("x86_64-lower: raw native target must be nothrow".into());
                }
                if function.ret.canonical() != Typ::Int
                    || function.params.len() != expected - 1
                    || function
                        .params
                        .iter()
                        .any(|(_, typ)| typ.canonical() != Typ::Int)
                {
                    return Err(
                        "x86_64-lower: indirect calls require an Int scalar signature".into(),
                    );
                }
            } else if scalar_expr_type(&args[0], ctx).map(|typ| typ.canonical()) != Some(Typ::Int) {
                return Err("x86_64-lower: indirect call target must be an Int address".into());
            }
            for arg in &args[1..] {
                if scalar_expr_type(arg, ctx).map(|typ| typ.canonical()) != Some(Typ::Int) {
                    return Err("x86_64-lower: indirect arguments must be Int scalars".into());
                }
            }
            let mut staged = Vec::with_capacity(expected);
            for arg in args {
                let slot = ctx.alloc_slot();
                lower_expr_into(emitter, ctx, arg, RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::str64(RAX, slot as u16));
                staged.push(slot);
            }
            emitter.emit_insns(&x86_64::ldr64(RAX, staged[0] as u16));
            emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 0));
            let valid = emitter.len();
            emitter.emit_insns(&x86_64::jcc_near(0x05, 0));
            if raw_native {
                emitter.emit_insns(&[0x0F, 0x0B]); // UD2: no language error carrier exists.
            } else {
                emit_failure_return(emitter, ctx)?;
            }
            emitter.patch_u32(valid + 2, emitter.len() - valid - 6);
            for (slot, reg) in staged[1..].iter().zip([RDI, RSI]) {
                emitter.emit_insns(&x86_64::ldr64(reg, *slot as u16));
            }
            if raw_native {
                emit_aligned_indirect_host_call(emitter);
            } else {
                emitter.emit_insns(&[0xFF, 0xD0]);
                emit_error_check(emitter, ctx)?;
            }
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
            }
            Ok(true)
        }

        _ => Ok(false),
    }
}

fn reg_arg_count() -> usize {
    // i386 has no r8/r9; keep the first four SysV-style regs only.
    if x86_64::is_32bit() { 4 } else { 6 }
}

fn emit_stack_cleanup(emitter: &mut CodeEmitter, args_len: usize) {
    let nreg = reg_arg_count();
    if args_len > nreg {
        let stack_bytes = (args_len - nreg) * if x86_64::is_32bit() { 4 } else { 8 };
        emitter.emit_width(&[0x81, 0xC4], &[0x48, 0x81, 0xC4]); // add esp/rsp, imm32
        emitter.emit_bytes(&(stack_bytes as u32).to_le_bytes());
    }
}

fn lower_call_argument(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    arg: &Expr,
    target_reg: u8,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if matches!(
        arg,
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. }
    ) {
        return lower_value_control(
            emitter,
            ctx,
            arg,
            pending_calls,
            |emitter, ctx, value, pending_calls| {
                lower_call_argument(emitter, ctx, value, target_reg, pending_calls)
            },
        );
    }

    if let Expr::StructInit { name, fields } = arg {
        if !ctx.structs.contains_key(name)
            || crate::native_emit::struct_layout::x86_fields(ctx.structs, name).is_err()
        {
            return Err("x86_64-lower: struct argument needs a known scalar-field layout".into());
        }
        let mut number = ctx.frame_size;
        let temporary = loop {
            let candidate = format!("__struct_arg_{number}");
            if !ctx.locals.contains_key(&candidate)
                && !ctx.functions.contains_key(&candidate)
                && !ctx.globals.contains_key(&candidate)
            {
                break candidate;
            }
            number += 1;
        };
        ctx.alloc_local(&temporary, &Typ::Named(name.clone()))?;
        lower_struct_init(emitter, ctx, &temporary, fields, pending_calls)?;
        return lower_ident_ref(emitter, ctx, target_reg, &temporary, pending_calls);
    }
    lower_expr_into(emitter, ctx, arg, target_reg, pending_calls)
}

fn lower_call_args(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    args: &[Expr],
    target_name: &str,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    // r8/r9 only exist in long mode — never select them for i386.
    let arg_regs: &[u8] = if x86_64::is_32bit() {
        &[RDI, RSI, RDX, RCX]
    } else {
        &[RDI, RSI, RDX, RCX, 8, 9]
    };
    let nreg = arg_regs.len();
    if args.len() > 16 {
        return Err(format!(
            "x86_64-lower: too many arguments in call to `{target_name}` in `{}`",
            ctx.fn_name
        ));
    }
    let params = ctx
        .functions
        .get(target_name)
        .map(|function| function.params.clone());
    let mut staged = Vec::with_capacity(args.len());
    // Capture every value in source order before selecting ABI registers or stack slots.
    for (index, arg) in args.iter().enumerate() {
        let slot = ctx.alloc_slot();
        if let Some((_, typ)) = params.as_ref().and_then(|params| params.get(index))
            && record_array_fields(ctx.structs, typ)?.is_some()
        {
            lower_typed_sequence_value(emitter, ctx, arg, typ, RAX, pending_calls)?;
        } else if let Some((param_name, typ)) = params.as_ref().and_then(|params| params.get(index))
            && param_name != "self"
            && let Typ::Named(name) = typ.canonical()
            && ctx.structs.contains_key(&name)
        {
            if x86_64::is_32bit()
                || crate::native_emit::struct_layout::x86_fields(ctx.structs, &name).is_err()
            {
                return Err(
                    "x86_64-lower: struct argument needs a scalar-field 64-bit layout".into(),
                );
            }
            let schema = crate::native_emit::struct_layout::x86_fields(ctx.structs, &name)?;
            let fields: HashMap<_, _> = schema
                .iter()
                .rev()
                .map(|(field, _)| (field.clone(), ctx.alloc_slot()))
                .collect();
            store_struct_value(emitter, ctx, arg, &fields, pending_calls)?;
            let base = fields
                .values()
                .copied()
                .max()
                .unwrap_or_else(|| ctx.alloc_slot());
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBP));
            emitter.emit_insns(&x86_64::sub_rmi32(RAX, base as i32 + 8));
        } else if matches!(params.as_ref().and_then(|params| params.get(index)), Some((name, Typ::Named(_))) if name == "self")
            && let Expr::Index {
                base,
                index: element_index,
            } = arg
        {
            if scalar_expr_type(arg, ctx).map(|typ| typ.canonical())
                != params
                    .as_ref()
                    .and_then(|params| params.get(index))
                    .map(|(_, typ)| typ.canonical())
            {
                return Err("x86_64-lower: indexed receiver type mismatch".into());
            }
            let slots = stage_record_index(emitter, ctx, base, element_index, pending_calls)?;
            load_record_index_address(emitter, ctx, slots)?;
        } else if matches!(params.as_ref().and_then(|params| params.get(index)), Some((name, Typ::Named(_))) if name == "self")
            && matches!(arg, Expr::Field { .. })
        {
            let mut base = arg;
            let mut path = Vec::new();
            while let Expr::Field { base: parent, name } = base {
                path.push(name.as_str());
                base = parent;
            }
            path.reverse();
            let Expr::Ident(local) = base else {
                return Err("x86_64-lower: receiver field needs addressable storage".into());
            };
            let prefix = format!("{}.", path.join("."));
            let Some(StackSlot::Struct { fields }) = ctx.locals.get(local) else {
                return Err("x86_64-lower: receiver field needs a known local layout".into());
            };
            let offset = fields
                .iter()
                .filter(|(name, _)| name.starts_with(&prefix))
                .map(|(_, offset)| *offset)
                .max()
                .ok_or("x86_64-lower: receiver field has no addressable storage")?;
            if let Some(&pointer) = ctx.receiver_pointers.get(local) {
                let root = *fields.values().max().unwrap();
                emitter.emit_insns(&x86_64::ldr64(RAX, pointer as u16));
                emitter.emit_insns(&x86_64::sub_rmi32(RAX, -((root - offset) as i32)));
            } else {
                emitter.emit_insns(&x86_64::mov_rr(RAX, RBP));
                emitter.emit_insns(&x86_64::sub_rmi32(RAX, offset as i32 + 8));
            }
        } else {
            lower_call_argument(emitter, ctx, arg, RAX, pending_calls)?;
        }
        emitter.emit_insns(&x86_64::str64(RAX, slot as u16));
        staged.push(slot);
    }
    for &slot in staged[nreg.min(staged.len())..].iter().rev() {
        emitter.emit_insns(&x86_64::ldr64(RAX, slot as u16));
        emitter.emit_insns(&x86_64::push_r(RAX));
    }
    for (&slot, &reg) in staged.iter().zip(arg_regs) {
        emitter.emit_insns(&x86_64::ldr64(reg, slot as u16));
    }
    Ok(())
}

/// Call a hosted C-ABI wrapper with argument registers already staged.
fn emit_aligned_host_call(emitter: &mut CodeEmitter, ptr: *const u8) {
    emitter.emit_insns(&x86_64::load_i64(RAX, ptr as i64));
    emit_aligned_indirect_host_call(emitter);
}

/// Call the native SysV target in RAX without any language error-page contract.
fn emit_aligned_indirect_host_call(emitter: &mut CodeEmitter) {
    // Operand spills and Harden frames may leave the stack unaligned for C calls.
    emitter.emit_insns(&x86_64::mov_rr(11, x86_64::REG_SP));
    emitter.emit_insns(&x86_64::load_i64(10, -16));
    emitter.emit_insns(&x86_64::and_rr(x86_64::REG_SP, 10));
    emitter.emit_insns(&x86_64::sub_rsp_i8(16));
    emitter.emit_insns(&x86_64::mov_m_r(x86_64::REG_SP, 0, 11));
    emitter.emit_insns(&[0xFF, 0xD0]);
    emitter.emit_insns(&x86_64::mov_r_m(x86_64::REG_SP, x86_64::REG_SP, 0));
}

fn lower_call_expr(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    callee: &Expr,
    args: &[Expr],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let target_name = match callee {
        Expr::Ident(name) => name.clone(),
        _ => {
            return Err(format!(
                "x86_64-lower: dynamic call target unsupported in `{}`",
                ctx.fn_name
            ));
        }
    };

    if target_name == crate::core_ir::COMPILER_UNREACHABLE {
        if !args.is_empty() || ctx.functions.contains_key(&target_name) {
            return Err(
                "x86_64-lower: private unreachable requires zero args and cannot be declared"
                    .into(),
            );
        }
        // UD2 is available in both native x86 modes, including freestanding.
        emitter.emit_insns(&[0x0F, 0x0B]);
        return Ok(());
    }
    if let Some(public) = crate::core_ir::compiler_print_callee(&target_name) {
        if ctx.functions.contains_key(&target_name) {
            return Err("x86_64-lower: private print operation cannot be declared".into());
        }
        if !ctx.runtime_error_slot || x86_64::is_32bit() {
            return Err("x86_64-lower: private print requires hosted 64-bit wrappers".into());
        }
        crate::native_emit::stdlib_wrappers::validate_intrinsic_call(
            &target_name,
            None,
            args.iter().map(|arg| scalar_expr_type(arg, ctx)),
        )
        .map_err(|error| {
            format!(
                "x86_64-lower: incompatible private print {}",
                error.category()
            )
        })?;
        let wrapper = jit_stdlib_wrapper(public).ok_or("missing private print wrapper")?;
        let pointer = crate::native_emit::native_link::resolve_native_fn(wrapper)
            .or_else(|| {
                crate::native_emit::native_link::bootstrap_jit_native();
                crate::native_emit::native_link::resolve_native_fn(wrapper)
            })
            .ok_or("x86_64-lower: private print wrapper unavailable")?;
        lower_call_args(emitter, ctx, args, &target_name, pending_calls)?;
        emit_aligned_host_call(emitter, pointer);
        emit_stack_cleanup(emitter, args.len());
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
        return Ok(());
    }
    if crate::core_ir::is_compiler_callee(&target_name) {
        return Err(format!(
            "x86_64-lower: unresolved private compiler operation `{target_name}`"
        ));
    }

    // Check the selected declared ABI before an inline builtin can bypass
    // argument staging. Lexical values retain their existing indirect policy.
    if x86_64::is_32bit()
        && !ctx.locals.contains_key(&target_name)
        && !ctx.globals.contains_key(&target_name)
        && let Some(function) = ctx.functions.get(&target_name)
    {
        super::validate_word32_scalar_signature(&function.params, &function.ret)?;
    }

    let has_override = ctx
        .functions
        .get(&target_name)
        .map_or(false, |f| !f.body.is_empty());
    if matches!(
        target_name.as_str(),
        "vector-push" | "vector-extend" | "vector-clear"
    ) && !has_override
    {
        let expected_args = if target_name == "vector-clear" { 1 } else { 2 };
        if args.len() != expected_args {
            return Err(format!(
                "x86_64-lower: {target_name} requires {expected_args} arguments"
            ));
        }
        let vector = &args[0];
        let Some(Typ::Vector(item)) = scalar_expr_type(vector, ctx) else {
            return Err("x86_64-lower: vector mutation requires vector storage".into());
        };
        if !matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool)
            || (target_name != "vector-clear"
                && !scalar_expr_type(&args[1], ctx).is_some_and(|actual| {
                    if target_name == "vector-extend" {
                        Typ::Vector(item.clone()).accepts_vector_source(&args[1], &actual)
                    } else {
                        actual.canonical() == item.canonical()
                    }
                }))
        {
            return Err("x86_64-lower: vector mutation requires matching scalar elements".into());
        }
        if !ctx.runtime_error_slot || x86_64::is_32bit() || ctx.is_interrupt {
            return Err("x86_64-lower: vector mutation requires the 64-bit JIT runtime".into());
        }
        let (root, path) = crate::native_emit::lower::field_path(vector)
            .ok_or("x86_64-lower: vector mutation requires addressable storage")?;
        if path.is_empty() {
            let offset = ctx.slot_offset(root)?;
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBP));
            emitter.emit_insns(&x86_64::sub_rmi32(RAX, offset as i32 + 8));
        } else if let Some((pointer, offset)) = ctx.receiver_field(root, &path) {
            emitter.emit_insns(&x86_64::ldr64(RAX, pointer as u16));
            emitter.emit_insns(&x86_64::sub_rmi32(RAX, -offset));
        } else {
            let Some(StackSlot::Struct { fields }) = ctx.locals.get(root) else {
                return Err("x86_64-lower: vector mutation requires a known field layout".into());
            };
            let offset = *fields
                .get(&path)
                .ok_or("x86_64-lower: vector field storage is missing")?;
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBP));
            emitter.emit_insns(&x86_64::sub_rmi32(RAX, offset as i32 + 8));
        }
        if target_name == "vector-clear" {
            emitter.emit_insns(&x86_64::mov_r_m(RAX, RAX, 0));
            emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
            let empty = emitter.len();
            emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
            emitter.emit_insns(&x86_64::zero_reg(RCX));
            emitter.emit_insns(&x86_64::mov_m_r(RAX, 0, RCX));
            emitter.patch_u32(empty + 2, emitter.len() - empty - 6);
            emitter.emit_insns(&x86_64::zero_reg(target_reg));
            return Ok(());
        }
        let saved = ctx.alloc_slot();
        emitter.emit_insns(&x86_64::str64(RAX, saved as u16));
        lower_expr_into(emitter, ctx, &args[1], RDX, pending_calls)?;
        emitter.emit_insns(&x86_64::ldr64(RSI, saved as u16));
        emitter.emit_insns(&x86_64::mov_r_m(RSI, RSI, 0));
        emitter.emit_insns(&x86_64::mov_rr(RDI, 15));
        emitter.emit_insns(&x86_64::load_i64(
            RAX,
            if target_name == "vector-extend" {
                crate::jit_runtime::extend_array as *const () as i64
            } else {
                crate::jit_runtime::push_array as *const () as i64
            },
        ));
        emitter.emit_insns(&x86_64::mov_rr(11, x86_64::REG_SP));
        emitter.emit_insns(&x86_64::load_i64(10, -16));
        emitter.emit_insns(&x86_64::and_rr(x86_64::REG_SP, 10));
        emitter.emit_insns(&x86_64::sub_rsp_i8(16));
        emitter.emit_insns(&x86_64::mov_m_r(x86_64::REG_SP, 0, 11));
        emitter.emit_insns(&[0xFF, 0xD0]);
        emitter.emit_insns(&x86_64::mov_r_m(x86_64::REG_SP, x86_64::REG_SP, 0));
        emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
        let valid = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(0x05, 0));
        emit_failure_return(emitter, ctx)?;
        emitter.patch_u32(valid + 2, emitter.len() - valid - 6);
        emitter.emit_insns(&x86_64::ldr64(RCX, saved as u16));
        emitter.emit_insns(&x86_64::mov_m_r(RCX, 0, RAX));
        emitter.emit_insns(&x86_64::zero_reg(target_reg));
        return Ok(());
    }

    if lower_builtin_call(
        emitter,
        ctx,
        target_name.as_str(),
        target_reg,
        args,
        pending_calls,
    )? {
        return Ok(());
    }

    if ctx.runtime_error_slot {
        let base = target_name
            .rsplit("::")
            .next()
            .unwrap_or(&target_name)
            .replace('_', "-");
        if ctx
            .functions
            .get(&target_name)
            .is_none_or(|function| function.body.is_empty())
            && matches!(
                base.as_str(),
                "push"
                    | "array-push"
                    | "insert"
                    | "remove"
                    | "pop"
                    | "clear"
                    | "reserve"
                    | "extend"
            )
            && args
                .first()
                .is_some_and(|arg| matches!(scalar_expr_type(arg, ctx), Some(Typ::Array(_))))
        {
            return Err("x86_64-lower: vector mutation cannot use fixed array storage".into());
        }
        if ctx
            .functions
            .get(&target_name)
            .is_none_or(|function| function.body.is_empty())
            && let Some(wrapper) = jit_stdlib_wrapper(&base)
            && let Some(ptr) = crate::native_emit::native_link::resolve_native_fn(wrapper)
        {
            if x86_64::is_32bit() || args.len() > 6 {
                return Err(
                    "x86_64-lower: JIT stdlib wrappers require 64-bit register arguments".into(),
                );
            }
            crate::native_emit::stdlib_wrappers::validate_scalar_call(
                wrapper,
                ctx.functions
                    .get(&target_name)
                    .map(|declared| (declared.params.as_slice(), &declared.ret)),
                args.iter().map(|arg| scalar_expr_type(arg, ctx)),
            )
            .map_err(|error| {
                format!(
                    "x86_64-lower: incompatible JIT stdlib {} `{target_name}`",
                    error.category()
                )
            })?;
            lower_call_args(emitter, ctx, args, &target_name, pending_calls)?;
            emit_aligned_host_call(emitter, ptr);
            emit_stack_cleanup(emitter, args.len());
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
            }
            return Ok(());
        }
    }

    if !ctx.functions.contains_key(&target_name) {
        lower_call_args(emitter, ctx, args, &target_name, pending_calls)?;
        // Extern function: emit CALL instruction with relocation.
        // The linker resolves the target — until then, displacement=0.
        if antidecomp::harden_active() {
            emitter.emit_insns(&antidecomp::junk_pad());
        }
        let site = emitter.len() as u32;
        emitter.emit_insns(&x86_64::call_rel32(0));
        pending_calls.push(PendingCall {
            site,
            target: target_name,
        });
        emit_stack_cleanup(emitter, args.len());
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
        return Ok(());
    }

    let may_throw = ctx.functions[&target_name].may_throw;
    // Save caller-saved regs before call (RAX excluded — gets return value).
    // Skip target_reg if caller-saved — holds return value after mov_rr.
    // i386: never touch r8-r15 (REX prefixes are illegal in protected mode).
    // ponytail: conservative save of all caller-saved regs; liveness would reduce.
    let saved: &[u8] = if x86_64::is_32bit() {
        &[RCX, RDX, RSI, RDI]
    } else {
        &[RCX, RDX, RSI, RDI, 8, 9, 10, 11]
    };
    for &reg in saved {
        if reg == target_reg {
            continue;
        }
        emitter.emit_insns(&x86_64::push_r(reg));
    }

    lower_call_args(emitter, ctx, args, &target_name, pending_calls)?;

    if antidecomp::harden_active() {
        emitter.emit_insns(&antidecomp::junk_pad());
    }
    let site = emitter.len() as u32;
    emitter.emit_insns(&x86_64::call_rel32(0));
    pending_calls.push(PendingCall {
        site,
        target: target_name,
    });

    emit_stack_cleanup(emitter, args.len());

    // Restore saved regs in reverse.
    for &reg in saved.iter().rev() {
        if reg == target_reg {
            continue;
        }
        emitter.emit_insns(&x86_64::pop_r(reg));
    }

    if may_throw {
        emit_error_check(emitter, ctx)?;
    }
    if target_reg != RAX {
        emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
    }
    Ok(())
}

fn store_struct_value(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    fields: &HashMap<String, u32>,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if matches!(
        expr,
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. }
    ) {
        return lower_value_control(
            emitter,
            ctx,
            expr,
            pending_calls,
            |emitter, ctx, value, pending_calls| {
                store_struct_value(emitter, ctx, value, fields, pending_calls)
            },
        );
    }

    let mut offsets: Vec<_> = fields.values().copied().collect();
    offsets.sort_unstable_by(|a, b| b.cmp(a));
    if matches!(expr, Expr::IntLit(0)) {
        emitter.emit_insns(&x86_64::zero_reg(RCX));
        for offset in offsets {
            emitter.emit_insns(&x86_64::str64(RCX, offset as u16));
        }
        return Ok(());
    }
    let Some(Typ::Named(name)) = scalar_expr_type(expr, ctx) else {
        return Err("x86_64-lower: struct value needs a known scalar-field layout".into());
    };
    if crate::native_emit::struct_layout::x86_fields(ctx.structs, &name).is_err() {
        return Err("x86_64-lower: struct value needs a known scalar-field layout".into());
    }
    if let Expr::Index { base, .. } = expr {
        let typ =
            scalar_expr_type(base, ctx).ok_or("x86_64-lower: unknown indexed record source")?;
        if record_array_fields(ctx.structs, &typ)?.is_none() {
            return Err(
                "x86_64-lower: indexed record transfer requires a fixed record array".into(),
            );
        }
    }
    if let Expr::Field { .. } = expr {
        let mut ordered: Vec<_> = fields
            .iter()
            .map(|(name, &slot)| (name.clone(), slot))
            .collect();
        ordered.sort_unstable_by_key(|a| std::cmp::Reverse(a.1));
        let staged: Vec<_> = ordered.iter().map(|(_, _)| ctx.alloc_slot()).collect();
        for ((field, _), &slot) in ordered.iter().zip(&staged) {
            lower_field_access(emitter, ctx, RAX, expr, field, pending_calls)?;
            emitter.emit_insns(&x86_64::str64(RAX, slot as u16));
        }
        for ((_, destination), slot) in ordered.iter().zip(staged) {
            emitter.emit_insns(&x86_64::ldr64(RAX, slot as u16));
            emitter.emit_insns(&x86_64::str64(RAX, *destination as u16));
        }
        return Ok(());
    }
    match expr {
        Expr::Ident(name)
            if matches!(ctx.locals.get(name), Some(StackSlot::Struct { .. }))
                || (!ctx.locals.contains_key(name) && ctx.globals.contains_key(name)) => {}
        Expr::StructInit { fields, .. } => {
            let schema = &ctx.structs[&name];
            if fields.len() != schema.len()
                || schema
                    .iter()
                    .any(|(field, _)| fields.iter().filter(|(name, _)| name == field).count() != 1)
            {
                return Err(format!(
                    "x86_64-lower: incomplete struct initializer for `{name}`"
                ));
            }
        }
        Expr::Call { .. } | Expr::Index { .. } => {}
        _ => {
            return Err(
                "x86_64-lower: struct value needs an addressable local, constructor, or call"
                    .into(),
            );
        }
    }
    if offsets.is_empty() && ctx.structs[&name].is_empty() && !matches!(expr, Expr::Call { .. }) {
        return Ok(());
    }
    if let Expr::Index { base, index } = expr {
        // Only this aggregate copy seam consumes a record payload address.
        lower_index_expr(emitter, ctx, RAX, base, index, pending_calls)?;
    } else {
        lower_call_argument(emitter, ctx, expr, RAX, pending_calls)?;
    }
    emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 1));
    let valid = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x07, 0));
    emit_failure_return(emitter, ctx)?;
    emitter.patch_u32(valid + 2, emitter.len() - valid - 6);

    for (index, offset) in offsets.into_iter().enumerate() {
        emitter.emit_insns(&x86_64::mov_r_m(RCX, RAX, (index * 8) as i32));
        emitter.emit_insns(&x86_64::str64(RCX, offset as u16));
    }
    Ok(())
}

fn lower_struct_init(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    name: &str,
    fields: &[(String, Expr)],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let Some(StackSlot::Struct { fields: slots }) = ctx.locals.get(name).cloned() else {
        return Err("x86_64-lower: missing struct initializer layout".into());
    };
    fn store(
        emitter: &mut CodeEmitter,
        ctx: &mut LowerCtx<'_>,
        values: &[(String, Expr)],
        prefix: &str,
        slots: &HashMap<String, u32>,
        pending_calls: &mut Vec<PendingCall>,
    ) -> Result<(), String> {
        for (field, value) in values {
            let field = if prefix.is_empty() {
                field.clone()
            } else {
                format!("{prefix}.{field}")
            };
            if let Expr::StructInit { fields, .. } = value {
                store(emitter, ctx, fields, &field, slots, pending_calls)?;
            } else if let Some(&slot) = slots.get(&field) {
                lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::str64(RAX, slot as u16));
            } else {
                let nested: HashMap<_, _> = slots
                    .iter()
                    .filter_map(|(name, &slot)| {
                        name.strip_prefix(&format!("{field}."))
                            .map(|name| (name.to_string(), slot))
                    })
                    .collect();
                if nested.is_empty() {
                    return Err(format!("x86_64-lower: missing struct field `{field}`"));
                }
                store_struct_value(emitter, ctx, value, &nested, pending_calls)?;
            }
        }
        Ok(())
    }
    store(emitter, ctx, fields, "", &slots, pending_calls)?;
    Ok(())
}

fn lower_field_access(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    base: &Expr,
    name: &str,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if let Expr::Index { base: array, index } = base {
        let typ = scalar_expr_type(array, ctx).ok_or("x86_64-lower: unknown record array")?;
        let FlatRecordArray { fields: schema, .. } = record_array_fields(ctx.structs, &typ)?
            .ok_or("x86_64-lower: field access requires a flat record array")?;
        let field = schema
            .iter()
            .position(|(field, _)| field == name)
            .ok_or("x86_64-lower: unknown record array field")?;
        let slots = stage_record_index(emitter, ctx, array, index, pending_calls)?;
        load_record_index_address(emitter, ctx, slots)?;
        emitter.emit_insns(&x86_64::mov_r_m(target_reg, RAX, (field * 8) as i32));
        return Ok(());
    }
    if let Expr::Field { base, name: parent } = base {
        return lower_field_access(
            emitter,
            ctx,
            target_reg,
            base,
            &format!("{parent}.{name}"),
            pending_calls,
        );
    }
    let Expr::Ident(base_name) = base else {
        return Err(format!("x86_64-lower: unsupported field base for `{name}`"));
    };
    if let Some((pointer, offset)) = ctx.receiver_field(base_name, name) {
        emitter.emit_insns(&x86_64::ldr64(target_reg, pointer as u16));
        emitter.emit_insns(&x86_64::mov_r_m(target_reg, target_reg, offset));
        return Ok(());
    }
    if !ctx.locals.contains_key(base_name) {
        let field_key = format!("{base_name}.{name}");
        if ctx.globals.contains_key(&field_key) {
            return lower_ident_ref(emitter, ctx, target_reg, &field_key, pending_calls);
        }
    }
    match ctx.locals.get(base_name) {
        Some(StackSlot::Struct { fields }) => {
            let field_key = if fields.contains_key(name) {
                name.to_string()
            } else {
                // Check for nested struct prefix: "inner" → "inner.val"
                let prefix = format!("{name}.");
                if let Some(match_key) = fields
                    .iter()
                    .filter(|(key, _)| key.starts_with(&prefix))
                    .min_by_key(|(_, offset)| **offset)
                    .map(|(key, _)| key)
                {
                    match_key.clone()
                } else {
                    return Err(format!(
                        "native-lower: field `{name}` not found on `{base_name}`"
                    ));
                }
            };
            if let Some(field_offset) = fields.get(&field_key) {
                emitter.emit_insns(&x86_64::ldr64(RAX, *field_offset as u16));
                if target_reg != RAX {
                    emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
                }
                Ok(())
            } else {
                emitter.emit_insns(&x86_64::load_i64(target_reg, 0));
                Ok(())
            }
        }
        _ => {
            emitter.emit_insns(&x86_64::load_i64(target_reg, 0));
            Ok(())
        }
    }
}

// RAX carries the source bits. NaNs use integer-only paths so signaling,
// payload and sign survive transfers; only non-NaNs use numeric conversion.
fn emit_f32_bit_cast(emitter: &mut CodeEmitter, from_float: bool) {
    emitter.emit_insns(&x86_64::push_r(RCX));
    emitter.emit_insns(&x86_64::push_r(RDX));
    let (exponent, fraction) = if from_float {
        (0x7ff0_0000_0000_0000u64, 0x000f_ffff_ffff_ffffu64)
    } else {
        emitter.emit_insns(&x86_64::load_i64(RCX, 0xffff_ffff));
        emitter.emit_insns(&x86_64::and_rr(RAX, RCX));
        (0x7f80_0000, 0x007f_ffff)
    };
    emitter.emit_insns(&x86_64::mov_rr(RDX, RAX));
    emitter.emit_insns(&x86_64::load_i64(RCX, exponent as i64));
    emitter.emit_insns(&x86_64::and_rr(RDX, RCX));
    emitter.emit_insns(&x86_64::cmp_rr(RDX, RCX));
    let finite = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(5, 0));
    emitter.emit_insns(&x86_64::mov_rr(RDX, RAX));
    emitter.emit_insns(&x86_64::load_i64(RCX, fraction as i64));
    emitter.emit_insns(&x86_64::and_rr(RDX, RCX));
    emitter.emit_insns(&x86_64::test_rr(RDX, RDX));
    let infinity = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(4, 0));
    if from_float {
        emitter.emit_insns(&x86_64::shr_reg_imm(RDX, 29));
        emitter.emit_insns(&x86_64::test_rr(RDX, RDX));
        let nonzero = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(5, 0));
        // An arbitrary f64 NaN must never narrow to infinity.
        emitter.emit_insns(&x86_64::load_i64(RDX, 1));
        emitter.patch_u32(nonzero + 2, emitter.len() - nonzero - 6);
        emitter.emit_insns(&x86_64::shr_reg_imm(RAX, 32));
        emitter.emit_insns(&x86_64::load_i64(RCX, 0x8000_0000));
        emitter.emit_insns(&x86_64::and_rr(RAX, RCX));
        emitter.emit_insns(&x86_64::or_rr(RAX, RDX));
        emitter.emit_insns(&x86_64::load_i64(RCX, 0x7f80_0000));
    } else {
        emitter.emit_insns(&x86_64::shl_reg_imm(RDX, 29));
        emitter.emit_insns(&x86_64::load_i64(RCX, 0x8000_0000));
        emitter.emit_insns(&x86_64::and_rr(RAX, RCX));
        emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 32));
        emitter.emit_insns(&x86_64::or_rr(RAX, RDX));
        emitter.emit_insns(&x86_64::load_i64(RCX, 0x7ff0_0000_0000_0000));
    }
    emitter.emit_insns(&x86_64::or_rr(RAX, RCX));
    let done = emitter.len();
    emitter.emit_insns(&x86_64::jmp_rel32(0));
    emitter.patch_u32(finite + 2, emitter.len() - finite - 6);
    emitter.patch_u32(infinity + 2, emitter.len() - infinity - 6);
    if from_float {
        emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, true));
        emitter.emit_insns(&x86_64::convert_scalar_float(0, 0, true));
        emitter.emit_insns(&x86_64::movd_xmm_gpr(0, RAX, false));
    } else {
        emitter.emit_insns(&x86_64::movd_xmm_gpr(0, RAX, true));
        emitter.emit_insns(&x86_64::convert_scalar_float(0, 0, false));
        emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, false));
    }
    emitter.patch_u32(done + 1, emitter.len() - done - 5);
    emitter.emit_insns(&x86_64::pop_r(RDX));
    emitter.emit_insns(&x86_64::pop_r(RCX));
}

// RAX is the already-evaluated Int64 source; preserve the sticky-bit scratch.
fn emit_integer_float_cast(emitter: &mut CodeEmitter, bits: u8, signed: bool) {
    let convert = if bits == 32 {
        x86_64::cvtsi2ss
    } else {
        x86_64::cvtsi2sd
    };
    if signed {
        emitter.emit_insns(&convert(0, RAX));
    } else {
        emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
        let low_half = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(9, 0)); // jns: direct signed conversion is exact-domain.
        emitter.emit_insns(&x86_64::push_r(RDX));
        emitter.emit_insns(&x86_64::load_i64(RDX, 1));
        emitter.emit_insns(&x86_64::and_rr(RDX, RAX));
        emitter.emit_insns(&x86_64::shr_reg_imm(RAX, 1));
        emitter.emit_insns(&x86_64::or_rr(RAX, RDX));
        // Sticky-half rounds at the chosen destination precision. Doubling is
        // exact here; converting via f64 first would double-round some f32 inputs.
        emitter.emit_insns(&convert(0, RAX));
        emitter.emit_insns(&if bits == 32 {
            x86_64::scalar_single(0x58, 0, 0)
        } else {
            x86_64::scalar_double(0x58, 0, 0)
        });
        emitter.emit_insns(&x86_64::pop_r(RDX));
        let done = emitter.len();
        emitter.emit_insns(&x86_64::jmp_rel32(0));
        emitter.patch_u32(low_half + 2, emitter.len() - low_half - 6);
        emitter.emit_insns(&convert(0, RAX));
        emitter.patch_u32(done + 1, emitter.len() - done - 5);
    }
    if bits == 32 {
        emitter.emit_insns(&x86_64::convert_scalar_float(0, 0, false));
    }
    emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, false));
}

fn emit_saturating_float_cast(emitter: &mut CodeEmitter, bits: u8, signed: bool) {
    let upper = 2.0f64.powi(i32::from(bits) - i32::from(signed));
    let lower = if signed { -upper } else { 0.0 };
    emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, true));
    emitter.emit_insns(&x86_64::ucomisd(0, 0));
    let nan = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(10, 0));
    emitter.emit_insns(&x86_64::load_i64(RDX, upper.to_bits() as i64));
    emitter.emit_insns(&x86_64::movq_xmm_gpr(1, RDX, true));
    emitter.emit_insns(&x86_64::ucomisd(0, 1));
    let high = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(3, 0));
    emitter.emit_insns(&x86_64::load_i64(RDX, lower.to_bits() as i64));
    emitter.emit_insns(&x86_64::movq_xmm_gpr(1, RDX, true));
    emitter.emit_insns(&x86_64::ucomisd(0, 1));
    let low = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(6, 0));
    if !signed && bits == 64 {
        emitter.emit_insns(&x86_64::load_i64(RDX, (2.0f64.powi(63)).to_bits() as i64));
        emitter.emit_insns(&x86_64::movq_xmm_gpr(1, RDX, true));
        emitter.emit_insns(&x86_64::ucomisd(0, 1));
        let small = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(2, 0));
        emitter.emit_insns(&x86_64::scalar_double(0x5c, 0, 1));
        emitter.emit_insns(&x86_64::cvttsd2si(RAX, 0));
        emitter.emit_insns(&x86_64::load_i64(RDX, i64::MIN));
        emitter.emit_insns(&x86_64::xor_rr(RAX, RDX));
        let converted = emitter.len();
        emitter.emit_insns(&x86_64::jmp_rel32(0));
        emitter.patch_u32(small + 2, emitter.len() - small - 6);
        emitter.emit_insns(&x86_64::cvttsd2si(RAX, 0));
        emitter.patch_u32(converted + 1, emitter.len() - converted - 5);
    } else {
        emitter.emit_insns(&x86_64::cvttsd2si(RAX, 0));
    }
    let mut ends = vec![emitter.len()];
    emitter.emit_insns(&x86_64::jmp_rel32(0));
    for (site, value) in [
        (nan, 0),
        (
            high,
            crate::core_ir::saturating_float_cast(f64::INFINITY, bits, signed),
        ),
        (
            low,
            crate::core_ir::saturating_float_cast(f64::NEG_INFINITY, bits, signed),
        ),
    ] {
        emitter.patch_u32(site + 2, emitter.len() - site - 6);
        emitter.emit_insns(&x86_64::load_i64(RAX, value));
        ends.push(emitter.len());
        emitter.emit_insns(&x86_64::jmp_rel32(0));
    }
    for site in ends {
        emitter.patch_u32(site + 1, emitter.len() - site - 5);
    }
}

fn lower_unary_expr(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    op: &str,
    expr: &Expr,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if op == "!" && scalar_expr_type(expr, ctx).is_some_and(|typ| typ.canonical() == Typ::Float) {
        // Reuse the ordered Float equality path, including its NaN parity guard.
        return lower_binary_expr(
            emitter,
            ctx,
            target_reg,
            "==",
            expr,
            &Expr::FloatLit(crate::core_ir::FloatVal(0.0)),
            pending_calls,
        );
    }
    if let Some((width, expected)) = crate::core_ir::float_to_bits_width(op)
        .map(|width| (width, Typ::Float))
        .or_else(|| crate::core_ir::bits_to_float_width(op).map(|width| (width, Typ::Int)))
    {
        if x86_64::is_32bit() || scalar_expr_type(expr, ctx) != Some(expected) {
            return Err(
                "x86_64-lower: IEEE bit conversion requires a resolved 64-bit scalar carrier"
                    .into(),
            );
        }
        lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
        if width == 32 {
            emit_f32_bit_cast(emitter, crate::core_ir::float_to_bits_width(op).is_some());
        }
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
        return Ok(());
    }
    if crate::core_ir::integer_cast_spec(op).is_some() {
        let value = match scalar_expr_type(expr, ctx) {
            Some(Typ::Int) => expr.clone(),
            Some(Typ::Bool) => Expr::Unary {
                op: "bool-to-int".into(),
                expr: Box::new(expr.clone()),
            },
            _ => return Err("x86_64-lower: integer cast needs Int or Bool".into()),
        };
        let value = crate::core_ir::integer_cast_expr(value, op).unwrap();
        return lower_expr_into(emitter, ctx, &value, target_reg, pending_calls);
    }
    if let Some(count) = crate::core_ir::array_copy::count(op) {
        return lower_array_copy(emitter, ctx, target_reg, expr, count, pending_calls);
    }
    if op == "vector-clone"
        && !matches!(scalar_expr_type(expr, ctx), Some(Typ::Vector(item)) if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool))
    {
        return Err("x86_64-lower: vector clone requires scalar vector elements".into());
    }
    lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
    match op {
        "vector-clone" => {
            if !ctx.runtime_error_slot || x86_64::is_32bit() || ctx.is_interrupt {
                return Err("x86_64-lower: vector clone requires the 64-bit JIT runtime".into());
            }
            emitter.emit_insns(&x86_64::mov_r_m(RDX, RAX, 0));
            emitter.emit_insns(&x86_64::mov_rr(RSI, RAX));
            emitter.emit_insns(&x86_64::add_rmi8(RSI, 16));
            emitter.emit_insns(&x86_64::mov_rr(RDI, 15));
            emitter.emit_insns(&x86_64::load_i64(
                RAX,
                crate::jit_runtime::clone_array as *const () as i64,
            ));
            emitter.emit_insns(&x86_64::mov_rr(11, x86_64::REG_SP));
            emitter.emit_insns(&x86_64::load_i64(10, -16));
            emitter.emit_insns(&x86_64::and_rr(x86_64::REG_SP, 10));
            emitter.emit_insns(&x86_64::sub_rsp_i8(16));
            emitter.emit_insns(&x86_64::mov_m_r(x86_64::REG_SP, 0, 11));
            emitter.emit_insns(&[0xFF, 0xD0]);
            emitter.emit_insns(&x86_64::mov_r_m(x86_64::REG_SP, x86_64::REG_SP, 0));
        }
        "float-cast-f32" | "float-cast-f64" => {
            if scalar_expr_type(expr, ctx) != Some(Typ::Float) {
                return Err("x86_64-lower: floating width cast needs Float".into());
            }
            if op == "float-cast-f32" {
                emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, true));
                emitter.emit_insns(&x86_64::convert_scalar_float(0, 0, true));
                emitter.emit_insns(&x86_64::convert_scalar_float(0, 0, false));
                emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, false));
            }
        }
        op if crate::core_ir::integer_float_cast_spec(op).is_some() => {
            if x86_64::is_32bit() || scalar_expr_type(expr, ctx) != Some(Typ::Int) {
                return Err("x86_64-lower: integer conversion needs a 64-bit Int carrier".into());
            }
            let (bits, signed) = crate::core_ir::integer_float_cast_spec(op).unwrap();
            emit_integer_float_cast(emitter, bits, signed);
        }
        op if crate::core_ir::saturating_float_cast_spec(op).is_some() => {
            if x86_64::is_32bit() || scalar_expr_type(expr, ctx) != Some(Typ::Float) {
                return Err("x86_64-lower: saturating conversion needs 64-bit Float layout".into());
            }
            let (bits, signed) = crate::core_ir::saturating_float_cast_spec(op).unwrap();
            emit_saturating_float_cast(emitter, bits, signed);
        }
        "float-to-int" => {
            if scalar_expr_type(expr, ctx) != Some(Typ::Float) {
                return Err("x86_64-lower: floating conversion needs Float".into());
            }
            emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, true));
            emitter.emit_insns(&x86_64::cvttsd2si(RAX, 0));
        }
        "bool-to-int" => {
            if scalar_expr_type(expr, ctx) != Some(Typ::Bool) {
                return Err("x86_64-lower: boolean promotion needs Bool".into());
            }
        }
        "-" => {
            let operand = scalar_expr_type(expr, ctx);
            if crate::core_ir::numeric_negation_domain(operand.as_ref(), false)
                != crate::core_ir::NumericNegationDomain::Numeric
            {
                return Err("x86_64-lower: numeric negation requires a resolved Int, Bool, or Float operand".into());
            }
            if operand == Some(Typ::Float) {
                emitter.emit_insns(&x86_64::load_i64(RDX, i64::MIN));
                emitter.emit_insns(&x86_64::xor_rr(RAX, RDX));
            } else {
                emitter.emit_insns(&x86_64::neg_reg(RAX));
            }
        }
        "!" => {
            emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x94, RAX)); // sete al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "~" => {
            emitter.emit_insns(&x86_64::not_reg(RAX));
        }
        "*" => {
            emitter.emit_insns(&x86_64::mov_reg_ptr(RAX, RAX)); // mov rax, [rax]
        }
        "&" => {}
        _ => {
            return Err(format!(
                "x86_64-lower: unsupported unary op `{op}` in `{}`",
                ctx.fn_name
            ));
        }
    }
    if target_reg != RAX {
        emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
    }
    Ok(())
}

struct FlatRecordArray {
    name: String,
    fields: Vec<(String, Typ)>,
}

/// Fixed arrays keep word slots; flat record slots hold independent payloads.
fn record_array_fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    typ: &Typ,
) -> Result<Option<FlatRecordArray>, String> {
    let Typ::Array(item) = typ.canonical() else {
        return Ok(None);
    };
    fn contains_record(typ: &Typ) -> bool {
        match typ.canonical() {
            Typ::Named(_) => true,
            Typ::Array(item) | Typ::Vector(item) => contains_record(&item),
            _ => false,
        }
    }
    let Typ::Named(name) = item.as_ref() else {
        if contains_record(&item) {
            return Err("x86_64-lower: nested record arrays require a nested aggregate ABI".into());
        }
        return Ok(None);
    };
    if x86_64::is_32bit() {
        return Err("x86_64-lower: record arrays require 64-bit mode".into());
    }
    let fields = crate::native_emit::struct_layout::flat_record_fields(structs, name)?;
    Ok(Some(FlatRecordArray {
        name: name.clone(),
        fields,
    }))
}

fn stage_record_index(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    index: &Expr,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(u32, u32), String> {
    let typ =
        scalar_expr_type(base, ctx).ok_or("x86_64-lower: record index needs a known array type")?;
    if record_array_fields(ctx.structs, &typ)?.is_none() {
        return Err("x86_64-lower: record index needs a flat record array".into());
    }
    let base_slot = ctx.alloc_slot();
    let index_slot = ctx.alloc_slot();
    lower_typed_sequence_value(emitter, ctx, base, &typ, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::str64(RAX, base_slot as u16));
    lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::str64(RAX, index_slot as u16));
    Ok((base_slot, index_slot))
}

/// Complete a staged place after RHS evaluation, before any destination write.
fn load_record_index_address(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    slots: (u32, u32),
) -> Result<(), String> {
    emitter.emit_insns(&x86_64::ldr64(RDI, slots.0 as u16));
    emitter.emit_insns(&x86_64::ldr64(RAX, slots.1 as u16));
    emitter.emit_insns(&x86_64::test_rr(RDI, RDI));
    let null = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
    emitter.emit_insns(&x86_64::mov_r_m(RDX, RDI, 0));
    emitter.emit_insns(&x86_64::cmp_rr(RAX, RDX));
    let bounds = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x03, 0));
    emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
    emitter.emit_insns(&x86_64::add_rr(RDI, RAX));
    emitter.emit_insns(&x86_64::mov_r_m(RAX, RDI, 16));
    emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 1));
    let payload = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x06, 0));
    let end = emitter.len();
    emitter.emit_insns(&x86_64::jmp_rel32(0));
    for branch in [null, bounds, payload] {
        emitter.patch_u32(branch + 2, emitter.len() - branch - 6);
    }
    emit_failure_return(emitter, ctx)?;
    emitter.patch_u32(end + 1, emitter.len() - end - 5);
    Ok(())
}

fn lower_record_array_item(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    item: &Expr,
    name: &str,
    schema: &[(String, Typ)],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if scalar_expr_type(item, ctx).map(|typ| typ.canonical()) != Some(Typ::Named(name.into())) {
        return Err("x86_64-lower: record array element type mismatch".into());
    }
    let fields: HashMap<_, _> = schema
        .iter()
        .rev()
        .map(|(field, _)| (field.clone(), ctx.alloc_slot()))
        .collect();
    store_struct_value(emitter, ctx, item, &fields, pending_calls)?;
    let offsets: Vec<_> = schema.iter().map(|(field, _)| fields[field]).collect();
    // shortcut: Each element uses a process-lifetime allocation plus a 16-byte
    // header; coallocate payloads if profiling shows this overhead matters.
    lower_heap_array(emitter, ctx, &offsets, pending_calls)?;
    emitter.emit_insns(&x86_64::add_rmi8(RAX, 16));
    Ok(())
}

fn lower_array_elem_read(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    offsets: &[u32],
    index: &Expr,
    target_reg: u8,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if let Expr::IntLit(i) = index {
        if *i >= 0 {
            let idx = *i as usize;
            if let Some(&off) = offsets.get(idx) {
                emitter.emit_insns(&x86_64::ldr64(RAX, off as u16));
                if target_reg != RAX {
                    emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
                }
                return Ok(());
            }
        }
        emit_failure_return(emitter, ctx)?;
        return Ok(());
    }
    let Some(&first) = offsets.first() else {
        lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
        emit_failure_return(emitter, ctx)?;
        return Ok(());
    };
    lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::load_i64(RDX, offsets.len() as i64));
    emitter.emit_insns(&x86_64::cmp_rr(RAX, RDX));
    let out_of_bounds = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x03, 0)); // unsigned >= also rejects negative indices
    emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
    emitter.emit_insns(&x86_64::load_i64(RDX, first as i64 + 8));
    emitter.emit_insns(&x86_64::add_rr(RAX, RDX));
    emitter.emit_insns(&x86_64::mov_rr(RDI, x86_64::REG_FP));
    emitter.emit_insns(&x86_64::sub_rr(RDI, RAX));
    emitter.emit_insns(&x86_64::mov_reg_ptr(RAX, RDI));
    let end = emitter.len();
    emitter.emit_insns(&x86_64::jmp_rel32(0));
    emitter.patch_u32(out_of_bounds + 2, emitter.len() - out_of_bounds - 6);
    emit_failure_return(emitter, ctx)?;
    emitter.patch_u32(end + 1, emitter.len() - end - 5);
    if target_reg != RAX {
        emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
    }
    Ok(())
}

/// The x86 owned Array value is a pointer to [len, capacity, word payload...].
fn lower_array_copy(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target: u8,
    source: &Expr,
    count: usize,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if x86_64::is_32bit() || ctx.is_interrupt {
        return Err("x86_64-lower: array copy requires the 64-bit hosted ABI".into());
    }
    if !scalar_expr_type(source, ctx).is_some_and(|typ| {
        crate::core_ir::array_copy::valid_operand(
            &crate::core_ir::array_copy::operation(count),
            &typ,
        )
    }) {
        return Err("x86_64-lower: array copy requires scalar Array elements".into());
    }
    let bytes = count
        .checked_add(2)
        .and_then(|words| words.checked_mul(8))
        .filter(|bytes| *bytes <= i64::MAX as usize)
        .ok_or("x86_64-lower: array copy allocation overflow")?;
    let payload_bytes = bytes - 16;
    let mut saved = Vec::new();
    for reg in [RAX, RCX, RDX, RSI, RDI, 8, 9, 10, 11] {
        if reg != target {
            saved.push((reg, ctx.alloc_slot()));
        }
    }
    let source_slot = ctx.alloc_slot();
    let result_slot = ctx.alloc_slot();
    if result_slot > u16::MAX as u32 {
        return Err("x86_64-lower: array copy spill frame is too large".into());
    }
    for &(reg, slot) in &saved {
        emitter.emit_insns(&x86_64::str64(reg, slot as u16));
    }
    lower_expr_into(emitter, ctx, source, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::str64(RAX, source_slot as u16));
    if count != 0 {
        emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
        let null = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
        emitter.emit_insns(&x86_64::mov_rr(RDX, RAX));
        emitter.emit_insns(&x86_64::load_i64(RCX, 7));
        emitter.emit_insns(&x86_64::and_rr(RDX, RCX));
        emitter.emit_insns(&x86_64::test_rr(RDX, RDX));
        let unaligned = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(0x05, 0));
        emitter.emit_insns(&x86_64::mov_r_m(RDX, RAX, 0));
        emitter.emit_insns(&x86_64::load_i64(RCX, count as i64));
        emitter.emit_insns(&x86_64::cmp_rr(RDX, RCX));
        let enough = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(0x03, 0));
        emitter.patch_u32(null + 2, emitter.len() - null - 6);
        emitter.patch_u32(unaligned + 2, emitter.len() - unaligned - 6);
        if ctx.runtime_error_slot
            || !matches!(
                ctx.ret_typ.canonical(),
                Typ::String | Typ::Array(_) | Typ::Vector(_) | Typ::Named(_)
            )
        {
            emit_failure_return(emitter, ctx)?;
        } else {
            // Native aggregate returns have no JIT error page or error-value ABI.
            // Terminate rather than return an invalid pointer that a caller could read.
            emitter.emit_insns(&x86_64::mov_rr(11, x86_64::REG_SP));
            emitter.emit_insns(&x86_64::load_i64(10, -16));
            emitter.emit_insns(&x86_64::and_rr(x86_64::REG_SP, 10));
            emitter.emit_insns(&x86_64::sub_rsp_i8(16));
            let site = emitter.len();
            emitter.emit_insns(&x86_64::call_rel32(0));
            pending_calls.push(PendingCall {
                site,
                target: "abort".into(),
            });
            emitter.emit_insns(&[0x0F, 0x0B]);
        }
        emitter.patch_u32(enough + 2, emitter.len() - enough - 6);
    }
    if ctx.runtime_error_slot {
        emitter.emit_insns(&x86_64::mov_rr(RDI, 15));
        emitter.emit_insns(&x86_64::ldr64(RSI, source_slot as u16));
        emitter.emit_insns(&x86_64::add_rmi8(RSI, 16));
        emitter.emit_insns(&x86_64::load_i64(RDX, count as i64));
        emit_aligned_host_call(
            emitter,
            crate::jit_runtime::clone_array as *const () as *const u8,
        );
    } else {
        lower_array_allocation(emitter, ctx, count, pending_calls)?;
        emitter.emit_insns(&x86_64::str64(RAX, result_slot as u16));
        if count != 0 {
            emitter.emit_insns(&x86_64::mov_rr(RDI, RAX));
            emitter.emit_insns(&x86_64::add_rmi8(RDI, 16));
            emitter.emit_insns(&x86_64::ldr64(RSI, source_slot as u16));
            emitter.emit_insns(&x86_64::add_rmi8(RSI, 16));
            emitter.emit_insns(&x86_64::load_i64(RDX, payload_bytes as i64));
            emitter.emit_insns(&x86_64::mov_rr(11, x86_64::REG_SP));
            emitter.emit_insns(&x86_64::load_i64(10, -16));
            emitter.emit_insns(&x86_64::and_rr(x86_64::REG_SP, 10));
            emitter.emit_insns(&x86_64::sub_rsp_i8(16));
            emitter.emit_insns(&x86_64::mov_m_r(x86_64::REG_SP, 0, 11));
            let site = emitter.len();
            emitter.emit_insns(&x86_64::call_rel32(0));
            pending_calls.push(PendingCall {
                site,
                target: "memcpy".into(),
            });
            emitter.emit_insns(&x86_64::mov_r_m(x86_64::REG_SP, x86_64::REG_SP, 0));
        }
        emitter.emit_insns(&x86_64::ldr64(RAX, result_slot as u16));
    }
    emitter.emit_insns(&x86_64::str64(RAX, result_slot as u16));
    for &(reg, slot) in &saved {
        emitter.emit_insns(&x86_64::ldr64(reg, slot as u16));
    }
    emitter.emit_insns(&x86_64::ldr64(target, result_slot as u16));
    Ok(())
}

fn lower_array_allocation(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    count: usize,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if x86_64::is_32bit() || ctx.is_interrupt {
        return Err("x86_64-lower: escaping arrays require the 64-bit hosted ABI".into());
    }
    if ctx.runtime_error_slot {
        emitter.emit_insns(&x86_64::mov_rr(RDI, 15));
        emitter.emit_insns(&x86_64::load_i64(RSI, count as i64));
        emitter.emit_insns(&x86_64::load_i64(
            RAX,
            crate::jit_runtime::allocate_array as *const () as i64,
        ));
    } else {
        let bytes = count
            .checked_add(2)
            .and_then(|n| n.checked_mul(8))
            .ok_or("x86_64-lower: array allocation overflow")?;
        emitter.emit_insns(&x86_64::load_i64(RDI, bytes as i64));
    }
    // Align C calls independently of the active profile and operand spills.
    emitter.emit_insns(&x86_64::mov_rr(11, x86_64::REG_SP));
    emitter.emit_insns(&x86_64::load_i64(10, -16));
    emitter.emit_insns(&x86_64::and_rr(x86_64::REG_SP, 10));
    emitter.emit_insns(&x86_64::sub_rsp_i8(16));
    emitter.emit_insns(&x86_64::mov_m_r(x86_64::REG_SP, 0, 11));
    if ctx.runtime_error_slot {
        emitter.emit_insns(&[0xFF, 0xD0]);
    } else {
        let site = emitter.len();
        emitter.emit_insns(&x86_64::call_rel32(0));
        pending_calls.push(PendingCall {
            site,
            target: "malloc".into(),
        });
        emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
        let allocated = emitter.len();
        emitter.emit_insns(&x86_64::jcc_near(0x05, 0));
        let site = emitter.len();
        emitter.emit_insns(&x86_64::call_rel32(0));
        pending_calls.push(PendingCall {
            site,
            target: "abort".into(),
        });
        emitter.emit_insns(&[0x0F, 0x0B]);
        emitter.patch_u32(allocated + 2, emitter.len() - allocated - 6);
    }
    emitter.emit_insns(&x86_64::mov_r_m(x86_64::REG_SP, x86_64::REG_SP, 0));
    emitter.emit_insns(&x86_64::load_i64(RDX, count as i64));
    emitter.emit_insns(&x86_64::mov_m_r(RAX, 0, RDX));
    emitter.emit_insns(&x86_64::mov_m_r(RAX, 8, RDX));
    Ok(())
}

fn lower_heap_array(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    offsets: &[u32],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    lower_array_allocation(emitter, ctx, offsets.len(), pending_calls)?;
    for (index, offset) in offsets.iter().enumerate() {
        emitter.emit_insns(&x86_64::ldr64(RDX, *offset as u16));
        emitter.emit_insns(&x86_64::mov_m_r(RAX, 16 + index as i32 * 8, RDX));
    }
    // shortcut: native heap values live until process exit, add release with language ownership.
    Ok(())
}

/// Materialize an array literal into fresh stack slots and return the offsets.
/// Used when a literal is indexed directly with a dynamic index; the stores
/// are emitted at the use site so a zero-iteration loop cannot leave the
/// slots uninitialized.
fn materialize_array_literal(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    items: &[Expr],
    pending_calls: &mut Vec<PendingCall>,
    record: Option<&FlatRecordArray>,
) -> Result<Vec<u32>, String> {
    if items
        .iter()
        .any(|item| scalar_expr_type(item, ctx).map(|typ| typ.canonical()) == Some(Typ::Void))
    {
        return Err("x86_64-lower: unit array elements require a zero-width layout".into());
    }
    let mut offsets = Vec::with_capacity(items.len());
    for _ in items {
        offsets.push(ctx.alloc_slot());
    }
    for (item, &off) in items.iter().zip(offsets.iter()) {
        if let Some(FlatRecordArray {
            name,
            fields: schema,
        }) = record
        {
            lower_record_array_item(emitter, ctx, item, name, schema, pending_calls)?;
        } else {
            lower_expr_into(emitter, ctx, item, RAX, pending_calls)?;
        }
        emitter.emit_insns(&x86_64::str64(RAX, off as u16));
    }
    Ok(offsets)
}

/// A vector literal keeps its legacy lowering. Only a known fixed-array
/// destination selects record payload storage; inference alone is ambiguous.
fn lower_typed_sequence_value(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    typ: &Typ,
    target: u8,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let record = record_array_fields(ctx.structs, typ)?;
    if record.is_some()
        && matches!(
            expr,
            Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. }
        )
    {
        return lower_value_control(
            emitter,
            ctx,
            expr,
            pending_calls,
            |emitter, ctx, value, pending_calls| {
                lower_typed_sequence_value(emitter, ctx, value, typ, target, pending_calls)
            },
        );
    }
    if let (Some(record), Expr::ArrayLit(items)) = (&record, expr) {
        let offsets = materialize_array_literal(emitter, ctx, items, pending_calls, Some(record))?;
        lower_heap_array(emitter, ctx, &offsets, pending_calls)?;
        if target != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target, RAX));
        }
        return Ok(());
    }
    lower_expr_into(emitter, ctx, expr, target, pending_calls)
}

fn lower_index_expr(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    base: &Expr,
    index: &Expr,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if let Some(typ) = scalar_expr_type(base, ctx)
        && record_array_fields(ctx.structs, &typ)?.is_some()
    {
        let slots = stage_record_index(emitter, ctx, base, index, pending_calls)?;
        load_record_index_address(emitter, ctx, slots)?;
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
        }
        return Ok(());
    }
    if let Expr::Ident(name) = base
        && let Some(StackSlot::InlineArray { offsets }) = ctx.locals.get(name).cloned()
    {
        return lower_array_elem_read(emitter, ctx, &offsets, index, target_reg, pending_calls);
    }
    if let Expr::ArrayLit(items) = base {
        let offsets = materialize_array_literal(emitter, ctx, items, pending_calls, None)?;
        return lower_array_elem_read(emitter, ctx, &offsets, index, target_reg, pending_calls);
    }
    let base_slot = ctx.alloc_slot();
    lower_expr_into(emitter, ctx, base, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::str64(RAX, base_slot as u16));
    lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::ldr64(RDI, base_slot as u16));
    if scalar_expr_type(base, ctx) == Some(Typ::Int) {
        emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
        emitter.emit_insns(&x86_64::add_rr(RDI, RAX));
        emitter.emit_insns(&x86_64::mov_r_m(target_reg, RDI, 0));
        return Ok(());
    }
    emitter.emit_insns(&x86_64::test_rr(RDI, RDI));
    let null = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
    emitter.emit_insns(&x86_64::mov_r_m(RDX, RDI, 0));
    emitter.emit_insns(&x86_64::cmp_rr(RAX, RDX));
    let out_of_bounds = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x03, 0));
    let nested = matches!(scalar_expr_type(base, ctx), Some(Typ::Array(elem)) if matches!(elem.canonical(), Typ::Array(_)));
    if nested {
        let root = crate::native_emit::global_data::indexed_root(base)
            .ok_or("x86_64-lower: nested indexing needs static array storage")?;
        if ctx.locals.contains_key(root) || !ctx.globals.contains_key(root) {
            return Err("x86_64-lower: nested indexing needs static array storage".into());
        }
        emitter.emit_insns(&x86_64::mov_rr(RDX, RDI));
    }
    emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
    emitter.emit_insns(&x86_64::add_rr(RDI, RAX));
    emitter.emit_insns(&x86_64::mov_r_m(RAX, RDI, 16));
    if nested {
        emitter.emit_insns(&x86_64::add_rr(RAX, RDX));
    }

    let end = emitter.len();
    emitter.emit_insns(&x86_64::jmp_rel32(0));
    emitter.patch_u32(null + 2, emitter.len() - null - 6);
    emitter.patch_u32(out_of_bounds + 2, emitter.len() - out_of_bounds - 6);
    emit_failure_return(emitter, ctx)?;
    emitter.patch_u32(end + 1, emitter.len() - end - 5);
    if target_reg != RAX {
        emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
    }
    Ok(())
}

fn lower_string_lit(
    emitter: &mut CodeEmitter,
    target_reg: u8,
    content: &str,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    // In 32-bit mode mov_ri64 emits 1-byte opcode + 4-byte immediate; in 64-bit it emits
    // 1-byte REX + 1-byte opcode + 8-byte immediate. The immediate starts after the opcode.
    let site = emitter.len() + if x86_64::is_32bit() { 1 } else { 2 };
    emitter.emit_insns(&x86_64::mov_ri64(target_reg, 0xDEADBEEF));
    pending_calls.push(PendingCall {
        site: site as u32,
        target: format!("@str_{}", content),
    });
    Ok(())
}

fn lower_value_control<F>(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    pending_calls: &mut Vec<PendingCall>,
    mut lower: F,
) -> Result<(), String>
where
    F: FnMut(
        &mut CodeEmitter,
        &mut LowerCtx<'_>,
        &Expr,
        &mut Vec<PendingCall>,
    ) -> Result<(), String>,
{
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            if scalar_expr_type(expr, ctx).is_none() {
                return Err(
                    "x86_64-lower: match value arm type mismatch or unknown result type".into(),
                );
            }
            let typ = scalar_expr_type(scrutinee, ctx)
                .ok_or("x86_64-lower: unknown match scrutinee type")?;
            if !crate::core_ir::match_value_exhaustive(&typ, arms, &|name| {
                ctx.structs.get(name).map(Vec::as_slice)
            }) {
                return Err("x86_64-lower: nonexhaustive match value".into());
            }
            let mut index = ctx.frame_size;
            let name = loop {
                let name = format!("__native_match_value_{index}");
                if !(ctx.locals.contains_key(&name) || ctx.globals.contains_key(&name)) {
                    break name;
                }
                index += 1;
            };
            ctx.alloc_local(&name, &typ)?;
            lower_stmt(
                emitter,
                ctx,
                &Stmt::Let(name.clone(), Some(typ.clone()), (**scrutinee).clone()),
                pending_calls,
            )?;
            let mut ends = Vec::new();
            for arm in arms {
                let (test, bindings) = crate::lower_core::value_pattern_test(
                    &arm.pattern,
                    &typ,
                    &|name| ctx.structs.get(name).map(Vec::as_slice),
                    Expr::Ident(name.clone()),
                )?;
                lower_expr_into(emitter, ctx, &test, RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 0));
                let next = emitter.len();
                emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
                let locals = ctx.locals.clone();
                let types = ctx.scalar_types.clone();
                let pointers = ctx.receiver_pointers.clone();
                let empty = ctx.empty_struct_bases.clone();
                for binding in &bindings {
                    if let Stmt::Let(name, Some(typ), _) = binding {
                        ctx.alloc_local(name, typ)?;
                    }
                    lower_stmt(emitter, ctx, binding, pending_calls)?;
                }
                let guard_next = if let Some(guard) = &arm.guard {
                    if !crate::core_ir::expr_diverges(guard)
                        && scalar_expr_type(guard, ctx) != Some(Typ::Bool)
                    {
                        return Err("x86_64-lower: match guard must be Bool".into());
                    }
                    lower_expr_into(emitter, ctx, guard, RAX, pending_calls)?;
                    emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 0));
                    let next = emitter.len();
                    emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
                    Some(next)
                } else {
                    None
                };
                lower(emitter, ctx, &arm.value, pending_calls)?;
                let done = emitter.len();
                emitter.emit_insns(&x86_64::jmp_rel32(0));
                ends.push(done);
                ctx.locals = locals;
                ctx.scalar_types = types;
                ctx.receiver_pointers = pointers;
                ctx.empty_struct_bases = empty;
                emitter.patch_u32(next + 2, emitter.len() - next - 6);
                if let Some(next) = guard_next {
                    emitter.patch_u32(next + 2, emitter.len() - next - 6);
                }
            }
            emit_failure_return(emitter, ctx)?;
            for done in ends {
                emitter.patch_u32(done + 1, emitter.len() - done - 5);
            }
            Ok(())
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            if !crate::core_ir::expr_diverges(cond)
                && scalar_expr_type(cond, ctx).map(|typ| typ.canonical()) != Some(Typ::Bool)
            {
                return Err("x86_64-lower: conditional requires a Bool condition".into());
            }
            let then_type = scalar_expr_type(then_expr, ctx);
            let else_type = scalar_expr_type(else_expr, ctx);
            if !crate::core_ir::expr_diverges(then_expr)
                && !crate::core_ir::expr_diverges(else_expr)
                && let (Some(then_type), Some(else_type)) = (then_type, else_type)
                && crate::core_ir::conditional_type(
                    then_expr,
                    Some(then_type),
                    else_expr,
                    Some(else_type),
                )
                .is_none()
            {
                return Err("x86_64-lower: conditional branch type mismatch".into());
            }
            lower_expr_into(emitter, ctx, cond, RAX, pending_calls)?;
            emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 0));
            let otherwise = emitter.len();
            emitter.emit_insns(&x86_64::jcc_near(0x04, 0));
            lower(emitter, ctx, then_expr, pending_calls)?;
            let done = emitter.len();
            emitter.emit_insns(&x86_64::jmp_rel32(0));
            emitter.patch_u32(otherwise + 2, emitter.len() - otherwise - 6);
            lower(emitter, ctx, else_expr, pending_calls)?;
            emitter.patch_u32(done + 1, emitter.len() - done - 5);
            Ok(())
        }
        Expr::Block { body, result } => {
            let locals = ctx.locals.clone();
            let types = ctx.scalar_types.clone();
            let pointers = ctx.receiver_pointers.clone();
            let empty = ctx.empty_struct_bases.clone();
            let lowered = (|| {
                for stmt in body {
                    if let Stmt::Let(name, typ, value) = stmt {
                        let resolved = typ
                            .clone()
                            .or_else(|| scalar_expr_type(value, ctx))
                            .ok_or("x86_64-lower: unknown block local type")?;
                        ctx.locals.remove(name);
                        ctx.receiver_pointers.remove(name);
                        ctx.empty_struct_bases.remove(name);
                        ctx.alloc_local(name, &resolved)?;
                    }
                    lower_stmt(emitter, ctx, stmt, pending_calls)?;
                }
                if crate::core_ir::stmts_diverge(body) {
                    return Ok(());
                }
                if let Some(result) = result {
                    lower(emitter, ctx, result, pending_calls)
                } else {
                    lower(emitter, ctx, &Expr::IntLit(0), pending_calls)
                }
            })();
            ctx.locals = locals;
            ctx.scalar_types = types;
            ctx.receiver_pointers = pointers;
            ctx.empty_struct_bases = empty;
            lowered
        }
        _ => Err("x86_64-lower: expected conditional or block value".into()),
    }
}

fn lower_expr_into(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    target_reg: u8,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if matches!(
        expr,
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. }
    ) {
        return lower_value_control(
            emitter,
            ctx,
            expr,
            pending_calls,
            |emitter, ctx, value, pending_calls| {
                lower_expr_into(emitter, ctx, value, target_reg, pending_calls)
            },
        );
    }

    match expr {
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. } => unreachable!(),
        Expr::UnitLit => lower_int_lit(emitter, target_reg, 0),
        Expr::IntLit(value) => lower_int_lit(emitter, target_reg, *value),
        Expr::BoolLit(value) => lower_bool_lit(emitter, target_reg, *value),
        Expr::Ident(name) => {
            lower_ident_ref(emitter, ctx, target_reg, name.as_str(), pending_calls)
        }
        Expr::Binary { op, lhs, rhs, .. } => lower_binary_expr(
            emitter,
            ctx,
            target_reg,
            op.as_str(),
            lhs,
            rhs,
            pending_calls,
        ),
        Expr::Call { callee, args, .. } => {
            lower_call_expr(emitter, ctx, target_reg, callee, args, pending_calls)
        }
        Expr::StructInit { name, fields, .. } => {
            lower_struct_init(emitter, ctx, name.as_str(), fields, pending_calls)
        }
        Expr::Field { base, name, .. } => {
            lower_field_access(emitter, ctx, target_reg, base, name.as_str(), pending_calls)
        }
        Expr::Unary { op, expr, .. } => {
            lower_unary_expr(emitter, ctx, target_reg, op.as_str(), expr, pending_calls)
        }
        Expr::Index { base, index, .. } => {
            if let Some(typ) = scalar_expr_type(base, ctx)
                && record_array_fields(ctx.structs, &typ)?.is_some()
            {
                return Err(
                    "x86_64-lower: record array value requires typed aggregate transfer".into(),
                );
            }
            lower_index_expr(emitter, ctx, target_reg, base, index, pending_calls)
        }
        Expr::StringLit(content) => {
            lower_string_lit(emitter, target_reg, content.as_str(), pending_calls)
        }
        Expr::FloatLit(v) => {
            if x86_64::is_32bit() {
                return Err("x86_64-lower: Float requires 64-bit mode".into());
            }
            lower_int_lit(emitter, target_reg, v.0.to_bits() as i64)
        }
        Expr::ArrayLit(items) => {
            if items.iter().any(|item| {
                scalar_expr_type(item, ctx).map(|typ| typ.canonical()) == Some(Typ::Void)
            }) {
                return Err("x86_64-lower: unit array elements require a zero-width layout".into());
            }

            let offsets = materialize_array_literal(emitter, ctx, items, pending_calls, None)?;
            lower_heap_array(emitter, ctx, &offsets, pending_calls)?;
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
            }
            Ok(())
        }
        Expr::Closure { body, .. } => {
            if let Some(Stmt::Return(Some(e)) | Stmt::Expr(e)) = body.last() {
                return lower_expr_into(emitter, ctx, e, target_reg, pending_calls);
            }
            lower_int_lit(emitter, target_reg, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsigned_word_ops_use_unsigned_x86_encodings_in_both_modes() {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _restore = Restore(x86_64::is_32bit());
        for word32 in [false, true] {
            x86_64::set_32bit(word32);
            for op in ["udiv", "urem", "lshr"] {
                let mut module =
                    crate::in_lang_parse::parse_in_source("fn main(a:Int,b:Int)->Int{return a/b;}")
                        .unwrap();
                let Decl::Function { body, .. } = &mut module.decls[0] else {
                    panic!("function")
                };
                let Stmt::Return(Some(Expr::Binary { op: actual, .. })) = &mut body[0] else {
                    panic!("binary")
                };
                *actual = op.into();
                let result = lower_module(&module, "main").expect("unsigned lower");
                let expected = if op == "lshr" {
                    x86_64::shr_reg_cl(RAX)
                } else {
                    x86_64::div_reg(RCX)
                };
                assert!(
                    result
                        .code
                        .windows(expected.len())
                        .any(|bytes| bytes == expected),
                    "{word32}/{op}"
                );
                if op != "lshr" {
                    let signed = x86_64::idiv_reg(RCX);
                    assert!(
                        !result
                            .code
                            .windows(signed.len())
                            .any(|bytes| bytes == signed)
                    );
                }
            }
        }
    }

    #[test]
    fn absolute_references_preserve_code_and_data_targets_in_both_word_modes() {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _restore = Restore(x86_64::is_32bit());
        for is_32bit in [false, true] {
            x86_64::set_32bit(is_32bit);
            let module = crate::in_lang_parse::parse_in_source(
                "var count:Int=41;\nfn target()->Int{return 1;}\nfn main()->Int{let pointer=target;let text=\"payload\";count=count+1;return count;}",
            ).unwrap();
            let result = lower_module_with_bases(&module, "main", 0x1000, 0x8000).unwrap();
            assert!(result.exports.windows(2).all(|pair| pair[0].1 <= pair[1].1));
            let repeated = lower_module_with_bases(&module, "main", 0x1000, 0x8000).unwrap();
            assert_eq!(result.exports, repeated.exports);
            let target = result
                .exports
                .iter()
                .find(|(name, _)| name == "target")
                .unwrap()
                .1;
            assert!(
                result
                    .absolute_refs
                    .iter()
                    .any(|reference| reference.target_section == NativeSection::Code
                        && reference.target_offset == u64::from(target))
            );
            assert!(
                result
                    .absolute_refs
                    .iter()
                    .any(|reference| reference.target_section == NativeSection::Data)
            );
            assert!(
                result
                    .absolute_refs
                    .iter()
                    .filter(|reference| reference.target_section == NativeSection::Code)
                    .count()
                    >= 2
            );
            assert_eq!(result.absolute_refs.len(), result.relocations.len());
            for reference in &result.absolute_refs {
                assert_eq!(reference.width, if is_32bit { 4 } else { 8 });
                let base = if reference.target_section == NativeSection::Code {
                    0x1000
                } else {
                    0x8000
                };
                let start = reference.site as usize;
                let mut bytes = [0; 8];
                bytes[..usize::from(reference.width)]
                    .copy_from_slice(&result.code[start..start + usize::from(reference.width)]);
                assert_eq!(u64::from_le_bytes(bytes), base + reference.target_offset);
            }
        }
    }

    #[test]
    fn absolute_reference_patches_reject_invalid_ranges_and_pointer_widths() {
        let reference = AbsoluteReference {
            site: 0,
            width: 8,
            target_section: NativeSection::Data,
            target_offset: 1,
        };
        let mut code = [0; 8];
        assert!(patch_absolute_reference(&mut code, reference, u64::MAX, false).is_err());
        assert!(patch_absolute_reference(&mut code[..4], reference, 0, false).is_err());
        assert!(patch_absolute_reference(&mut code, reference, 0, true).is_err());
        assert!(
            patch_absolute_reference(
                &mut code,
                AbsoluteReference {
                    width: 4,
                    ..reference
                },
                u64::from(u32::MAX),
                true
            )
            .is_err()
        );
        assert_eq!(code, [0; 8]);
    }

    use super::*;
    use crate::core_ir::UnifiedModule;

    #[cfg(all(target_arch = "x86_64", unix))]
    #[test]
    fn string_comparisons_execute_by_content_in_both_layouts() {
        for layout in [X86StringLayout::Instring, X86StringLayout::Cstring] {
            for op in ["==", "!=", "===", "!=="] {
                let module = UnifiedModule::new(vec![Decl::Function {
                    name: "compare".into(),
                    params: vec![("left".into(), Typ::String), ("right".into(), Typ::String)],
                    ret: Typ::Bool,
                    type_params: vec![],
                    body: vec![Stmt::Return(Some(Expr::Binary {
                        op: op.into(),
                        lhs: Box::new(Expr::Ident("left".into())),
                        rhs: Box::new(Expr::Ident("right".into())),
                    }))],
                }]);
                let result =
                    lower_module_with_bases_layout(&module, "compare", 0, 0, layout).unwrap();
                let offsets: Vec<_> = result
                    .exports
                    .iter()
                    .map(|(name, offset)| (name.clone(), *offset, 0))
                    .collect();
                let mut runtime = crate::jit_runtime::JitRuntime::new();
                runtime.load(&result.code, &offsets, &[]).unwrap();
                for (left, right, equal) in [
                    ("", "", true),
                    ("λ雪", "λ雪", true),
                    ("a", "ab", false),
                    ("ab", "ac", false),
                ] {
                    let mut left_bytes = Vec::new();
                    let mut right_bytes = Vec::new();
                    append_string_payload(&mut left_bytes, left, layout).unwrap();
                    append_string_payload(&mut right_bytes, right, layout).unwrap();
                    let actual = unsafe {
                        runtime.invoke(
                            "compare",
                            &[left_bytes.as_ptr() as i64, right_bytes.as_ptr() as i64],
                        )
                    };
                    let expected = if matches!(op, "==" | "===") {
                        equal
                    } else {
                        !equal
                    };
                    assert_eq!(
                        actual,
                        Some(i64::from(expected)),
                        "{layout:?}: {left:?} {op} {right:?}"
                    );
                }
            }
            let module = UnifiedModule::new(vec![Decl::Function {
                name: "match_string".into(),
                params: vec![("text".into(), Typ::String)],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![Stmt::Match {
                    scrutinee: Expr::Ident("text".into()),
                    arms: [("\"first\"", 1), ("\"λ\"", 42), ("_", 0)]
                        .into_iter()
                        .map(|(pattern, value)| MatchArm {
                            pattern: pattern.into(),
                            guard: None,
                            body: vec![Stmt::Return(Some(Expr::IntLit(value)))],
                        })
                        .collect(),
                }],
            }]);
            let result =
                lower_module_with_bases_layout(&module, "match_string", 0, 0, layout).unwrap();
            let offsets: Vec<_> = result
                .exports
                .iter()
                .map(|(name, offset)| (name.clone(), *offset, 0))
                .collect();
            let relocations: Vec<_> = result
                .relocations
                .iter()
                .map(|site| (*site, result.codegen_base))
                .collect();
            let mut runtime = crate::jit_runtime::JitRuntime::new();
            runtime.load(&result.code, &offsets, &relocations).unwrap();
            for (text, expected) in [("λ", 42), ("first", 1), ("", 0), ("λ雪", 0)] {
                let mut bytes = Vec::new();
                append_string_payload(&mut bytes, text, layout).unwrap();
                let actual = unsafe { runtime.invoke("match_string", &[bytes.as_ptr() as i64]) };
                assert_eq!(actual, Some(expected), "{layout:?}: match {text:?}");
            }
        }
    }

    #[cfg(all(target_arch = "x86_64", unix))]
    #[test]
    fn string_concat_executes_with_real_payloads_and_ordered_nested_calls() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let _profile = antidecomp::ProfileGuard::new(profile);
            let module = crate::in_lang_parse::parse_in_source(
            "fn concat(left:String,right:String)->String{return left+right;}\nfn main()->Int{return 42;}",
        )
        .unwrap();
            let result = lower_module(&module, "concat").unwrap();
            let offsets: Vec<_> = result
                .exports
                .iter()
                .map(|(name, offset)| (name.clone(), *offset, 0))
                .collect();
            let relocations: Vec<_> = result
                .relocations
                .iter()
                .map(|site| (*site, result.codegen_base))
                .collect();
            let mut runtime = crate::jit_runtime::JitRuntime::new();
            runtime.load(&result.code, &offsets, &relocations).unwrap();
            for (left, right, expected) in [
                ("λ", "雪", "λ雪"),
                ("", "", ""),
                ("", "right", "right"),
                ("a\0", "\0b", "a\0\0b"),
            ] {
                let mut a = Vec::new();
                let mut b = Vec::new();
                append_string_payload(&mut a, left, X86StringLayout::Instring).unwrap();
                append_string_payload(&mut b, right, X86StringLayout::Instring).unwrap();
                let args = [a.as_ptr() as i64, b.as_ptr() as i64];
                let actual = unsafe { runtime.invoke("concat", &args) }.unwrap();
                // Fail safely on the old address-add implementation before dereferencing its result.
                assert_ne!(actual, args[0].wrapping_add(args[1]));
                assert_ne!(actual, 0);
                let length = unsafe { std::ptr::read_unaligned(actual as *const u64) } as usize;
                assert_eq!(length, expected.len());
                let bytes =
                    unsafe { std::slice::from_raw_parts((actual as *const u8).add(8), length) };
                assert_eq!(bytes, expected.as_bytes());
            }
            let module = crate::in_lang_parse::parse_in_source(
                r#"
var trace:Int=0
fn left()->String {trace=trace*10+1;return "λ";}
fn right()->String {trace=trace*10+2;return "雪";}
fn concat()->String {return (left()+right())+"!";}
fn read()->Int {return trace;}
fn main()->Int {return 42;}
"#,
            )
            .unwrap();
            let result = lower_module(&module, "concat").unwrap();
            let offsets: Vec<_> = result
                .exports
                .iter()
                .map(|(name, offset)| (name.clone(), *offset, 0))
                .collect();
            let relocations: Vec<_> = result
                .relocations
                .iter()
                .map(|site| (*site, result.codegen_base))
                .collect();
            let mut runtime = crate::jit_runtime::JitRuntime::new();
            runtime
                .load_with_data_relocations(
                    &result.code,
                    &offsets,
                    &relocations,
                    &result.data,
                    &result.global_relocs,
                    &result.data_relocations,
                )
                .unwrap();
            let actual = unsafe { runtime.invoke("concat", &[]) }.unwrap();
            assert_eq!(unsafe { runtime.invoke("read", &[]) }, Some(12));
            assert_ne!(actual, 0);
            let length = unsafe { std::ptr::read_unaligned(actual as *const u64) } as usize;
            assert_eq!(length, "λ雪!".len());
            assert_eq!(
                unsafe { std::slice::from_raw_parts((actual as *const u8).add(8), length) },
                "λ雪!".as_bytes()
            );
        }
    }

    #[test]
    fn string_concat_refuses_non_hosted_layouts_and_modes() {
        let module = crate::in_lang_parse::parse_in_source(
            "fn concat(left:String,right:String)->String{return left+right;}\nfn main()->Int{return 42;}",
        )
        .unwrap();
        for (layout, runtime) in [
            (X86StringLayout::Cstring, true),
            (X86StringLayout::Instring, false),
        ] {
            assert!(
                lower_module_impl(&module, "concat", 0, 0, layout, runtime)
                    .err()
                    .unwrap()
                    .contains("hosted 64-bit Instring")
            );
        }
        struct WidthGuard(bool);
        impl Drop for WidthGuard {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _guard = WidthGuard(x86_64::is_32bit());
        x86_64::set_32bit(true);
        assert!(
            lower_module_impl(&module, "concat", 0, 0, X86StringLayout::Instring, true)
                .err()
                .unwrap()
                .contains("hosted 64-bit Instring")
        );
    }

    #[test]
    fn optimized_global_expressions_emit_initialized_data() {
        let source = "var difference: Int = 20 - (3 * 4)\nvar negative: Int = -(2 + 3)\nvar mask: Int = (1 << 5) | 3\nfn main() -> Int { return difference; }";
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).expect("parse");
            crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            let result = lower_module(&module, "main").expect("lower");
            let expected: Vec<_> = [8i64, -5, 35]
                .into_iter()
                .flat_map(i64::to_le_bytes)
                .collect();
            assert_eq!(
                &result.data[..24],
                expected.as_slice(),
                "profile {profile:?}"
            );
        }
    }

    fn make_simple_fn_module() -> UnifiedModule {
        let src = r#"
fn answer() -> Int {
  return 42
}

fn main() -> void { return }
"#;
        crate::in_lang_parse::parse_in_source(src).expect("parse")
    }

    #[test]
    fn string_length_needs_no_runtime_imports() {
        for name in ["str-len", "str_len", "__inrt_str_len"] {
            let module = crate::in_lang_parse::parse_in_source(&format!(
                "fn main() -> Int {{ return {name}(\"hello\"); }}"
            ))
            .unwrap();
            lower_module(&module, "main").unwrap();
            let native = lower_native_module(&module, "main").unwrap();
            assert!(native.externs.is_empty(), "{:?}", native.externs);
        }
    }

    #[test]
    fn jit_rejects_unresolved_calls_but_native_retains_linker_imports() {
        let module = crate::in_lang_parse::parse_in_source(
            "extern c fn missing() -> Int;\nfn main() -> Int { return missing(); }",
        )
        .unwrap();
        let error = lower_module(&module, "main")
            .err()
            .expect("unresolved import must be rejected");
        assert!(
            error.contains("unresolved runtime imports: missing"),
            "{error}"
        );
        let native = lower_native_module(&module, "main").unwrap();
        assert_eq!(native.externs, ["missing"]);
        assert_eq!(native.extern_calls.len(), 1);
        let valid =
            crate::in_lang_parse::parse_in_source("fn main() -> Int { return 42; }").unwrap();
        lower_module(&valid, "main").unwrap();
    }

    #[test]
    fn jit_stdlib_rejects_stack_arguments_before_calling_host_wrappers() {
        crate::native_emit::native_link::bootstrap_jit_native();
        let module = crate::in_lang_parse::parse_in_source(
            "fn main() -> Bool { return str-eq(\"a\", \"a\", \"a\", \"a\", \"a\", \"a\", \"a\"); }",
        )
        .unwrap();
        let error = lower_module(&module, "main")
            .err()
            .expect("stack arguments must be rejected");
        assert!(
            error.contains("JIT stdlib wrappers require 64-bit register arguments"),
            "{error}"
        );
    }

    #[test]
    fn unresolved_function_addresses_never_leave_placeholders() {
        let module = crate::in_lang_parse::parse_in_source(
            "extern c fn missing() -> Int;\nfn main() -> Int { return missing; }",
        )
        .unwrap();
        for result in [
            lower_module(&module, "main"),
            lower_native_module(&module, "main"),
        ] {
            assert!(
                result
                    .err()
                    .expect("unresolved import must be rejected")
                    .contains("unresolved function address `missing`")
            );
        }
    }

    #[test]
    fn jit_string_return_and_stdlib_extern_lower_together() {
        for name in ["process-run", "process_run"] {
            let src = format!(
                "import std.process;\ncapability process.spawn;\nfn main() -> String {{ return {name}(\"true\"); }}\n"
            );
            let module = crate::in_lang_parse::parse_in_source(&src).expect("parse");
            crate::native_emit::native_link::bootstrap_jit_native();
            let result = lower_module(&module, "main").expect("lower");
            let native = lower_native_module(&module, "main").expect("native lower");
            assert_eq!(native.externs, ["process-run"]);
            assert!(
                native
                    .extern_calls
                    .iter()
                    .all(|(target, _)| target == "process-run")
            );
            let has_call_rax = result.code.windows(2).any(|w| w == [0xFF, 0xD0]);
            let has_unresolved_rel32 = result.code.windows(5).any(|w| w == [0xE8, 0, 0, 0, 0]);
            assert!(has_call_rax, "stdlib extern must lower to call rax");
            assert!(
                !has_unresolved_rel32,
                "stdlib extern must not stay an unresolved rel32 call"
            );
            result
                .require_resolved_imports()
                .expect("resolved stdlib import");
        }
    }

    #[test]
    fn jit_stdlib_rejects_incompatible_extern_signatures_and_keeps_defined_overrides() {
        crate::native_emit::native_link::bootstrap_jit_native();
        for (name, params, ret, args) in [
            ("env-temp-dir", vec![], Typ::Float, vec![]),
            (
                "process-run",
                vec![("command".into(), Typ::Int)],
                Typ::Int,
                vec![Expr::IntLit(1)],
            ),
        ] {
            let module = UnifiedModule::new(vec![
                Decl::Function {
                    name: name.into(),
                    params,
                    ret: ret.clone(),
                    body: vec![],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret,
                    body: vec![Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident(name.into())),
                        args,
                    }))],
                    type_params: vec![],
                },
            ]);
            assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
            let error = lower_module(&module, "main")
                .err()
                .expect("incompatible wrapper must refuse");
            assert!(
                error.contains("incompatible JIT stdlib declaration"),
                "{error}"
            );
        }
        let module = crate::in_lang_parse::parse_in_source("fn process-run(command:Int)->Int{return command;}\nfn main()->Int{return process-run(42);}").unwrap();
        let result = lower_module(&module, "main").unwrap();
        result.require_resolved_imports().unwrap();
        let module =
            crate::in_lang_parse::parse_in_source("fn main()->String{return process-run(1);}")
                .unwrap();
        let error = lower_module(&module, "main")
            .err()
            .expect("invalid low-level argument must refuse");
        assert!(
            error.contains("incompatible JIT stdlib arguments"),
            "{error}"
        );
    }

    fn make_arith_fn_module() -> UnifiedModule {
        let src = r#"
fn add(a: Int, b: Int) -> Int {
  return a + b
}

fn main() -> void { return }
"#;
        crate::in_lang_parse::parse_in_source(src).expect("parse")
    }

    fn make_multi_fn_module() -> UnifiedModule {
        let src = r#"
fn helper() -> Int {
  return 7
}

fn entry() -> Int {
  return helper()
}

fn main() -> void { return }
"#;
        crate::in_lang_parse::parse_in_source(src).expect("parse")
    }

    fn make_if_module() -> UnifiedModule {
        let src = r#"
fn max(a: Int, b: Int) -> Int {
  if a > b {
    return a
  } else {
    return b
  }
}

fn main() -> void { return }
"#;
        crate::in_lang_parse::parse_in_source(src).expect("parse")
    }

    #[test]
    fn lower_simple_return() {
        let module = make_simple_fn_module();
        let result = lower_module(&module, "answer").expect("lower");
        assert!(!result.code.is_empty());
        // Should contain `mov rax, 42` and `ret`
        assert!(result.code.windows(2).any(|w| w == [0x48, 0xB8]));
        assert!(result.code.contains(&0xC3));
    }

    #[test]
    fn lower_arithmetic() {
        let module = make_arith_fn_module();
        let result = lower_module(&module, "add").expect("lower");
        assert!(!result.code.is_empty());
        assert!(result.code.contains(&0xC3)); // ret
    }

    #[test]
    fn lower_multi_function_call() {
        let module = make_multi_fn_module();
        let result = lower_module(&module, "entry").expect("lower");
        assert!(!result.code.is_empty());
        // Should contain a call instruction
        assert!(result.code.contains(&0xE8)); // call rel32
        assert!(result.code.contains(&0xC3)); // ret
    }

    #[test]
    fn rejects_loop_jumps_outside_lexical_loops() {
        for jump in ["break", "continue"] {
            for body in [
                format!("{jump};"),
                format!("while false {{ {jump}; }} {jump};"),
            ] {
                let module = crate::in_lang_parse::parse_in_source(&format!(
                    "fn main() -> Int {{ {body} return 0; }}"
                ))
                .unwrap();
                assert!(
                    lower_module(&module, "main")
                        .err()
                        .unwrap()
                        .contains("outside a loop")
                );
            }
        }
    }

    #[test]
    fn word32_scalar_float_abi_rejects_definitions_and_selected_calls() {
        struct WidthGuard(bool);
        impl Drop for WidthGuard {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _width = WidthGuard(x86_64::is_32bit());
        x86_64::set_32bit(true);
        for source in [
            "fn main(value:Float)->Int{return 42;}",
            "fn main(value:Float)->Float{return value;}",
            "extern asm fn read_float()->Float\nfn main()->Int{read_float();return 42;}",
            "extern asm fn read_float()->Float\nextern asm fn consume_float(value:Float)->Int\nfn main()->Int{return consume_float(read_float());}",
            "extern asm fn consume_float(value:Float)->Int\nfn main(value:Float)->Int{return consume_float(value);}",
            "extern asm fn array-len(value:[Int])->Float\nfn main()->Int{array-len([1,2]);return 42;}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            let error = lower_native_module(&module, "main").err().unwrap();
            assert!(
                error.contains("Float requires 64-bit mode"),
                "{source}: {error}"
            );
        }
        for source in [
            "fn main(value:Int)->Int{return value;}",
            "fn main(value:Bool)->Bool{return value;}",
            "extern asm fn read_int()->Int\nfn main()->Int{return read_int();}",
            "extern asm fn read_float()->Float\nfn main()->Int{return 42;}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            assert!(
                !lower_native_module(&module, "main")
                    .unwrap()
                    .code
                    .is_empty(),
                "{source}"
            );
        }
        x86_64::set_32bit(false);
        let module =
            crate::in_lang_parse::parse_in_source("fn main(value:Float)->Float{return value;}")
                .unwrap();
        assert!(
            !lower_native_module(&module, "main")
                .unwrap()
                .code
                .is_empty()
        );
    }
    #[test]
    fn rejects_32bit_float_logical_not_and_keeps_integer_not() {
        struct WidthGuard(bool);
        impl Drop for WidthGuard {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _width = WidthGuard(x86_64::is_32bit());
        x86_64::set_32bit(true);
        for source in [
            "fn main(value:Float)->Bool{return !value;}",
            "extern asm fn read_float()->Float\nfn main()->Bool{return !read_float();}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            assert!(
                lower_native_module(&module, "main")
                    .err()
                    .unwrap()
                    .contains("Float requires 64-bit mode"),
                "{source}"
            );
        }
        for source in [
            "fn main(value:Int)->Bool{return !value;}",
            "extern asm fn read_int()->Int\nfn main()->Bool{return !read_int();}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            assert!(
                !lower_native_module(&module, "main")
                    .unwrap()
                    .code
                    .is_empty()
            );
        }
    }

    #[test]
    fn keeps_32bit_local_arrays_without_allocator_imports() {
        struct WidthGuard(bool);
        impl Drop for WidthGuard {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _width = WidthGuard(x86_64::is_32bit());
        x86_64::set_32bit(true);
        let module = crate::in_lang_parse::parse_in_source("fn main(index: Int) -> Int { let xs: [Int] = [1, 2]; let alias = xs; alias[index] = 7; return xs[index] + array_len(alias); }").unwrap();
        let result = lower_native_module(&module, "main").unwrap();
        assert!(result.externs.is_empty());
        let mut loop_module = crate::in_lang_parse::parse_in_source("fn main(item: Int) -> Int { let xs: [Int] = [1, 2]; var sum: Int = 0; while false { if item == 1 { continue; } sum += item; } return sum; }").unwrap();
        let crate::core_ir::Decl::Function { body, .. } = &mut loop_module.decls[0] else {
            panic!("function");
        };
        let Stmt::Loop { kind, cond, .. } = &mut body[2] else {
            panic!("loop");
        };
        *kind = LoopKind::For {
            binding: "item".into(),
        };
        *cond = Some(Expr::Ident("xs".into()));
        assert!(
            lower_native_module(&loop_module, "main")
                .unwrap()
                .externs
                .is_empty()
        );
        let escaped = crate::in_lang_parse::parse_in_source(
            "fn main() -> [Int] { let xs = [1, 2]; return xs; }",
        )
        .unwrap();
        assert!(
            lower_native_module(&escaped, "main")
                .err()
                .unwrap()
                .contains("escaping 32-bit arrays")
        );
    }

    #[test]
    fn lowers_vec_for_loop() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("values".into(), Typ::Array(Box::new(Typ::Int)))],
                ret: Typ::Int,
                body: vec![
                    Stmt::Loop {
                        kind: LoopKind::For {
                            binding: "value".into(),
                        },
                        cond: Some(Expr::Ident("values".into())),
                        body: vec![],
                    },
                    Stmt::Return(Some(Expr::IntLit(0))),
                ],
                type_params: vec![],
            }],
        };
        let result = lower_module(&module, "main").expect("for-loop should lower");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_if_else() {
        let module = make_if_module();
        let result = lower_module(&module, "max").expect("lower");
        assert!(!result.code.is_empty());
        // Should contain je/jne
        assert!(result.code.contains(&0x74) || result.code.contains(&0x75));
    }

    #[test]
    fn lower_prologue_and_epilogue() {
        let module = make_simple_fn_module();
        let result = lower_module(&module, "answer").expect("lower");
        // prologue: push rbp (0x55)
        assert_eq!(result.code[0], 0x55);
        // epilogue: ... ret (0xC3)
        assert_eq!(result.code[result.code.len() - 1], 0xC3);
    }

    #[test]
    fn exports_contains_functions() {
        let module = make_multi_fn_module();
        let result = lower_module(&module, "entry").expect("lower");
        assert!(result.exports.iter().any(|(name, _)| name == "entry"));
        assert!(result.exports.iter().any(|(name, _)| name == "helper"));
        assert!(result.exports.iter().any(|(name, _)| name == "main"));
    }

    #[test]
    fn entry_offset_is_valid() {
        let module = make_simple_fn_module();
        let result = lower_module(&module, "answer").expect("lower");
        assert!(result.entry_offset < result.code.len() as u32);
    }

    #[test]
    fn rejects_empty_module() {
        let module = UnifiedModule::new(Vec::new());
        assert!(lower_module(&module, "main").is_err());
    }

    #[test]
    fn find_loop_sizes() {
        // Test with outb to reproduce the real scenario
        let src = r#"
fn answer() -> Int {
  let i = 0
  while i < 3 {
    let ch = 49 + i
    outb(0x3F8, ch)
    outb(0x3F8, 10)
    i = i + 1
  }
  return 0
}

fn main() -> void { return }
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "answer").expect("lower");
        eprintln!("Loop test code size: {} bytes", result.code.len());

        let code = &result.code;
        for i in 0..code.len() {
            // jmp rel32 (0xE9 + rel32)
            if i + 4 < code.len() && code[i] == 0xE9 && code[i + 1..i + 5] != [0, 0, 0, 0] {
                let off = i32::from_le_bytes(code[i + 1..i + 5].try_into().unwrap());
                let target = (i as i32 + 5 + off) as i32;
                eprintln!(
                    "  jmp_rel32 at {:x}: offset={} target={} (backward={})",
                    i,
                    off,
                    target,
                    target < i as i32
                );
            }
            // jmp rel8 (0xEB + rel8)
            if i + 1 < code.len() && code[i] == 0xEB {
                let off = code[i + 1] as i8;
                let target = (i as i32 + 2 + off as i32) as i32;
                eprintln!(
                    "  jmp_rel8 at {:x}: offset={} target={} (backward={})",
                    i,
                    off,
                    target,
                    target < i as i32
                );
            }
            // jcc_near je (0F 84 + rel32)
            if i + 4 < code.len() && code[i] == 0x0F && code[i + 1] == 0x84 {
                let off = i32::from_le_bytes(code[i + 2..i + 6].try_into().unwrap());
                let target = (i as i32 + 6 + off) as i32;
                eprintln!(
                    "  jcc_near(je) at {:x}: offset={} target={}",
                    i, off, target
                );
            }
        }
    }

    #[test]
    fn lower_while_loop() {
        let src = r#"
fn answer() -> Int {
  let i = 0
  while i < 3 {
    i = i + 1
  }
  return 0
}

fn main() -> void { return }
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "answer").expect("lower");
        let code = &result.code;

        // Find the backward jump (jmp rel8 = 0xEB or jmp rel32 = 0xE9)
        let mut found_backward_jmp = false;
        let mut found_exit_jmp = false;
        for i in 0..code.len() {
            if i + 1 < code.len() && code[i] == 0xEB {
                let off = code[i + 1] as i8;
                let target = (i as i32 + 2 + off as i32) as usize;
                if target < i {
                    found_backward_jmp = true;
                }
            }
            if i + 4 < code.len() && code[i] == 0xE9 {
                let off = i32::from_le_bytes(code[i + 1..i + 5].try_into().unwrap());
                let target = (i as i32 + 5 + off) as usize;
                if target < i {
                    found_backward_jmp = true;
                }
            }
            // jcc_near je = 0F 84
            if i + 5 < code.len() && code[i] == 0x0F && code[i + 1] == 0x84 {
                found_exit_jmp = true;
            }
        }
        assert!(found_backward_jmp, "no backward jump found");
        assert!(found_exit_jmp, "no exit conditional jump found");
    }

    #[test]
    fn lower_while_loop_with_flag() {
        // Reproducer from the Space NVMe driver: a loop using a `done` flag that is
        // updated in the body must re-evaluate the condition each iteration.
        let src = r#"
fn count() -> Int {
  let done = 0
  let to = 0
  while done == 0 {
    to = to + 1
    if to >= 5 {
      done = 1
    }
  }
  return to
}

fn main() -> void { return }
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "count").expect("lower");
        let code = &result.code;

        // The backward jump must target the condition, not skip it. The condition
        // includes a comparison (cmp r/m, imm) and a conditional exit (0F 84).
        let mut found_backward_jmp = false;
        let mut found_exit_jmp = false;
        for i in 0..code.len() {
            if i + 1 < code.len() && code[i] == 0xEB {
                let off = code[i + 1] as i8;
                let target = (i as i32 + 2 + off as i32) as usize;
                if target < i {
                    found_backward_jmp = true;
                }
            }
            if i + 4 < code.len() && code[i] == 0xE9 {
                let off = i32::from_le_bytes(code[i + 1..i + 5].try_into().unwrap());
                let target = (i as i32 + 5 + off) as usize;
                if target < i {
                    found_backward_jmp = true;
                }
            }
            if i + 5 < code.len() && code[i] == 0x0F && code[i + 1] == 0x84 {
                found_exit_jmp = true;
            }
        }
        assert!(found_backward_jmp, "no backward jump found");
        assert!(found_exit_jmp, "no exit conditional jump found");
    }

    #[test]
    fn lower_seventh_argument_is_loaded_from_stack() {
        // Reproducer from the Space NVMe driver: a 7-argument function must load
        // the 7th argument from the caller's stack-argument area.
        let src = r#"
fn seven(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int, g: Int) -> Int {
  return g
}

fn main() -> void { return }
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "seven").expect("lower");
        let code = &result.code;

        // The prologue must load the 7th argument from [rbp + 16] and store it to
        // g's local slot. mov rax, [rbp+16] is 48 8B 45 10; mov [rbp-56], rax is
        // 48 89 45 C8. Fast emit no longer `rep stosq`-wipes the frame, so there
        // is no RDI/RCX save above the stack-arg area.
        let load_stack_arg = [0x48u8, 0x8B, 0x45, 0x10];
        let store_to_slot = [0x48u8, 0x89, 0x45, 0xC8];
        let stosq = [0xF3u8, 0x48, 0xAB];
        assert!(
            code.windows(load_stack_arg.len())
                .any(|w| w == load_stack_arg),
            "missing prologue load of stack argument: expected `mov rax, [rbp+16]`"
        );
        assert!(
            code.windows(store_to_slot.len())
                .any(|w| w == store_to_slot),
            "missing prologue store to g slot: expected `mov [rbp-56], rax`"
        );
        assert!(
            !code.windows(stosq.len()).any(|w| w == stosq),
            "fast default emit must not wipe the frame with `rep stosq`"
        );
    }

    #[test]
    fn seventh_argument_is_next_to_the_return_address() {
        let src = r#"
fn seven(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int, g: Int) -> Int {
  return g
}

fn main() -> Int {
  return seven(1, 2, 3, 4, 5, 6, 63)
}
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "main").expect("lower");
        // The outgoing value is reloaded from its staged frame slot before push rax.
        let stack_arg_end = result
            .code
            .windows(5)
            .position(|bytes| bytes[..3] == [0x48, 0x8B, 0x45] && bytes[4] == 0x50)
            .map(|site| site + 5)
            .or_else(|| {
                result
                    .code
                    .windows(8)
                    .position(|bytes| bytes[..3] == [0x48, 0x8B, 0x85] && bytes[7] == 0x50)
                    .map(|site| site + 8)
            })
            .expect("staged stack argument");
        let call = result.code[stack_arg_end..]
            .iter()
            .position(|&byte| byte == 0xE8)
            .expect("call")
            + stack_arg_end;
        assert!(
            !result.code[stack_arg_end..call]
                .windows(2)
                .any(|bytes| bytes == [0x41, 0x53]),
            "caller register saves must precede stack arguments"
        );
        let cleanup = result.code[call..]
            .windows(7)
            .position(|bytes| bytes == [0x48, 0x81, 0xC4, 8, 0, 0, 0])
            .expect("stack cleanup")
            + call;
        let restore = result.code[call..]
            .windows(2)
            .position(|bytes| bytes == [0x41, 0x5B])
            .expect("saved r11 restore")
            + call;
        assert!(
            cleanup < restore,
            "stack arguments must be removed before restoring registers"
        );
    }

    #[test]
    fn lower_match_int_arm() {
        let src = r#"
fn classify(x: Int) -> Int {
  match x {
    1 { return 10 }
    2 { return 20 }
    _ { return 99 }
  }
  return 0
}

fn main() -> void { return }
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "classify").expect("lower");
        assert!(!result.code.is_empty());
        // Should contain CMP (0x48 0x81 0xF8 or 0x48 0x39) and JNE (0x0F 0x85) and ret (0xC3)
        assert!(result.code.contains(&0xC3));
    }

    #[test]
    fn lower_match_default_only() {
        let src = r#"
fn default-match(x: Int) -> Int {
  match x {
    _ { return 42 }
  }
  return 0
}

fn main() -> void { return }
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "default_match").expect("lower");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_throw_generates_code() {
        let src = r#"
fn thrower() -> Int {
  throw 42
  return 0
}

fn main() -> void { return }
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "thrower").expect("lower");
        assert!(!result.code.is_empty());
        let store = x86_64::mov_m_r(15, 0, RAX);
        assert!(result.code.windows(store.len()).any(|bytes| bytes == store));
        assert!(
            lower_native_module(&module, "thrower")
                .err()
                .expect("native artifacts must provide error storage")
                .contains("runtime-provided writable error slot")
        );
    }

    #[test]
    fn lower_try_catch_generates_code() {
        let src = r#"
fn catcher() -> Int {
  try {
    let x = 1
  } catch e {
    return e
  }
  return 0
}

fn main() -> void { return }
"#;
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        let result = lower_module(&module, "catcher").expect("lower");
        assert!(!result.code.is_empty());
        assert!(result.code.contains(&0xC3));
    }

    #[test]
    fn lower_propagate_generates_conditional_return() {
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "propagate".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Propagate, Stmt::Return(Some(Expr::IntLit(7)))],
            type_params: vec![],
        }]);
        let result = lower_module(&module, "propagate").expect("lower");
        assert!(result.code.windows(2).any(|window| window == [0x0F, 0x84]));
        assert!(
            result
                .code
                .windows(4)
                .any(|bytes| bytes == [0x41, 0x80, 0x3F, 0x00])
        );
        let load = x86_64::mov_r_m(RAX, 15, 8);
        assert!(result.code.windows(load.len()).any(|bytes| bytes == load));
    }
}

#[cfg(test)]
mod profile_smoke_tests {
    use super::*;
    use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule};
    use crate::native_emit::antidecomp;

    #[test]
    fn lower_simple_default_and_harden() {
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            type_params: vec![],
        }]);
        antidecomp::clear_profile();
        let d = lower_module(&module, "main").expect("default lower");
        assert!(!d.code.is_empty());
        antidecomp::set_profile(crate::emit_profile::EmitProfile::Harden);
        let h = lower_module(&module, "main").expect("harden lower");
        antidecomp::clear_profile();
        assert!(h.code.len() >= d.code.len());
        assert_eq!(h.code[0], 0x53); // push rbx
        assert_eq!(d.code[0], 0x55); // push rbp
        assert_eq!(&d.code[1..5], &[0x48, 0x8D, 0x2C, 0x24]); // lea rbp, [rsp]
        assert!(
            h.code.windows(2).any(|w| w == [0x0F, 0x0B]),
            "harden should emit ud2 decoys"
        );
        assert!(
            !d.code.windows(3).any(|w| w == [0x48, 0x89, 0xE5]),
            "default must not use classic mov rbp, rsp"
        );
    }
}

#[cfg(test)]
#[test]
fn lowers_nested_scalar_struct_copies() {
    let module = crate::in_lang_parse::parse_in_source("struct Inner { Int x }\nstruct Outer { Inner inner }\nfn main() -> Int { var source: Outer = Outer { inner: Inner { x: 42 } }; var copied: Outer = source; return copied.inner.x; }").unwrap();
    lower_module(&module, "main").expect("nested scalar struct copy");
}

#[cfg(test)]
mod selected_mmio_contract_tests {
    fn lower(source: &str) -> Result<(), String> {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        super::lower_module(&module, "main").map(|_| ())
    }

    #[test]
    fn raw_lowering_checks_selected_mmio_declarations_and_actual_arguments() {
        for name in [
            "load8", "load16", "load32", "load64", "store8", "store16", "store32", "store64",
        ] {
            let store = name.starts_with("store");
            let signature = if store {
                "address:Int,value:Int"
            } else {
                "address:Int"
            };
            let ret = if store { "void" } else { "Int" };
            let arguments = if store { "0,42" } else { "0" };
            let source = format!(
                "extern asm fn {name}({signature})->{ret}\nfn main()->Int{{{name}({arguments});return 42;}}"
            );
            assert!(lower(&source).is_ok(), "canonical {name}");

            let bad_signature = if store {
                "address:Int,value:Bool"
            } else {
                "address:Bool"
            };
            let bad_arguments = if store { "0,false" } else { "false" };
            for source in [
                format!(
                    "extern asm fn {name}({bad_signature})->{ret}\nfn main()->Int{{{name}({bad_arguments});return 42;}}"
                ),
                format!(
                    "extern asm fn {name}({signature})->Bool\nfn main()->Int{{{name}({arguments});return 42;}}"
                ),
            ] {
                let error = lower(&source).unwrap_err();
                assert!(
                    error.contains("incompatible MMIO declaration"),
                    "{name}: {error}"
                );
            }
            for arguments in [
                bad_arguments,
                if store { "0,missing" } else { "missing" },
                "",
                if store { "0,42,7" } else { "0,42" },
            ] {
                let source = format!("fn main()->Int{{{name}({arguments});return 42;}}");
                let error = lower(&source).unwrap_err();
                assert!(
                    error.contains("incompatible MMIO arguments"),
                    "{name}: {error}"
                );
            }
        }
    }

    #[test]
    fn mmio_accepts_function_addresses_without_overriding_local_types() {
        assert!(
            lower(
                "fn target()->Int{return 42;}\nfn main()->Int{store64(0x38000000,target);return 42;}"
            )
            .is_ok()
        );
        let error = lower("fn target()->Int{return 42;}\nfn main()->Int{let target=false;store64(0x38000000,target);return 42;}").unwrap_err();
        assert!(error.contains("incompatible MMIO arguments"), "{error}");
    }

    #[test]
    fn defined_mmio_names_keep_their_ordinary_function_signature() {
        for name in [
            "load8", "load16", "load32", "load64", "store8", "store16", "store32", "store64",
        ] {
            // This is a real definition, not a selected memory intrinsic. Its
            // Bool parameter and Int return must retain their declared meaning.
            let source = format!(
                "fn {name}(value:Bool)->Int{{if value{{return 1;}}return 42;}}\nfn main()->Int{{return {name}(false);}}"
            );
            assert!(lower(&source).is_ok(), "defined override {name}");
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod void_fallthrough_tests {
    use super::*;

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn void_conditional_returns_keep_fallthrough_cleanup() {
        for body in [
            "if flag==0{return;}store64(0,42);",
            "if flag==0{if flag==0{return;}}store64(0,42);",
            "while flag==0{return;}store64(0,42);",
            "if flag==0{return;}store64(0,42);return;",
        ] {
            for (flag, expected) in [(0, 0), (1, 42)] {
                let source = format!(
                    "var value:Int=0;\nfn store64(p:Int,v:Int)->Void{{value=v;}}\nfn helper(flag:Int)->Void{{{body}}}\nfn main()->Int{{helper({flag});return value;}}"
                );
                let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                let result = lower_module(&module, "main").unwrap();
                let offsets: Vec<_> = result
                    .exports
                    .iter()
                    .map(|(name, offset)| (name.clone(), *offset, 0))
                    .collect();
                let relocations: Vec<_> = result
                    .relocations
                    .iter()
                    .map(|site| (*site, result.codegen_base))
                    .collect();
                let mut runtime = crate::jit_runtime::JitRuntime::new();
                runtime
                    .load_with_data_relocations(
                        &result.code,
                        &offsets,
                        &relocations,
                        &result.data,
                        &result.global_relocs,
                        &result.data_relocations,
                    )
                    .unwrap();
                assert_eq!(
                    unsafe { runtime.invoke("main", &[]) },
                    Some(expected),
                    "{source}"
                );
            }
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod float_logical_not_regressions {
    use super::*;

    #[test]
    fn float_not_uses_numeric_zero_and_evaluates_once() {
        for (value, expected) in [
            (0.0, true),
            (-0.0, true),
            (2.5, false),
            (f64::INFINITY, false),
            (f64::NEG_INFINITY, false),
            (f64::NAN, false),
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(
                    "var calls:Int=0;\nfn next()->Float{calls+=1;return 0.0;}\nfn main()->Int{if !next()==true && calls==1{return 42;}return 7;}"
                ).unwrap();
            for decl in &mut module.decls {
                if let Decl::Function { name, body, .. } = decl {
                    if name == "next" {
                        body[1] =
                            Stmt::Return(Some(Expr::FloatLit(crate::core_ir::FloatVal(value))));
                    }
                    if name == "main" {
                        for statement in body.iter_mut() {
                            crate::core_opt::map_stmt_exprs_mut(statement, &mut |expr| {
                                if let Expr::BoolLit(truth) = expr {
                                    *truth = expected;
                                }
                            });
                        }
                    }
                }
            }
            let result = lower_module(&module, "main").unwrap();
            let offsets = result
                .exports
                .iter()
                .map(|(name, offset)| (name.clone(), *offset, 0))
                .collect::<Vec<_>>();
            let relocations = result
                .relocations
                .iter()
                .map(|site| (*site, result.codegen_base))
                .collect::<Vec<_>>();
            let mut runtime = crate::jit_runtime::JitRuntime::new();
            runtime
                .load_with_data_relocations(
                    &result.code,
                    &offsets,
                    &relocations,
                    &result.data,
                    &result.global_relocs,
                    &result.data_relocations,
                )
                .unwrap();
            assert_eq!(
                unsafe { runtime.invoke("main", &[]) },
                Some(42),
                "! {value}"
            );
        }
    }
}

#[cfg(test)]
mod selected_inline_length_tests {
    use super::*;

    #[test]
    fn selected_length_declarations_validate_before_emission() {
        for name in ["str-len", "str_len", "array-len", "array_len"] {
            let (typ, value) = if name.starts_with("str") {
                ("String", "\"abc\"")
            } else {
                ("[Int]", "[1,2]")
            };
            for ret in ["Float", "Bool"] {
                let source = format!(
                    "extern asm fn {name}(value:{typ})->{ret};\nfn main()->Int{{let value:{ret}={name}({value});return 42;}}"
                );
                let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                let error = lower_module(&module, "main")
                    .err()
                    .expect("bad selected declaration must refuse");
                assert!(
                    error.contains("incompatible length declaration"),
                    "{name}: {error}"
                );
            }
            let source = format!(
                "extern asm fn {name}(value:{typ})->Int;\nfn main()->Int{{return {name}({value});}}"
            );
            let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            lower_module(&module, "main").unwrap();
        }
        let module = crate::in_lang_parse::parse_in_source("fn str-len(value:Float)->Float{return value;}\nfn main()->Int{let value:Float=str-len(2.5);return 42;}").unwrap();
        lower_module(&module, "main").unwrap();
    }
}

#[cfg(test)]
mod flat_record_array_tests {
    use super::*;

    #[test]
    fn record_arrays_refuse_32bit_and_nonflat_schemas() {
        struct WidthGuard(bool);
        impl Drop for WidthGuard {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _width = WidthGuard(x86_64::is_32bit());
        let structs = HashMap::from([
            ("Row".into(), vec![("x".into(), Typ::Int)]),
            (
                "Nested".into(),
                vec![("inner".into(), Typ::Named("Row".into()))],
            ),
        ]);
        x86_64::set_32bit(true);
        assert!(
            record_array_fields(&structs, &Typ::Array(Box::new(Typ::Named("Row".into())))).is_err()
        );
        x86_64::set_32bit(false);
        assert!(
            record_array_fields(&structs, &Typ::Array(Box::new(Typ::Named("Nested".into()))))
                .is_err()
        );
        assert!(
            record_array_fields(
                &structs,
                &Typ::Array(Box::new(Typ::Named("Unknown".into())))
            )
            .is_err()
        );
    }

    #[test]
    fn vector_record_literals_keep_legacy_layout_and_index_refusal() {
        let source = "struct Row{Int x}\nfn main()->Int{let first:Row=Row{x:20};let second:Row=Row{x:22};let rows:[Row]=[first,second];return array-len(rows)+40;}";
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { body, .. } = decl {
                for stmt in body {
                    if let Stmt::Let(name, typ, _) = stmt
                        && name == "rows"
                    {
                        *typ = Some(Typ::Vector(Box::new(Typ::Named("Row".into()))));
                    }
                }
            }
        }
        let result = lower_module_with_bases(&module, "main", 0, 0).unwrap();
        // Legacy vector construction allocates one slot table, without the new
        // per-element record payload allocations of a fixed record array.
        assert_eq!(
            result
                .extern_calls
                .iter()
                .filter(|(name, _)| name == "malloc")
                .count(),
            1
        );
        for decl in &mut module.decls {
            if let Decl::Function { body, .. } = decl {
                body.pop();
                body.push(Stmt::Let(
                    "copy".into(),
                    Some(Typ::Named("Row".into())),
                    Expr::Index {
                        base: Box::new(Expr::Ident("rows".into())),
                        index: Box::new(Expr::IntLit(0)),
                    },
                ));
                body.push(Stmt::Return(Some(Expr::IntLit(42))));
            }
        }
        assert!(
            lower_module(&module, "main")
                .err()
                .unwrap()
                .contains("fixed record array")
        );
    }

    #[test]
    fn nested_record_arrays_do_not_use_pointer_slots_as_relative_child_headers() {
        let structs = HashMap::from([("Row".into(), vec![("x".into(), Typ::Int)])]);
        let nested = Typ::Array(Box::new(Typ::Array(Box::new(Typ::Named("Row".into())))));
        assert!(
            record_array_fields(&structs, &nested)
                .err()
                .unwrap()
                .contains("nested record arrays")
        );
        let module = crate::in_lang_parse::parse_in_source("struct Row{Int x}\nvar rows:[[Row]]=[[Row{x:42}]]\nfn main()->Int{return array-len(rows);}").unwrap();
        assert!(
            lower_module(&module, "main")
                .err()
                .unwrap()
                .contains("nested record arrays")
        );
    }

    #[cfg(all(unix, target_arch = "x86_64"))]
    #[test]
    fn flat_record_arrays_preserve_values_aliases_and_heap_lifetimes() {
        for source in [
            r#"struct Row{Int x}
fn main()->Int{let rows:[Row]=[Row{x:20},Row{x:22}];return rows[0].x+rows[1].x;}"#,
            r#"struct Row{Int x}
fn main()->Int{let same:Row=Row{x:20};let rows:[Row]=[same,same];let copy:Row=rows[0];rows[0].x=22;return copy.x+rows[1].x+rows[0].x - 20;}"#,
            r#"struct Row{Int x;Int y}
fn main()->Int{let rows:[Row]=[Row{x:20,y:1},Row{x:1,y:2}];let alias=rows;alias[0]=Row{x:21,y:2};alias[0]=alias[0];let copy:Row=rows[0];rows[0].x=19;return copy.x+copy.y+rows[0].x;}"#,
            r#"struct Row{Int x}
fn make()->[Row]{let rows:[Row]=[Row{x:20},Row{x:22}];return rows;}
fn main()->Int{let rows=make();let trash:[Int]=[99,99,99];return rows[0].x+rows[1].x;}"#,
            r#"struct Row{Int x}
fn alter(rows:[Row])->Void{rows[0].x=22;}
fn main()->Int{let rows:[Row]=[Row{x:20},Row{x:20}];alter(rows);return rows[0].x+rows[1].x;}"#,
            r#"struct Row{Int x}
fn change(self:Row)->Void{self.x=self.x+2;}
fn main()->Int{let rows:[Row]=[Row{x:20},Row{x:20}];change(rows[0]);return rows[0].x+rows[1].x;}"#,
            r#"struct Row{Int x}
fn byvalue(value:Row)->Int{value.x=99;return 22;}
fn main()->Int{let rows:[Row]=[Row{x:20}];return byvalue(rows[0])+rows[0].x;}"#,
            r#"struct Row{Float amount;String text;Bool flag}
fn main()->Int{let rows:[Row]=[Row{amount:1.5,text:"λ",flag:false}];rows[0].text="雪";rows[0].flag=true;rows[0].amount=2.5;if rows[0].amount==2.5 && rows[0].text=="雪" && rows[0].flag{return 42;}return 0;}"#,
            r#"struct Row{Int x}
var rows:[Row]=[Row{x:20},Row{x:22}]
fn main()->Int{let copy:Row=rows[0];rows[0]=rows[1];rows[1].x=20;return copy.x+rows[0].x;}"#,
            r#"struct Row{Int x;Int y}
var rows:[Row]=[Row{x:20,y:1}]
var trace:Int=0
fn get()->[Row]{trace=trace*10+1;return rows;}
fn at()->Int{trace=trace*10+2;return 0;}
fn next()->Row{trace=trace*10+3;return Row{x:40,y:2};}
fn main()->Int{get()[at()]=next();if trace!=123{return 1;}return rows[0].x+rows[0].y;}"#,
            r#"struct Row{Int x;Int y}
fn fail()->Row{throw 7;return Row{x:99,y:99};}
fn main()->Int{let rows:[Row]=[Row{x:20,y:22}];try{rows[0]=fail();}catch e{}return rows[0].x+rows[0].y;}"#,
            r#"struct Row{Int x}
fn read(rows:[Row],index:Int)->Int{return rows[index].x;}
fn main()->Int{let rows:[Row]=[Row{x:42}];if read(rows,-1)!=1 || read(rows,1)!=1{return 2;}return read(rows,0);}"#,
            r#"struct Row{Int x}
fn read(rows:[Row],index:Int)->Int{return rows[index].x;}
fn main()->Int{let rows:[Row]=[];if read(rows,0)==1{return 42;}return 0;}"#,
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let _profile = antidecomp::ProfileGuard::new(profile);
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                let result =
                    lower_module_impl(&module, "main", 0, 0, X86StringLayout::Instring, true)
                        .unwrap();
                let offsets: Vec<_> = result
                    .exports
                    .iter()
                    .map(|(name, offset)| (name.clone(), *offset, 0))
                    .collect();
                let relocations: Vec<_> = result
                    .relocations
                    .iter()
                    .map(|site| (*site, result.codegen_base))
                    .collect();
                let mut runtime = crate::jit_runtime::JitRuntime::new();
                runtime
                    .load_with_data_relocations(
                        &result.code,
                        &offsets,
                        &relocations,
                        &result.data,
                        &result.global_relocs,
                        &result.data_relocations,
                    )
                    .unwrap();
                assert_eq!(
                    unsafe { runtime.invoke("main", &[]) },
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }
}

#[cfg(test)]
mod empty_definition_export_tests {
    use super::*;
    #[test]
    fn unit_definitions_export_but_bodyless_extern_stays_undefined() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "external".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![],
                type_params: vec![],
            },
            Decl::Function {
                name: "helper".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Return(None)],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Return(None)],
                type_params: vec![],
            },
        ]);
        let lowered = lower_module(&module, "main").unwrap();
        assert!(lowered.exports.iter().any(|(name, _)| name == "main"));
        assert!(lowered.exports.iter().any(|(name, _)| name == "helper"));
        assert!(!lowered.exports.iter().any(|(name, _)| name == "external"));
    }
}

#[cfg(test)]
mod declaration_storage_tests {
    use super::*;
    #[test]
    fn declaration_reserves_a_slot_without_emitting_a_store() {
        let structs = HashMap::new();
        let functions = HashMap::new();
        let mut ctx = LowerCtx::new("test", &[], &structs, &functions, HashMap::new());
        let declaration = Stmt::Declare("value".into(), Typ::Int);
        alloc_declared_locals(&mut ctx, std::slice::from_ref(&declaration)).unwrap();
        let mut emitter = CodeEmitter::new();
        let mut pending = vec![];
        lower_stmt(&mut emitter, &mut ctx, &declaration, &mut pending).unwrap();
        assert!(ctx.locals.contains_key("value"));
        assert!(emitter.bytes.is_empty());
        lower_stmt(
            &mut emitter,
            &mut ctx,
            &Stmt::Assign("value".into(), Expr::IntLit(42)),
            &mut pending,
        )
        .unwrap();
        assert!(!emitter.bytes.is_empty());
    }
}

#[cfg(test)]
mod scalar_array_copy_tests {
    use super::*;
    fn module(count: usize) -> UnifiedModule {
        UnifiedModule::new(vec![Decl::Function {
            name: "copy".into(),
            params: vec![("source".into(), Typ::Array(Box::new(Typ::Int)))],
            ret: Typ::Array(Box::new(Typ::Int)),
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::Unary {
                op: crate::core_ir::array_copy::operation(count),
                expr: Box::new(Expr::Ident("source".into())),
            }))],
        }])
    }
    #[test]
    fn native_array_copy_uses_owned_allocator_and_payload_copy_imports() {
        for count in [0, 2] {
            let result = lower_native_module(&module(count), "copy").unwrap();
            assert!(result.extern_calls.iter().any(|(name, _)| name == "malloc"));
            assert!(result.extern_calls.iter().any(|(name, _)| name == "abort"));
            assert_eq!(
                result
                    .extern_calls
                    .iter()
                    .filter(|(name, _)| name == "memcpy")
                    .count(),
                usize::from(count != 0)
            );
        }
        assert!(
            lower_native_module(&module(usize::MAX), "copy")
                .err()
                .expect("overflow must refuse lowering")
                .contains("overflow")
        );
        let mut invalid = module(2);
        let Decl::Function { params, ret, .. } = &mut invalid.decls[0] else {
            panic!("copy")
        };
        params[0].1 = Typ::Vector(Box::new(Typ::Int));
        *ret = params[0].1.clone();
        assert!(lower_native_module(&invalid, "copy").is_err());
    }
    #[test]
    fn empty_literal_can_copy_zero_elements_into_contextual_scalar_array() {
        let mut empty = module(0);
        let Decl::Function { body, .. } = &mut empty.decls[0] else {
            panic!("copy")
        };
        let Stmt::Return(Some(Expr::Unary { expr, .. })) = &mut body[0] else {
            panic!("copy value")
        };
        **expr = Expr::ArrayLit(vec![]);
        assert!(lower_native_module(&empty, "copy").is_ok());
        assert!(lower_module(&empty, "copy").is_ok());
        let Decl::Function { body, .. } = &mut empty.decls[0] else {
            panic!("copy")
        };
        let Stmt::Return(Some(Expr::Unary { op, .. })) = &mut body[0] else {
            panic!("copy value")
        };
        *op = crate::core_ir::array_copy::operation(1);
        assert!(lower_native_module(&empty, "copy").is_err());
    }

    #[cfg(all(unix, target_arch = "x86_64"))]
    #[test]
    fn jit_scalar_array_copy_is_independent_bounded_and_fresh_when_empty() {
        for (element, payload) in [
            (Typ::Int, [20_u64, 22]),
            (Typ::Bool, [1, 0]),
            (Typ::Float, [(-0.0_f64).to_bits(), 1.5_f64.to_bits()]),
        ] {
            for count in [0, 2] {
                let mut input = module(count);
                let Decl::Function { params, ret, .. } = &mut input.decls[0] else {
                    panic!("copy")
                };
                params[0].1 = Typ::Array(Box::new(element.clone()));
                *ret = params[0].1.clone();
                let result = lower_module(&input, "copy").unwrap();
                let exports = result
                    .exports
                    .iter()
                    .map(|(name, offset)| (name.clone(), *offset, 0))
                    .collect::<Vec<_>>();
                let relocations = result
                    .relocations
                    .iter()
                    .map(|site| (*site, result.codegen_base))
                    .collect::<Vec<_>>();
                let mut runtime = crate::jit_runtime::JitRuntime::new();
                runtime
                    .load_with_data_relocations(
                        &result.code,
                        &exports,
                        &relocations,
                        &result.data,
                        &result.global_relocs,
                        &result.data_relocations,
                    )
                    .unwrap();
                let source = [3_u64, 3, payload[0], payload[1], 99];
                let pointer = unsafe { runtime.invoke("copy", &[source.as_ptr() as i64]) }.unwrap()
                    as *mut u64;
                assert!(!pointer.is_null());
                assert_ne!(pointer.cast_const(), source.as_ptr());
                unsafe {
                    assert_eq!(*pointer, count as u64);
                    assert_eq!(*pointer.add(1), count as u64);
                    if count != 0 {
                        assert_eq!(*pointer.add(2), payload[0]);
                        assert_eq!(*pointer.add(3), payload[1]);
                        *pointer.add(2) = 7;
                    }
                }
                assert_eq!(source, [3, 3, payload[0], payload[1], 99]);
                if count == 0 {
                    let empty = unsafe { runtime.invoke("copy", &[0]) }.unwrap() as *const u64;
                    assert!(!empty.is_null());
                    assert_ne!(empty, pointer.cast_const());
                    assert_eq!(unsafe { *empty }, 0);
                }
            }
        }
        let mut bounded = module(2);
        let Decl::Function { ret, body, .. } = &mut bounded.decls[0] else {
            panic!("copy")
        };
        *ret = Typ::Int;
        let Stmt::Return(Some(copy)) = body.pop().unwrap() else {
            panic!("copy return")
        };
        body.push(Stmt::Let(
            "copy".into(),
            Some(Typ::Array(Box::new(Typ::Int))),
            copy,
        ));
        body.push(Stmt::Return(Some(Expr::IntLit(42))));
        let result = lower_module(&bounded, "copy").unwrap();
        let exports = result
            .exports
            .iter()
            .map(|(name, offset)| (name.clone(), *offset, 0))
            .collect::<Vec<_>>();
        let mut runtime = crate::jit_runtime::JitRuntime::new();
        runtime.load(&result.code, &exports, &[]).unwrap();
        let short = [1_u64, 1, 20];
        assert_eq!(
            unsafe { runtime.invoke("copy", &[short.as_ptr() as i64]) },
            Some(1)
        );
        assert_eq!(unsafe { runtime.invoke("copy", &[0]) }, Some(1));
    }
}

#[cfg(test)]
mod integer_float_cast_tests {
    use super::*;

    #[test]
    fn integer_float_lowering_uses_selected_precision_and_unsigned_sticky_half() {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _restore = Restore(x86_64::is_32bit());
        x86_64::set_32bit(false);
        for (op, bits, signed) in [
            ("int-to-float", 64, true),
            ("int-to-float-f32", 32, true),
            ("uint-to-float", 64, false),
            ("uint-to-float-f32", 32, false),
        ] {
            let module = UnifiedModule::new(vec![Decl::Function {
                name: "convert".into(),
                params: vec![("value".into(), Typ::Int)],
                ret: Typ::Float,
                type_params: vec![],
                body: vec![Stmt::Return(Some(Expr::Unary {
                    op: op.into(),
                    expr: Box::new(Expr::Ident("value".into())),
                }))],
            }]);
            for result in [
                lower_module(&module, "convert"),
                lower_native_module(&module, "convert"),
            ] {
                let result = result.expect("owned conversion lowering");
                let conversion = if bits == 32 {
                    x86_64::cvtsi2ss(0, RAX)
                } else {
                    x86_64::cvtsi2sd(0, RAX)
                };
                assert!(
                    result
                        .code
                        .windows(conversion.len())
                        .any(|bytes| bytes == conversion),
                    "{op}"
                );
                if bits == 32 {
                    let double_conversion = x86_64::cvtsi2sd(0, RAX);
                    assert!(
                        !result
                            .code
                            .windows(double_conversion.len())
                            .any(|bytes| bytes == double_conversion),
                        "f32 must not convert through f64"
                    );
                    let widen = x86_64::convert_scalar_float(0, 0, false);
                    assert!(result.code.windows(widen.len()).any(|bytes| bytes == widen));
                }
                if !signed {
                    for instruction in [
                        x86_64::shr_reg_imm(RAX, 1),
                        x86_64::or_rr(RAX, RDX),
                        if bits == 32 {
                            x86_64::scalar_single(0x58, 0, 0)
                        } else {
                            x86_64::scalar_double(0x58, 0, 0)
                        },
                    ] {
                        assert!(
                            result
                                .code
                                .windows(instruction.len())
                                .any(|bytes| bytes == instruction),
                            "{op}"
                        );
                    }
                }
            }
            x86_64::set_32bit(true);
            assert!(
                lower_module(&module, "convert").is_err(),
                "Word32 must refuse {op}"
            );
            x86_64::set_32bit(false);
        }
    }

    #[test]
    fn unsigned_conversion_branch_targets_land_on_conversion_and_carrier_steps() {
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                x86_64::set_32bit(self.0);
            }
        }
        let _restore = Restore(x86_64::is_32bit());
        x86_64::set_32bit(false);
        for bits in [32, 64] {
            let mut emitter = CodeEmitter::new();
            emit_integer_float_cast(&mut emitter, bits, false);
            let code = &emitter.bytes;
            let branch = x86_64::test_rr(RAX, RAX).len();
            assert_eq!(&code[branch..branch + 2], &[0x0f, 0x89]);
            let displacement =
                u32::from_le_bytes(code[branch + 2..branch + 6].try_into().unwrap()) as usize;
            let direct = branch + 6 + displacement;
            let conversion = if bits == 32 {
                x86_64::cvtsi2ss(0, RAX)
            } else {
                x86_64::cvtsi2sd(0, RAX)
            };
            assert_eq!(
                &code[direct..direct + conversion.len()],
                conversion.as_slice()
            );
            let jump = direct - 5;
            assert_eq!(code[jump], 0xe9);
            let displacement =
                u32::from_le_bytes(code[jump + 1..jump + 5].try_into().unwrap()) as usize;
            assert_eq!(jump + 5 + displacement, direct + conversion.len());
        }
    }
}

#[cfg(test)]
mod ieee_bit_cast_tests {
    use super::*;
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            x86_64::set_32bit(self.0);
        }
    }
    fn module(op: &str, operand: Typ) -> UnifiedModule {
        let result = if op.starts_with("float-to-bits") {
            Typ::Int
        } else {
            Typ::Float
        };
        UnifiedModule::new(vec![Decl::Function {
            name: "convert".into(),
            params: vec![("value".into(), operand)],
            ret: result,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::Unary {
                op: op.into(),
                expr: Box::new(Expr::Ident("value".into())),
            }))],
        }])
    }
    #[test]
    fn ieee_bit_conversions_lower_owned_native_and_jit_with_strict_operand_domains() {
        let _restore = Restore(x86_64::is_32bit());
        x86_64::set_32bit(false);
        for (op, input) in [
            ("float-to-bits-f32", Typ::Float),
            ("float-to-bits-f64", Typ::Float),
            ("bits-to-float-f32", Typ::Int),
            ("bits-to-float-f64", Typ::Int),
        ] {
            for result in [
                lower_module(&module(op, input.clone()), "convert"),
                lower_native_module(&module(op, input), "convert"),
            ] {
                let result = result.expect("owned IEEE conversion");
                assert!(result.externs.is_empty() && result.extern_calls.is_empty());
                if op == "float-to-bits-f32" {
                    let extract = x86_64::movd_xmm_gpr(0, RAX, false);
                    assert!(
                        result
                            .code
                            .windows(extract.len())
                            .any(|bytes| bytes == extract)
                    );
                }
                if op.ends_with("f64") {
                    assert!(
                        !result.code.windows(2).any(|bytes| bytes == [0x0f, 0x5a]),
                        "f64 reinterpretation must not convert"
                    );
                }
            }
            for wrong in [
                Typ::Bool,
                Typ::String,
                if op.starts_with("float-to-bits") {
                    Typ::Int
                } else {
                    Typ::Float
                },
            ] {
                assert!(
                    lower_native_module(&module(op, wrong), "convert").is_err(),
                    "{op}"
                );
            }
            x86_64::set_32bit(true);
            assert!(
                lower_module(
                    &module(
                        op,
                        if op.starts_with("float-to-bits") {
                            Typ::Float
                        } else {
                            Typ::Int
                        }
                    ),
                    "convert"
                )
                .is_err()
            );
            x86_64::set_32bit(false);
        }
    }
    #[test]
    fn f32_nan_paths_bypass_float_conversions_and_restore_scratch_registers() {
        let _restore = Restore(x86_64::is_32bit());
        x86_64::set_32bit(false);
        for from_float in [false, true] {
            let mut emitter = CodeEmitter::new();
            emit_f32_bit_cast(&mut emitter, from_float);
            let bytes = &emitter.bytes;
            assert_eq!(&bytes[..2], &[0x51, 0x52]);
            assert_eq!(&bytes[bytes.len() - 2..], &[0x5a, 0x59]);
            let exponent = if from_float {
                0x7ff0_0000_0000_0000u64
            } else {
                0x7f80_0000u64
            };
            let mut prefix = vec![];
            if !from_float {
                prefix.extend(x86_64::load_i64(RCX, 0xffff_ffff));
                prefix.extend(x86_64::and_rr(RAX, RCX));
            }
            prefix.extend(x86_64::mov_rr(RDX, RAX));
            prefix.extend(x86_64::load_i64(RCX, exponent as i64));
            prefix.extend(x86_64::and_rr(RDX, RCX));
            prefix.extend(x86_64::cmp_rr(RDX, RCX));
            let branch = 2 + prefix.len();
            assert_eq!(&bytes[branch..branch + 2], &[0x0f, 0x85]);
            let numeric = branch
                + 6
                + u32::from_le_bytes(bytes[branch + 2..branch + 6].try_into().unwrap()) as usize;
            let jump = numeric - 5;
            assert_eq!(bytes[jump], 0xe9);
            let restore = jump
                + 5
                + u32::from_le_bytes(bytes[jump + 1..jump + 5].try_into().unwrap()) as usize;
            assert_eq!(
                restore,
                bytes.len() - 2,
                "NaN path must skip all FP conversion and rejoin restore"
            );
            assert!(!bytes[2..jump].windows(2).any(|part| part == [0x0f, 0x5a]));
            let numeric_start = if from_float {
                x86_64::movq_xmm_gpr(0, RAX, true)
            } else {
                x86_64::movd_xmm_gpr(0, RAX, true)
            };
            assert_eq!(
                &bytes[numeric..numeric + numeric_start.len()],
                numeric_start.as_slice()
            );
        }
    }
}
