mod scalar_mir;

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

#[cfg(test)]
pub(crate) fn resolve_module_dependencies(
    module: &UnifiedModule,
    entry: Option<&str>,
    target: super::CompileTarget,
) -> Result<UnifiedModule, String> {
    resolve_module_dependencies_with_bindings(module, &Default::default(), entry, target)
        .map(crate::boundary_ir::CompileArtifact::into_semantic)
}

pub(crate) fn resolve_module_dependencies_with_bindings(
    module: &UnifiedModule,
    bindings: &crate::boundary_ir::UnaryMethodBindings,
    entry: Option<&str>,
    target: super::CompileTarget,
) -> Result<crate::boundary_ir::CompileArtifact, String> {
    let selected_entry = entry.map(|entry| {
        if target == super::CompileTarget::Jit {
            resolve_jit_entry(module, entry)
        } else {
            entry.to_string()
        }
    });
    let entry = selected_entry.as_deref();
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

    let resolved_module =
        dep_resolver::resolve_deps_for_entry_with_bindings(module, bindings, &crate_db, entry)?;

    // Existing caller modules retain their frontend's validation/compile-only policy.
    // This Rust validation view is needed only when lazy resolution adds source declarations.
    if resolved_module.module.decls.len() == module.decls.len() {
        let mut artifact = crate::boundary_ir::CompileArtifact::from_semantic(module.clone());
        artifact.unary_methods = resolved_module.unary_methods;
        return Ok(artifact);
    }

    // Debug log how many deps were resolved
    if resolved_module.files_parsed > 0 {
        eprintln!(
            "[dep-resolve] parsed {} files, added {} functions",
            resolved_module.files_parsed, resolved_module.functions_added,
        );
    }

    let caller_names: std::collections::HashSet<_> = module
        .decls
        .iter()
        .map(dep_resolver::declaration_name)
        .collect();
    let mut normalized = crate::typecheck::normalize_module(
        crate::parser_registry::ParserId::Rust,
        &resolved_module.module,
    );
    crate::typecheck::TypeChecker::new()
        .resolve_unary_methods(&mut normalized, &resolved_module.unary_methods)?;
    let mut caller_shapes = module.clone();
    crate::lower_core::desugar_module(&mut caller_shapes);
    let caller_functions: std::collections::HashSet<_> = caller_shapes
        .decls
        .iter()
        .filter_map(|decl| {
            if let Decl::Function { name, .. } = decl {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect();
    let caller_globals: std::collections::HashSet<_> = caller_shapes
        .decls
        .iter()
        .filter_map(|decl| {
            if let Decl::Global { name, .. } = decl {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect();
    let mut validation = normalized.clone();
    crate::lower_core::desugar_module(&mut validation);
    if let Some(entry) = entry {
        validation = super::native::native_entry_module(&validation, entry);
    }
    for decl in &mut validation.decls {
        match decl {
            Decl::Function { name, body, .. } if caller_functions.contains(name) => body.clear(),
            Decl::Global {
                name,
                init,
                constant_value,
                ..
            } if caller_globals.contains(name) => {
                *init = None;
                *constant_value = false;
            }
            _ => {}
        }
    }
    let report = crate::core_ir_verifier::verify_module(
        &validation,
        &crate::core_ir_verifier::VerifyOptions {
            entry: None,
            require_entry: false,
        },
    );
    if !report.ok {
        return Err(format!(
            "Rust dependency validation failed: {}",
            report
                .reason
                .unwrap_or_else(|| "invalid imported source".into())
        ));
    }
    let mut expanded = module.clone();
    // Normalization removes inline constants and generic templates, and may
    // append specializations. Declaration positions no longer identify owners.
    expanded.decls.extend(
        normalized
            .decls
            .into_iter()
            .filter(|decl| !caller_names.contains(dep_resolver::declaration_name(decl))),
    );
    let mut artifact = crate::boundary_ir::CompileArtifact::from_semantic(expanded);
    artifact.unary_methods = resolved_module.unary_methods;
    Ok(artifact)
}

pub(super) fn host_supports_jit() -> bool {
    lower::host_supports_native_subset() || cfg!(all(target_os = "macos", target_arch = "x86_64"))
}

/// JIT compile: lower to native machine code, load into JitRuntime, invoke entry.
#[cfg(test)]
pub fn compile_jit(
    module: &UnifiedModule,
    module_id: &str,
    request: &OwnedCompileRequest,
) -> Result<NativeCompileResult, String> {
    compile_jit_with_execution_state(module, module_id, request, &mut false)
}

pub(super) fn compile_jit_with_execution_state(
    module: &UnifiedModule,
    _module_id: &str,
    request: &OwnedCompileRequest,
    execution_started: &mut bool,
) -> Result<NativeCompileResult, String> {
    if !host_supports_jit() {
        return Err(
            "jit-host-unsupported: JIT requires macOS AArch64/x86_64 or Linux AArch64/x86_64"
                .to_string(),
        );
    }

    let _profile_guard = antidecomp::ProfileGuard::new(request.profile);

    let entry = request
        .entry
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or("main");
    let resolved_entry = resolve_jit_entry(module, entry);

    // Rust aggregate ABIs are not consistently represented by every native backend.
    let compile_only = request
        .path
        .extension()
        .is_some_and(|extension| extension == "rs")
        && !rust_entry_can_execute(module, &resolved_entry, entry);
    if !compile_only
        && request
            .path
            .extension()
            .is_some_and(|extension| extension == "rs")
    {
        let reachable = super::native::native_entry_module(module, &resolved_entry);
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
    let entry_result_kind = module
        .decls
        .iter()
        .find_map(|decl| match decl {
            crate::core_ir::Decl::Function { name, ret, .. } if name == &resolved_entry => {
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

    let scalar_mir = if !compile_only {
        scalar_mir::try_lower(module, &resolved_entry)
            .map_err(|reason| format!("jit-lowering-failed: {reason}"))?
    } else {
        None
    };
    let used_scalar_mir = scalar_mir.is_some();
    // Preserve the native backend for sources outside the scalar MIR subset.
    let lowered = if let Some(lowered) = scalar_mir {
        Ok(lowered)
    } else if cfg!(target_arch = "x86_64") {
        x86_64_lower::lower_jit_module(module, &resolved_entry, compile_only).map(|result| {
            native_emit::lower::LoweredModule {
                code: result.code,
                data: result.data,
                data_relocations: result.data_relocations,
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
                degradations: result.degradations,
                error_slot_refs: Vec::new(),
            }
        })
    } else {
        lower::lower_module_with_jobs(
            module,
            &resolved_entry,
            NativeLinkage::Executable,
            jobs_for_request(request),
        )
    };
    let lowered = match lowered {
        Ok(lowered) => lowered,
        Err(reason)
            if compile_only
                && !crate::config::env_config().strict_lowering
                && (reason.starts_with("unsupported native initializer for global ")
                    || (request.entry.is_none()
                        && reason.starts_with("source-type-proof:")
                        && {
                            let proofs = crate::typecheck::TypeChecker::new()
                                .typed_proof_nodes(&module.decls);
                            !proofs.is_empty() && proofs.values().all(|proof| !proof.is_invalid())
                        })) =>
        {
            return Ok(NativeCompileResult {
                artifact_path: String::new(),
                eval_exit_code: None,
                eval_result: None,
                abi_path: None,
                backend_level: "owned-core-ir-only".into(),
                runtime_level: "not-executed".into(),
                reason_code: "jit-ir-only".into(),
                reason: format!("Rust compile-only module has no native code: {reason}"),
                degradations: Vec::new(),
            });
        }
        Err(reason) => return Err(format!("jit-lowering-failed: {reason}")),
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
    rt.load_with_data_relocations(
        &lowered.code,
        &function_offsets,
        &lowered.relocations,
        &lowered.data,
        &lowered.global_relocs,
        &lowered.data_relocations,
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
        *execution_started = true;
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
    } else if used_scalar_mir {
        "JIT-compiled scalar MIR function executed via in-memory MAP_JIT page"
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

pub(super) fn rust_entry_can_execute(module: &UnifiedModule, entry: &str, requested: &str) -> bool {
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
    let Some((params, ret)) = module.decls.iter().find_map(|decl| match decl {
        Decl::Function {
            name, params, ret, ..
        } if name == entry => Some((params, ret)),
        _ => None,
    }) else {
        return false;
    };
    if !params.is_empty() || !scalar(ret) {
        return false;
    }
    let reachable = super::native::native_entry_module(module, entry);
    if !crate::typecheck::TypeChecker::new()
        .typed_proof_functions(&reachable.decls)
        .is_empty()
    {
        return false;
    }
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
    let structs: std::collections::HashMap<&str, &[(String, Typ)]> = reachable
        .decls
        .iter()
        .filter_map(|decl| {
            if let Decl::Struct { name, fields, .. } = decl {
                Some((name.as_str(), fields.as_slice()))
            } else {
                None
            }
        })
        .collect();
    fn layout_words(
        typ: &Typ,
        structs: &std::collections::HashMap<&str, &[(String, Typ)]>,
        active: &mut std::collections::HashSet<String>,
        indirect: &mut std::collections::HashSet<String>,
    ) -> Option<usize> {
        if let Some(Typ::Named(referent)) = crate::core_ir::rust_references::aggregate_referent(typ)
        {
            if indirect.contains(&referent) {
                return Some(1);
            }
            if indirect.len() >= 40 {
                return None;
            }
            indirect.insert(referent.clone());
            // Validate the pointee, but start a new by-value layout path.
            // A Box can close a storage cycle; an inline field never can.
            layout_words(
                &Typ::Named(referent.clone()),
                structs,
                &mut std::collections::HashSet::new(),
                indirect,
            )?;
            indirect.remove(&referent);
            return Some(1);
        }
        match typ.canonical() {
            Typ::Int | Typ::Bool | Typ::Float | Typ::String => Some(1),
            Typ::Vector(item) => {
                layout_words(&item, structs, active, indirect)?;
                Some(3)
            }
            Typ::Named(name) => {
                let fields = structs.get(name.as_str())?;
                if active.len() >= 40 || !active.insert(name.clone()) {
                    return None;
                }
                let words = fields.iter().try_fold(0usize, |total, (_, field)| {
                    total.checked_add(layout_words(field, structs, active, indirect)?)
                });
                active.remove(&name);
                words
            }
            _ => None,
        }
    }
    let return_structs = structs
        .iter()
        .map(|(name, fields)| ((*name).to_string(), fields.to_vec()))
        .collect::<std::collections::HashMap<_, _>>();
    let internal_type = |typ: &Typ| {
        scalar(typ)
            || matches!(typ.canonical(), Typ::Array(item)
            if matches!(item.canonical(), Typ::Int | Typ::Bool | Typ::Float)
                || matches!(item.canonical(), Typ::Named(name)
                    if crate::native_emit::struct_layout::flat_record_fields(&return_structs, &name).is_ok()))
            || matches!(typ.canonical(), Typ::Vector(item)
            if matches!(item.canonical(), Typ::Int | Typ::Bool | Typ::Float | Typ::String))
    };
    let supported_return = |ret: &Typ| {
        if internal_type(ret) {
            return true;
        }
        let Typ::Named(name) = ret.canonical() else {
            return false;
        };
        if layout_words(
            ret,
            &structs,
            &mut std::collections::HashSet::new(),
            &mut std::collections::HashSet::new(),
        )
        .is_none()
        {
            return false;
        }
        let layout = if cfg!(target_arch = "x86_64") {
            crate::native_emit::struct_layout::x86_fields(&return_structs, &name)
        } else {
            crate::native_emit::struct_layout::arm_fields(&return_structs, &name)
        };
        layout.is_ok_and(|fields| {
            // ARM returns words in x0..x7; x86 stages words in a u16-offset frame.
            let max_words = if cfg!(target_arch = "x86_64") {
                usize::from(u16::MAX) / 8
            } else {
                8
            };
            fields.len() <= max_words
                && fields.iter().all(|(_, typ)| {
                    (scalar(typ) && typ.canonical() != Typ::Void)
                        || (cfg!(target_arch = "x86_64")
                            && matches!(typ.canonical(), Typ::Vector(_)))
                })
        })
    };
    reachable.decls.iter().all(|decl| match decl {
        Decl::Function { params, ret, .. } => {
            if !supported_return(ret) {
                return false;
            }
            if params.iter().all(|(_, typ)| internal_type(typ)) {
                return true;
            }
            // Aggregate fields may carry a known vector header. Standalone
            // vectors of records still require their own element-transfer ABI.
            if params.iter().any(|(_, typ)| {
                !internal_type(typ)
                    && (!matches!(typ.canonical(), Typ::Named(_))
                        || layout_words(
                            typ,
                            &structs,
                            &mut std::collections::HashSet::new(),
                            &mut std::collections::HashSet::new(),
                        )
                        .is_none())
            }) {
                return false;
            }
            if cfg!(target_arch = "x86_64") {
                // Each named argument is staged through the backend's scalar-field
                // pointer ABI, including receivers; unsupported fields stay compile-only.
                return params.len() <= 16
                    && params.iter().all(|(_, typ)| {
                        if internal_type(typ) {
                            return true;
                        }
                        let Typ::Named(name) = typ.canonical() else {
                            return false;
                        };
                        crate::native_emit::struct_layout::x86_fields(&return_structs, &name)
                            .is_ok_and(|fields| {
                                fields.len() <= usize::from(u16::MAX) / 8
                                    && fields.iter().all(|(_, field)| {
                                        (scalar(field) && field.canonical() != Typ::Void)
                                            || matches!(field.canonical(), Typ::Vector(_))
                                    })
                            })
                    });
            }
            if !cfg!(target_arch = "aarch64") {
                return false;
            }
            let words = params.iter().try_fold(0usize, |total, (name, typ)| {
                let layout = layout_words(
                    typ,
                    &structs,
                    &mut std::collections::HashSet::new(),
                    &mut std::collections::HashSet::new(),
                )?;
                let words = if (name == "self" && matches!(typ.canonical(), Typ::Named(_)))
                    || crate::core_ir::rust_references::aggregate_referent(typ).is_some()
                {
                    1
                } else {
                    layout
                };
                total.checked_add(words)
            });
            words.is_some_and(|words| words <= 32)
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
    fn scalar_struct_arguments_and_receivers_use_the_supported_host_abi() {
        for source in [
            "struct Pair{x:i64,y:i64} fn take(prefix:i64,p:Pair)->i64{prefix+p.x+p.y} fn main()->i64{take(1,if true{Pair{x:20,y:21}}else{Pair{x:0,y:0}})}",
            "struct Reader{value:i64} impl Reader{fn take(&self,items:Vec<i64>)->i64{if items.capacity()>=10{self.value}else{0}}} fn main()->i64{let reader=Reader{value:42};reader.take(Vec::with_capacity(10))}",
        ] {
            let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            assert_eq!(
                rust_entry_can_execute(&module, "main", "main"),
                cfg!(any(target_arch = "aarch64", target_arch = "x86_64")),
                "{source}"
            );
        }
    }

    #[test]
    fn aggregate_parameter_execution_requires_known_bounded_layouts() {
        use crate::core_ir::Typ;
        let source = "struct Bag { items: Vec<i64> } impl Bag { fn read(&self) -> i64 { return self.items[0]; } } fn main() -> i64 { let bag: Bag = Bag { items: vec![42] }; return Bag::read(&bag); }";
        let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        assert_eq!(
            rust_entry_can_execute(&module, "main", "main"),
            cfg!(any(target_arch = "aarch64", target_arch = "x86_64"))
        );
        for field in [
            Typ::Array(Box::new(Typ::Int)),
            Typ::Named("Opaque".into()),
            Typ::Named("Bag".into()),
            Typ::Generic("T".into()),
            Typ::Vector(Box::new(Typ::Named("Opaque".into()))),
            Typ::Void,
        ] {
            let mut changed = module.clone();
            for decl in &mut changed.decls {
                if let Decl::Struct { name, fields, .. } = decl
                    && name == "Bag"
                {
                    fields[0].1 = field.clone();
                }
            }
            assert!(
                !rust_entry_can_execute(&changed, "main", "main"),
                "{field:?}"
            );
        }
        let mut entry = module.clone();
        for decl in &mut entry.decls {
            if let Decl::Function { name, params, .. } = decl
                && name == "main"
            {
                params.push(("argument".into(), Typ::Int));
            }
        }
        assert!(!rust_entry_can_execute(&entry, "main", "main"));
        let mut wide = module.clone();
        for decl in &mut wide.decls {
            if let Decl::Function { name, params, .. } = decl
                && name == "Bag::read"
            {
                params.extend((0..32).map(|i| (format!("arg{i}"), Typ::Int)));
            }
        }
        assert!(!rust_entry_can_execute(&wide, "main", "main"));
        for decl in &mut wide.decls {
            if let Decl::Function { name, params, .. } = decl
                && name == "Bag::read"
            {
                params.pop();
            }
        }
        assert_eq!(
            rust_entry_can_execute(&wide, "main", "main"),
            cfg!(target_arch = "aarch64")
        );
    }

    #[test]
    fn rust_execution_keeps_entry_scalar_and_vector_elements_supported() {
        for (source, expected) in [
            (
                "fn work(items: Vec<i64>) -> i64 { items[0] = 20; return items[0]; } fn main() -> i64 { return work(vec![0]); }",
                true,
            ),
            (
                "fn work(mut items:[i64;2])->i64{items[0]=0;items[1]}fn main()->i64{work([20i64,22i64])}",
                true,
            ),
            ("fn main()->[i64;2]{[20i64,22i64]}", false),
            ("fn main() -> Vec<i64> { return vec![20]; }", false),
            (
                "struct Item { x: i64 } fn work(items: Vec<Item>) -> i64 { return 20; } fn main() -> i64 { return work(vec![Item { x: 20 }]); }",
                false,
            ),
        ] {
            let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            assert_eq!(
                rust_entry_can_execute(&module, "main", "main"),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn helper_struct_returns_require_known_bounded_scalar_layouts() {
        use crate::core_ir::Typ;
        let source = "struct Entry{value:i64} fn make()->Entry{Entry{value:42}} fn main()->i64{make().value}";
        let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        assert!(rust_entry_can_execute(&parsed, "main", "main"));
        let nested = crate::compiler::rust_front::parse_rust_source("struct Inner{value:i64} struct Entry{inner:Inner} fn make()->Entry{Entry{inner:Inner{value:42}}} fn main()->i64{make().inner.value}").unwrap();
        assert!(rust_entry_can_execute(&nested, "main", "main"));
        for unsupported in [
            Typ::Void,
            Typ::Array(Box::new(Typ::Int)),
            Typ::Named("Unknown".into()),
            Typ::Named("Entry".into()),
            Typ::Generic("T".into()),
        ] {
            let mut changed = parsed.clone();
            let Decl::Struct { fields, .. } = &mut changed.decls[0] else {
                unreachable!()
            };
            fields[0].1 = unsupported.clone();
            assert!(
                !rust_entry_can_execute(&changed, "main", "main"),
                "{unsupported:?}"
            );
        }
        for words in [8, 9] {
            let mut changed = parsed.clone();
            let Decl::Struct { fields, .. } = &mut changed.decls[0] else {
                unreachable!()
            };
            *fields = (0..words)
                .map(|index| (format!("field{index}"), Typ::Int))
                .collect();
            assert_eq!(
                rust_entry_can_execute(&changed, "main", "main"),
                words <= 8 || cfg!(target_arch = "x86_64"),
                "{words} scalar return words"
            );
        }
        let mut aggregate_entry = parsed.clone();
        let Decl::Function { ret, .. } = aggregate_entry
            .decls
            .iter_mut()
            .find(|decl| matches!(decl, Decl::Function { name, .. } if name == "main"))
            .unwrap()
        else {
            unreachable!()
        };
        *ret = Typ::Named("Entry".into());
        assert!(!rust_entry_can_execute(&aggregate_entry, "main", "main"));
    }

    #[test]
    fn jit_string_decode_rejects_invalid_references() {
        assert_eq!(decode_jit_string(0), Some(String::new()));
        assert_eq!(decode_jit_string(1), None);
        let oversized = u64::MAX;
        assert_eq!(decode_jit_string((&oversized as *const u64) as i64), None);
    }
}

#[cfg(test)]
mod imported_rust_validation_tests {
    use super::*;

    #[test]
    fn dependency_resolution_without_imports_preserves_caller_validation_policy() {
        let module = crate::compiler::rust_front::parse_rust_source(
            "#[cfg(feature=\"one\")] const VALUE:i64=1; #[cfg(not(feature=\"one\"))] const VALUE:i64=2; fn main()->i64{42}"
        ).unwrap();
        assert_eq!(
            module
                .decls
                .iter()
                .filter(|decl| matches!(decl, Decl::Global {name, ..} if name == "VALUE"))
                .count(),
            2
        );
        assert_eq!(
            resolve_module_dependencies(&module, Some("main"), super::super::CompileTarget::Jit)
                .unwrap(),
            module
        );
    }

    use crate::owned_compile::{CompileTarget, EvalValue, compile_owned};
    use crate::parser_registry::ParserCli;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture {
        vendor: std::path::PathBuf,
        caller: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.vendor);
            let _ = fs::remove_dir_all(&self.caller);
        }
    }

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    ))]
    fn cpp_callers_cannot_reinterpret_invalid_rust_dependency_values() {
        let id = format!(
            "codex_cross_front_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let vendor_parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor");
        fs::create_dir_all(&vendor_parent).unwrap();
        let vendor = vendor_parent.join(&id);
        let caller = std::env::temp_dir().join(&id);
        fs::create_dir(&vendor).unwrap();
        if let Err(error) = fs::create_dir(&caller) {
            fs::remove_dir_all(&vendor).unwrap();
            panic!("create caller fixture: {error}");
        }
        let fixture = Fixture { vendor, caller };
        fs::create_dir(fixture.vendor.join("src")).unwrap();
        fs::write(fixture.vendor.join("src/lib.rs"), "").unwrap();
        for (case, source, valid, promote_caller) in [
            (
                "bad_local",
                "pub fn answer()->i64{let bad:i64=false;bad+42}",
                false,
                false,
            ),
            (
                "bad_condition",
                "pub fn answer()->i64{if 1{42}else{0}}",
                false,
                false,
            ),
            (
                "complement",
                "pub fn answer()->i64{let value:i64=1;!value+44}",
                true,
                false,
            ),
            (
                "bool_not",
                "pub fn answer()->i64{let flag:bool=false;if !flag{42}else{0}}",
                true,
                false,
            ),
            ("caller_promotion", "pub fn answer()->i64{42}", true, true),
        ] {
            let source =
                format!("{source} pub fn unsupported_unused()->usize{{format!(\"unused\").len()}}");
            fs::write(fixture.vendor.join(format!("src/{case}.rs")), source).unwrap();
            let path = fixture.caller.join(format!("{case}.cpp"));
            let body = if promote_caller {
                format!("bool flag=false; long bias=flag; return {id}::{case}::answer()+bias;")
            } else {
                format!("return {id}::{case}::answer();")
            };
            fs::write(&path, format!("namespace {id} {{ namespace {case} {{ long answer(); }} }} int main() {{{body}}}")).unwrap();
            let report = compile_owned(&OwnedCompileRequest {
                path,
                module_id: "cross_front".into(),
                parser: ParserCli::Auto,
                target: CompileTarget::Jit,
                entry: Some("main".into()),
                out: None,
                linkage: NativeLinkage::Executable,
                target_triple: None,
                jobs: 1,
                debug: false,
                profile: crate::emit_profile::EmitProfile::Default,
                emit: None,
                base: None,
            });
            assert_eq!(report.success, valid, "{case}: {report:?}");
            if valid {
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{case}: {report:?}"
                );
            } else {
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("dependency-resolution-failed"),
                    "{case}: {report:?}"
                );
            }
        }
        let library = crate::compiler::rust_front::parse_rust_source(&format!(
            "pub fn exported()->i64{{{id}::bad_local::answer()}}"
        ))
        .unwrap();
        let constants = crate::compiler::rust_front::parse_rust_source(&format!(
            "const A:i64=1;const B:i64=2;const C:i64=3;fn main()->i64{{{id}::complement::answer()}}"
        ))
        .unwrap();
        let resolved =
            resolve_module_dependencies(&constants, Some("main"), CompileTarget::Jit).unwrap();
        assert_eq!(
            &resolved.decls[..constants.decls.len()],
            constants.decls.as_slice()
        );
        assert!(resolved.decls.iter().any(|decl| matches!(decl,
            Decl::Function { name, .. } if name == &format!("{id}::complement::answer"))));
        assert!(
            resolve_module_dependencies(&library, None, super::super::CompileTarget::Jit)
                .err()
                .unwrap()
                .contains("Rust dependency validation failed")
        );
        for (name, requested) in [("namespace.entry", "entry"), ("fallback", "missing")] {
            let mut selected = library.clone();
            let Decl::Function { name: actual, .. } = &mut selected.decls[0] else {
                unreachable!()
            };
            *actual = name.into();
            assert!(
                resolve_module_dependencies(
                    &selected,
                    Some(requested),
                    super::super::CompileTarget::Jit
                )
                .err()
                .unwrap()
                .contains("Rust dependency validation failed"),
                "{name}"
            );
            // Native explicit missing entries must not acquire JIT's first-function fallback.
            assert!(
                resolve_module_dependencies(
                    &selected,
                    Some(requested),
                    super::super::CompileTarget::Native
                )
                .is_ok(),
                "{name}"
            );
        }
    }
}
