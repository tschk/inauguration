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
use crate::native_emit::global_data::{build_data_section, collect_globals};
use crate::native_emit::x86_64::{self, CodeEmitter, RAX, RBP, RBX, RCX, RDI, RDX, RSI};
use std::collections::HashMap;

/// Core IR stdlib call name -> C-ABI wrapper registered in the JIT symbol cache.
fn jit_stdlib_wrapper(name: &str) -> Option<&'static str> {
    Some(match name {
        "join" => "in_vec_join",
        "chain" | "extend" => "in_vec_extend",
        "push" => "in_vec_push",
        "push-str" => "in_vec_push_words",
        "print" => "in_print",
        "read-file" => "in_fs_read_to_string",
        "write-file" => "in_fs_write",
        "fs-exists" => "in_fs_exists",
        "create-dir" => "in_fs_create_dir",
        "remove-file" => "in_fs_remove_file",
        "process-run" => "in_process_run",
        "env-get" => "in_env_var",
        "env-set" => "in_env_set_var",
        "env-has" => "in_env_has",
        "env-temp-dir" => "in_env_temp_dir",
        "env-current-dir" => "in_env_current_dir",
        "path-join" => "in_path_join",
        "path-dirname" => "in_path_dirname",
        "path-basename" => "in_path_basename",
        "path-extname" => "in_path_extname",
        "path-normalize" => "in_path_normalize",
        "str-concat" => "in_str_concat",
        "str-eq" => "in_str_eq",
        "json-stringify" => "in_json_stringify",
        "str-table-has" => "in_str_table_has",
        "str-table-get-int" => "in_str_table_get_int",
        "str-contains" => "in_str_contains",
        "str-starts-with" => "in_str_starts_with",
        "str-ends-with" => "in_str_ends_with",
        "str-index-of" => "in_str_index_of",
        "str-is-int" => "in_str_is_int",
        "str-slice" => "in_str_slice",
        "str-split-lines" => "in_str_split_lines",
        "str-split-spaces" => "in_str_split_spaces",
        "str-tokenize-expr" => "in_str_tokenize_expr",
        "str-to-int" => "in_str_to_int",
        "str-trim" => "in_str_trim",
        "array-len" => "in_array_len",
        _ => return None,
    })
}

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

fn emit_failure_return(emitter: &mut CodeEmitter, is_interrupt: bool) {
    emitter.emit_insns(&x86_64::load_i64(RAX, 1));
    emit_function_epilogue(emitter, is_interrupt);
}

pub struct X86_64CompileResult {
    pub code: Vec<u8>,
    pub entry_offset: u32,
    pub exports: Vec<(String, u32)>,
    /// Byte offsets in `code` where 8-byte absolute addresses were written.
    /// At load time, patch each by adding (actual_base - codegen_base).
    pub relocations: Vec<u32>,
    /// (site, data offset) for every absolute reference to a global slot.
    /// Native linkers rebase these against `.data`; the JIT maps the data
    /// section and rebases to the mapped address. Sites here are excluded
    /// from the code-relative `relocations` pass at JIT load time.
    pub global_relocs: Vec<(u32, u64)>,
    /// The base address used during codegen (KERNEL_BASE = 0x101100).
    pub codegen_base: u64,
    /// Initialised data section bytes (global variable initial values).
    pub data: Vec<u8>,
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
                for (field, _field_ty) in fields.into_iter().rev() {
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

/// Layout of string-literal data in the emitted artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X86StringLayout {
    /// Hosted JIT ABI: `{ u64 byte length, bytes }` — the runtime instring
    /// representation read by `decode_jit_string` / `instring_from_ptr`.
    Instring,
    /// Freestanding ABI: NUL-terminated bytes; a string's value address is
    /// its first byte. Used by the SCI and boot-image emitters.
    Cstring,
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
    let prepared = crate::lower_core::prepared_native_module(module);
    let module = prepared.as_ref();
    let functions = collect_functions(module)?;
    let structs = collect_structs(module);
    let globals = collect_globals(module, &structs);
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
        )?;
        all_pending_globals.extend(pending);
    }

    // Resolve calls — collect string refs, resolve function addresses and calls
    let mut str_refs: Vec<(u32, String)> = Vec::new();
    let mut relocations: Vec<u32> = Vec::new();
    for call in &all_pending_calls {
        if call.target.starts_with("@addr_") {
            // Function address reference: write absolute address at site
            let fn_name = &call.target[6..];
            if let Some(&func_offset) = function_offsets.get(fn_name) {
                let abs_addr = code_base + func_offset as u64;
                let site = call.site as usize;
                if x86_64::is_32bit() {
                    if site + 4 <= emitter.bytes.len() {
                        emitter.bytes[site..site + 4]
                            .copy_from_slice(&(abs_addr as u32).to_le_bytes());
                        relocations.push(call.site);
                    }
                } else if site + 8 <= emitter.bytes.len() {
                    emitter.bytes[site..site + 8].copy_from_slice(&abs_addr.to_le_bytes());
                    relocations.push(call.site);
                }
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
        let code_end = emitter.len();
        let mut str_offset = 0u64;
        for s in &all_strings {
            let abs_addr = code_base + code_end as u64 + str_offset;
            for &(site, ref content) in &str_refs {
                if content == s {
                    let site_u = site as usize;
                    if x86_64::is_32bit() {
                        if site_u + 4 <= emitter.bytes.len() {
                            emitter.bytes[site_u..site_u + 4]
                                .copy_from_slice(&(abs_addr as u32).to_le_bytes());
                            relocations.push(site);
                        }
                    } else if site_u + 8 <= emitter.bytes.len() {
                        emitter.bytes[site_u..site_u + 8].copy_from_slice(&abs_addr.to_le_bytes());
                        relocations.push(site);
                    }
                }
            }
            // Payload layout depends on the target artifact kind.
            let start = code_end as usize + str_offset as usize;
            match layout {
                X86StringLayout::Instring => {
                    // Length-prefixed payload (`decode_jit_string` reads u64
                    // len then bytes).
                    let padded = (8 + s.len() + 7) & !7;
                    let end = start + padded;
                    if end > emitter.bytes.len() {
                        emitter.bytes.resize(end, 0);
                    }
                    emitter.bytes[start..start + 8]
                        .copy_from_slice(&(s.len() as u64).to_le_bytes());
                    emitter.bytes[start + 8..start + 8 + s.len()].copy_from_slice(s.as_bytes());
                    str_offset += padded as u64;
                }
                X86StringLayout::Cstring => {
                    // NUL-terminated payload; the referenced address is the
                    // first byte of the string.
                    let padded = (s.len() + 1 + 7) & !7;
                    let end = start + padded;
                    if end > emitter.bytes.len() {
                        emitter.bytes.resize(end, 0);
                    }
                    emitter.bytes[start..start + s.len()].copy_from_slice(s.as_bytes());
                    emitter.bytes[start + s.len()] = 0;
                    str_offset += padded as u64;
                }
            }
        }
    }

    // Patch global-variable references now that the data section address is known.
    let mut global_relocs: Vec<(u32, u64)> = Vec::new();
    for pg in &all_pending_globals {
        let abs_addr = data_base + pg.offset;
        let site = pg.site as usize;
        global_relocs.push((pg.site, pg.offset));
        if pg.width == 4 && site + 4 <= emitter.bytes.len() {
            emitter.bytes[site..site + 4].copy_from_slice(&(abs_addr as u32).to_le_bytes());
            relocations.push(pg.site);
        } else if pg.width == 8 && site + 8 <= emitter.bytes.len() {
            emitter.bytes[site..site + 8].copy_from_slice(&abs_addr.to_le_bytes());
            relocations.push(pg.site);
        }
    }

    let entry_offset = function_offsets.get(entry).copied().unwrap_or(0);
    let exports: Vec<(String, u32)> = function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset))
        .collect();
    let data = build_data_section(module, &globals)?;

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
        global_relocs,
        codegen_base: code_base,
        data,
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
    for func in functions.values_mut() {
        crate::core_bindings::normalize_function(&mut func.params, &mut func.body, &module_names);
        rename_calls(&mut func.body, &name_map);
    }
    let fallible = crate::core_effects::fallible_functions(
        functions
            .values()
            .map(|info| (info.name.as_str(), info.body.as_slice())),
    );
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
        Stmt::Return(None) => {}
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            rename_calls_in_expr(cond, name_map);
            rename_calls(then_body, name_map);
            rename_calls(else_body, name_map);
        }
        Stmt::Loop { body, .. } => {
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
        Expr::IntLit(_)
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
        Stmt::Return(None) => {}
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
) -> Result<Vec<PendingGlobal>, String> {
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
                    || ctx.structs[&struct_name].iter().any(|(_, typ)| {
                        !matches!(
                            typ.canonical(),
                            Typ::Int | Typ::Bool | Typ::Float | Typ::String
                        )
                    })
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
        emit_failure_return(emitter, ctx.is_interrupt);
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

    // If no explicit return, emit default epilogue
    if !ctx.emitted_return {
        if matches!(&func.ret, Typ::Named(_) | Typ::Array(_)) {
            // ponytail: missing aggregate returns require control-flow validation.
            emitter.emit_insns(&x86_64::load_i64(RAX, 0));
        } else if func.ret == Typ::Void {
            emitter.emit_insns(&x86_64::zero_reg(RAX));
        }
        emit_function_epilogue(emitter, is_interrupt);
    }

    Ok(pending_globals)
}

fn alloc_declared_locals(ctx: &mut LowerCtx<'_>, body: &[Stmt]) -> Result<(), String> {
    for stmt in body {
        match stmt {
            Stmt::Let(name, typ, expr) => {
                if x86_64::is_32bit()
                    && let Expr::ArrayLit(items) = expr
                {
                    let array_type = typ
                        .clone()
                        .or_else(|| scalar_expr_type(expr, ctx))
                        .unwrap_or(Typ::Array(Box::new(Typ::Void)));
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
            Typ::Named(_) | Typ::Array(_) | Typ::Vector(_) | Typ::Void
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
                if let Typ::Named(name) = &ret_typ
                    && ctx.structs.contains_key(name)
                {
                    if ctx.structs[name].iter().any(|(_, typ)| {
                        !matches!(
                            typ.canonical(),
                            Typ::Int | Typ::Bool | Typ::Float | Typ::String
                        )
                    }) {
                        return Err(
                            "x86_64-lower: struct return needs a supported scalar-field layout"
                                .into(),
                        );
                    }
                    let schema = ctx.structs[name].clone();
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
                    lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
                }
            } else {
                emitter.emit_insns(&x86_64::zero_reg(RAX));
            }
            emit_function_epilogue(emitter, ctx.is_interrupt);
            ctx.emitted_return = true;
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
                if let Some(StackSlot::Struct { fields: field_map }) = ctx.locals.get(name).cloned()
                {
                    for (field_name, value) in fields {
                        if let Some(&off) = field_map.get(field_name) {
                            lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                            emitter.emit_insns(&x86_64::str64(RAX, off as u16));
                        }
                    }
                }
                return Ok(());
            }
            if !ctx.locals.contains_key(name) {
                let resolved = typ.clone().unwrap_or(Typ::Int);
                ctx.alloc_local(name, &resolved)?;
            }
            if let Some(StackSlot::Struct { fields }) = ctx.locals.get(name).cloned() {
                return store_struct_value(emitter, ctx, expr, &fields, pending_calls);
            }
            lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
            store_local_error_type(emitter, ctx, name, expr)?;
            match ctx.locals.get(name) {
                Some(StackSlot::Scalar(offset)) => {
                    emitter.emit_insns(&x86_64::str64(RAX, *offset as u16));
                }
                _ => {}
            }
            Ok(())
        }
        Stmt::Assign(name, expr) => {
            // Local bindings and parameters shadow globals.
            if !ctx.locals.contains_key(name)
                && let Some(&offset) = ctx.globals.get(name)
            {
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
            lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
            if let Some(off) = offset {
                emitter.emit_insns(&x86_64::str64(RAX, off as u16));
                store_local_error_type(emitter, ctx, name, expr)?;
            }
            Ok(())
        }
        Stmt::FieldAssign {
            base, name, value, ..
        } => {
            // s.x = value → compute addr = &s + field_offset, store value
            let Expr::Ident(base_name) = base else {
                // Pointer/reference field bases are opaque in the owned
                // scalar ABI. Keep lowering by evaluating the RHS; this is
                // the same conservative fallback used for other unsupported
                // aggregate writes.
                lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                return Ok(());
            };
            if let Some((pointer, offset)) = ctx.receiver_field(base_name, name) {
                lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                emitter.emit_insns(&x86_64::ldr64(RCX, pointer as u16));
                emitter.emit_insns(&x86_64::mov_m_r(RCX, offset, RAX));
                return Ok(());
            }
            if !ctx.locals.contains_key(base_name) {
                // Global struct: collect_globals maps `{base}.{field}` to a
                // data-section slot, so store through the pending-global path
                // instead of the evaluate-and-discard fallback below.
                let field_key = format!("{base_name}.{name}");
                if let Some(&offset) = ctx.globals.get(&field_key) {
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
                // The typed subset may not materialize bindings introduced by
                // destructuring patterns (for example `let Some(state) = …`).
                // Preserve evaluation of the assigned value and continue with
                // the scalar fallback rather than rejecting the whole module.
                lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                return Ok(());
            }
            let field_offset = match ctx.locals.get(base_name) {
                Some(StackSlot::Struct { fields, .. }) => fields
                    .get(name)
                    .copied()
                    .ok_or_else(|| format!("field `{name}` not found in struct"))?,
                _ => {
                    // Unknown/opaque values have no representable field
                    // layout in the owned ABI; evaluate-and-discard matches
                    // the existing scalar fallback for field reads.
                    lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
                    return Ok(());
                }
            };
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
                emit_failure_return(emitter, ctx.is_interrupt);
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
            emit_failure_return(emitter, ctx.is_interrupt);
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
                        type_misses.extend(emit_string_match_test(emitter)?);
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

fn emit_string_match_test(emitter: &mut CodeEmitter) -> Result<Vec<u32>, String> {
    if x86_64::is_32bit() {
        return Err("x86_64-lower: string matching requires 64-bit mode".into());
    }
    // Preserve RAX for later arms; R8/R9 walk the two length-prefixed payloads.
    let mut misses = Vec::with_capacity(3);
    emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
    misses.push(emitter.len());
    emitter.emit_insns(&x86_64::jcc_near(4, 0));
    emitter.emit_insns(&x86_64::mov_rr(8, RAX));
    emitter.emit_insns(&x86_64::mov_rr(9, RCX));
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
            let mut not_equal = emit_string_match_test(emitter)?;
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
        && matches!(ctx.scalar_types.get(name).map(Typ::canonical), Some(Typ::Named(ref typ)) if ctx.structs.contains_key(typ))
        && let Some(&offset) = ctx.globals.get(name)
    {
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
    let offset = ctx.slot_offset(name).unwrap_or_else(|_| {
        let off = ctx.alloc_slot();
        ctx.locals.insert(name.to_string(), StackSlot::Scalar(off));
        off
    });
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

fn lower_checked_div_or_mod(emitter: &mut CodeEmitter, is_interrupt: bool, modulo: bool) {
    emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
    emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
    emitter.emit_insns(&x86_64::test_rr(RCX, RCX));
    let nonzero = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(5, 0));
    emitter.emit_insns(&x86_64::load_i64(RAX, 1));
    emit_function_epilogue(emitter, is_interrupt);
    let valid = emitter.len();
    emitter.patch_u32(nonzero + 2, (valid as i32 - nonzero as i32 - 6) as u32);

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
}

fn scalar_expr_type(expr: &Expr, ctx: &LowerCtx<'_>) -> Option<Typ> {
    if let Expr::ArrayLit(items) = expr {
        return Some(Typ::Array(Box::new(
            items
                .iter()
                .find_map(|item| scalar_expr_type(item, ctx))
                .unwrap_or(Typ::Void),
        )));
    }
    crate::native_emit::lower::expr_type(expr).or_else(|| match expr {
        Expr::Ident(name) => ctx.scalar_types.get(name).map(Typ::canonical),
        Expr::Index { base, .. } => match scalar_expr_type(base, ctx)?.canonical() {
            Typ::Array(item) | Typ::Vector(item) => Some(*item),
            _ => None,
        },
        Expr::Field { base, name } => {
            let mut typ = scalar_expr_type(base, ctx)?;
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
        Expr::Unary { op, expr } if op == "-" => scalar_expr_type(expr, ctx),
        Expr::Unary { op, .. } if op == "!" => Some(Typ::Bool),
        Expr::Unary { op, .. }
            if op == "~"
                || op == "bool-to-int"
                || op == "float-to-int"
                || crate::core_ir::integer_cast_spec(op).is_some()
                || crate::core_ir::saturating_float_cast_spec(op).is_some() =>
        {
            Some(Typ::Int)
        }
        Expr::Unary { op, .. }
            if matches!(
                op.as_str(),
                "int-to-float" | "float-cast-f32" | "float-cast-f64"
            ) =>
        {
            Some(Typ::Float)
        }
        Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "&" | "|" | "^") => {
            crate::core_ir::bitwise_result_type(
                scalar_expr_type(lhs, ctx),
                scalar_expr_type(rhs, ctx),
            )
        }
        Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "<<" | ">>" | "%") => {
            if scalar_expr_type(lhs, ctx) == Some(Typ::Int)
                && scalar_expr_type(rhs, ctx) == Some(Typ::Int)
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
            if matches!(
                op.as_str(),
                "+" | "-" | "*" | "/" | "+=" | "-=" | "*=" | "/="
            ) =>
        {
            if scalar_expr_type(lhs, ctx) == Some(Typ::Float)
                || scalar_expr_type(rhs, ctx) == Some(Typ::Float)
            {
                Some(Typ::Float)
            } else {
                Some(Typ::Int)
            }
        }
        _ => None,
    })
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
            lower_checked_div_or_mod(emitter, ctx.is_interrupt, matches!(op, "%" | "%="));
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
        "^" => {
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
        "invoke" | "invoke1" | "invoke2" => {
            if args.len() >= 1 {
                lower_expr_into(emitter, ctx, &args[0], RDI, pending_calls)?;
                if args.len() >= 2 {
                    lower_expr_into(emitter, ctx, &args[1], RSI, pending_calls)?;
                }
                if args.len() >= 3 {
                    lower_expr_into(emitter, ctx, &args[2], RDX, pending_calls)?;
                }
                if args.len() == 1 {
                    emitter.emit_width(&[0x89, 0xF8, 0xFF, 0xD0], &[0x48, 0x89, 0xF8, 0xFF, 0xD0]); // mov eax/rax, edi/rdi; call eax/rax
                } else if args.len() >= 2 {
                    emitter.emit_width(&[0x89, 0xF8], &[0x48, 0x89, 0xF8]); // mov eax/rax, edi/rdi
                    emitter.emit_width(&[0x89, 0xF7], &[0x48, 0x89, 0xF7]); // mov edi/rdi, esi/rsi
                    if args.len() >= 3 {
                        emitter.emit_width(&[0x89, 0xD6], &[0x48, 0x89, 0xD6]); // mov esi/rsi, edx/rdx
                    }
                    emitter.emit_width(&[0xFF, 0xD0], &[0xFF, 0xD0]); // call eax/rax
                }
            } else {
                return Err(format!(
                    "x86_64-lower: `invoke` requires at least 1 argument in `{}`",
                    ctx.fn_name
                ));
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
    if let Expr::StructInit { name, fields } = arg {
        if !ctx.structs.contains_key(name)
            || ctx.structs[name].iter().any(|(_, typ)| {
                !matches!(
                    typ.canonical(),
                    Typ::Int | Typ::Bool | Typ::Float | Typ::String
                )
            })
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
        if let Some((param_name, typ)) = params.as_ref().and_then(|params| params.get(index))
            && param_name != "self"
            && let Typ::Named(name) = typ.canonical()
            && let Some(schema) = ctx.structs.get(&name).cloned()
        {
            if x86_64::is_32bit()
                || schema.iter().any(|(_, typ)| {
                    !matches!(
                        typ.canonical(),
                        Typ::Int | Typ::Bool | Typ::Float | Typ::String
                    )
                })
            {
                return Err(
                    "x86_64-lower: struct argument needs a scalar-field 64-bit layout".into(),
                );
            }
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
        if let Some(wrapper) = jit_stdlib_wrapper(&base)
            && let Some(ptr) = crate::native_emit::native_link::resolve_native_fn(wrapper)
        {
            if x86_64::is_32bit() || args.len() > 6 {
                return Err(
                    "x86_64-lower: JIT stdlib wrappers require 64-bit register arguments".into(),
                );
            }
            lower_call_args(emitter, ctx, args, &target_name, pending_calls)?;
            emitter.emit_insns(&x86_64::load_i64(RAX, ptr as i64));
            // Operand spills and Harden frames may leave the stack unaligned for C calls.
            emitter.emit_insns(&x86_64::mov_rr(11, x86_64::REG_SP));
            emitter.emit_insns(&x86_64::load_i64(10, -16));
            emitter.emit_insns(&x86_64::and_rr(x86_64::REG_SP, 10));
            emitter.emit_insns(&x86_64::sub_rsp_i8(16));
            emitter.emit_insns(&x86_64::mov_m_r(x86_64::REG_SP, 0, 11));
            emitter.emit_insns(&[0xFF, 0xD0]);
            emitter.emit_insns(&x86_64::mov_r_m(x86_64::REG_SP, x86_64::REG_SP, 0));
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
    if !ctx.structs.get(&name).is_some_and(|schema| {
        schema.iter().all(|(_, typ)| {
            matches!(
                typ.canonical(),
                Typ::Int | Typ::Bool | Typ::Float | Typ::String
            )
        })
    }) {
        return Err("x86_64-lower: struct value needs a known scalar-field layout".into());
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
        Expr::Call { .. } => {}
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
    lower_call_argument(emitter, ctx, expr, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 1));
    let valid = emitter.len();
    emitter.emit_insns(&x86_64::jcc_near(0x07, 0));
    emit_failure_return(emitter, ctx.is_interrupt);
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
    let field_offsets: Vec<(String, u32)> = match ctx.locals.get(name) {
        Some(StackSlot::Struct { fields: field_map }) => fields
            .iter()
            .filter_map(|(fn_, _)| {
                if let Some(&off) = field_map.get(fn_.as_str()) {
                    Some((fn_.clone(), off))
                } else {
                    let prefix = format!("{fn_}.");
                    field_map.iter().find_map(|(k, &v)| {
                        if k.starts_with(&prefix) {
                            Some((fn_.clone(), v))
                        } else {
                            None
                        }
                    })
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    for (field_name, field_offset) in &field_offsets {
        if let Some((_, value)) = fields.iter().find(|(fn_, _)| fn_ == field_name) {
            lower_expr_into(emitter, ctx, value, RAX, pending_calls)?;
            emitter.emit_insns(&x86_64::str64(RAX, *field_offset as u16));
        }
    }
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
    let Expr::Ident(base_name) = base else {
        emitter.emit_insns(&x86_64::load_i64(target_reg, 0));
        return Ok(());
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
                if let Some(match_key) = fields.keys().find(|k| k.starts_with(&prefix)) {
                    match_key.clone()
                } else {
                    // The merged frontend can retain a partial/opaque view
                    // of an external struct. Treat an unknown field as the
                    // scalar fallback rather than stopping module lowering.
                    emitter.emit_insns(&x86_64::load_i64(target_reg, 0));
                    return Ok(());
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
    lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
    match op {
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
        "int-to-float" => {
            if scalar_expr_type(expr, ctx) != Some(Typ::Int) {
                return Err("x86_64-lower: integer conversion needs Int".into());
            }
            emitter.emit_insns(&x86_64::cvtsi2sd(0, RAX));
            emitter.emit_insns(&x86_64::movq_xmm_gpr(0, RAX, false));
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
            if scalar_expr_type(expr, ctx) == Some(Typ::Float) {
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
        emit_failure_return(emitter, ctx.is_interrupt);
        return Ok(());
    }
    let Some(&first) = offsets.first() else {
        lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
        emit_failure_return(emitter, ctx.is_interrupt);
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
    emit_failure_return(emitter, ctx.is_interrupt);
    emitter.patch_u32(end + 1, emitter.len() - end - 5);
    if target_reg != RAX {
        emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
    }
    Ok(())
}

fn lower_heap_array(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    offsets: &[u32],
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if x86_64::is_32bit() || ctx.is_interrupt {
        return Err("x86_64-lower: escaping arrays require the 64-bit hosted ABI".into());
    }
    if ctx.runtime_error_slot {
        emitter.emit_insns(&x86_64::mov_rr(RDI, 15));
        emitter.emit_insns(&x86_64::load_i64(RSI, offsets.len() as i64));
        emitter.emit_insns(&x86_64::load_i64(
            RAX,
            crate::jit_runtime::allocate_array as *const () as i64,
        ));
    } else {
        let bytes = offsets
            .len()
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
    emitter.emit_insns(&x86_64::load_i64(RDX, offsets.len() as i64));
    emitter.emit_insns(&x86_64::mov_m_r(RAX, 0, RDX));
    emitter.emit_insns(&x86_64::mov_m_r(RAX, 8, RDX));
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
) -> Result<Vec<u32>, String> {
    let mut offsets = Vec::with_capacity(items.len());
    for _ in items {
        offsets.push(ctx.alloc_slot());
    }
    for (item, &off) in items.iter().zip(offsets.iter()) {
        lower_expr_into(emitter, ctx, item, RAX, pending_calls)?;
        emitter.emit_insns(&x86_64::str64(RAX, off as u16));
    }
    Ok(offsets)
}

fn lower_index_expr(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    base: &Expr,
    index: &Expr,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if let Expr::Ident(name) = base
        && let Some(StackSlot::InlineArray { offsets }) = ctx.locals.get(name).cloned()
    {
        return lower_array_elem_read(emitter, ctx, &offsets, index, target_reg, pending_calls);
    }
    if let Expr::ArrayLit(items) = base {
        let offsets = materialize_array_literal(emitter, ctx, items, pending_calls)?;
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
    emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
    emitter.emit_insns(&x86_64::add_rr(RDI, RAX));
    emitter.emit_insns(&x86_64::mov_r_m(RAX, RDI, 16));
    let end = emitter.len();
    emitter.emit_insns(&x86_64::jmp_rel32(0));
    emitter.patch_u32(null + 2, emitter.len() - null - 6);
    emitter.patch_u32(out_of_bounds + 2, emitter.len() - out_of_bounds - 6);
    emit_failure_return(emitter, ctx.is_interrupt);
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

fn lower_expr_into(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    target_reg: u8,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    match expr {
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
            let offsets = materialize_array_literal(emitter, ctx, items, pending_calls)?;
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
    use super::*;
    use crate::core_ir::UnifiedModule;

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

fn main() -> void { return 0 }
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
            "fn missing() -> Int {}\nfn main() -> Int { return missing(); }",
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
            "fn missing() -> Int {}\nfn main() -> Int { return missing; }",
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
        let src = "import std.process;\ncapability process.spawn;\nfn main() -> String { return process_run(\"true\"); }\n";
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        crate::native_emit::native_link::bootstrap_jit_native();
        let result = lower_module(&module, "main").expect("lower");
        let native = lower_native_module(&module, "main").expect("native lower");
        assert_eq!(native.externs, ["process_run"]);
        let has_call_rax = result.code.windows(2).any(|w| w == [0xFF, 0xD0]);
        let has_unresolved_rel32 = result.code.windows(5).any(|w| w == [0xE8, 0, 0, 0, 0]);
        assert!(has_call_rax, "stdlib extern must lower to call rax");
        assert!(
            !has_unresolved_rel32,
            "stdlib extern must not stay an unresolved rel32 call"
        );
    }

    fn make_arith_fn_module() -> UnifiedModule {
        let src = r#"
fn add(a: Int, b: Int) -> Int {
  return a + b
}

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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
                body: vec![Stmt::Loop {
                    kind: LoopKind::For {
                        binding: "value".into(),
                    },
                    cond: Some(Expr::Ident("values".into())),
                    body: vec![],
                }],
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

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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

fn main() -> void { return 0 }
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
fn refuses_struct_copies_with_non_scalar_fields() {
    let module = crate::in_lang_parse::parse_in_source("struct Inner { Int x }\nstruct Outer { Inner inner }\nfn main() -> Int { var source: Outer = Outer { inner: Inner { x: 42 } }; var copied: Outer = source; return 42; }").unwrap();
    match lower_module(&module, "main") {
        Err(error) => assert!(error.contains("scalar-field layout"), "{error}"),
        Ok(_) => panic!("non-scalar struct copy accepted"),
    }
}
