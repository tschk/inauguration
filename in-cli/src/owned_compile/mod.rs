use crate::compile_cache;
use crate::core_ir::{Decl, ModuleIdentityReport};
use crate::core_ir_verifier;
use crate::in_lang_parse;

use crate::emit_profile::EmitProfile;
use crate::external_guard::ExternalInvocationGuard;
use crate::native_backend;
use crate::native_emit::NativeLinkage;
use crate::native_emit::lower::LoweringDegradation;
use crate::parser_registry::{self, ParserCli};
use serde::Serialize;

use std::path::{Path, PathBuf};
use std::time::Instant;

mod jit;
mod metadata;
mod native;
mod report;
mod util;

#[cfg(test)]
mod tests;

pub use jit::resolve_jit_entry;
pub use report::report_to_json;
use report::{
    base_report, count_call_edges, count_functions, finalize_report, jobs_for_request,
    module_has_function, timing_waves_for_jobs,
};

use jit::compile_jit_with_execution_state;
pub(crate) use native::{
    compile_native, pure_entry_exit_code, pure_entry_scalar_value, set_native_artifact_permissions,
};
use util::{default_linkage_label, emit_label, linkage_label, target_label};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CompileTarget {
    Native,
    /// Native lowering + in-memory JIT execution (no object file on disk)
    Jit,
}

/// Optional emit mode that changes the artifact produced by `compile_owned`.
#[derive(Debug, Clone)]
pub enum OwnedEmit {
    /// Emit a raw SCI component binary loaded at the given virtual base address.
    Sci { base: u64 },
}

/// Value an executed entry point produced, in the shape the source declared.
///
/// An entry can return a float, which arrives in the floating-point result
/// register rather than the integer one, and a bool, which is a distinct type
/// even though it shares a register with an integer. Reporting every result as
/// an integer meant a `-> Bool` function was reported as `Int(1)` and a
/// `-> Float` function as `Int(0)`.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub enum EvalValue {
    Int(i64),
    Bool(bool),
    Float(f64),
    Str(String),
}

impl EvalValue {
    /// Integer form for legacy consumers. Non-integer values have no integer form.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            EvalValue::Int(value) => Some(*value),
            EvalValue::Bool(value) => Some(i64::from(*value)),
            EvalValue::Float(_) | EvalValue::Str(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OwnedCompileRequest {
    pub path: PathBuf,
    pub module_id: String,
    pub parser: ParserCli,
    pub target: CompileTarget,
    pub entry: Option<String>,
    pub out: Option<PathBuf>,
    pub linkage: NativeLinkage,
    pub target_triple: Option<String>,
    pub jobs: usize,
    pub debug: bool,
    pub profile: EmitProfile,
    pub emit: Option<OwnedEmit>,
    /// Optional load address for static-lib / freestanding objects that need
    /// position-dependent code/data layouts.
    pub base: Option<u64>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct OwnedCompileReport {
    pub schema_version: u32,
    pub owned: bool,
    pub path: String,
    pub module_id: String,
    #[serde(default)]
    pub package_name: Option<String>,
    pub module_identity: Option<ModuleIdentityReport>,
    pub target: String,
    #[serde(default)]
    pub target_triple: Option<String>,
    pub entry: Option<String>,
    #[serde(default = "default_linkage_label")]
    pub linkage: String,
    pub frontend_level: String,
    pub semantic_level: String,
    pub backend_level: String,
    pub runtime_level: String,
    pub external_invocations: Vec<String>,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
    pub success: bool,
    pub artifact_path: Option<String>,
    pub executable_path: Option<String>,
    pub abi_path: Option<String>,
    /// Functions the lowerer replaced with a runtime trap. Empty means the
    /// artifact contains no substituted code. Check this rather than `success`
    /// when a build must be fully compiled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degradations: Vec<LoweringDegradation>,
    pub parsed_function_count: usize,
    pub typed_function_count: usize,
    pub call_edge_count: usize,
    pub jobs: usize,
    pub timing_micros: u128,
    pub timing_waves_us: Option<Vec<u128>>,
    pub cache_hit: bool,
    pub frontend_hash: Option<String>,
    pub eval_exit_code: Option<u8>,
    pub eval_result: Option<EvalValue>,
    /// True once hosted entry invocation starts, including runtime failures.
    pub execution_started: bool,
    pub error: Option<String>,
}

pub fn compile_owned(request: &OwnedCompileRequest) -> OwnedCompileReport {
    let _human_debug_guard = crate::in_lang_parse::lexer::HumanInDebugGuard::new(request.debug);
    let started = Instant::now();
    let jobs = jobs_for_request(request);
    let cwd = compile_cache::workspace_cwd_for_path(&request.path);

    let (is_multi, multi_sources, source) = match discover_sources(request) {
        Ok(res) => res,
        Err((code, msg)) => {
            return OwnedCompileReport {
                reason_code: Some(code),
                reason: Some(msg.clone()),
                error: Some(msg),
                ..base_report(request, jobs, started)
            };
        }
    };
    let mut frontend_hash = compile_cache::source_frontend_hash(&request.path, &source);
    let reuse_cache =
        request.target != CompileTarget::Jit && request.profile == EmitProfile::Default;
    let _guard = ExternalInvocationGuard::enter();

    let mut report = base_report(request, jobs, started);
    report.timing_micros = 0;
    report.frontend_hash = Some(frontend_hash.clone());

    let primary_path = if is_multi {
        std::path::Path::new(&multi_sources[0].0)
    } else {
        &request.path
    };
    let resolved = parser_registry::resolve_parser_id(primary_path, request.parser);
    let mut unary_methods = crate::boundary_ir::UnaryMethodBindings::new();
    let primary_artifact = if is_multi
        && matches!(
            resolved,
            parser_registry::ResolvedBuildParser::CoreIr(parser_registry::ParserId::Rust)
        ) {
        crate::compiler::rust_front::parse_rust_module_artifact(primary_path)
            .map(Some)
            .map_err(parser_registry::ParserRegistryError::Msg)
    } else {
        parser_registry::parse_artifact_with_resolved(resolved, primary_path)
    };
    let mut module = match primary_artifact {
        Ok(Some(artifact)) => {
            unary_methods = artifact.unary_methods;
            artifact.semantic
        }
        Ok(None) => {
            let reason = "owned compile requires a Core IR frontend. All languages now route through Core IR via Tree-sitter.".to_string();
            report.reason_code = Some("frontend-parse-failed".to_string());
            report.reason = Some(reason.clone());
            report.error = Some(reason);
            return finalize_report(report, started, &cwd, &frontend_hash);
        }
        Err(err) => {
            // For freestanding targets (e.g. x86_64-unknown-none), retry without requiring `fn main`.
            // Component declarations don't have a `main` entry point.
            let err_str = err.to_string();
            if err_str.contains("missing required `fn main`")
                && primary_path.extension().is_some_and(|e| e == "in")
                && (request.linkage == NativeLinkage::StaticLib
                    || request
                        .target_triple
                        .as_deref()
                        .is_some_and(|t| t.ends_with("-none")))
            {
                match in_lang_parse::parse_in_library_file(&request.path) {
                    Ok(module) => module,
                    Err(lib_err) => {
                        let reason = lib_err;
                        report.reason_code = Some("frontend-parse-failed".to_string());
                        report.reason = Some(reason.clone());
                        report.error = Some(reason);
                        return finalize_report(report, started, &cwd, &frontend_hash);
                    }
                }
            } else {
                let reason = err_str;
                report.reason_code = Some("frontend-parse-failed".to_string());
                report.reason = Some(reason.clone());
                report.error = Some(reason);
                return finalize_report(report, started, &cwd, &frontend_hash);
            }
        }
    };

    // Resolve multi-file imports for .in sources
    let mut pkg_entry: Option<String> = None;
    if primary_path.extension().is_some_and(|e| e == "in") {
        let source_dir = primary_path.parent().map(PathBuf::from).unwrap_or_default();
        let mut import_resolver = crate::module_resolver::ModuleResolver::new();
        import_resolver.add_search_path(source_dir.clone());
        import_resolver.add_search_path(PathBuf::from("."));

        // Check for package manifest to set name and dependency search paths
        if let Some(pkg) = crate::package_manifest::compile_context_for_source(&request.path) {
            report.package_name = Some(pkg.name);
            pkg_entry = pkg.entry;
            for dep in pkg.dependency_search_paths {
                import_resolver.add_search_path(dep);
            }
        }

        match import_resolver.resolve_imports(&source) {
            Ok(imported) => {
                for imp in imported {
                    module.decls.extend(imp.decls);
                }
                // Re-inline constants after merging imported modules.
                // Constants from imported files (e.g. FB_COLOR_WHITE in fb.in)
                // are not inlined into functions in the importing file (e.g.
                // compositor.in) during per-file parsing, because the constant
                // decls are only merged here. Without this re-inline pass, those
                // Expr::Ident references are treated as uninitialized global
                // variables by the native lowering, producing 0 instead of the
                // constant value.
                crate::in_lang_parse::inline_const_values(&mut module);
            }
            Err(e) => {
                if request.debug {
                    eprintln!("[import] warning: {e}");
                }
                report.reason_code = Some("import-resolution-failed".to_string());
                report.reason = Some(e.clone());
                report.error = Some(e);
            }
        }
    }

    // ponytail: link cargo dependencies + crate root for Rust sources
    let is_rust_source = primary_path.extension().is_some_and(|e| e == "rs");
    if is_rust_source {
        let project_dir = crate::cargo_linker::find_crate_dir(primary_path).unwrap_or_else(|| {
            primary_path
                .parent()
                .unwrap_or(Path::new("."))
                .to_path_buf()
        });
        let skip_deps = std::env::var_os("IN_CARGO_LINK")
            .map(|v| v == "0")
            .unwrap_or(false);
        let deps = if skip_deps {
            Vec::new()
        } else {
            std::thread::Builder::new()
                .name("in-cargo-link".into())
                .stack_size(64 * 1024 * 1024)
                .spawn({
                    let project_dir = project_dir.clone();
                    move || crate::cargo_linker::compile_cargo_dependency_artifacts(&project_dir)
                })
                .ok()
                .and_then(|h| h.join().ok())
                .unwrap_or_default()
        };
        if request.debug {
            eprintln!("[cargo-link] {} dependency crates", deps.len());
        }
        if !deps.is_empty() {
            if let Err(error) =
                merge_retained_dependency_artifacts(&mut module, &mut unary_methods, deps)
            {
                report.reason_code = Some("dependency-resolution-failed".into());
                report.reason = Some(error.clone());
                report.error = Some(error);
                return finalize_report(report, started, &cwd, &frontend_hash);
            }
        }
        // Also compile the crate's library root (lib.rs) to make crate-local
        // functions available (e.g. inauguration::agent_mode::analyze_path)
        let crate_root = crate::cargo_linker::find_crate_root(&project_dir);
        if let Ok(ref root) = crate_root {
            // Directory inputs already schedule each listed source for its normal
            // parse stage. Do not reload the exact same crate root as a dependency.
            let canonical_root = root.canonicalize().ok();
            let root_in_inputs = std::iter::once(primary_path)
                .chain(multi_sources.iter().map(|(path, _)| Path::new(path)))
                .any(|source| {
                    source == root
                        || canonical_root
                            .as_ref()
                            .is_some_and(|root| source.canonicalize().ok().as_ref() == Some(root))
                });
            if !root_in_inputs && request.debug {
                eprintln!("[cargo-link] crate root {}", root.display());
            }
            if !root_in_inputs {
                let root_path = root.clone();
                let parsed = std::thread::Builder::new()
                    .name("in-crate-root".into())
                    .stack_size(32 * 1024 * 1024)
                    .spawn(move || crate::compiler::rust_front::parse_rust_artifact(&root_path))
                    .and_then(|h| h.join().map_err(|_| std::io::Error::other("join")))
                    .ok();
                match parsed {
                    Some(Ok(crate_module)) => {
                        let crate_name = crate::cargo_linker::crate_package_name(&project_dir)
                            .or_else(|| {
                                project_dir
                                    .file_stem()
                                    .map(|s| s.to_string_lossy().into_owned())
                            })
                            .unwrap_or_default();
                        if let Err(error) = merge_retained_dependency_artifacts(
                            &mut module,
                            &mut unary_methods,
                            vec![(crate_name, crate_module)],
                        ) {
                            report.reason_code = Some("dependency-resolution-failed".into());
                            report.reason = Some(error.clone());
                            report.error = Some(error);
                            return finalize_report(report, started, &cwd, &frontend_hash);
                        }
                    }
                    Some(Err(err)) => {
                        if request.debug {
                            eprintln!("[cargo-link] crate root parse failed: {err}");
                        }
                    }
                    None => {
                        if request.debug {
                            eprintln!("[cargo-link] crate root parse panicked (skipped)");
                        }
                    }
                }
            }
        }
    }

    // Multi-file project: parse and merge all additional source files
    if is_multi && multi_sources.len() > 1 {
        let has_c_src = multi_sources
            .iter()
            .any(|(p, _)| p.ends_with(".c") || p.ends_with(".cpp"));
        for (source_path, _source_content) in &multi_sources[1..] {
            let path = std::path::Path::new(source_path);
            let resolved = parser_registry::resolve_parser_id(path, request.parser);
            let artifact = if matches!(
                resolved,
                parser_registry::ResolvedBuildParser::CoreIr(parser_registry::ParserId::Rust)
            ) {
                crate::compiler::rust_front::parse_rust_module_artifact(path)
                    .map(Some)
                    .map_err(parser_registry::ParserRegistryError::Msg)
            } else {
                parser_registry::parse_artifact_with_resolved(resolved, path)
            };
            if let Some(extra) = match artifact {
                Ok(artifact) => artifact,
                Err(error) => {
                    let error = error.to_string();
                    report.reason_code = Some("dependency-resolution-failed".into());
                    report.reason = Some(error.clone());
                    report.error = Some(error);
                    return finalize_report(report, started, &cwd, &frontend_hash);
                }
            } {
                // Empty crate name preserves direct-append naming, while keeping
                // the same receiver/callable-origin proof checks as Cargo merges.
                if let Err(error) = merge_retained_dependency_artifacts(
                    &mut module,
                    &mut unary_methods,
                    vec![(String::new(), extra)],
                ) {
                    report.reason_code = Some("dependency-resolution-failed".into());
                    report.reason = Some(error.clone());
                    report.error = Some(error);
                    return finalize_report(report, started, &cwd, &frontend_hash);
                }
            }
        }
        // When C/C++ sources are present: strip empty externs and
        // dedup. Keep only .in's main and C's function bodies.
        if has_c_src {
            let mut c_names: std::collections::HashSet<String> = std::collections::HashSet::new();
            for decl in &module.decls {
                if let Decl::Function { name, body, .. } = decl {
                    if !body.is_empty() && name != "main" {
                        c_names.insert(name.clone());
                    }
                }
            }
            let mut kept_main = false;
            module.decls.retain(|decl| {
                match decl {
                    Decl::Function { name, body, .. } => {
                        // Remove empty externs (C provides bodies, tracked by name)
                        if body.is_empty() && c_names.contains(name) {
                            return false;
                        }
                        // Remove duplicate main and synthetic C entry wrappers
                        if name == "main" {
                            if kept_main {
                                return false;
                            }
                            kept_main = true;
                        }
                        true
                    }
                    _ => true,
                }
            });
        }
    }

    if crate::dep_resolver::requires_source_resolution(&module) {
        let dependency_entry = request
            .entry
            .as_deref()
            .or(pkg_entry.as_deref())
            .or_else(|| {
                ((request.target == CompileTarget::Jit
                    || request.linkage == NativeLinkage::Executable)
                    && module_has_function(&module, "main"))
                .then_some("main")
            });
        match jit::resolve_module_dependencies_with_bindings(
            &module,
            &unary_methods,
            dependency_entry,
            request.target,
        ) {
            Ok(expanded) => {
                module = expanded.semantic;
                unary_methods = expanded.unary_methods;
            }
            Err(error) => {
                report.reason_code = Some("dependency-resolution-failed".into());
                report.reason = Some(error.clone());
                report.error = Some(error);
                return finalize_report(report, started, &cwd, &frontend_hash);
            }
        }
    }

    let integer_semantics = if request.target == CompileTarget::Jit {
        crate::integer_semantics::IntegerSemantics::Word64
    } else {
        crate::target::integer_semantics(request.target_triple.as_deref())
    };
    // Desugaring and optimization must evaluate constants in the same target word.
    crate::lower_core::desugar_module_with_semantics(&mut module, integer_semantics);
    if let parser_registry::ResolvedBuildParser::CoreIr(parser_id) = resolved
        && crate::typecheck::uses_family_normalization(parser_id)
    {
        module = crate::typecheck::normalize_module_with_semantics(
            parser_id,
            &module,
            integer_semantics,
        );
    }

    if let Err(error) =
        crate::typecheck::TypeChecker::new().resolve_unary_methods(&mut module, &unary_methods)
    {
        report.reason_code = Some("rust-unary-resolution-failed".into());
        report.reason = Some(error.clone());
        report.error = Some(error);
        return finalize_report(report, started, &cwd, &frontend_hash);
    }

    let proof_entry = request
        .entry
        .as_deref()
        .or(pkg_entry.as_deref())
        .or_else(|| module_has_function(&module, "main").then_some("main"));
    let proof_entry = proof_entry.map(|entry| jit::resolve_jit_entry(&module, entry));
    let proof_roots = if request.linkage == crate::native_emit::NativeLinkage::StaticLib {
        None
    } else {
        proof_entry.as_deref()
    };
    if let Err(error) = reject_invalid_type_proofs(&module) {
        report.reason_code = Some("rust-invalid-associated-type".into());
        report.reason = Some(error.clone());
        report.error = Some(error);
        return finalize_report(report, started, &cwd, &frontend_hash);
    }
    if let Err(error) = reject_uninitialized_locals(&module, proof_roots) {
        report.reason_code = Some("uninitialized-local".into());
        report.reason = Some(error.clone());
        report.error = Some(error);
        return finalize_report(report, started, &cwd, &frontend_hash);
    }
    if request.target != CompileTarget::Jit {
        let context = format!(
            "{frontend_hash}:{:?}:{:?}:{:?}",
            request.profile, request.emit, request.base
        );
        frontend_hash = compile_cache::resolved_artifact_hash(&context, &module, &unary_methods);
        report.frontend_hash = Some(frontend_hash.clone());
    }

    report.frontend_level = "core-ir-direct".to_string();
    report.module_identity = Some(module.identity_report(&request.module_id));
    report.parsed_function_count = count_functions(&module);

    // Validate invocable Rust entries before optimizations can erase invalid conditions.
    // Complex Rust programs keep their compile-only path; unrelated unsupported functions
    // do not prevent verification of a supported entry's reachable functions.
    let effective_entry = request.entry.clone().or(pkg_entry);
    let requested_entry = effective_entry.as_deref().unwrap_or("main");
    let rust_entry = if is_rust_source && matches!(request.target, CompileTarget::Jit) {
        let entry = jit::resolve_jit_entry(&module, requested_entry);
        jit::rust_entry_can_execute(&module, &entry, requested_entry).then_some(entry)
    } else {
        None
    };
    let skip_verify = is_rust_source || crate::config::env_config().skip_verify;
    if !skip_verify || rust_entry.is_some() {
        let reachable;
        let (verified_module, entry) = if let Some(entry) = rust_entry {
            reachable = native::native_entry_module(&module, &entry);
            (&reachable, Some(entry))
        } else {
            (&module, effective_entry.clone())
        };
        let verify_opts = core_ir_verifier::VerifyOptions {
            require_entry: entry.is_some(),
            entry,
        };
        let verify_report = core_ir_verifier::verify_module(verified_module, &verify_opts);
        if !verify_report.ok {
            report.reason_code = Some(format!(
                "verify-{}",
                verify_report.reason_code.as_deref().unwrap_or("failed")
            ));
            report.reason = verify_report.reason.clone();
            report.error = verify_report.reason;
            report.call_edge_count = verify_report.call_edges.len();
            return finalize_report(report, started, &cwd, &frontend_hash);
        }
        report.call_edge_count = verify_report.call_edges.len();
    }

    if reuse_cache {
        if let Some(cached) = check_and_return_cache(request, &cwd, &frontend_hash, jobs, started) {
            return cached;
        }
    }

    report.semantic_level = "typed-subset".to_string();
    report.typed_function_count = report.parsed_function_count;
    report.call_edge_count = count_call_edges(&module, &request.module_id);

    // Profile-aware IR optimize before lowering (default/lean/harden).
    // Static-lib objects keep every function: assembly capsules and sibling
    // objects resolve them by symbol (e.g. IRQ/SysTick handler entry points).
    // SCI images keep every function too: an SCI is a dynamic-linking
    // surface whose export table must expose all defined symbols, not just
    // those reachable from the entry (e.g. a shared library whose entry is
    // never invoked by the loader).
    {
        // `remove_dead_functions` drops everything not reachable from the entry,
        // and falls back to a kernel entry name when it is not told one. A bare
        // source file with a `main` must not be stripped to nothing, so name the
        // module's own entry when the caller did not.
        let entry = effective_entry
            .clone()
            .or_else(|| module_has_function(&module, "main").then(|| "main".to_string()));
        // Resolve only an existing optimizer root. Entry-less libraries retain
        // their keep-all discovery policy below.
        let entry = entry.map(|entry| {
            if request.target == CompileTarget::Jit {
                jit::resolve_jit_entry(&module, &entry)
            } else {
                entry
            }
        });
        let keep_all = request.linkage == crate::native_emit::NativeLinkage::StaticLib
            || matches!(request.emit, Some(OwnedEmit::Sci { .. }))
            || (matches!(request.target, CompileTarget::Jit) && entry.is_none());
        if let Err(error) = optimize_retained_module(
            &mut module,
            &unary_methods,
            entry.as_deref(),
            request.profile,
            keep_all,
            integer_semantics,
        ) {
            report.reason_code = Some("rust-unary-resolution-failed".into());
            report.reason = Some(error.clone());
            report.error = Some(error);
            return finalize_report(report, started, &cwd, &frontend_hash);
        }
        report.typed_function_count = count_functions(&module);
        report.call_edge_count = count_call_edges(&module, &request.module_id);
    }

    if let Some(OwnedEmit::Sci { base }) = request.emit {
        let entry_name = effective_entry.as_deref().unwrap_or("main");
        return emit_sci_binary_if_requested(
            request,
            base,
            &module,
            entry_name,
            started,
            &cwd,
            &frontend_hash,
            report,
        );
    }

    match request.target {
        CompileTarget::Native => match compile_native(&module, &request.module_id, request) {
            Ok(native_result) => {
                report.backend_level = native_result.backend_level;
                report.runtime_level = native_result.runtime_level;
                report.reason_code = Some(native_result.reason_code.to_string());
                report.reason = Some(native_result.reason.to_string());
                report.success = true;
                report.eval_exit_code = native_result.eval_exit_code;
                report.eval_result = native_result.eval_result;
                report.executable_path = if request.linkage == NativeLinkage::Executable {
                    Some(native_result.artifact_path.clone())
                } else {
                    None
                };
                report.artifact_path = Some(native_result.artifact_path);
                report.abi_path = native_result.abi_path;
                report.degradations = native_result.degradations;
            }
            Err(err) if err == "native-host-unsupported" => {
                let status = native_backend::native_backend_status();
                report.backend_level = "contract-only".to_string();
                report.runtime_level = "none".to_string();
                report.reason_code = Some(status.reason_code.to_string());
                report.reason = Some(status.reason.to_string());
            }
            Err(err) if err.starts_with("native-target-not-implemented:") => {
                report.backend_level = "contract-only".to_string();
                report.runtime_level = "none".to_string();
                report.reason_code = Some("native-target-not-implemented".to_string());
                report.reason = Some(err.clone());
                report.error = Some(err);
            }
            Err(err) if err.starts_with("native-package-not-implemented:") => {
                report.backend_level = "contract-only".to_string();
                report.runtime_level = "none".to_string();
                report.reason_code = Some("native-package-not-implemented".to_string());
                report.reason = Some(err.clone());
                report.error = Some(err);
            }
            Err(err) => {
                report.backend_level = "owned-native-subset".to_string();
                report.runtime_level = "inrt-native".to_string();
                report.reason_code = Some("native-lowering-failed".to_string());
                report.reason = Some(err.clone());
                report.error = Some(err);
            }
        },
        CompileTarget::Jit => {
            let jit_start = Instant::now();
            let jit_outcome = compile_jit_with_execution_state(
                &module,
                &request.module_id,
                request,
                &mut report.execution_started,
            );
            let jit_us = jit_start.elapsed().as_micros();
            if request.debug {
                eprintln!("[jit] compile took {jit_us} µs");
            }
            match jit_outcome {
                Ok(jit_result) => {
                    report.backend_level = jit_result.backend_level;
                    report.runtime_level = jit_result.runtime_level;
                    report.reason_code = Some(jit_result.reason_code.to_string());
                    report.reason = Some(jit_result.reason.to_string());
                    report.success = true;
                    report.eval_exit_code = jit_result.eval_exit_code;
                    report.eval_result = jit_result.eval_result;
                    report.degradations = jit_result.degradations;
                }
                Err(err) => {
                    report.backend_level = "owned-native-subset".to_string();
                    report.runtime_level = "inrt-jit".to_string();
                    report.reason_code = Some("jit-failed".to_string());
                    report.reason = Some(err.clone());
                    report.error = Some(err);
                }
            }
        }
    }

    check_and_report_degradations(&mut report);

    finalize_report(report, started, &cwd, &frontend_hash)
}

fn discover_sources(
    request: &OwnedCompileRequest,
) -> Result<(bool, Vec<(String, String)>, String), (String, String)> {
    let has_cargo = request.path.join("Cargo.toml").exists();
    let is_multi = request.path.is_dir() && !has_cargo;

    let mut multi_sources: Vec<(String, String)> = Vec::new(); // (path, source)
    if is_multi {
        // Scan for .in, .c, .cpp, .rs files in directory
        if let Ok(dir) = std::fs::read_dir(&request.path) {
            for entry in dir.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext == "in" || ext == "c" || ext == "cpp" || ext == "rs" {
                            if let Ok(src) = std::fs::read_to_string(&path) {
                                multi_sources.push((path.to_string_lossy().to_string(), src));
                            }
                        }
                    }
                }
            }
        }
        if multi_sources.is_empty() {
            return Err((
                "frontend-parse-failed".to_string(),
                format!("no source files found in {}", request.path.display()),
            ));
        }
    }

    let source = if is_multi {
        multi_sources[0].1.clone()
    } else {
        match std::fs::read_to_string(&request.path) {
            Ok(content) => content,
            Err(err) => {
                return Err(("frontend-read-failed".to_string(), err.to_string()));
            }
        }
    };

    Ok((is_multi, multi_sources, source))
}

fn check_and_return_cache(
    request: &OwnedCompileRequest,
    cwd: &std::path::Path,
    frontend_hash: &str,
    jobs: usize,
    started: Instant,
) -> Option<OwnedCompileReport> {
    let mut cached = crate::compile_cache::read_cached_report(cwd, frontend_hash)?;
    let requested_out = request.out.as_ref().map(|path| path.display().to_string());
    let cached_out = cached
        .executable_path
        .clone()
        .or_else(|| cached.artifact_path.clone());
    if cached.target == target_label(request.target)
        && cached.entry == request.entry
        && cached.target_triple == request.target_triple
        && cached.module_id == request.module_id
        && cached.linkage == linkage_label(request.linkage)
        && emit_label(request.emit.as_ref()) == emit_label(None)
        && requested_out == cached_out
    {
        // Report-only cache must still materialize artifacts; missing out files
        // force a full rebuild so product link steps don't see a green phantom.
        let artifact_present = requested_out
            .as_ref()
            .map(|p| std::path::Path::new(p).is_file())
            .unwrap_or(true);
        if artifact_present {
            cached.cache_hit = true;
            cached.jobs = jobs;
            cached.timing_micros = started.elapsed().as_micros();
            cached.timing_waves_us = Some(timing_waves_for_jobs(jobs, cached.timing_micros));
            cached.frontend_hash = Some(frontend_hash.to_string());
            return Some(cached);
        }
    }
    None
}

fn emit_sci_binary_if_requested(
    request: &OwnedCompileRequest,
    base: u64,
    module: &crate::core_ir::UnifiedModule,
    entry_name: &str,
    started: Instant,
    cwd: &std::path::Path,
    frontend_hash: &str,
    mut report: OwnedCompileReport,
) -> OwnedCompileReport {
    let triple = request
        .target_triple
        .as_deref()
        .unwrap_or("x86_64-unknown-none");
    if !triple.contains("x86_64") {
        let err = format!("SCI emit requires an x86_64 target triple, got `{triple}`");
        report.reason_code = Some("sci-target-unsupported".to_string());
        report.reason = Some(err.clone());
        report.error = Some(err);
        return finalize_report(report, started, cwd, frontend_hash);
    }
    let out_path = match request.out.as_ref() {
        Some(path) => path,
        None => {
            let err = "SCI emit requires --out".to_string();
            report.reason_code = Some("sci-missing-output".to_string());
            report.reason = Some(err.clone());
            report.error = Some(err);
            return finalize_report(report, started, cwd, frontend_hash);
        }
    };
    crate::native_emit::x86_64::set_32bit(false);
    match crate::native_emit::sci::emit_sci_binary(module, entry_name, base) {
        Ok(bytes) => {
            if let Err(err) = std::fs::write(out_path, &bytes) {
                let err = format!("write SCI binary `{}`: {err}", out_path.display());
                report.reason_code = Some("sci-write-failed".to_string());
                report.reason = Some(err.clone());
                report.error = Some(err);
                return finalize_report(report, started, cwd, frontend_hash);
            }
            metadata::emit_component_metadata_sidecar(module, entry_name, out_path);
            report.success = true;
            report.backend_level = "owned-native-subset-freestanding".to_string();
            report.runtime_level = "sci-component".to_string();
            report.reason_code = Some("native-x86_64-sci-binary".to_string());
            report.reason = Some(
                "inauguration owns SCI component binary emission for freestanding x86_64"
                    .to_string(),
            );
            report.artifact_path = Some(out_path.display().to_string());
            if cfg!(unix) {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(out_path, std::fs::Permissions::from_mode(0o755));
            }
        }
        Err(err) => {
            report.reason_code = Some("sci-lowering-failed".to_string());
            report.reason = Some(err.clone());
            report.error = Some(err);
        }
    }
    finalize_report(report, started, cwd, frontend_hash)
}

fn check_and_report_degradations(report: &mut OwnedCompileReport) {
    // Code the lowerer could not compile is trapped rather than miscompiled, so
    // the artifact is still produced. Callers that require a fully compiled
    // artifact opt in with `IN_STRICT_LOWERING`.
    if !report.degradations.is_empty() {
        let live = report
            .degradations
            .iter()
            .filter(|degradation| degradation.reachable)
            .count();
        let total = report.degradations.len();
        if live == 0 {
            eprintln!(
                "in: {total} function(s) were not compiled to native code (none reachable from the entry)"
            );
        } else {
            eprintln!(
                "in: {total} function(s) were not compiled to native code ({live} reachable from the entry); reaching one aborts the program with exit {}",
                crate::inrt::INRT_TRAP_EXIT_CODE
            );
        }
        for degradation in &report.degradations {
            let marker = if degradation.reachable {
                " [reachable]"
            } else {
                ""
            };
            eprintln!("in: {}{marker}", degradation.describe());
        }
    }
    if report.success
        && !report.degradations.is_empty()
        && crate::config::env_config().strict_lowering
    {
        let detail = report
            .degradations
            .iter()
            .map(LoweringDegradation::describe)
            .collect::<Vec<_>>()
            .join("; ");
        report.success = false;
        report.reason_code = Some("native-lowering-degraded".to_string());
        report.reason = Some(format!(
            "{} function(s) were not compiled to native code: {detail}",
            report.degradations.len()
        ));
        report.error = report.reason.clone();
    }
}

fn merge_retained_dependency_artifacts(
    module: &mut crate::core_ir::UnifiedModule,
    bindings: &mut crate::boundary_ir::UnaryMethodBindings,
    dependencies: Vec<(String, crate::boundary_ir::CompileArtifact)>,
) -> Result<(), String> {
    let mut artifact = crate::boundary_ir::CompileArtifact::from_semantic(std::mem::replace(
        module,
        crate::core_ir::UnifiedModule::new(Vec::new()),
    ));
    artifact.unary_methods = std::mem::take(bindings);
    let result = crate::cargo_linker::merge_dependency_artifacts(&mut artifact, dependencies);
    *module = artifact.semantic;
    *bindings = artifact.unary_methods;
    result
}

/// Shared owned/coverage lifecycle: source proof survives substitution, resolves
/// before folding/DCE, then erases to the preexisting native representation.
/// Only callable roots poison an executable; unrelated library declarations
/// remain eligible for ordinary DCE. native_entry_module retains schemas, so
/// inspect function proof results rather than blanket-scanning that view.
fn reject_uninitialized_locals(
    module: &crate::core_ir::UnifiedModule,
    entry: Option<&str>,
) -> Result<(), String> {
    let Some(entry) = entry else {
        return crate::core_ir_verifier::check_definite_initialization(module);
    };
    // Keep ordinary modules on the allocation-free presence path.
    if crate::core_ir_verifier::initialization_errors_by_function(&module.decls).is_empty() {
        return Ok(());
    }
    let reachable = native::native_entry_module(module, entry);
    let errors = crate::core_ir_verifier::initialization_errors_by_function(&reachable.decls);
    match errors.iter().min_by_key(|(name, _)| *name) {
        Some((_, reason)) => Err(reason.clone()),
        None => Ok(()),
    }
}

pub(crate) fn reject_invalid_type_proofs(
    module: &crate::core_ir::UnifiedModule,
) -> Result<(), String> {
    if !crate::core_ir::type_proofs::present(&module.decls) {
        return Ok(());
    }
    // Definite source errors remain invalid even when entry pruning removes their owner.
    let proofs = crate::typecheck::TypeChecker::new().typed_proof_nodes(&module.decls);
    let mut invalid: Vec<_> = proofs
        .into_iter()
        .filter(|(_, proof)| proof.is_invalid())
        .collect();
    invalid.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some((name, proof)) = invalid.first() {
        Err(format!(
            "invalid Rust associated type in `{name}`: {}",
            proof.message()
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn optimize_retained_module(
    module: &mut crate::core_ir::UnifiedModule,
    bindings: &crate::boundary_ir::UnaryMethodBindings,
    entry: Option<&str>,
    profile: crate::emit_profile::EmitProfile,
    keep_all: bool,
    semantics: crate::integer_semantics::IntegerSemantics,
) -> Result<(), String> {
    reject_invalid_type_proofs(module)?;
    reject_uninitialized_locals(module, if keep_all { None } else { entry })?;
    fn count_tags(decls: &mut [Decl]) -> (usize, bool) {
        let mut tags = 0;
        let mut reference_values = false;
        crate::core_opt::map_decl_exprs_mut(decls, &mut |expr| {
            if let crate::core_ir::Expr::Unary { op, .. } = expr {
                tags += usize::from(op == crate::core_ir::rust_references::NEGATE_VALUE);
                reference_values |= crate::core_ir::rust_references::value_kind(op).is_some();
            }
        });
        (tags, reference_values)
    }
    let (initial_tags, reference_values) = count_tags(&mut module.decls);
    // The existing BORROW place preparation has already desugared. Remove its
    // value-level Call before the inliner classifies pure borrowed arguments;
    // the outer producer still retains exact shared/mutable proof.
    if initial_tags != 0 || reference_values {
        crate::typecheck::TypeChecker::new().resolve_complements(module);
        crate::typecheck::TypeChecker::new().resolve_unary_methods(module, bindings)?;
    }
    let has_tags = count_tags(&mut module.decls).0 != 0;
    let identity = module.identity.clone();
    let mut normalization_error = None;
    let mut normalize = |decls: &mut Vec<Decl>| {
        // Continue only on real resolution or structural inline progress.
        // Counting straddles resolution, not inlining (which may clone tags).
        for round in 0..=crate::core_opt::frontend_inline_round_limit(profile) {
            let before = count_tags(decls).0;
            let mut view = crate::core_ir::UnifiedModule::with_identity(
                std::mem::take(decls),
                identity.clone(),
            );
            let result =
                crate::typecheck::TypeChecker::new().resolve_unary_methods(&mut view, bindings);
            *decls = view.decls;
            if let Err(error) = result {
                normalization_error = Some(error);
                break;
            }
            let after = count_tags(decls).0;
            if round == crate::core_opt::frontend_inline_round_limit(profile) {
                break;
            }
            // Forwarding wrappers can become inlinable before they expose a
            // concrete operator. Structural inline progress must also continue
            // resolution; effectful/unmodified generic calls stop immediately.
            let inline_progress =
                crate::core_opt::inline_for_frontend_normalization(decls, profile);
            if after >= before && !inline_progress {
                break;
            }
        }
    };
    crate::core_opt::optimize_with_linkage_and_semantics_and_normalizer(
        &mut module.decls,
        entry,
        profile,
        keep_all,
        semantics,
        if has_tags { Some(&mut normalize) } else { None },
    );
    if let Some(error) = normalization_error {
        return Err(error);
    }
    // Deferred generic functions retain the established compile-only policy:
    // native scalar-domain validation refuses their unsupported execution.
    crate::core_ir::rust_references::erase_declarations(&mut module.decls);
    Ok(())
}

#[cfg(test)]
mod associated_type_root_tests {
    use super::*;
    use crate::core_ir::{Expr, Stmt, Typ, UnifiedModule};
    #[test]
    fn invalid_type_proofs_are_checked_before_entry_pruning() {
        let function = |name: &str, ret: Typ, value: Expr| Decl::Function {
            name: name.into(),
            params: vec![],
            ret,
            body: vec![Stmt::Return(Some(value))],
            type_params: vec![],
        };
        let mut module = UnifiedModule::new(vec![
            function(
                "bad",
                crate::core_ir::type_proofs::ResolutionFailure::Invalid("cycle".into()).typ(),
                Expr::IntLit(77),
            ),
            function("main", Typ::Int, Expr::IntLit(42)),
        ]);
        assert!(reject_invalid_type_proofs(&module).is_err());
        if let Decl::Function { body, .. } = &mut module.decls[1] {
            *body = vec![Stmt::Return(Some(Expr::Call {
                callee: Box::new(Expr::Ident("bad".into())),
                args: vec![],
            }))];
        }
        assert!(reject_invalid_type_proofs(&module).is_err());
    }
    #[test]
    fn unused_global_invalid_proofs_are_rejected_but_unsupported_proofs_are_deferred() {
        use crate::core_ir::type_proofs::ResolutionFailure;
        let mut module = UnifiedModule::new(vec![Decl::Global {
            name: "unused".into(),
            typ: ResolutionFailure::Unsupported("unowned layout".into()).typ(),
            init: Some(Box::new(Expr::IntLit(0))),
            mutable: false,
            constant_value: false,
        }]);
        assert!(reject_invalid_type_proofs(&module).is_ok());
        let Decl::Global { typ, .. } = &mut module.decls[0] else {
            unreachable!()
        };
        *typ = ResolutionFailure::Invalid("cycle".into()).typ();
        let error = reject_invalid_type_proofs(&module).unwrap_err();
        assert!(
            error.contains("unused") && error.contains("cycle"),
            "{error}"
        );
    }
}
