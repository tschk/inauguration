use super::*;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempFile {
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn temp_source(name: &str, suffix: &str, source: &str) -> TempFile {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "inauguration-agent-mode-{}-{unique}-{name}.{suffix}",
        std::process::id()
    ));
    fs::write(&path, source).expect("write temp source");
    TempFile { path }
}

#[test]
fn json_report_includes_core_graph_and_parser_decision() {
    let temp = temp_source(
        "main",
        "in",
        "fn helper() -> void { return; }\nfn main() -> void { helper(); return; }\n",
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.parser_decision.route, "core_ir");
    assert_eq!(report.parser_decision.parser_id.as_deref(), Some("in"));
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    let core = report.core_ir_summary.as_ref().expect("core summary");
    assert_eq!(core.function_count, 2);
    let graph = report.graph_facts.as_ref().expect("graph facts");
    assert!(graph.has_main);
    assert!(
        graph.call_edges.contains(&CallEdge {
            caller: "main".to_string(),
            callee: "helper".to_string(),
        }),
        "{:?}",
        graph.call_edges
    );
    let value = serde_json::to_value(&report).expect("serialize report");
    assert!(value.get("diagnostics").is_some());
    assert!(value.get("parser_decision").is_some());
    assert!(value.get("core_ir_summary").is_some());
    assert!(value.get("graph_facts").is_some());
    assert!(value.get("size_timing").is_some());
    assert!(value.get("repair_plans").is_some());
}

#[test]
fn in_report_uses_level_three_after_diagnostic_contract() {
    let temp = temp_source("level-three", "in", "fn main() -> void { return; }\n");
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.language_level.level, 3);
    assert_eq!(
        report.language_level.label,
        ".in bounded subset with source diagnostics"
    );
}

#[test]
fn fix_plan_reports_parse_failure_repair() {
    let temp = temp_source(
        "library",
        "icore",
        r#"{"icoreVersion":1,"decls":[{"kind":"struct","name":"Library","fields":[]}]}"#,
    );
    let plan = fix_plan_report(&temp.path, &AgentModeConfig::default()).expect("fix plan");
    assert_eq!(plan.diagnostics[0].code, "AGENT_PARSE_FAILED");
    assert_eq!(plan.repair_plans[0].id, "inspect-parser-input");
    assert_eq!(plan.repair_plans[0].actions[0].kind, "manual_review");
}

#[test]
fn icore_v2_report_uses_body_subset_level() {
    let temp = temp_source(
        "body",
        "icore",
        r#"{
                "icoreVersion": 2,
                "decls": [
                    {
                        "kind": "function",
                        "name": "helper",
                        "params": [],
                        "return": "Int",
                        "body": [{ "kind": "return", "value": 1 }]
                    },
                    {
                        "kind": "function",
                        "name": "main",
                        "params": [],
                        "return": "Void",
                        "body": [{ "kind": "call", "callee": "helper" }]
                    }
                ]
            }"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.language_level.level, 2);
    assert_eq!(report.language_level.label, "icore v2 body subset");
}

#[test]
fn icore_v2_empty_body_report_still_uses_v2_level() {
    let temp = temp_source(
        "empty-body",
        "icore",
        r#"{
                "icoreVersion": 2,
                "decls": [
                    {
                        "kind": "function",
                        "name": "main",
                        "params": [],
                        "return": "Void",
                        "body": []
                    }
                ]
            }"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.language_level.level, 2);
    assert_eq!(report.language_level.label, "icore v2 body subset");
}

#[test]
fn icore_v1_report_uses_declaration_level() {
    let temp = temp_source(
        "v1",
        "icore",
        r#"{
                "icoreVersion": 1,
                "decls": [
                    {
                        "kind": "function",
                        "name": "main",
                        "params": [],
                        "return": "Void",
                        "body": []
                    }
                ]
            }"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.language_level.level, 1);
    assert_eq!(report.language_level.label, "icore v1 declarations");
}

#[test]
fn in_report_includes_surface_effects_and_capabilities() {
    let temp = temp_source(
        "surface",
        "in",
        r#"
import host.log;
capability process.stdout;
extern rust fn host_log(text: String) -> void;
fn main() -> void { host_log("ready"); return; }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert!(report.effects.contains(&"import:host.log".to_string()));
    assert!(report.effects.contains(&"extern:rust:host_log".to_string()));
    assert!(report.capabilities.contains(&"process.stdout".to_string()));
}

#[test]
fn in_report_includes_package_and_module_effects() {
    let temp = temp_source(
        "module-facts",
        "in",
        r#"
package agents.video;
module agents.video.main;
fn main() -> void { return; }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert!(report.effects.contains(&"package:agents.video".to_string()));
    assert!(
        report
            .effects
            .contains(&"module:agents.video.main".to_string())
    );
    let summary = report.core_ir_summary.expect("core summary");
    assert_eq!(summary.identity.package.as_deref(), Some("agents.video"));
    assert_eq!(
        summary.identity.module.as_deref(),
        Some("agents.video.main")
    );
}

#[test]
fn in_report_indexes_resolved_semantic_imports() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-agent-mode-package-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create temp package");
    fs::write(
        dir.join("inauguration.package"),
        r#"name: hyperchat
version: 0.1.0
dependencies:
  postgres:
    version: ^1.0.0
"#,
    )
    .expect("write manifest");
    let source_path = dir.join("main.in");
    fs::write(
        &source_path,
        "package hyperchat;\nuse database.postgres;\nfn main() -> void { return; }\n",
    )
    .expect("write source");

    let report = json_report(&source_path, &AgentModeConfig::default()).expect("report");

    assert!(
        report
            .effects
            .contains(&"use:database.postgres:resolved".to_string())
    );
    assert_eq!(report.package_symbol_index.len(), 1);
    assert_eq!(
        report.package_symbol_index[0].id,
        "symbol:dependency:postgres"
    );
    assert!(report.package_diagnostics.is_empty());
    fs::remove_dir_all(dir).expect("remove temp package");
}

#[test]
fn in_report_warns_when_calling_resolved_dependency_symbol_directly() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-agent-mode-package-call-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create temp package");
    fs::write(
        dir.join("inauguration.package"),
        r#"name: hyperchat
version: 0.1.0
dependencies:
  postgres:
    version: ^1.0.0
"#,
    )
    .expect("write manifest");
    let source_path = dir.join("main.in");
    fs::write(
        &source_path,
        "package hyperchat;\nuse database.postgres;\nfn main() -> void { postgres(\"select 1\"); return; }\n",
    )
    .expect("write source");

    let report = json_report(&source_path, &AgentModeConfig::default()).expect("report");

    assert!(report.package_diagnostics.is_empty());
    assert!(report.diagnostics.iter().any(|item| item.code == "INPKG002"
        && item.message.contains("database.postgres")
        && item.message.contains("postgres")));
    assert_eq!(report.repair_plans[0].id, "wrap-package-dependency");
    fs::remove_dir_all(dir).expect("remove temp package");
}

#[test]
fn in_report_allows_explicit_bound_dependency_symbol_call() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-agent-mode-package-bind-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create temp package");
    fs::write(
        dir.join("inauguration.package"),
        r#"name: hyperchat
version: 0.1.0
dependencies:
  postgres:
    version: ^1.0.0
"#,
    )
    .expect("write manifest");
    let source_path = dir.join("main.in");
    fs::write(
        &source_path,
        "package hyperchat;\nuse database.postgres;\nbind database.postgres as postgres;\nfn main() -> void { postgres(\"select 1\"); return; }\n",
    )
    .expect("write source");

    let report = json_report(&source_path, &AgentModeConfig::default()).expect("report");

    assert!(
        report
            .effects
            .contains(&"bind:database.postgres:postgres:resolved".to_string())
    );
    assert!(
        report
            .package_symbol_index
            .iter()
            .any(|item| item.id == "symbol:binding:postgres"
                && item.kind == "binding"
                && item.source_import == "database.postgres")
    );
    assert!(
        !report
            .diagnostics
            .iter()
            .any(|item| item.code == "INPKG002")
    );
    fs::remove_dir_all(dir).expect("remove temp package");
}

#[test]
fn in_report_warns_for_unresolved_semantic_imports() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "inauguration-agent-mode-package-missing-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create temp package");
    fs::write(
        dir.join("inauguration.package"),
        "name: hyperchat\nversion: 0.1.0\n",
    )
    .expect("write manifest");
    let source_path = dir.join("main.in");
    fs::write(
        &source_path,
        "package hyperchat;\nuse database.postgres;\nfn main() -> void { return; }\n",
    )
    .expect("write source");

    let report = json_report(&source_path, &AgentModeConfig::default()).expect("report");

    assert!(report.package_symbol_index.is_empty());
    assert_eq!(report.package_diagnostics.len(), 1);
    assert_eq!(report.package_diagnostics[0].code, "INPKG001");
    assert!(
        report
            .diagnostics
            .iter()
            .any(|item| item.code == "INPKG001")
    );
    assert_eq!(report.repair_plans[0].id, "declare-package-dependency");
    fs::remove_dir_all(dir).expect("remove temp package");
}

#[test]
fn in_report_warns_when_extern_required_capability_is_missing() {
    let temp = temp_source(
        "missing-capability",
        "in",
        r#"
extern rust fn host_log(text: String) -> void requires process.stdout;
fn main() -> void { host_log("ready"); return; }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.diagnostics[0].code, "AGENT_MISSING_CAPABILITY");
    assert_eq!(report.repair_plans[0].id, "declare-missing-capability");
    assert_eq!(
        report.repair_plans[0].actions[0].replacement.as_deref(),
        Some("\ncapability process.stdout;\n")
    );
}

#[test]
fn in_report_accepts_declared_extern_capability() {
    let temp = temp_source(
        "declared-capability",
        "in",
        r#"
capability process.stdout;
extern rust fn host_log(text: String) -> void requires process.stdout;
fn main() -> void { host_log("ready"); return; }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    assert!(
        report
            .effects
            .contains(&"extern:rust:host_log:requires=process.stdout".to_string())
    );
}

#[test]
fn in_report_checks_std_import_capabilities() {
    let temp = temp_source(
        "std-missing-capability",
        "in",
        r#"
import std.io;
fn main() -> void { print("ready"); return; }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.diagnostics[0].code, "AGENT_MISSING_CAPABILITY");
    assert!(
        report
            .effects
            .contains(&"extern:std:print:requires=process.stdout".to_string())
    );
}

#[test]
fn in_report_checks_std_http_import_capabilities() {
    let temp = temp_source(
        "std-http-missing-capability",
        "in",
        r#"
import std.http;
fn main() -> String { return http-get("https://example.com"); }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.diagnostics[0].code, "AGENT_MISSING_CAPABILITY");
    assert!(
        report
            .effects
            .contains(&"extern:std:http-get:requires=network.http".to_string())
    );
}

#[test]
fn in_report_includes_std_json_import_effects() {
    let temp = temp_source(
        "std-json-effects",
        "in",
        r#"
import std.json;
fn main() -> String { return json-parse("{}"); }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    assert!(
        report
            .effects
            .contains(&"extern:std:json-parse".to_string())
    );
    assert!(
        report
            .effects
            .contains(&"extern:std:json-stringify".to_string())
    );
}

#[test]
fn in_report_checks_std_process_import_capabilities() {
    let temp = temp_source(
        "std-process-missing-capability",
        "in",
        r#"
import std.process;
fn main() -> String { return process-run("pwd"); }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.diagnostics[0].code, "AGENT_MISSING_CAPABILITY");
    assert!(
        report
            .effects
            .contains(&"extern:std:process-run:requires=process.spawn".to_string())
    );
}

#[test]
fn in_report_checks_std_cli_import_capabilities() {
    let temp = temp_source(
        "std-cli-missing-capability",
        "in",
        r#"
import std.cli;
fn main() -> String { return arg(0); }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.diagnostics[0].code, "AGENT_MISSING_CAPABILITY");
    assert!(
        report
            .effects
            .contains(&"extern:std:arg:requires=process.args".to_string())
    );
    assert!(
        report
            .effects
            .contains(&"extern:std:arg-count:requires=process.args".to_string())
    );
}

#[test]
fn in_report_checks_std_env_import_capabilities() {
    let temp = temp_source(
        "std-env-missing-capability",
        "in",
        r#"
import std.env;
fn main() -> String { return env-get("HOME"); }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(report.diagnostics[0].code, "AGENT_MISSING_CAPABILITY");
    assert!(
        report
            .effects
            .contains(&"extern:std:env-get:requires=env.read".to_string())
    );
    assert!(
        report
            .effects
            .contains(&"extern:std:env-set:requires=env.write".to_string())
    );
    assert!(
        report
            .effects
            .contains(&"extern:std:env-has:requires=env.read".to_string())
    );
}

#[test]
fn in_report_includes_orchestration_facts_as_status_only() {
    let temp = temp_source(
        "orchestration",
        "in",
        r#"
enable distributed-workers;
@gpu
distributed fn process(video: Video) -> void { return; }
parallel { process(ready()); }
struct Video { Int id }
fn main() -> void { return; }
"#,
    );
    let report = json_report(&temp.path, &AgentModeConfig::default()).expect("report");
    assert_eq!(
        report.orchestration.enabled_extensions,
        vec!["distributed-workers"]
    );
    assert_eq!(report.orchestration.distributed_functions, vec!["process"]);
    assert_eq!(report.orchestration.parallel_regions, 1);
    assert!(
        report
            .orchestration
            .local_plan
            .iter()
            .any(|step| step.kind == "distributed_fn" && step.name == "process")
    );
    assert_eq!(report.orchestration.distributed_jobs[0].function, "process");
    assert!(report.orchestration.runtime_status.iter().any(
        |status| status.implemented && status.reason_code == "local-distributed-simulator"
    ));
    assert!(
        report
            .effects
            .contains(&"enable:distributed-workers".into())
    );
    assert!(report.effects.contains(&"distributed:process".into()));
}

#[test]
fn explain_summarizes_successful_report() {
    let temp = temp_source("explain", "in", "fn main() -> void { return; }\n");
    let explanation = explain(&temp.path, &AgentModeConfig::default()).expect("explain");
    assert!(
        explanation
            .summary
            .iter()
            .any(|line| line.contains("no diagnostics")),
        "{:?}",
        explanation.summary
    );
}

/// Lowerer codes must be explainable through the same registry as the rest, and
/// the explanation must be keyed on the constant the lowerer emits.
#[test]
fn explains_lowerer_degradation_codes() {
    for code in [
        crate::native_emit::lower::DEGRADATION_SKIPPED_FUNCTION,
        crate::native_emit::lower::DEGRADATION_UNRESOLVED_CALL,
    ] {
        let rule = explain_diagnostic(code).unwrap_or_else(|| panic!("{code} not explainable"));
        assert_eq!(rule.code, code);
        assert!(!rule.meaning.is_empty());
        assert!(!rule.fix.is_empty());
    }
    assert!(explain_diagnostic("IN3999").is_none());
}
