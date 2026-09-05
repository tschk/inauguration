//! Core IR → x86_64 lowering for the owned native subset.
//!
//! Lowers a subset of Core IR functions into x86_64 machine code.
//! Supports scalar function bodies with:
//!   - let bindings (Int, Bool, String)
//!   - return
//!   - if/else
//!   - while loops
//!   - arithmetic (add, sub, mul)
//!   - direct function calls
//!   - struct init/field access (scalar fields only)

use crate::core_ir::{Decl, Expr, LoopKind, MatchArm, Stmt, Typ, UnifiedModule};
use crate::native_emit::x86_64::{self, CodeEmitter, RAX, RBP, RBX, RCX, RDI, RDX, REG_SP, RSI};
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// JIT mode: resolve extern stdlib calls to in-process wrapper addresses
    /// instead of leaving rel32 placeholders for the system linker.
    pub(crate) static TL_JIT_EXTERNS: RefCell<bool> = const { RefCell::new(false) };
}

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
        _ => return None,
    })
}

pub const X86_64_TRIPLE: &str = "x86_64-unknown-none";

pub struct X86_64CompileResult {
    pub code: Vec<u8>,
    pub entry_offset: u32,
    pub exports: Vec<(String, u32)>,
    /// Byte offsets in `code` where 8-byte absolute addresses were written.
    /// At load time, patch each by adding (actual_base - codegen_base).
    pub relocations: Vec<u32>,
    /// The base address used during codegen (KERNEL_BASE = 0x101100).
    pub codegen_base: u64,
    /// Initialised data section bytes (global variable initial values).
    pub data: Vec<u8>,
    /// Undefined symbols (externs) — names of functions called but not
    /// defined in this module. Resolved at link time.
    pub externs: Vec<String>,
}

#[derive(Debug, Clone)]
struct FunctionInfo {
    name: String,
    params: Vec<(String, Typ)>,
    ret: Typ,
    body: Vec<Stmt>,
}

struct LowerCtx<'a> {
    /// Map local name → stack offset (negative from RBP)
    locals: HashMap<String, StackSlot>,
    /// Current stack frame size (negative, grows down)
    frame_size: u32,
    /// Set when a return statement has been emitted
    emitted_return: bool,
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
    /// Error handling: offset for error flag byte (Throw/Try)
    error_flag_offset: u32,
    /// Error handling: offset for error value (Throw/Try)
    error_value_offset: u32,
    /// Return type of the current function
    ret_typ: Typ,
}

#[derive(Debug, Clone)]
enum StackSlot {
    Scalar(u32), // offset from RBP (negative)
    Struct { fields: HashMap<String, u32> },
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
            frame_size: 0,
            emitted_return: false,
            is_interrupt: false,
            structs,
            functions,
            fn_name: fn_name.to_string(),
            globals,
            pending_globals: Vec::new(),
            error_flag_offset: 0,
            error_value_offset: 0,
            ret_typ: Typ::Int,
        };
        // Allocate stack slots for parameters
        // On x86_64 (System V), first 6 integer args go in RDI, RSI, RDX, RCX, R8, R9.
        // All parameters get a local stack slot so the body can reload them the same
        // way. Register params are stored to their slots in the prologue; stack params
        // are loaded from the caller's argument area and stored to their slots there.
        for (name, _typ) in params.iter() {
            let offset = ctx.alloc_slot();
            ctx.locals.insert(name.clone(), StackSlot::Scalar(offset));
        }
        ctx
    }

    fn alloc_slot(&mut self) -> u32 {
        let offset = self.frame_size;
        self.frame_size += 8;
        offset
    }

    fn alloc_local(&mut self, name: &str, typ: &Typ) -> Result<(), String> {
        if self.locals.contains_key(name) {
            return Ok(());
        }
        match typ {
            Typ::Int | Typ::Bool | Typ::String => {
                let offset = self.alloc_slot();
                self.locals
                    .insert(name.to_string(), StackSlot::Scalar(offset));
                Ok(())
            }
            Typ::Named(struct_name) => {
                let fields = self.structs.get(struct_name).cloned().ok_or_else(|| {
                    format!(
                        "x86_64-lower: unknown struct type `{struct_name}` in `{}`",
                        self.fn_name
                    )
                })?;
                let mut slots = HashMap::new();
                for (field, _field_ty) in fields {
                    // ponytail: all struct fields map to scalar slots
                    slots.insert(field.clone(), self.alloc_slot());
                }
                self.locals
                    .insert(name.to_string(), StackSlot::Struct { fields: slots });
                Ok(())
            }
            _ => Err(format!(
                "x86_64-lower: unsupported local type in `{}`",
                self.fn_name
            )),
        }
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

/// Lower a Core IR module to x86_64 machine code using the historical default
/// load addresses (code at 0x101100, data at 0x200000).
pub fn lower_module(module: &UnifiedModule, entry: &str) -> Result<X86_64CompileResult, String> {
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
    let functions = collect_functions(module)?;
    let structs = collect_structs(module);
    let globals = collect_globals(module);

    let all_strings = collect_string_literals(module);

    let mut emitter = CodeEmitter::new();
    let mut function_offsets: HashMap<String, u32> = HashMap::new();
    let mut all_pending_calls: Vec<PendingCall> = Vec::new();
    let mut all_pending_globals: Vec<PendingGlobal> = Vec::new();

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
            &mut all_pending_calls,
            is_interrupt,
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
            }
        } else if call.target.starts_with("@str_") {
            str_refs.push((call.site, call.target[5..].to_string()));
        } else if let Some(&target_offset) = function_offsets.get(&call.target) {
            let rel_offset = target_offset as i32 - call.site as i32 - 5; // call is 5 bytes
            emitter.patch_u32(call.site + 1, rel_offset as u32);
        }
        // else: extern call — keep rel32=0, symbol unresolved until linked
    }

    // Append string data section and patch string literal references
    if !str_refs.is_empty() || !all_strings.is_empty() {
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
            // Write string bytes with null terminator, 8-byte aligned
            let padded = (s.len() + 1 + 7) & !7;
            let start = code_end as usize + str_offset as usize;
            let end = start + padded;
            if end > emitter.bytes.len() {
                emitter.bytes.resize(end, 0);
            }
            emitter.bytes[start..start + s.len()].copy_from_slice(s.as_bytes());
            emitter.bytes[start + s.len()] = 0;
            str_offset += padded as u64;
        }
    }

    // Patch global-variable references now that the data section address is known.
    for pg in &all_pending_globals {
        let abs_addr = data_base + pg.offset;
        let site = pg.site as usize;
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
    let data = build_data_section(module, &globals);

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
        codegen_base: code_base,
        data,
        externs,
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
            },
        );
    }
    // Build disambiguation map and update call targets
    let mut name_map: HashMap<String, String> = HashMap::new();
    for unique in functions.keys() {
        let orig = unique.split("__dup").next().unwrap_or(unique).to_string();
        name_map.insert(orig, unique.clone());
    }
    for func in functions.values_mut() {
        rename_calls(&mut func.body, &name_map);
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
        Stmt::Break | Stmt::Propagate => {}
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
                for s in &c.body {
                    collect_strings_from_stmt(s, out);
                }
            }
        }
        Stmt::Expr(expr) => collect_strings_from_expr(expr, out),
        Stmt::Break | Stmt::Propagate => {}
    }
}

/// Collect all unique string literal contents from the module.
fn collect_string_literals(module: &UnifiedModule) -> Vec<String> {
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

/// Collect global variable names and assign them fixed absolute addresses.
/// Returns: (name → address) map.
fn collect_globals(module: &UnifiedModule) -> HashMap<String, u64> {
    let mut globals = HashMap::new();
    let mut offset = 0u64;
    for decl in &module.decls {
        if let Decl::Global { name, .. } = decl {
            globals.insert(name.clone(), offset);
            offset += 8;
        }
    }
    globals
}

fn build_data_section(module: &UnifiedModule, globals: &HashMap<String, u64>) -> Vec<u8> {
    let mut data = Vec::new();
    let mut max_offset = 0u64;
    for decl in &module.decls {
        if let Decl::Global { name, init, .. } = decl {
            let offset = *globals.get(name).unwrap_or(&0);
            let val: i64 = match init.as_deref() {
                Some(Expr::IntLit(v)) => *v,
                Some(Expr::BoolLit(b)) => *b as i64,
                _ => 0,
            };
            while data.len() < offset as usize {
                data.push(0);
            }
            data.extend_from_slice(&val.to_le_bytes());
            if offset >= max_offset {
                max_offset = offset + 8;
            }
        }
    }
    data
}

fn lower_function(
    emitter: &mut CodeEmitter,
    func: &FunctionInfo,
    structs: &HashMap<String, Vec<(String, Typ)>>,
    functions: &HashMap<String, FunctionInfo>,
    globals: &HashMap<String, u64>,
    pending_calls: &mut Vec<PendingCall>,
    is_interrupt: bool,
) -> Result<Vec<PendingGlobal>, String> {
    // Validate return type and store for use in Return handling
    let _ret_is_struct = matches!(&func.ret, Typ::Named(_) | Typ::Array(_));
    match &func.ret {
        Typ::Int | Typ::Bool | Typ::Float | Typ::String | Typ::Void | Typ::Named(_) => {}
        _ => {
            return Err(format!(
                "x86_64-lower: unsupported return type in `{}`",
                func.name
            ));
        }
    }

    let mut ctx = LowerCtx::new(
        &func.name,
        &func.params,
        structs,
        functions,
        globals.clone(),
    );
    ctx.is_interrupt = is_interrupt;
    ctx.ret_typ = func.ret.clone();
    // Pre-allocate locals for let bindings
    alloc_declared_locals(&mut ctx, &func.body)?;

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
        emitter.emit_insns(&x86_64::prologue());
    } else {
        emitter.emit_insns(&x86_64::prologue());
    }

    // Allocate stack frame. Add extra padding for expression temporaries that
    // the lowering code pushes/pops without declaring them as locals; the
    // previous frame size only counted declared locals, which led to stack
    // corruption when complex expressions spilled values past the frame.
    ctx.frame_size += 2048;
    let frame_size = ctx.frame_reserve();
    if frame_size > 0 {
        emitter.emit_insns(&x86_64::sub_rsp_i32(frame_size as i32));
    }

    // For normal functions: save clobbered param registers (RDI, RCX) that
    // rep stosq will destroy, into [rbp+16+i*8] (caller's stack area).
    // For interrupt functions: [rbp+16] = R15 in saved regs — can't use.
    // Instead skip save AND zero-fill; params survive in registers.
    let param_regs = [RDI, RSI, RDX, RCX, 8, 9];
    let stosq_clobbers = [true, false, false, true, false, false];
    let n_stack_params = func.params.len().saturating_sub(6) as i32;
    if !is_interrupt {
        // Save clobbered params before zero-fill destroys them
        for (i, (name, _)) in func.params.iter().enumerate() {
            if i < 6 && ctx.locals.contains_key(name) && stosq_clobbers[i] {
                let temp_disp = if x86_64::is_32bit() {
                    8 + i as i32 * 4 + n_stack_params * 4
                } else {
                    16 + i as i32 * 8 + n_stack_params * 8
                };
                emitter.emit_insns(&x86_64::mov_m_r(RBP, temp_disp, param_regs[i]));
            }
        }
        // Zero-fill the allocated stack frame
        if frame_size >= 8 {
            if x86_64::is_32bit() {
                let dwords = frame_size / 4;
                emitter.emit_bytes(&[0x31, 0xC0]); // xor eax, eax
                let mut mov_ecx = vec![0xB9];
                mov_ecx.extend_from_slice(&dwords.to_le_bytes());
                emitter.emit_insns(&mov_ecx);
                emitter.emit_bytes(&[0x8D, 0x3C, 0x24]); // lea edi, [esp]
                emitter.emit_bytes(&[0xF3, 0xAB]); // rep stosd
            } else {
                let qwords = frame_size / 8;
                emitter.emit_bytes(&[0x48, 0x31, 0xC0]); // xor eax, eax
                let mut mov_rcx = vec![0x48, 0xC7, 0xC1];
                mov_rcx.extend_from_slice(&qwords.to_le_bytes());
                emitter.emit_insns(&mov_rcx);
                emitter.emit_bytes(&[0x48, 0x8D, 0x3C, 0x24]); // lea rdi, [rsp]
                emitter.emit_bytes(&[0xF3, 0x48, 0xAB]); // rep stosq
            }
        }
        // Restore clobbered params, then store ALL params to stack slots
        for (i, (name, _)) in func.params.iter().enumerate() {
            if i < 6 {
                if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(name) {
                    if stosq_clobbers[i] {
                        let temp_disp = if x86_64::is_32bit() {
                            8 + i as i32 * 4 + n_stack_params * 4
                        } else {
                            16 + i as i32 * 8 + n_stack_params * 8
                        };
                        emitter.emit_insns(&x86_64::mov_r_m(param_regs[i], RBP, temp_disp));
                    }
                    emitter.emit_insns(&x86_64::str64(param_regs[i], *offset as u16));
                }
            } else if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(name) {
                // Stack params: load from caller's argument area and store to the
                // local slot so the body can reload via ldr64.
                let stack_offset = if x86_64::is_32bit() {
                    12 + ((i - 6) * 4) as i32
                } else {
                    16 + ((i - 6) * 8) as i32
                };
                emitter.emit_insns(&x86_64::mov_r_m(RAX, RBP, stack_offset));
                emitter.emit_insns(&x86_64::str64(RAX, *offset as u16));
            }
        }
    } else {
        // Interrupt functions: skip save and zero-fill entirely.
        // [rbp+16+i*8] would corrupt saved R15 in the handler's reg context.
        // Params survive in registers (no rep stosq clobber).
        // But still store params to their stack slots so the function body
        // can reload them via ldr64 from [rbp-offset-8].
        for (i, (name, _)) in func.params.iter().enumerate() {
            if i < 6 {
                if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(name) {
                    emitter.emit_insns(&x86_64::str64(param_regs[i], *offset as u16));
                }
            }
        }
    }

    // Lower function body
    for stmt in &func.body {
        lower_stmt(emitter, &mut ctx, stmt, pending_calls)?;
    }

    let pending_globals = ctx.pending_globals;

    // If no explicit return, emit default epilogue
    if !ctx.emitted_return {
        if matches!(&func.ret, Typ::Named(_) | Typ::Array(_)) {
            // ponytail: struct/array return — return 0 (tag)
            emitter.emit_insns(&x86_64::load_i64(RAX, 0));
        } else if func.ret == Typ::Void {
            emitter.emit_insns(&x86_64::zero_reg(RAX));
        }
        // Epilogue
        if frame_size > 0 {
            emitter.emit_insns(&x86_64::add_rmi8(REG_SP, frame_size as u8));
        }
        if is_interrupt {
            // Interrupt epilogue: restore frame, pop all GPRs in reverse, iretq.
            // Epilogue from asm: mov rsp, rbp; pop rbp (but NOT ret)
            emitter.emit_insns(&x86_64::mov_rr(x86_64::REG_SP, x86_64::REG_FP));
            emitter.emit_insns(&x86_64::pop_r(x86_64::REG_FP));
            // Pop all saved GPRs (reverse of push order)
            for &reg in &[
                15u8, 14, 13, 12, 11, 10, 9, 8, RDI, RSI, RBP, RBX, RDX, RCX, RAX,
            ] {
                emitter.emit_insns(&x86_64::pop_r(reg));
            }
            // iret/iretq pops RIP/CS/RFLAGS from interrupt stack frame
            emitter.emit_width(&[0xCF], &[0x48, 0xCF]);
        } else {
            emitter.emit_insns(&x86_64::epilogue());
        }
    }

    Ok(pending_globals)
}

fn alloc_declared_locals(ctx: &mut LowerCtx<'_>, body: &[Stmt]) -> Result<(), String> {
    for stmt in body {
        match stmt {
            Stmt::Let(name, typ, _) => {
                if let Some(typ) = typ {
                    ctx.alloc_local(name, typ)?;
                } else {
                    // Infer type from expression
                    ctx.alloc_local(name, &Typ::Int)?;
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
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    alloc_declared_locals(ctx, &arm.body)?;
                }
            }
            Stmt::Throw(_) => {}
            Stmt::Try { body, catches, .. } => {
                alloc_declared_locals(ctx, body)?;
                for catch in catches {
                    ctx.alloc_local(&catch.pattern, &Typ::Int)?;
                    alloc_declared_locals(ctx, &catch.body)?;
                }
            }
            _ => {}
        }
    }
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
                if matches!(ret_typ, Typ::Named(_) | Typ::Array(_)) {
                    // ponytail: struct/array return — just return 0 (tag)
                    emitter.emit_insns(&x86_64::load_i64(RAX, 0));
                } else {
                    lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
                }
            } else {
                emitter.emit_insns(&x86_64::zero_reg(RAX));
            }
            // Epilogue
            let frame_size = ctx.frame_reserve();
            if frame_size > 0x7F {
                emitter.emit_insns(&x86_64::add_rmi8(REG_SP, frame_size as u8));
            } else if frame_size > 0 {
                emitter.emit_insns(&x86_64::add_rmi8(REG_SP, frame_size as u8));
            }
            if ctx.is_interrupt {
                // Interrupt epilogue: leave, pop all GPRs, iretq
                emitter.emit_insns(&x86_64::mov_rr(x86_64::REG_SP, x86_64::REG_FP));
                emitter.emit_insns(&x86_64::pop_r(x86_64::REG_FP));
                for &reg in &[
                    15u8, 14, 13, 12, 11, 10, 9, 8, RDI, RSI, RBP, RBX, RDX, RCX, RAX,
                ] {
                    emitter.emit_insns(&x86_64::pop_r(reg));
                }
                emitter.emit_width(&[0xCF], &[0x48, 0xCF]); // iret/iretq
            } else {
                emitter.emit_insns(&x86_64::epilogue());
            }
            ctx.emitted_return = true;
            Ok(())
        }
        Stmt::Let(name, typ, expr) => {
            if !ctx.locals.contains_key(name) {
                let resolved = typ.clone().unwrap_or(Typ::Int);
                ctx.alloc_local(name, &resolved)?;
            }
            lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
            match ctx.locals.get(name) {
                Some(StackSlot::Scalar(offset)) => {
                    emitter.emit_insns(&x86_64::str64(RAX, *offset as u16));
                }
                Some(StackSlot::Struct { fields }) => {
                    // ponytail: struct let — store RAX to first field, zero the rest
                    let mut sorted: Vec<&u32> = fields.values().collect();
                    sorted.sort();
                    if let Some(first) = sorted.first() {
                        emitter.emit_insns(&x86_64::str64(RAX, **first as u16));
                    }
                    for off in sorted.iter().skip(1) {
                        emitter.emit_insns(&x86_64::load_i64(RAX, 0));
                        emitter.emit_insns(&x86_64::str64(RAX, **off as u16));
                    }
                }
                _ => {}
            }
            Ok(())
        }
        Stmt::Assign(name, expr) => {
            // Check if this is a global variable
            if let Some(&offset) = ctx.globals.get(name) {
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
            // ponytail: Assign to non-scalar local (struct field) emits expr but skips store
            let offset = match ctx.locals.get(name) {
                Some(StackSlot::Scalar(off)) => Some(*off),
                _ => None,
            };
            lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
            if let Some(off) = offset {
                emitter.emit_insns(&x86_64::str64(RAX, off as u16));
            }
            Ok(())
        }
        Stmt::FieldAssign {
            base, name, value, ..
        } => {
            // s.x = value → compute addr = &s + field_offset, store value
            let Expr::Ident(base_name) = base else {
                return Err(format!(
                    "x86_64-lower: unsupported field assign base in `{}`",
                    ctx.fn_name
                ));
            };
            if !ctx.locals.contains_key(base_name) {
                return Err(format!(
                    "x86_64-lower: unknown local `{base_name}` for field assign"
                ));
            }
            let field_offset = match ctx.locals.get(base_name) {
                Some(StackSlot::Struct { fields, .. }) => fields
                    .get(name)
                    .copied()
                    .ok_or_else(|| format!("field `{name}` not found in struct"))?,
                _ => {
                    return Err(format!(
                        "expected struct for field assign `{base_name}.{name}`"
                    ));
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
            // a[i] = value → compute addr = base + i*8, store value
            lower_expr_into(emitter, ctx, base, RDI, pending_calls)?;
            lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
            // RAX = index; shl rax, 3 (multiply by 8 for Int)
            emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3));
            // add rdi, rax
            emitter.emit_insns(&x86_64::add_rr(RDI, RAX));
            // value into rsi
            lower_expr_into(emitter, ctx, value, RSI, pending_calls)?;
            // mov [rdi], rsi
            emitter.emit_insns(&x86_64::mov_ptr_reg(RDI, RSI));
            Ok(())
        }
        Stmt::Break => {
            // ponytail: break is a no-op for now
            Ok(())
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => lower_if(emitter, ctx, cond, then_body, else_body, pending_calls),
        Stmt::Loop {
            kind: LoopKind::For { .. },
            ..
        } => Err(format!(
            "x86_64-lower: Vec iteration is not implemented in `{}`",
            ctx.fn_name
        )),
        Stmt::Propagate => Err(format!(
            "x86_64-lower: error propagation is not implemented in `{}`",
            ctx.fn_name
        )),
        Stmt::Loop { cond, body, .. } => lower_loop(emitter, ctx, cond, body, pending_calls),
        Stmt::Match {
            scrutinee, arms, ..
        } => lower_match(emitter, ctx, scrutinee, arms, pending_calls),
        Stmt::Throw(expr) => {
            lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
            emitter.emit_insns(&x86_64::str64(RAX, ctx.error_value_offset as u16));
            // Set error flag byte to 1
            let flag_disp = -(ctx.error_flag_offset as i32 + 8);
            if flag_disp >= i8::MIN as i32 && flag_disp <= i8::MAX as i32 {
                emitter.emit_bytes(&[0xC6, 0x45, flag_disp as u8, 0x01]);
            } else {
                let mut code = vec![0xC6, 0x85];
                code.extend_from_slice(&flag_disp.to_le_bytes());
                code.push(0x01);
                emitter.emit_insns(&code);
            }
            Ok(())
        }
        Stmt::Try { body, catches, .. } => {
            let saved_flag_offset = ctx.error_value_offset + 8;
            let flag_disp = -(ctx.error_flag_offset as i32 + 8);
            let saved_disp = -(saved_flag_offset as i32 + 8);

            // Save current error flag: al = byte [rbp+flag_disp]; byte [rbp+saved_disp] = al
            if flag_disp >= i8::MIN as i32 && flag_disp <= i8::MAX as i32 {
                emitter.emit_bytes(&[0x8A, 0x45, flag_disp as u8]);
            } else {
                let mut code = vec![0x8A, 0x85];
                code.extend_from_slice(&flag_disp.to_le_bytes());
                emitter.emit_insns(&code);
            }
            if saved_disp >= i8::MIN as i32 && saved_disp <= i8::MAX as i32 {
                emitter.emit_bytes(&[0x88, 0x45, saved_disp as u8]);
            } else {
                let mut code = vec![0x88, 0x85];
                code.extend_from_slice(&saved_disp.to_le_bytes());
                emitter.emit_insns(&code);
            }

            // Clear error flag
            if flag_disp >= i8::MIN as i32 && flag_disp <= i8::MAX as i32 {
                emitter.emit_bytes(&[0xC6, 0x45, flag_disp as u8, 0x00]);
            } else {
                let mut code = vec![0xC6, 0x85];
                code.extend_from_slice(&flag_disp.to_le_bytes());
                code.push(0x00);
                emitter.emit_insns(&code);
            }

            // Lower try body
            for stmt in body {
                lower_stmt(emitter, ctx, stmt, pending_calls)?;
            }

            // Check error flag: cmp byte [rbp+flag_disp], 0; jne handler
            if flag_disp >= i8::MIN as i32 && flag_disp <= i8::MAX as i32 {
                emitter.emit_bytes(&[0x80, 0x7D, flag_disp as u8, 0x00]);
            } else {
                let mut code = vec![0x80, 0xBD];
                code.extend_from_slice(&flag_disp.to_le_bytes());
                code.push(0x00);
                emitter.emit_insns(&code);
            }
            let handler_branch = emitter.len();
            emitter.emit_bytes(&[0x0F, 0x85, 0, 0, 0, 0]); // jne rel32 placeholder
            let end_branch = emitter.len();
            emitter.emit_insns(&x86_64::jmp_rel32(0)); // jmp end placeholder

            // Handler
            let handler_offset = emitter.len();
            // Clear error flag
            if flag_disp >= i8::MIN as i32 && flag_disp <= i8::MAX as i32 {
                emitter.emit_bytes(&[0xC6, 0x45, flag_disp as u8, 0x00]);
            } else {
                let mut code = vec![0xC6, 0x85];
                code.extend_from_slice(&flag_disp.to_le_bytes());
                code.push(0x00);
                emitter.emit_insns(&code);
            }

            if let Some(catch_arm) = catches.first() {
                // Load error value into RAX
                emitter.emit_insns(&x86_64::ldr64(RAX, ctx.error_value_offset as u16));
                // Store to catch pattern local
                if let Some(StackSlot::Scalar(offset)) = ctx.locals.get(&catch_arm.pattern) {
                    emitter.emit_insns(&x86_64::str64(RAX, *offset as u16));
                }
                for catch_stmt in &catch_arm.body {
                    lower_stmt(emitter, ctx, catch_stmt, pending_calls)?;
                }
            }

            let end_offset = emitter.len();

            // Patch handler branch (jne rel32)
            let handler_delta = handler_offset as i32 - handler_branch as i32 - 6;
            emitter.patch_u32(handler_branch + 2, handler_delta as u32);
            // Patch end branch (jmp rel32)
            let end_delta = end_offset as i32 - end_branch as i32 - 5;
            emitter.patch_u32(end_branch + 1, end_delta as u32);

            // Restore saved error flag: al = byte [rbp+saved_disp]; byte [rbp+flag_disp] = al
            if saved_disp >= i8::MIN as i32 && saved_disp <= i8::MAX as i32 {
                emitter.emit_bytes(&[0x8A, 0x45, saved_disp as u8]);
            } else {
                let mut code = vec![0x8A, 0x85];
                code.extend_from_slice(&saved_disp.to_le_bytes());
                emitter.emit_insns(&code);
            }
            if flag_disp >= i8::MIN as i32 && flag_disp <= i8::MAX as i32 {
                emitter.emit_bytes(&[0x88, 0x45, flag_disp as u8]);
            } else {
                let mut code = vec![0x88, 0x85];
                code.extend_from_slice(&flag_disp.to_le_bytes());
                emitter.emit_insns(&code);
            }

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
    let loop_start = emitter.len();

    if let Some(cond) = cond {
        lower_expr_into(emitter, ctx, cond, RAX, pending_calls)?;
        emitter.emit_insns(&x86_64::cmp_rmi8(RAX, 0));
        let exit_branch = emitter.len();
        // Use near conditional jump (6 bytes) to avoid rel8 overflow for large bodies
        emitter.emit_bytes(&[0x0F, 0x84, 0, 0, 0, 0]); // jcc_near(0x04, 0) placeholder

        for stmt in body {
            lower_stmt(emitter, ctx, stmt, pending_calls)?;
        }

        // Backward jump to loop_start
        let loop_end = emitter.len();
        let back_delta = loop_start as i32 - loop_end as i32;
        if back_delta - 2 >= i8::MIN as i32 && back_delta - 2 <= i8::MAX as i32 {
            emitter.emit_insns(&x86_64::jmp_rel8((back_delta - 2) as i8));
        } else {
            emitter.emit_insns(&x86_64::jmp_rel32(back_delta - 5));
        }

        // Patch exit branch (jcc_near rel32)
        let exit_offset = emitter.len();
        let exit_delta = exit_offset as i32 - exit_branch as i32 - 6;
        emitter.patch_u32(exit_branch + 2, exit_delta as u32);
    } else {
        // Infinite loop
        for stmt in body {
            lower_stmt(emitter, ctx, stmt, pending_calls)?;
        }
        let loop_end = emitter.len();
        let back_delta = loop_start as i32 - loop_end as i32;
        if back_delta - 2 >= i8::MIN as i32 && back_delta - 2 <= i8::MAX as i32 {
            emitter.emit_insns(&x86_64::jmp_rel8((back_delta - 2) as i8));
        } else {
            emitter.emit_insns(&x86_64::jmp_rel32(back_delta - 5));
        }
    }

    Ok(())
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
    let mut default_body: Option<&[Stmt]> = None;

    for arm in arms {
        if is_default_match_pattern(&arm.pattern) {
            default_body = Some(arm.body.as_slice());
            continue;
        }

        if let Some(value) = parse_int_match_pattern(&arm.pattern) {
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
            end_branches.push(end_branch);
        } else {
            // ponytail: non-int pattern (enum variant, range, etc.) — skip entirely to avoid crash
            // (unconditional execution of non-int arms causes issues on x86_64 self-host)
            // Extract vars anyway so they exist if referenced, but don't execute body
            let vars = extract_pattern_vars(&arm.pattern);
            for var in &vars {
                if !ctx.locals.contains_key(var) {
                    ctx.alloc_local(var, &Typ::Int)?;
                }
            }
            // No unconditional execution — prevents cascading command handlers from crashing
        }
    }

    if let Some(body) = default_body {
        for stmt in body {
            lower_stmt(emitter, ctx, stmt, pending_calls)?;
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

fn is_default_match_pattern(pattern: &str) -> bool {
    matches!(
        pattern.trim().trim_end_matches(':'),
        "_" | "-" | "else" | "default" | "case else" | "case default"
    )
}

fn parse_int_match_pattern(pattern: &str) -> Option<i64> {
    let trimmed = pattern.trim().trim_end_matches(':').trim();
    let trimmed = trimmed.strip_prefix("case ").unwrap_or(trimmed).trim();
    trimmed.parse::<i64>().ok()
}

fn maybe_push_var(word: &str, vars: &mut Vec<String>) {
    if word.len() == 1 && word.chars().next().map_or(false, |c| c.is_uppercase()) {
        return;
    }
    if matches!(
        word,
        "true"
            | "false"
            | "mut"
            | "ref"
            | "self"
            | "Self"
            | "let"
            | "fn"
            | "if"
            | "else"
            | "match"
            | "while"
            | "for"
            | "return"
            | "use"
            | "mod"
            | "pub"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "where"
            | "as"
            | "in"
            | "move"
            | "static"
            | "const"
            | "type"
            | "unsafe"
            | "extern"
            | "crate"
            | "super"
            | "dyn"
    ) {
        return;
    }
    let w = word.to_string();
    if !vars.contains(&w) {
        vars.push(w);
    }
}

/// Extract variable names from a match pattern string.
fn extract_pattern_vars(pattern: &str) -> Vec<String> {
    let mut vars = Vec::new();
    let s = pattern.trim().trim_end_matches(':');
    let mut current = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            current.push(c);
        } else {
            if !current.is_empty() {
                maybe_push_var(&current, &mut vars);
                current.clear();
            }
        }
    }
    if !current.is_empty() {
        maybe_push_var(&current, &mut vars);
    }
    vars
}

fn lower_int_lit(emitter: &mut CodeEmitter, target_reg: u8, value: i64) -> Result<(), String> {
    emitter.emit_insns(&x86_64::load_i64(target_reg, value));
    Ok(())
}

fn lower_bool_lit(emitter: &mut CodeEmitter, target_reg: u8, value: bool) -> Result<(), String> {
    emitter.emit_insns(&x86_64::load_i64(target_reg, if value { 1 } else { 0 }));
    Ok(())
}

fn lower_ident_ref(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    name: &str,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
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
        } else {
            let addr_offset = if x86_64::is_32bit() { 3 } else { 4 };
            let site = emitter.len() + addr_offset;
            emitter.emit_insns(&x86_64::mov_r_from_abs32(target_reg, 0));
            ctx.pending_globals.push(PendingGlobal {
                site,
                width: 4,
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

fn lower_binary_expr(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    lower_expr_into(emitter, ctx, lhs, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::push_r(RAX));
    lower_expr_into(emitter, ctx, rhs, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::pop_r(RBX));

    match op {
        "+" => {
            emitter.emit_insns(&x86_64::add_rr(RAX, RBX));
        }
        "-" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::sub_rr(RAX, RCX));
        }
        "*" => {
            emitter.emit_insns(&x86_64::imul_rr(RAX, RBX));
        }
        ">" => {
            emitter.emit_insns(&x86_64::cmp_rr(RBX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x9F, RAX)); // setg al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        ">=" => {
            emitter.emit_insns(&x86_64::cmp_rr(RBX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x9D, RAX)); // setge al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "&&" => {
            emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x95, RAX)); // setne al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::test_rr(RBX, RBX));
            emitter.emit_insns(&x86_64::setcc(0x95, RBX)); // setne bl
            emitter.emit_insns(&x86_64::movzx_r8(RBX, RBX));
            emitter.emit_insns(&x86_64::and_rr(RCX, RBX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RCX));
        }
        "||" => {
            emitter.emit_insns(&x86_64::test_rr(RAX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x95, RAX)); // setne al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::test_rr(RBX, RBX));
            emitter.emit_insns(&x86_64::setcc(0x95, RBX)); // setne bl
            emitter.emit_insns(&x86_64::movzx_r8(RBX, RBX));
            emitter.emit_insns(&x86_64::or_rr(RCX, RBX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RCX));
        }
        "<=" => {
            emitter.emit_insns(&x86_64::cmp_rr(RBX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x9E, RAX)); // setle al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "/" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::zero_reg(RDX));
            emitter.emit_insns(&x86_64::div_reg(RCX));
        }
        "<" => {
            emitter.emit_insns(&x86_64::cmp_rr(RBX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x9C, RAX)); // setl al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "==" | "=" => {
            emitter.emit_insns(&x86_64::cmp_rr(RBX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x94, RAX)); // sete al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "!=" => {
            emitter.emit_insns(&x86_64::cmp_rr(RBX, RAX));
            emitter.emit_insns(&x86_64::setcc(0x95, RAX)); // setne al
            emitter.emit_insns(&x86_64::movzx_r8(RAX, RAX));
        }
        "^" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::xor_rr(RAX, RCX));
        }
        "<<" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::shl_reg_cl(RAX));
        }
        ">>" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::shr_reg_cl(RAX));
        }
        "&" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::and_rr(RAX, RCX));
        }
        "+=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::add_rr(RAX, RCX));
        }
        "-=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::sub_rr(RAX, RCX));
        }
        "|" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::or_rr(RAX, RCX));
        }
        "*=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::imul_rr(RAX, RCX));
        }
        "/=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::zero_reg(RDX));
            emitter.emit_insns(&x86_64::div_reg(RCX));
        }
        "%=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::zero_reg(RDX));
            emitter.emit_insns(&x86_64::div_reg(RCX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
        }
        "&=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::and_rr(RAX, RCX));
        }
        "|=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::or_rr(RAX, RCX));
        }
        "^=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::xor_rr(RAX, RCX));
        }
        "<<=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::shl_reg_cl(RAX));
        }
        ">>=" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::shr_reg_cl(RAX));
        }
        "%" => {
            emitter.emit_insns(&x86_64::mov_rr(RCX, RAX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RBX));
            emitter.emit_insns(&x86_64::zero_reg(RDX));
            emitter.emit_insns(&x86_64::div_reg(RCX));
            emitter.emit_insns(&x86_64::mov_rr(RAX, RDX));
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
                lower_expr_into(emitter, ctx, &args[0], RBX, pending_calls)?;
                let val_reg = if x86_64::is_32bit() { RCX } else { RSI };
                lower_expr_into(emitter, ctx, &args[1], val_reg, pending_calls)?;
                emitter.emit_bytes(&[0x66, 0x89, 0xDA]); // mov dx, bx
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
                lower_expr_into(emitter, ctx, &args[0], RBX, pending_calls)?;
                lower_expr_into(emitter, ctx, &args[1], RAX, pending_calls)?;
                emitter.emit_bytes(&[0x66, 0x89, 0xDA]); // mov dx, bx
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
                lower_expr_into(emitter, ctx, &args[0], RBX, pending_calls)?;
                lower_expr_into(emitter, ctx, &args[1], RAX, pending_calls)?;
                emitter.emit_bytes(&[0x66, 0x89, 0xDA]); // mov dx, bx
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
    if args.len() > nreg {
        for arg in args[nreg..].iter().rev() {
            lower_expr_into(emitter, ctx, arg, RAX, pending_calls)?;
            emitter.emit_insns(&x86_64::push_r(RAX));
        }
    }
    for (i, arg) in args[..nreg.min(args.len())].iter().enumerate() {
        if i > 0 {
            for j in 0..i {
                emitter.emit_insns(&x86_64::push_r(arg_regs[j]));
            }
        }
        lower_expr_into(emitter, ctx, arg, arg_regs[i], pending_calls)?;
        if i > 0 {
            for j in (0..i).rev() {
                emitter.emit_insns(&x86_64::pop_r(arg_regs[j]));
            }
        }
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
            // Non-Ident callee: emit 0 for return
            emitter.emit_insns(&x86_64::load_i64(target_reg, 0));
            return Ok(());
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

    if TL_JIT_EXTERNS.with(|m| *m.borrow()) {
        let base = target_name
            .rsplit("::")
            .next()
            .unwrap_or(&target_name)
            .replace('_', "-");
        if let Some(wrapper) = jit_stdlib_wrapper(&base)
            && let Some(ptr) = crate::native_emit::native_link::resolve_native_fn(wrapper)
        {
            lower_call_args(emitter, ctx, args, &target_name, pending_calls)?;
            emitter.emit_insns(&x86_64::load_i64(RAX, ptr as i64));
            emitter.emit_insns(&[0xFF, 0xD0]);
            emit_stack_cleanup(emitter, args.len());
            if target_reg != RAX {
                emitter.emit_insns(&x86_64::xor_rr(target_reg, target_reg));
            }
            return Ok(());
        }
    }

    if !ctx.functions.contains_key(&target_name) {
        lower_call_args(emitter, ctx, args, &target_name, pending_calls)?;
        // Extern function: emit CALL instruction with relocation.
        // The linker resolves the target — until then, displacement=0.
        let site = emitter.len() as u32;
        emitter.emit_insns(&x86_64::call_rel32(0));
        pending_calls.push(PendingCall {
            site,
            target: target_name,
        });
        emit_stack_cleanup(emitter, args.len());
        if target_reg != RAX {
            emitter.emit_insns(&x86_64::xor_rr(target_reg, target_reg));
        }
        return Ok(());
    }

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

    if target_reg != RAX {
        emitter.emit_insns(&x86_64::mov_rr(target_reg, RAX));
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
) -> Result<(), String> {
    let Expr::Ident(base_name) = base else {
        emitter.emit_insns(&x86_64::load_i64(target_reg, 0));
        return Ok(());
    };
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
                    return Err(format!(
                        "x86_64-lower: unknown field `{name}` in `{}`",
                        ctx.fn_name
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
                Err(format!(
                    "x86_64-lower: unknown field `{name}` in `{}`",
                    ctx.fn_name
                ))
            }
        }
        _ => {
            emitter.emit_insns(&x86_64::load_i64(target_reg, 0));
            Ok(())
        }
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
    lower_expr_into(emitter, ctx, expr, RAX, pending_calls)?;
    match op {
        "-" => {
            emitter.emit_insns(&x86_64::neg_reg(RAX));
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

fn lower_index_expr(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target_reg: u8,
    base: &Expr,
    index: &Expr,
    pending_calls: &mut Vec<PendingCall>,
) -> Result<(), String> {
    lower_expr_into(emitter, ctx, base, RDI, pending_calls)?;
    lower_expr_into(emitter, ctx, index, RAX, pending_calls)?;
    emitter.emit_insns(&x86_64::shl_reg_imm(RAX, 3)); // shl rax, 3
    emitter.emit_insns(&x86_64::add_rr(RDI, RAX)); // add rdi, rax
    emitter.emit_insns(&x86_64::mov_reg_ptr(RAX, RDI)); // mov rax, [rdi]
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
            lower_field_access(emitter, ctx, target_reg, base, name.as_str())
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
        _ => Err(format!(
            "x86_64-lower: unsupported expression in `{}`",
            ctx.fn_name
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::UnifiedModule;

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
    fn jit_string_return_and_stdlib_extern_lower_together() {
        let src = "import std.process;\ncapability process.spawn;\nfn main() -> String { return process_run(\"true\"); }\n";
        let module = crate::in_lang_parse::parse_in_source(src).expect("parse");
        crate::native_emit::native_link::bootstrap_jit_native();
        TL_JIT_EXTERNS.with(|m| *m.borrow_mut() = true);
        let result = lower_module(&module, "main").expect("lower");
        TL_JIT_EXTERNS.with(|m| *m.borrow_mut() = false);
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
    fn rejects_vec_for_loop() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![],
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
        let error = match lower_module(&module, "main") {
            Ok(_) => panic!("Vec iteration must reject"),
            Err(error) => error,
        };
        assert!(error.contains("Vec iteration is not implemented"));
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
        // 48 89 45 C8.  It must also save the rep-stosq-clobbered register params
        // above the stack-arg area so they don't overwrite argument 7: mov [rbp+24], rdi
        // is 48 89 7D 18 and mov [rbp+48], rcx is 48 89 4D 30.
        let load_stack_arg = [0x48u8, 0x8B, 0x45, 0x10];
        let store_to_slot = [0x48u8, 0x89, 0x45, 0xC8];
        let save_rdi_above_stack_arg = [0x48u8, 0x89, 0x7D, 0x18];
        let save_rcx_above_stack_arg = [0x48u8, 0x89, 0x4D, 0x30];
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
            code.windows(save_rdi_above_stack_arg.len())
                .any(|w| w == save_rdi_above_stack_arg),
            "missing clobbered RDI save above stack arg: expected `mov [rbp+24], rdi`"
        );
        assert!(
            code.windows(save_rcx_above_stack_arg.len())
                .any(|w| w == save_rcx_above_stack_arg),
            "missing clobbered RCX save above stack arg: expected `mov [rbp+48], rcx`"
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
        let stack_arg = [0x48, 0xB8, 63, 0, 0, 0, 0, 0, 0, 0, 0x50];
        let stack_arg_end = result
            .code
            .windows(stack_arg.len())
            .position(|bytes| bytes == stack_arg)
            .expect("stack argument")
            + stack_arg.len();
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
        // Should contain byte store (0xC6) for error flag
        assert!(result.code.contains(&0xC6));
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
}
