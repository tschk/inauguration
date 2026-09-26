//! Static coverage: what compiles to native code, and what blocks the rest.
//!
//! The native lowerer traps on constructs it cannot compile instead of
//! fabricating a value, so a caller can always ask "did everything compile?"
//! This module answers that per source file, naming every site that did not
//! compile and grouping the reasons, in the shape of `scriptc coverage`.
//!
//! Granularity is per function: the lowerer degrades whole functions, so that is
//! the truthful unit. Counts are only rendered when analysis got far enough to
//! mean something — a file that failed to parse or verify reports the failure
//! instead of a percentage.

use crate::core_ir::UnifiedModule;
use crate::native_emit::lower::{
    DEGRADATION_SKIPPED_FUNCTION, DEGRADATION_UNRESOLVED_CALL, LoweringDegradation, LoweredModule,
    NativeLinkage, host_supports_native_subset, lower_module_with_jobs,
};
use crate::parser_registry::{self, ParserCli};
use crate::owned_compile::resolve_jit_entry;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const COVERAGE_SCHEMA_VERSION: u32 = 1;

/// How far a file got through the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageStatus {
    /// Every function compiled; nothing was substituted.
    FullyLowered,
    /// The artifact is usable, but some functions trap if reached.
    Degraded,
    /// Analysis failed, so coverage cannot be measured.
    Rejected,
    /// This host has no native backend, so lowering was not attempted.
    UnsupportedHost,
}

impl CoverageStatus {
    /// Whether the counts describe a real lowering attempt.
    #[must_use]
    pub fn has_counts(self) -> bool {
        matches!(self, Self::FullyLowered | Self::Degraded)
    }
}

/// One reason code and how many sites it blocks, in the style of a compiler
/// coverage report's blocker list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageBlocker {
    pub code: String,
    pub message: String,
    pub count: usize,
    /// How many of the sites the entry can reach.
    pub reachable: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageReport {
    pub schema_version: u32,
    pub path: String,
    pub parser_id: Option<String>,
    pub entry: String,
    pub status: CoverageStatus,
    /// Functions the parsed module declared, before optimization.
    pub functions_in_source: usize,
    /// Functions the lowerer was handed, after optimization.
    pub functions_analyzed: usize,
    pub functions_lowered: usize,
    pub functions_not_lowered: usize,
    /// Calls whose target has no definition in the analyzed unit. These are a
    /// scoping fact rather than a lowerer limitation: a build that resolves
    /// dependencies (packages, cargo crates) can still satisfy them.
    pub unresolved_calls: usize,
    pub degradations: Vec<LoweringDegradation>,
    pub blockers: Vec<CoverageBlocker>,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
}

impl CoverageReport {
    fn rejected(path: &Path, reason_code: &str, reason: String) -> Self {
        Self {
            schema_version: COVERAGE_SCHEMA_VERSION,
            path: path.display().to_string(),
            parser_id: None,
            entry: String::new(),
            status: CoverageStatus::Rejected,
            functions_in_source: 0,
            functions_analyzed: 0,
            functions_lowered: 0,
            functions_not_lowered: 0,
            unresolved_calls: 0,
            degradations: Vec::new(),
            blockers: Vec::new(),
            reason_code: Some(reason_code.to_string()),
            reason: Some(reason),
        }
    }

    /// Share of analyzed functions that compiled, or `None` when counts are not
    /// meaningful.
    #[must_use]
    pub fn lowered_fraction(&self) -> Option<f64> {
        if !self.status.has_counts() || self.functions_analyzed == 0 {
            return None;
        }
        Some(self.functions_lowered as f64 / self.functions_analyzed as f64)
    }
}

/// Measure static coverage for one source path.
///
/// Mirrors the owned build's pre-lowering pipeline (parse, desugar, family
/// typecheck, profile optimize) so the numbers match what a build would produce.
pub fn coverage_for_path(path: &Path, parser: ParserCli, entry: Option<&str>) -> CoverageReport {
    if !host_supports_native_subset() {
        let mut report = CoverageReport::rejected(
            path,
            "coverage-host-unsupported",
            "native lowering needs a macOS AArch64 or Linux x86_64 host".to_string(),
        );
        report.status = CoverageStatus::UnsupportedHost;
        return report;
    }

    let resolved = parser_registry::resolve_parser_id(path, parser);
    let mut module = match parser_registry::parse_with_resolved(resolved, path) {
        Ok(Some(module)) => module,
        Ok(None) => {
            return CoverageReport::rejected(
                path,
                "coverage-no-core-ir-front",
                "no Core IR frontend resolved for this path".to_string(),
            );
        }
        Err(err) => {
            // Library `.in` files have no `fn main`; they are still worth measuring.
            if path.extension().is_some_and(|ext| ext == "in") {
                match crate::in_lang_parse::parse_in_library_file(path) {
                    Ok(module) => module,
                    Err(_) => {
                        return CoverageReport::rejected(
                            path,
                            "coverage-parse-failed",
                            err.to_string(),
                        );
                    }
                }
            } else {
                return CoverageReport::rejected(path, "coverage-parse-failed", err.to_string());
            }
        }
    };

    let parser_id = match resolved {
        parser_registry::ResolvedBuildParser::CoreIr(id) => Some(id.as_str().to_string()),
        parser_registry::ResolvedBuildParser::Swift => None,
    };

    let functions_in_source = count_functions(&module);
    crate::lower_core::desugar_module(&mut module);
    if let parser_registry::ResolvedBuildParser::CoreIr(parser_id) = resolved
        && crate::typecheck::uses_family_typecheck(parser_id)
    {
        module = crate::typecheck::normalize_module(parser_id, &module);
    }

    // Same entry choice as a build: the caller's, else `main`, else whatever the
    // module offers.
    let requested = entry
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| resolve_jit_entry(&module, "main"));
    let effective_entry = if entry.is_some() {
        requested.clone()
    } else {
        resolve_jit_entry(&module, &requested)
    };

    crate::core_opt::optimize_with_profile(
        &mut module.decls,
        Some(&effective_entry),
        crate::emit_profile::EmitProfile::Default,
    );
    let functions_analyzed = count_functions(&module);
    if functions_analyzed == 0 {
        return CoverageReport::rejected(
            path,
            "coverage-no-functions",
            format!("no functions reachable from entry `{effective_entry}`"),
        );
    }

    let lowered: LoweredModule =
        match lower_module_with_jobs(&module, &effective_entry, NativeLinkage::Executable, 1) {
            Ok(lowered) => lowered,
            Err(err) => {
                return CoverageReport::rejected(path, "coverage-lowering-failed", err);
            }
        };

    let functions_not_lowered = lowered
        .degradations
        .iter()
        .filter(|degradation| degradation.code == DEGRADATION_SKIPPED_FUNCTION)
        .count();
    let unresolved_calls = lowered
        .degradations
        .iter()
        .filter(|degradation| degradation.code == DEGRADATION_UNRESOLVED_CALL)
        .count();
    let blockers = group_blockers(&lowered.degradations);
    let status = if lowered.degradations.is_empty() {
        CoverageStatus::FullyLowered
    } else {
        CoverageStatus::Degraded
    };

    CoverageReport {
        schema_version: COVERAGE_SCHEMA_VERSION,
        path: path.display().to_string(),
        parser_id,
        entry: effective_entry,
        status,
        functions_in_source,
        functions_analyzed,
        functions_lowered: functions_analyzed.saturating_sub(functions_not_lowered),
        functions_not_lowered,
        unresolved_calls,
        degradations: lowered.degradations,
        blockers,
        reason_code: None,
        reason: None,
    }
}

/// Group degradations by code and message, most frequent first.
fn group_blockers(degradations: &[LoweringDegradation]) -> Vec<CoverageBlocker> {
    let mut grouped: Vec<CoverageBlocker> = Vec::new();
    for degradation in degradations {
        let message = degradation.describe();
        if let Some(existing) = grouped
            .iter_mut()
            .find(|blocker| blocker.code == degradation.code && blocker.message == message)
        {
            existing.count += 1;
            existing.reachable += usize::from(degradation.reachable);
            continue;
        }
        grouped.push(CoverageBlocker {
            code: degradation.code.clone(),
            message,
            count: 1,
            reachable: usize::from(degradation.reachable),
        });
    }
    grouped.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.code.cmp(&b.code)));
    grouped
}

fn count_functions(module: &UnifiedModule) -> usize {
    module
        .decls
        .iter()
        .filter(|decl| matches!(decl, crate::core_ir::Decl::Function { .. }))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_source(name: &str, source: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "inauguration-coverage-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, source).unwrap();
        path
    }

    #[test]
    fn clean_program_reports_fully_lowered() {
        if !host_supports_native_subset() {
            return;
        }
        let path = temp_source(
            "clean.in",
            "fn add(a: Int, b: Int) -> Int { return a + b; }\n\nfn main() -> Int { return add(1, 2); }\n",
        );
        let report = coverage_for_path(&path, ParserCli::Auto, None);
        fs::remove_file(&path).unwrap();

        assert_eq!(report.status, CoverageStatus::FullyLowered, "{report:?}");
        assert_eq!(report.functions_not_lowered, 0);
        assert!(report.degradations.is_empty());
        assert!(report.blockers.is_empty());
        assert_eq!(report.lowered_fraction(), Some(1.0));
    }

    #[test]
    fn unlowered_function_is_reported_as_a_blocker() {
        if !host_supports_native_subset() {
            return;
        }
        let path = temp_source(
            "degraded.in",
            r#"
fn nothing() -> void { return; }

fn bad() -> Int {
  let v = nothing();
  return 1;
}

fn main() -> Int {
  return bad();
}
"#,
        );
        let report = coverage_for_path(&path, ParserCli::Auto, None);
        fs::remove_file(&path).unwrap();

        assert_eq!(report.status, CoverageStatus::Degraded, "{report:?}");
        assert_eq!(report.functions_not_lowered, 1);
        assert_eq!(report.blockers.len(), 1);
        assert_eq!(report.blockers[0].code, DEGRADATION_SKIPPED_FUNCTION);
        assert_eq!(report.blockers[0].reachable, 1);
        assert_eq!(report.functions_lowered, report.functions_analyzed - 1);
    }

    #[test]
    fn unparseable_source_is_rejected_without_counts() {
        let path = temp_source("broken.in", "fn main() -> Int { return 1;\n");
        let report = coverage_for_path(&path, ParserCli::Auto, None);
        fs::remove_file(&path).unwrap();

        assert_eq!(report.status, CoverageStatus::Rejected, "{report:?}");
        assert_eq!(report.lowered_fraction(), None);
        assert!(report.reason_code.is_some());
    }

    /// A call with no definition in this file is reported as unresolved rather
    /// than as a lowerer limitation.
    #[test]
    fn call_without_definition_is_counted_as_unresolved() {
        if !host_supports_native_subset() {
            return;
        }
        let path = temp_source(
            "unresolved.in",
            "fn main() -> Int { return missing(1); }\n",
        );
        let report = coverage_for_path(&path, ParserCli::Auto, None);
        fs::remove_file(&path).unwrap();

        assert_eq!(report.status, CoverageStatus::Degraded, "{report:?}");
        assert_eq!(report.unresolved_calls, 1, "{report:?}");
        assert_eq!(report.functions_not_lowered, 0, "{report:?}");
        assert_eq!(
            report.blockers[0].code,
            DEGRADATION_UNRESOLVED_CALL,
            "{report:?}"
        );
    }
}
