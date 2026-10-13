use crate::core_ir::{Decl, UnifiedModule};
use crate::crate_db;
use crate::dep_resolver;
use crate::jit_runtime;
use crate::native_emit::native_link;
use crate::native_emit::{self, NativeLinkage, antidecomp, lower, x86_64_lower};

use super::native::NativeCompileResult;
use super::report::jobs_for_request;
use super::{EvalValue, OwnedCompileRequest};

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
/// Tries: exact match, namespaced (`.<entry>`), then falls
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

    let _profile_guard = antidecomp::ProfileGuard::new(request.profile);

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

    // Rust aggregate ABIs are not consistently represented by every native backend.
    let compile_only = request
        .path
        .extension()
        .is_some_and(|extension| extension == "rs")
        && !rust_entry_can_execute(expanded_module, &resolved_entry, entry);
    if !compile_only
        && request
            .path
            .extension()
            .is_some_and(|extension| extension == "rs")
    {
        let reachable = super::native::native_entry_module(expanded_module, &resolved_entry);
        let verification = crate::core_ir_verifier::verify_module(
            &reachable,
            &crate::core_ir_verifier::VerifyOptions {
                entry: Some(resolved_entry.clone()),
                require_entry: true,
            },
        );
        if !verification.ok {
            return Err(format!(
                "jit-verification-failed: {}",
                verification
                    .reason
                    .unwrap_or_else(|| "invalid Rust Core IR".into())
            ));
        }
    }

    native_link::bootstrap_jit_native();

    // Scalar results arrive in the integer result register; Float uses its bits
    // and String points to an instring. The declared return type selects decoding.
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
        let result = x86_64_lower::lower_module(expanded_module, &resolved_entry)
            .map_err(|e| format!("jit-lowering-failed: {e}"))?;
        // Wrap into LoweredModule-compatible shape
        native_emit::lower::LoweredModule {
            code: result.code,
            data: result.data,
            global_relocs: result.global_relocs,
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
    rt.load_with_globals(
        &lowered.code,
        &function_offsets,
        &lowered.relocations,
        &lowered.data,
        &lowered.global_relocs,
    )
    .map_err(|e| format!("jit-load-failed: {e}"))?;

    // Code the lowerer replaced with a trap cannot be executed: the trap writes
    // to stderr and exits, which would take the compiler process down with it.
    // Only traps the entry can reach matter; unreachable ones are dead code.
    if !compile_only && let Some(first) = degradations.iter().find(|d| d.reachable) {
        let live = degradations.iter().filter(|d| d.reachable).count();
        return Err(format!(
            "jit-degraded: {live} reachable function(s) could not be compiled to native code, starting with `{}` ({}); refusing to execute code that would trap",
            first.function, first.code
        ));
    }
    let (exit_code, eval_result) = if compile_only {
        (None, None)
    } else {
        let raw = unsafe { rt.invoke(&resolved_entry, &[]) }.ok_or_else(|| {
            format!("jit-invocation-failed: entry `{resolved_entry}` could not be invoked")
        })?;
        if let Some(value) = rt.invocation_error() {
            return Err(format!(
                "jit-uncaught-error: entry `{resolved_entry}` threw {value}"
            ));
        }
        match entry_result_kind {
            EntryResultKind::Int => (Some(raw as u8), Some(EvalValue::Int(raw))),
            EntryResultKind::Bool => (Some(raw as u8), Some(EvalValue::Bool(raw != 0))),
            EntryResultKind::Float => (Some(0), Some(EvalValue::Float(f64::from_bits(raw as u64)))),
            EntryResultKind::String => {
                let string = decode_jit_string(raw).ok_or_else(|| {
                    format!("jit-string-result-invalid: entry `{resolved_entry}` returned an invalid string reference")
                })?;
                // The decoded string is the value; the pointer is not an exit status.
                (Some(0), Some(EvalValue::Str(string)))
            }
            EntryResultKind::None => {
                // The entry returns something an exit status cannot carry (Void,
                // an aggregate). Run it for its effects, report no result.
                (Some(0), None)
            }
        }
    };

    let reason_code = if compile_only {
        "jit-compiled-only"
    } else {
        "jit-executed"
    };
    let reason = if compile_only {
        "JIT-compiled without execution: Rust module uses Core IR or invocation ABI features that cannot yet be executed"
    } else {
        "JIT-compiled native function executed via in-memory MAP_JIT page"
    };

    Ok(NativeCompileResult {
        artifact_path: String::new(),
        eval_exit_code: exit_code,
        eval_result,
        abi_path: None,
        backend_level: "owned-native-jit".to_string(),
        runtime_level: "inrt-jit".to_string(),
        reason_code: reason_code.to_string(),
        reason: reason.to_string(),
        degradations,
    })
}

fn rust_entry_can_execute(module: &UnifiedModule, entry: &str, requested: &str) -> bool {
    use crate::core_ir::Typ;
    let scalar = |typ: &Typ| {
        matches!(
            typ.canonical(),
            Typ::Int | Typ::Bool | Typ::Float | Typ::String | Typ::Void
        )
    };
    if entry != requested && !entry.ends_with(&format!(".{requested}")) {
        return false;
    }
    let Some(params) = module.decls.iter().find_map(|decl| match decl {
        Decl::Function { name, params, .. } if name == entry => Some(params),
        _ => None,
    }) else {
        return false;
    };
    if !params.is_empty() {
        return false;
    }
    let reachable = super::native::native_entry_module(module, entry);
    let mut names = std::collections::HashSet::new();
    for decl in &reachable.decls {
        let name = match decl {
            Decl::Struct { name, .. }
            | Decl::Class { name, .. }
            | Decl::Function { name, .. }
            | Decl::Global { name, .. } => name,
            _ => continue,
        };
        // Rust type and value namespaces can collide in the flat Core IR namespace.
        // Such modules cannot yet pass the verification required for execution.
        if !names.insert(name) {
            return false;
        }
    }
    reachable.decls.iter().all(|decl| match decl {
        Decl::Function { params, ret, .. } => {
            scalar(ret) && params.iter().all(|(_, typ)| scalar(typ))
        }
        _ => true,
    })
}

fn decode_jit_string(raw: i64) -> Option<String> {
    const MAX_BYTES: usize = 64 * 1024 * 1024;
    let ptr = raw as *const u8;
    // Runtime builtins use null for an empty result on invalid input ranges.
    if ptr.is_null() {
        return Some(String::new());
    }
    if !(ptr as usize).is_multiple_of(8) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_string_decode_rejects_invalid_references() {
        assert_eq!(decode_jit_string(0), Some(String::new()));
        assert_eq!(decode_jit_string(1), None);
        let oversized = u64::MAX;
        assert_eq!(decode_jit_string((&oversized as *const u64) as i64), None);
    }
}
