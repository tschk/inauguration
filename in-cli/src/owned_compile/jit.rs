use crate::core_ir::{Decl, Expr, Stmt, UnifiedModule};
use crate::crate_db;
use crate::dep_resolver;
use crate::jit_runtime;
use crate::native_emit::native_link;
use crate::native_emit::{self, NativeLinkage, antidecomp, lower, x86_64_lower};
use crate::parser_registry::ParserCli;
use std::path::PathBuf;

use super::native::NativeCompileResult;
use super::report::jobs_for_request;
use super::{CompileTarget, EvalValue, OwnedCompileRequest};

/// Which register or pointer the entry's result has to be read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryResultKind {
    Int,
    Bool,
    Float,
    String,
    None,
}

/// Resolve a JIT entry name against `module`'s function declarations.
/// Tries: exact match, namespaced (`.<entry>`), suffix match, then falls
/// back to the first function so library crates without main can compile.
pub fn resolve_jit_entry(module: &UnifiedModule, entry: &str) -> String {
    let mut func_names = module.decls.iter().filter_map(|d| match d {
        Decl::Function { name, .. } => Some(name.as_str()),
        _ => None,
    });

    if func_names.clone().any(|n| n == entry) {
        return entry.to_string();
    }
    let dot_entry = format!(".{entry}");
    if let Some(found) = func_names.clone().find(|n| n.ends_with(&dot_entry)) {
        return found.to_string();
    }
    // Suffix match: entry is a suffix of a function name (beyond a dot)
    if let Some(found) = func_names.clone().find(|n| {
        n.ends_with(entry) && n.as_bytes().get(n.len() - entry.len().wrapping_sub(1)) == Some(&b'.')
    }) {
        return found.to_string();
    }
    // Entry not found — use first non-closure function so libs compile
    func_names
        .find(|n| !n.starts_with("__closure_"))
        .map(|s| s.to_string())
        .unwrap_or_else(|| entry.to_string())
}

/// JIT compile: lower to native machine code, load into JitRuntime, invoke entry.
pub fn compile_jit(
    module: &UnifiedModule,
    _module_id: &str,
    request: &OwnedCompileRequest,
) -> Result<NativeCompileResult, String> {
    if !lower::host_supports_native_subset() {
        return Err(
            "jit-host-unsupported: JIT currently requires macOS AArch64 or Linux x86_64"
                .to_string(),
        );
    }

    antidecomp::set_profile(request.profile);
    struct _ProfileGuard;
    impl Drop for _ProfileGuard {
        fn drop(&mut self) {
            antidecomp::clear_profile();
        }
    }
    let _profile_guard = _ProfileGuard;

    // Lazy dependency resolution: before lowering, resolve external
    // function calls by loading their source crates.
    let crate_db = crate_db::CrateDb::new();
    // Register std, core, alloc in parallel — each crate search is I/O bound.
    std::thread::scope(|s| {
        let crate_db = &crate_db;
        s.spawn(move || {
            for root in &crate_db.search_roots {
                let std_root = root.join("std");
                if std_root.join("src").exists() {
                    crate_db.register_crate("std", std_root);
                    break;
                }
            }
        });
        s.spawn(move || {
            for name in &["core", "alloc"] {
                for root in &crate_db.search_roots {
                    let crate_root = root.join(name);
                    if crate_root.join("src").exists() {
                        crate_db.register_crate(name, crate_root);
                        break;
                    }
                }
            }
        });
    });

    let resolved_module = dep_resolver::resolve_deps(module, &crate_db);
    let expanded_module = &resolved_module.module;

    // Debug log how many deps were resolved
    if resolved_module.files_parsed > 0 {
        eprintln!(
            "[dep-resolve] parsed {} files, added {} functions",
            resolved_module.files_parsed, resolved_module.functions_added,
        );
    }

    let entry = request
        .entry
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or("main");
    let resolved_entry = resolve_jit_entry(expanded_module, entry);

    native_link::bootstrap_jit_native();

    // How to read the entry's result value, which the declared return type
    // decides: `Int` and `Bool` arrive in x0, `Float` in d0, and `String` as a
    // pointer to an instring.
    let entry_result_kind = expanded_module
        .decls
        .iter()
        .find_map(|decl| match decl {
            crate::core_ir::Decl::Function { name, ret, .. }
                if name == &resolved_entry || name.ends_with(&format!(".{resolved_entry}")) =>
            {
                Some(match ret.canonical() {
                    crate::core_ir::Typ::Int => EntryResultKind::Int,
                    crate::core_ir::Typ::Bool => EntryResultKind::Bool,
                    crate::core_ir::Typ::Float => EntryResultKind::Float,
                    crate::core_ir::Typ::String => EntryResultKind::String,
                    _ => EntryResultKind::None,
                })
            }
            _ => None,
        })
        .unwrap_or(EntryResultKind::None);

    // Select lowering based on host architecture
    let lowered = if cfg!(target_arch = "x86_64") {
        let result = {
            x86_64_lower::TL_JIT_EXTERNS.with(|m| *m.borrow_mut() = true);
            let r = x86_64_lower::lower_module(expanded_module, &resolved_entry);
            x86_64_lower::TL_JIT_EXTERNS.with(|m| *m.borrow_mut() = false);
            r
        }
        .map_err(|e| format!("jit-lowering-failed: {e}"))?;
        // Wrap into LoweredModule-compatible shape
        native_emit::lower::LoweredModule {
            code: result.code,
            entry_offset: Some(result.entry_offset),
            exports: vec![],
            function_offsets: result.exports.into_iter().collect(),
            relocations: result
                .relocations
                .into_iter()
                .map(|o| (o, result.codegen_base))
                .collect(),
            external_refs: Vec::new(),
            degradations: Vec::new(),
            error_slot_refs: Vec::new(),
        }
    } else {
        let jobs = jobs_for_request(request);
        lower::lower_module_with_jobs(
            expanded_module,
            &resolved_entry,
            NativeLinkage::Executable,
            jobs,
        )
        .map_err(|e| format!("jit-lowering-failed: {e}"))?
    };

    let degradations = lowered.degradations;

    // Build function offset table for all compiled functions
    let function_offsets: Vec<(String, u32, u32)> = lowered
        .function_offsets
        .iter()
        .map(|(name, &offset)| {
            let next = lowered
                .function_offsets
                .values()
                .filter(|&&o| o > offset)
                .min()
                .copied()
                .unwrap_or(lowered.code.len() as u32);
            (name.clone(), offset, next - offset)
        })
        .collect();

    // Build function offset table for all compiled functions

    let mut rt = jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .map_err(|e| format!("jit-load-failed: {e}"))?;

    // ponytail: skip invoke for Rust programs — the JIT-compiled Rust std lib
    // code doesn't handle struct layouts correctly and crashes at runtime.
    // Only JIT-execute non-Rust (in-lang, icore) programs.
    let is_rust = request.path.extension().is_some_and(|e| e == "rs");
    // Code the lowerer replaced with a trap cannot be executed: the trap writes
    // to stderr and exits, which would take the compiler process down with it.
    // Only traps the entry can reach matter; unreachable ones are dead code.
    if !is_rust && let Some(first) = degradations.iter().find(|d| d.reachable) {
        let live = degradations.iter().filter(|d| d.reachable).count();
        return Err(format!(
            "jit-degraded: {live} reachable function(s) could not be compiled to native code, starting with `{}` ({}); refusing to execute code that would trap",
            first.function, first.code
        ));
    }
    let (exit_code, eval_result) = if is_rust {
        (0, None)
    } else {
        match entry_result_kind {
            EntryResultKind::Int => {
                let raw = unsafe { rt.invoke(&resolved_entry, &[]).unwrap_or(1) };
                (raw as u8, Some(EvalValue::Int(raw)))
            }
            EntryResultKind::Bool => {
                let raw = unsafe { rt.invoke(&resolved_entry, &[]).unwrap_or(1) };
                (raw as u8, Some(EvalValue::Bool(raw != 0)))
            }
            EntryResultKind::Float => {
                let value = unsafe { rt.invoke_float(&resolved_entry) };
                (0, value.map(EvalValue::Float))
            }
            EntryResultKind::String => {
                let raw = unsafe { rt.invoke(&resolved_entry, &[]).unwrap_or(1) };
                let decode_string = raw != 0 && request.out.is_none();
                let string = if decode_string {
                    decode_jit_string(raw).unwrap_or_default()
                } else {
                    String::new()
                };
                // The decoded string is the value; the pointer is not an exit status.
                (0, Some(EvalValue::Str(string)))
            }
            EntryResultKind::None => {
                // The entry returns something an exit status cannot carry (Void,
                // an aggregate). Run it for its effects, report no result.
                let _ = unsafe { rt.invoke(&resolved_entry, &[]).unwrap_or(1) };
                (0, None)
            }
        }
    };

    let reason_code = if is_rust {
        "jit-compiled-only"
    } else {
        "jit-executed"
    };
    let reason = if is_rust {
        "JIT-compiled without runtime execution (Rust programs need native linking)"
    } else {
        "JIT-compiled native function executed via in-memory MAP_JIT page"
    };

    Ok(NativeCompileResult {
        artifact_path: String::new(),
        eval_exit_code: Some(exit_code),
        eval_result,
        abi_path: None,
        backend_level: "owned-native-jit".to_string(),
        runtime_level: "inrt-jit".to_string(),
        reason_code: reason_code.to_string(),
        reason: reason.to_string(),
        degradations,
    })
}

fn decode_jit_string(raw: i64) -> Option<String> {
    const MAX_BYTES: usize = 64 * 1024 * 1024;
    let ptr = raw as *const u8;
    if ptr.is_null() || !(ptr as usize).is_multiple_of(8) {
        return None;
    }
    let len = unsafe { *(ptr as *const u64) as usize };
    if len > MAX_BYTES {
        return None;
    }
    let data = unsafe { ptr.add(8) };
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    Some(String::from_utf8_lossy(bytes).to_string())
}

fn try_const_answer_entry(module: &UnifiedModule, entry: &str) -> Option<u8> {
    for decl in &module.decls {
        if let Decl::Function {
            name, body, ret, ..
        } = decl
        {
            if name != entry {
                continue;
            }
            if *ret != crate::core_ir::Typ::Int {
                return None;
            }
            if body.len() != 1 {
                return None;
            }
            if let Stmt::Return(Some(Expr::IntLit(val))) = &body[0] {
                let code = (*val & 0xff) as u8;
                return Some(code);
            }
        }
    }
    None
}

pub fn const_eval_entry_exit_code(
    module: &UnifiedModule,
    _module_id: &str,
    entry: &str,
) -> Result<u8, String> {
    if let Some(code) = try_const_answer_entry(module, entry) {
        return Ok(code);
    }
    let code = match eval_entry_via_jit(module, entry) {
        Ok(code) => code,
        // The program cannot be executed to learn its exit code because it
        // contains code the lowerer replaced with a trap. The native entry stub
        // computes the real code at runtime, so a caller that only reports it can
        // continue with an unknown value instead of a guess.
        Err(err) if err.starts_with("jit-degraded:") => {
            return Err(format!("entry-exit-unknown: {err}"));
        }
        Err(err) => return Err(err),
    };
    // An exit status is eight bits: the OS truncates it exactly as it does for
    // C's `main`. Refusing a larger value made a program returning 1000
    // impossible to build, and `try_const_answer_entry` above already truncates.
    Ok((code & 0xff) as u8)
}

fn eval_entry_via_jit(module: &UnifiedModule, entry: &str) -> Result<i64, String> {
    if !lower::host_supports_native_subset() {
        return Err(
            "jit-host-unsupported: JIT currently requires macOS AArch64 or Linux x86_64"
                .to_string(),
        );
    }
    let request = OwnedCompileRequest {
        path: PathBuf::from("const_eval.in"),
        module_id: "App".to_string(),
        parser: ParserCli::Auto,
        target: CompileTarget::Jit,
        entry: Some(entry.to_string()),
        out: None,
        linkage: NativeLinkage::Executable,
        target_triple: None,
        jobs: 1,
        debug: false,
        profile: crate::emit_profile::EmitProfile::Default,
        emit: None,
        base: None,
    };
    let result = compile_jit(module, "App", &request)?;
    result
        .eval_result
        .and_then(|value| value.as_int())
        .ok_or_else(|| "jit did not produce an integer result for entry".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_string_decode_rejects_unaligned_reference() {
        assert_eq!(decode_jit_string(1), None);
    }
}
