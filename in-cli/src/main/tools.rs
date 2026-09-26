use crate::util::resolve_invocation_path;
use crate::{InError, Result};
use inauguration::agent_mode;
use inauguration::coverage::{CoverageBlocker, CoverageReport, CoverageStatus};
use inauguration::native_emit::lower::{
    DEGRADATION_SKIPPED_FUNCTION, DEGRADATION_UNRESOLVED_CALL,
};
use inauguration::parser_registry::ParserCli;
use std::fs;
use std::path::Path;

pub(crate) fn cmd_agent(
    invocation_cwd: &Path,
    path: &str,
    module_id: &str,
    parser: ParserCli,
) -> Result<()> {
    let report = agent_mode::analyze_path(invocation_cwd, path, module_id, parser);
    let json = serde_json::to_string_pretty(&report)
        .map_err(|err| InError::Message(format!("serialize agent report: {err}")))?;
    println!("{json}");
    if report
        .diagnostics
        .iter()
        .any(|diagnostic| matches!(diagnostic.severity, agent_mode::DiagnosticSeverity::Error))
    {
        Err(InError::Message("agent diagnostics failed".to_string()))
    } else {
        Ok(())
    }
}

pub(crate) fn cmd_coverage(
    invocation_cwd: &Path,
    path: &str,
    parser: ParserCli,
    entry: Option<&str>,
    json: bool,
) -> Result<()> {
    let source_path = resolve_invocation_path(invocation_cwd, path);
    let report = inauguration::coverage::coverage_for_path(&source_path, parser, entry);

    if json {
        let raw = serde_json::to_string_pretty(&report)
            .map_err(|err| InError::Message(format!("serialize coverage report: {err}")))?;
        println!("{raw}");
        return Ok(());
    }

    render_coverage(&report);
    Ok(())
}

/// Print a coverage report. Counts are withheld when analysis failed, because a
/// percentage from a partial run would overstate what is known.
fn render_coverage(report: &CoverageReport) {
    let parser = report.parser_id.as_deref().unwrap_or("unknown");
    println!("coverage: {} ({parser})", report.path);

    match report.status {
        CoverageStatus::Rejected | CoverageStatus::UnsupportedHost => {
            if let Some(code) = &report.reason_code {
                println!("  blocked: {code}");
            }
            if let Some(reason) = &report.reason {
                println!("  {reason}");
            }
            return;
        }
        CoverageStatus::FullyLowered | CoverageStatus::Degraded => {}
    }

    println!("  entry                 {}", report.entry);
    println!("  functions in source   {}", report.functions_in_source);
    println!("  functions analyzed    {}", report.functions_analyzed);
    match report.lowered_fraction() {
        Some(fraction) => println!(
            "  lowered to native     {}  ({:.1}%)",
            report.functions_lowered,
            fraction * 100.0
        ),
        None => println!("  lowered to native     {}", report.functions_lowered),
    }
    println!("  not lowered           {}", report.functions_not_lowered);
    println!();

    if report.blockers.is_empty() {
        println!("  fully lowered — nothing was substituted with a trap.");
        return;
    }

    // Split the two kinds apart: a construct the lowerer cannot compile is a
    // coverage gap, while a call with no definition in this unit is a scoping
    // fact that dependency resolution can still satisfy.
    let not_lowered: Vec<&CoverageBlocker> = report
        .blockers
        .iter()
        .filter(|blocker| blocker.code == DEGRADATION_SKIPPED_FUNCTION)
        .collect();
    if !not_lowered.is_empty() {
        println!("  not lowered (the lowerer cannot compile these):");
        for blocker in not_lowered {
            println!("    x{}  {}", blocker.count, blocker.message);
        }
        println!();
    }

    if report.unresolved_calls > 0 {
        let mut targets: Vec<&str> = report
            .degradations
            .iter()
            .filter(|degradation| degradation.code == DEGRADATION_UNRESOLVED_CALL)
            .filter_map(|degradation| degradation.target.as_deref())
            .collect();
        targets.sort_unstable();
        targets.dedup();
        println!(
            "  unresolved calls (no definition in this unit): {} site(s) to {} target(s)",
            report.unresolved_calls,
            targets.len()
        );
        const SHOWN_TARGETS: usize = 12;
        let shown = targets
            .iter()
            .take(SHOWN_TARGETS)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        println!("    {shown}");
        if targets.len() > SHOWN_TARGETS {
            println!("    … and {} more", targets.len() - SHOWN_TARGETS);
        }
        println!();
    }

    let reachable: usize = report
        .degradations
        .iter()
        .filter(|degradation| degradation.reachable)
        .count();
    if reachable == 0 {
        println!("  none reachable from the entry — the artifact carries dead trap bodies only.");
    } else {
        println!(
            "  {reachable} reachable from the entry — reaching one aborts the program with exit {}.",
            inauguration::inrt::INRT_TRAP_EXIT_CODE
        );
    }
}

pub(crate) fn cmd_explain(diagnostic_code: &str, json: bool) -> Result<()> {
    let Some(rule) = agent_mode::explain_diagnostic(diagnostic_code) else {
        return Err(InError::Message(format!(
            "unknown diagnostic code: {diagnostic_code}"
        )));
    };
    if json {
        let raw = serde_json::to_string_pretty(&rule)
            .map_err(|err| InError::Message(format!("serialize diagnostic rule: {err}")))?;
        println!("{raw}");
    } else {
        println!("{}", rule.code);
        println!("{}", rule.meaning);
        println!("fix: {}", rule.fix);
    }
    Ok(())
}

pub(crate) fn cmd_fix(
    invocation_cwd: &Path,
    plan: bool,
    json: bool,
    path: &str,
    module_id: &str,
    parser: ParserCli,
) -> Result<()> {
    if !plan {
        return Err(InError::Message(
            "`in fix` currently requires --plan so agents review typed edits before applying"
                .to_string(),
        ));
    }
    let report = agent_mode::fix_plan(invocation_cwd, path, module_id, parser);
    if json {
        let raw = serde_json::to_string_pretty(&report)
            .map_err(|err| InError::Message(format!("serialize fix plan: {err}")))?;
        println!("{raw}");
    } else {
        println!("repair plans: {}", report.repair_plans.len());
        for plan in &report.repair_plans {
            println!("{}: {}", plan.applies_to_code, plan.title);
            println!("  {}", plan.rationale);
            for action in &plan.actions {
                println!("  {}: {}", action.kind, action.description);
            }
        }
    }
    Ok(())
}

pub(crate) fn cmd_canonicalize(invocation_cwd: &Path, path: &str, check: bool) -> Result<()> {
    let source_path = resolve_invocation_path(invocation_cwd, path);
    let source = fs::read_to_string(&source_path)?;
    let canonical = inauguration::in_canonical::canonicalize_in_source(&source)
        .map_err(|err| InError::Message(format!("canonicalize: {err}")))?;
    if check {
        if source == canonical {
            return Ok(());
        }
        return Err(InError::Message(format!(
            "{} is not canonical",
            source_path.display()
        )));
    }
    print!("{canonical}");
    Ok(())
}

pub(crate) fn cmd_languages(json: bool) -> Result<()> {
    let entries = inauguration::language_support::all_language_support();
    if json {
        let reports: Vec<_> = entries
            .iter()
            .map(inauguration::boundary_capability::language_support_json)
            .collect();
        let raw = serde_json::to_string_pretty(&reports)
            .map_err(|err| InError::Message(format!("serialize language support: {err}")))?;
        println!("{raw}");
        return Ok(());
    }

    println!(
        "{:<12} {:<12} {:<18} {:<34} runtime",
        "language", "parser", "capabilities", "front"
    );
    for entry in entries {
        let caps_str = entry.capabilities.join(", ");
        println!(
            "{:<12} {:<12} {:<18} {:<34} {}",
            entry.language,
            entry.parser_id.unwrap_or("swift"),
            caps_str,
            entry.front,
            entry.runtime_boundary
        );
    }
    Ok(())
}
