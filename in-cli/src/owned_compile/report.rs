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
    use std::time::Duration;

    fn dummy_report() -> OwnedCompileReport {
        OwnedCompileReport {
            schema_version: 1,
            owned: true,
            path: "test.in".to_string(),
            module_id: "test".to_string(),
            module_identity: None,
            package_name: None,
            target: "native".to_string(),
            target_triple: None,
            entry: None,
            linkage: "dynamic".to_string(),
            frontend_level: "unsupported".to_string(),
            semantic_level: "failed".to_string(),
            backend_level: "contract-only".to_string(),
            runtime_level: "none".to_string(),
            external_invocations: Vec::new(),
            reason_code: None,
            reason: None,
            success: true,
            artifact_path: None,
            executable_path: None,
            abi_path: None,
            degradations: Vec::new(),
            parsed_function_count: 0,
            typed_function_count: 0,
            call_edge_count: 0,
            jobs: 2,
            timing_micros: 0,
            timing_waves_us: None,
            cache_hit: false,
            frontend_hash: None,
            eval_exit_code: None,
            eval_result: None,
            execution_started: false,
            error: None,
        }
    }

    #[test]
    fn test_finalize_report_success() {
        let report = dummy_report();
        let started = Instant::now()
            .checked_sub(Duration::from_micros(100))
            .unwrap();
        let cwd = std::env::temp_dir();

        let _token = external_guard::ExternalInvocationGuard::enter();
        let finalized = finalize_report(report, started, cwd.as_path(), "hash123");

        assert!(finalized.success);
        assert!(finalized.timing_micros >= 100);
        assert!(finalized.timing_waves_us.is_some());
        let waves = finalized.timing_waves_us.unwrap();
        assert_eq!(waves.len(), 2);
        assert!(finalized.error.is_none());
        assert!(finalized.reason.is_none());
        assert!(finalized.reason_code.is_none());
    }

    #[test]
    fn test_finalize_report_forbidden_invocation() {
        let report = dummy_report();
        let started = Instant::now();
        let cwd = std::env::temp_dir();

        let _token = external_guard::ExternalInvocationGuard::enter();
        external_guard::ExternalInvocationGuard::record("swiftc");

        let finalized = finalize_report(report, started, cwd.as_path(), "hash123");

        assert!(!finalized.success);
        assert_eq!(
            finalized.reason_code.as_deref(),
            Some("external-tool-invoked")
        );
        assert!(finalized.reason.as_ref().unwrap().contains("swiftc"));
        assert!(finalized.error.is_some());
        assert!(!finalized.external_invocations.is_empty());
    }

    #[test]
    fn test_finalize_report_cache_hit_skips_write() {
        let mut report = dummy_report();
        report.cache_hit = true;
        let started = Instant::now();
        // Use a path that would fail to write if it tried (e.g. root) but it shouldn't try
        let cwd = std::path::Path::new("/does_not_exist_and_cannot_be_written");

        let _token = external_guard::ExternalInvocationGuard::enter();
        // Should not panic or print error because it shouldn't attempt to write cache
        let _finalized = finalize_report(report, started, cwd, "hash123");
    }
}
