use crate::compile_cache;
use crate::core_ir::{Decl, UnifiedModule};
use crate::external_guard;
use std::time::Instant;

use super::util::{linkage_label, target_label};
use super::{CompileTarget, OwnedCompileReport, OwnedCompileRequest};

pub fn count_functions(module: &UnifiedModule) -> usize {
    module
        .decls
        .iter()
        .filter(|decl| matches!(decl, Decl::Function { .. }))
        .count()
}

/// Whether the module declares a function with this name.
pub fn module_has_function(module: &UnifiedModule, name: &str) -> bool {
    module
        .decls
        .iter()
        .any(|decl| matches!(decl, Decl::Function { name: declared, .. } if declared == name))
}

pub fn count_call_edges(module: &UnifiedModule, module_id: &str) -> usize {
    let sil = crate::compiler::driver::lower_unified_module(
        module,
        module.effective_module_id(module_id),
    );
    let artifact = crate::hybrid_sil::parse_textual_sil(&sil);
    let cleaned = crate::hybrid_sil::remove_debug_insts(&artifact);
    crate::hybrid_sil::extract_call_graph(&cleaned)
        .call_edges
        .len()
}

pub fn jobs_for_request(request: &OwnedCompileRequest) -> usize {
    request.jobs.max(1)
}

pub fn timing_waves_for_jobs(jobs: usize, total_micros: u128) -> Vec<u128> {
    if jobs <= 1 {
        return vec![total_micros];
    }
    let boundaries: Vec<usize> = (1..=jobs).collect();
    let mut waves = Vec::with_capacity(boundaries.len());
    for &boundary in &boundaries {
        let share = (total_micros * boundary as u128) / jobs as u128;
        waves.push(share);
    }
    if let Some((last, rest)) = waves.split_last_mut() {
        let sum: u128 = rest.iter().sum();
        *last = total_micros.saturating_sub(sum);
    }
    waves
}

pub fn base_report(
    request: &OwnedCompileRequest,
    jobs: usize,
    started: Instant,
) -> OwnedCompileReport {
    OwnedCompileReport {
        schema_version: 1,
        owned: true,
        path: request.path.display().to_string(),
        module_id: request.module_id.clone(),
        module_identity: None,
        package_name: None,
        target: target_label(request.target).to_string(),
        target_triple: request.target_triple.clone(),
        entry: request.entry.clone(),
        linkage: linkage_label(request.linkage).to_string(),
        frontend_level: "unsupported".to_string(),
        semantic_level: "failed".to_string(),
        backend_level: match request.target {
            CompileTarget::Native => "contract-only".to_string(),
            CompileTarget::Jit => "owned-native-subset".to_string(),
        },
        runtime_level: match request.target {
            CompileTarget::Native => "none".to_string(),
            CompileTarget::Jit => "inrt-jit".to_string(),
        },
        external_invocations: Vec::new(),
        reason_code: None,
        reason: None,
        success: false,
        artifact_path: None,
        executable_path: None,
        abi_path: None,
        degradations: Vec::new(),
        parsed_function_count: 0,
        typed_function_count: 0,
        call_edge_count: 0,
        jobs,
        timing_micros: started.elapsed().as_micros(),
        timing_waves_us: None,
        cache_hit: false,
        frontend_hash: None,
        eval_exit_code: None,
        eval_result: None,
        execution_started: false,
        error: None,
    }
}

pub fn finalize_report(
    mut report: OwnedCompileReport,
    started: Instant,
    cwd: &std::path::Path,
    frontend_hash: &str,
) -> OwnedCompileReport {
    report.external_invocations = external_guard::ExternalInvocationGuard::active_invocations();
    if let Err(reason) =
        external_guard::assert_no_forbidden_invocations(&report.external_invocations)
    {
        report.success = false;
        report.reason_code = Some("external-tool-invoked".to_string());
        report.reason = Some(reason.clone());
        report.error = Some(reason);
    }
    report.timing_micros = started.elapsed().as_micros();
    report.timing_waves_us = Some(timing_waves_for_jobs(report.jobs, report.timing_micros));
    if !report.cache_hit {
        if let Err(e) = compile_cache::write_cached_report(cwd, frontend_hash, &report) {
            eprintln!("[cache] warning: failed to write compile cache: {e}");
        }
    }
    report
}

pub fn report_to_json(report: &OwnedCompileReport) -> Result<String, String> {
    serde_json::to_string_pretty(report)
        .map_err(|err| format!("serialize owned compile report: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owned_compile::{CompileTarget, OwnedCompileRequest};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn test_finalize_report() {
        let request = OwnedCompileRequest {
            path: PathBuf::from("dummy.in"),
            module_id: "dummy".to_string(),
            parser: crate::owned_compile::ParserCli::Auto,
            target: CompileTarget::Native,
            entry: None,
            out: None,
            linkage: crate::owned_compile::NativeLinkage::Executable,
            target_triple: None,
            jobs: 2,
            debug: false,
            profile: crate::owned_compile::EmitProfile::Default,
            emit: None,
            base: None,
        };
        let started = Instant::now();

        let mut report = base_report(&request, 2, started);
        report.success = true;

        // We don't want to actually write to the filesystem, so we simulate cache hit
        // or mock the directory. We can set cache_hit to true to avoid cache writing.
        report.cache_hit = true;

        // Since we didn't start a guard and record anything, active_invocations should be empty
        let cwd = std::path::Path::new(".");
        let frontend_hash = "dummy_hash";

        // Wait slightly to ensure timing is non-zero
        std::thread::sleep(Duration::from_micros(10));

        let finalized = finalize_report(report, started, cwd, frontend_hash);

        assert!(
            finalized.success,
            "success should remain true if no forbidden tools"
        );
        assert!(finalized.timing_micros > 0);
        assert!(finalized.timing_waves_us.is_some());
        let waves = finalized.timing_waves_us.unwrap();
        assert_eq!(waves.len(), 2);
        assert!(waves.iter().sum::<u128>() == finalized.timing_micros);
        assert!(finalized.external_invocations.is_empty());
    }
}
