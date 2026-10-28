use super::types::{
    AnnotationFact, CallEdge, DistributedJobFact, GraphFacts, LanguageLevel, OrchestrationFacts,
    OrchestrationPlanStep, ParserDecision, RuntimeStatusFact,
};
use crate::parser_registry::{ParserCli, ParserId, ResolvedBuildParser};
use std::path::Path;

pub(super) fn parser_cli(preference: &super::types::AgentParserPreference) -> ParserCli {
    match preference {
        super::types::AgentParserPreference::Auto => ParserCli::Auto,
        super::types::AgentParserPreference::In => ParserCli::In,
        super::types::AgentParserPreference::Icore => ParserCli::Icore,
    }
}

pub(super) fn parser_preference_from_cli(parser: ParserCli) -> super::types::AgentParserPreference {
    match parser {
        ParserCli::Auto => super::types::AgentParserPreference::Auto,
        ParserCli::In => super::types::AgentParserPreference::In,
        ParserCli::Icore => super::types::AgentParserPreference::Icore,
    }
}

pub(super) fn resolved_parser_id(resolved: ResolvedBuildParser) -> Option<ParserId> {
    match resolved {
        ResolvedBuildParser::CoreIr(id) => Some(id),
        ResolvedBuildParser::Swift => None,
    }
}

pub(super) fn parser_decision(
    path: &Path,
    preference: &super::types::AgentParserPreference,
    resolved: ResolvedBuildParser,
) -> ParserDecision {
    match resolved {
        ResolvedBuildParser::CoreIr(id) => ParserDecision {
            requested: parser_preference_label(preference).to_string(),
            route: "core_ir".to_string(),
            parser_id: Some(id.as_str().to_string()),
            parser_family: Some(id.family_label().to_string()),
            source_path: path.display().to_string(),
            reason: "parser_registry resolved source to a Core IR frontend".to_string(),
        },
        ResolvedBuildParser::Swift => ParserDecision {
            requested: parser_preference_label(preference).to_string(),
            route: "swift_sil_emit".to_string(),
            parser_id: None,
            parser_family: Some("Swift SIL emit".to_string()),
            source_path: path.display().to_string(),
            reason: "parser_registry did not find a Core IR parser and selected Swift SIL emit"
                .to_string(),
        },
    }
}

fn parser_preference_label(preference: &super::types::AgentParserPreference) -> &'static str {
    match preference {
        super::types::AgentParserPreference::Auto => "auto",
        super::types::AgentParserPreference::In => "in",
        super::types::AgentParserPreference::Icore => "icore",
    }
}

pub(super) fn language_level(resolved: ResolvedBuildParser) -> LanguageLevel {
    match resolved {
        ResolvedBuildParser::Swift => LanguageLevel {
            level: 2,
            label: "native Swift subset to SIL".to_string(),
        },
        ResolvedBuildParser::CoreIr(ParserId::In) => LanguageLevel {
            level: 3,
            label: ".in bounded subset with source diagnostics".to_string(),
        },
        ResolvedBuildParser::CoreIr(ParserId::Icore) => LanguageLevel {
            level: 1,
            label: "icore v1 declarations".to_string(),
        },
        ResolvedBuildParser::CoreIr(
            ParserId::Rust
            | ParserId::Go
            | ParserId::V
            | ParserId::Java
            | ParserId::Groovy
            | ParserId::C
            | ParserId::Cpp
            | ParserId::ObjCpp
            | ParserId::JavaScript
            | ParserId::TypeScript
            | ParserId::OCaml,
        ) => LanguageLevel {
            level: 2,
            label: "bounded body lowering".to_string(),
        },
        ResolvedBuildParser::CoreIr(
            ParserId::Clojure
            | ParserId::Nim
            | ParserId::D
            | ParserId::Crystal
            | ParserId::VbNet
            | ParserId::Odin
            | ParserId::Hare,
        ) => LanguageLevel {
            level: 0,
            label: "known parser id without compatible wired front".to_string(),
        },
        ResolvedBuildParser::CoreIr(_) => LanguageLevel {
            level: 1,
            label: "Tree-sitter declaration extraction".to_string(),
        },
    }
}

pub(super) fn language_level_for_module(
    current: LanguageLevel,
    parser_id: Option<&str>,
    source: &str,
) -> LanguageLevel {
    if parser_id == Some("icore") && icore_source_version(source) == Some(2) {
        return LanguageLevel {
            level: 2,
            label: "icore v2 body subset".to_string(),
        };
    }
    current
}

pub(super) fn icore_source_version(source: &str) -> Option<u32> {
    serde_json::from_str::<serde_json::Value>(source)
        .ok()?
        .get("icoreVersion")?
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
}

pub(super) fn graph_facts_from_sil(textual_sil: &str) -> GraphFacts {
    let artifact = crate::hybrid_sil::parse_textual_sil(textual_sil);
    let cleaned = crate::hybrid_sil::remove_debug_insts(&artifact);
    let report = crate::hybrid_sil::extract_call_graph(&cleaned);
    GraphFacts {
        function_count: cleaned.functions.len(),
        block_count: cleaned.cfg_blocks.len(),
        instruction_count: cleaned.instructions.len(),
        call_edges: report
            .call_edges
            .into_iter()
            .map(|(caller, callee)| CallEdge { caller, callee })
            .collect(),
        has_main: cleaned
            .functions
            .iter()
            .any(|function| function.function_id == "main"),
        entry_function: if cleaned.function_id == "unknown" {
            None
        } else {
            Some(cleaned.function_id)
        },
    }
}

pub(super) fn orchestration_facts_from_surface(
    facts: crate::in_lang_parse::InOrchestrationFacts,
) -> OrchestrationFacts {
    let mut runtime_status = Vec::new();
    for name in &facts.enabled_extensions {
        let (implemented, reason_code) = if name == "distributed-workers" {
            (true, "local-distributed-simulator")
        } else {
            (false, "extension-runtime-not-implemented")
        };
        runtime_status.push(RuntimeStatusFact {
            name: name.clone(),
            implemented,
            reason_code: reason_code.to_string(),
        });
    }
    if !facts.distributed_functions.is_empty()
        && !runtime_status
            .iter()
            .any(|status| status.name == "distributed-workers")
    {
        runtime_status.push(RuntimeStatusFact {
            name: "distributed-workers".to_string(),
            implemented: true,
            reason_code: "local-distributed-simulator".to_string(),
        });
    }
    if facts
        .annotations
        .iter()
        .any(|annotation| annotation.name == "gpu")
        && !runtime_status
            .iter()
            .any(|status| status.name == "gpu-optimizer")
    {
        runtime_status.push(RuntimeStatusFact {
            name: "gpu-optimizer".to_string(),
            implemented: false,
            reason_code: "gpu-runtime-not-implemented".to_string(),
        });
    }
    let mut local_plan = Vec::new();
    for (idx, task) in facts.parallel_tasks.iter().enumerate() {
        local_plan.push(OrchestrationPlanStep {
            id: format!("parallel:{}:{idx}", task.region),
            kind: "parallel_task".to_string(),
            name: task.name.clone(),
            mode: "local-deterministic-sequential".to_string(),
            depends_on: Vec::new(),
        });
    }
    for name in &facts.distributed_functions {
        local_plan.push(OrchestrationPlanStep {
            id: format!("distributed:{name}"),
            kind: "distributed_fn".to_string(),
            name: name.clone(),
            mode: "local-worker-simulator".to_string(),
            depends_on: Vec::new(),
        });
    }
    let distributed_jobs = facts
        .distributed_functions
        .iter()
        .enumerate()
        .map(|(idx, name)| DistributedJobFact {
            id: format!("job:{idx}:{name}"),
            function: name.clone(),
            worker: "local-simulated-worker".to_string(),
            max_retries: 0,
            status: "planned".to_string(),
        })
        .collect();
    OrchestrationFacts {
        enabled_extensions: facts.enabled_extensions,
        annotations: facts
            .annotations
            .into_iter()
            .map(|annotation| AnnotationFact {
                name: annotation.name,
                target: annotation.target,
            })
            .collect(),
        distributed_functions: facts.distributed_functions,
        parallel_regions: facts.parallel_regions,
        local_plan,
        distributed_jobs,
        runtime_status,
    }
}
