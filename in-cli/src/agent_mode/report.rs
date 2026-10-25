use super::diagnostics::{
    core_diagnostics, dependency_symbol_call_diagnostics, diagnostic, excerpt_bounds,
};
use super::facts::{
    graph_facts_from_sil, language_level, language_level_for_module,
    orchestration_facts_from_surface, parser_cli, parser_decision, parser_preference_from_cli,
    resolved_parser_id,
};
use super::repair::{micros, repair_plan_for};
use super::summary::summarize_core_ir;
use super::types::*;
use crate::core_ir::UnifiedModule;
use crate::in_lang_parse::InSurfaceInfo;
use crate::parser_registry::{self, ParserCli};
use std::fs;
use std::path::Path;
use std::time::Instant;

pub fn json_report(
    path: impl AsRef<Path>,
    config: &AgentModeConfig,
) -> Result<AgentReport, AgentModeError> {
    build_report(path.as_ref(), config)
}

pub fn explain(
    path: impl AsRef<Path>,
    config: &AgentModeConfig,
) -> Result<AgentExplanation, AgentModeError> {
    let report = build_report(path.as_ref(), config)?;
    let mut summary = Vec::new();
    summary.push(format!(
        "parser {} via {}",
        report
            .parser_decision
            .parser_id
            .as_deref()
            .unwrap_or("swift"),
        report.parser_decision.route
    ));
    if let Some(core) = &report.core_ir_summary {
        summary.push(format!(
            "{} decls, {} functions, {} structs",
            core.decl_count, core.function_count, core.struct_count
        ));
    }
    if let Some(graph) = &report.graph_facts {
        summary.push(format!(
            "{} SIL functions, {} blocks, {} instructions, {} call edges",
            graph.function_count,
            graph.block_count,
            graph.instruction_count,
            graph.call_edges.len()
        ));
    }
    if report.diagnostics.is_empty() {
        summary.push("no diagnostics".to_string());
    } else {
        summary.push(format!("{} diagnostics", report.diagnostics.len()));
    }
    Ok(AgentExplanation {
        schema_version: 1,
        parser_decision: report.parser_decision,
        diagnostics: report.diagnostics,
        summary,
        graph_facts: report.graph_facts,
        size_timing: report.size_timing,
    })
}

pub fn fix_plan_report(
    path: impl AsRef<Path>,
    config: &AgentModeConfig,
) -> Result<AgentFixPlan, AgentModeError> {
    let report = build_report(path.as_ref(), config)?;
    Ok(fix_plan_from_report(report))
}

pub fn analyze_path(cwd: &Path, path: &str, module_id: &str, parser: ParserCli) -> AgentReport {
    let source_path = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    let config = AgentModeConfig {
        parser: parser_preference_from_cli(parser),
        module_id: module_id.to_string(),
    };
    match build_report(&source_path, &config) {
        Ok(report) => report,
        Err(err) => io_error_report(&source_path, &config, err),
    }
}

pub fn fix_plan(cwd: &Path, path: &str, module_id: &str, parser: ParserCli) -> AgentFixPlan {
    let source_path = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    let config = AgentModeConfig {
        parser: parser_preference_from_cli(parser),
        module_id: module_id.to_string(),
    };
    match fix_plan_report(&source_path, &config) {
        Ok(plan) => plan,
        Err(err) => fix_plan_from_report(io_error_report(&source_path, &config, err)),
    }
}

fn fix_plan_from_report(report: AgentReport) -> AgentFixPlan {
    AgentFixPlan {
        schema_version: 1,
        parser_decision: report.parser_decision,
        diagnostics: report.diagnostics,
        repair_plans: report.repair_plans,
        size_timing: report.size_timing,
    }
}

struct SurfaceFacts {
    effects: Vec<String>,
    capabilities: Vec<String>,
    orchestration: OrchestrationFacts,
    package_symbol_index: Vec<crate::package_manifest::PackageSymbolIndexEntry>,
    package_diagnostics: Vec<crate::package_manifest::PackageDiagnostic>,
}

struct CoreIrFacts {
    lower_micros: u64,
    graph_micros: u64,
    textual_sil_bytes: usize,
    textual_sil_lines: usize,
    core_ir_summary: Option<crate::agent_mode::summary::CoreIrSummary>,
    graph_facts: Option<GraphFacts>,
}

fn evaluate_parse_result(
    parsed: Result<Option<UnifiedModule>, crate::parser_registry::ParserRegistryError>,
    path: &Path,
    source: &str,
    parser_id: Option<&str>,
    config: &AgentModeConfig,
    language_level: &mut LanguageLevel,
    diagnostics: &mut Vec<AgentDiagnostic>,
) -> (usize, CoreIrFacts, SurfaceFacts) {
    let mut core_decl_count = 0;

    let mut core_ir_facts = CoreIrFacts {
        lower_micros: 0,
        graph_micros: 0,
        textual_sil_bytes: 0,
        textual_sil_lines: 0,
        core_ir_summary: None,
        graph_facts: None,
    };

    let mut surface_facts = SurfaceFacts {
        effects: Vec::new(),
        capabilities: Vec::new(),
        orchestration: OrchestrationFacts::default(),
        package_symbol_index: Vec::new(),
        package_diagnostics: Vec::new(),
    };

    match parsed {
        Ok(Some(module)) => {
            core_decl_count = module.decls.len();
            *language_level = language_level_for_module(language_level.clone(), parser_id, source);

            if parser_id == Some("in")
                && let Ok(surface) = crate::in_lang_parse::parse_in_surface_info(source)
            {
                surface_facts =
                    extract_surface_facts(surface, path, source, parser_id, &module, diagnostics);
            }

            diagnostics.extend(core_diagnostics(&module, parser_id, source));
            core_ir_facts = extract_core_ir_facts(&module, &config.module_id);
        }
        Ok(None) => {
            diagnostics.push(diagnostic(
                "AGENT_SWIFT_SIL_ROUTE",
                AgentDiagnosticSeverity::Info,
                None,
                parser_id,
                Some("Core IR parser route for agent-mode reports"),
                excerpt_bounds(source, None),
                Some("Pass a .in/.icore/polyglot source with a registered Core IR frontend or force parser=in/icore"),
                "agent-mode core reporting does not invoke the Swift SIL emit route",
            ));
        }
        Err(err) => {
            diagnostics.push(diagnostic(
                "AGENT_PARSE_FAILED",
                AgentDiagnosticSeverity::Error,
                None,
                parser_id,
                Some("source accepted by the resolved parser and lowered to Core IR"),
                excerpt_bounds(source, None),
                Some("Check the parser decision, source extension, magic parser line, and frontend-supported syntax"),
                &err.to_string(),
            ));
        }
    }

    (core_decl_count, core_ir_facts, surface_facts)
}

fn extract_core_ir_facts(module: &UnifiedModule, module_id: &str) -> CoreIrFacts {
    let summary = summarize_core_ir(module);
    let lower_start = Instant::now();
    let textual_sil = lower_textual_sil(module, module_id);
    let lower_micros = micros(lower_start);
    let textual_sil_bytes = textual_sil.len();
    let textual_sil_lines = textual_sil.lines().count();
    let graph_start = Instant::now();
    let graph_facts = Some(graph_facts_from_sil(&textual_sil));
    let graph_micros = micros(graph_start);

    CoreIrFacts {
        lower_micros,
        graph_micros,
        textual_sil_bytes,
        textual_sil_lines,
        core_ir_summary: Some(summary),
        graph_facts,
    }
}

fn build_report(path: &Path, config: &AgentModeConfig) -> Result<AgentReport, AgentModeError> {
    let total_start = Instant::now();
    let source = fs::read_to_string(path)?;
    let source_bytes = source.len();
    let source_lines = source.lines().count();
    let resolved = parser_registry::resolve_parser_id(path, parser_cli(&config.parser));
    let parser_decision = parser_decision(path, &config.parser, resolved);
    let mut language_level = language_level(resolved);
    let parser_id = resolved_parser_id(resolved).map(|id| id.as_str().to_string());
    let mut diagnostics = Vec::new();
    let mut repair_plans = Vec::new();
    let parse_start = Instant::now();
    let parsed = parser_registry::parse_with_resolved(resolved, path);
    let parse_micros = micros(parse_start);

    let (core_decl_count, core_ir_facts, surface_facts) = evaluate_parse_result(
        parsed,
        path,
        &source,
        parser_id.as_deref(),
        config,
        &mut language_level,
        &mut diagnostics,
    );

    repair_plans.extend(
        diagnostics
            .iter()
            .filter_map(|d| repair_plan_for(d, &source)),
    );

    Ok(AgentReport {
        schema_version: 1,
        diagnostics,
        parser_decision,
        language_level,
        core_ir_summary: core_ir_facts.core_ir_summary,
        graph_facts: core_ir_facts.graph_facts,
        orchestration: surface_facts.orchestration,
        effects: surface_facts.effects,
        capabilities: surface_facts.capabilities,
        package_symbol_index: surface_facts.package_symbol_index,
        package_diagnostics: surface_facts.package_diagnostics,
        size_timing: SizeTiming {
            source_bytes,
            source_lines,
            core_decl_count,
            textual_sil_bytes: core_ir_facts.textual_sil_bytes,
            textual_sil_lines: core_ir_facts.textual_sil_lines,
            parse_micros,
            lower_micros: core_ir_facts.lower_micros,
            graph_micros: core_ir_facts.graph_micros,
            total_micros: micros(total_start),
        },
        repair_plans,
    })
}

fn extract_surface_facts(
    surface: InSurfaceInfo,
    path: &Path,
    source: &str,
    parser_id: Option<&str>,
    module: &UnifiedModule,
    diagnostics: &mut Vec<AgentDiagnostic>,
) -> SurfaceFacts {
    let mut effects = Vec::new();
    let mut capabilities = Vec::new();

    let mut package_symbol_index;

    let declared_capabilities = surface.capabilities.clone();
    let mut extern_bindings = surface.externs.clone();
    for import in &surface.imports {
        extern_bindings.extend(crate::in_lang_parse::in_standard_import_bindings(import));
    }
    if let Ok((root, manifest)) = crate::package_manifest::load_package_manifest_from_source(path) {
        let lock = crate::package_lock::discover_package_lock(&root.root).and_then(|lock_root| {
            crate::package_lock::load_package_lock(&lock_root.lock_path).ok()
        });
        for import in &surface.semantic_imports {
            extern_bindings.extend(
                crate::package_extern::package_import_bindings_for_semantic_import(
                    import,
                    &root.root,
                    &manifest,
                    lock.as_ref(),
                ),
            );
        }
    }
    for binding in &extern_bindings {
        for required in &binding.required_capabilities {
            if !declared_capabilities.contains(required) {
                diagnostics.push(diagnostic(
                    "AGENT_MISSING_CAPABILITY",
                    AgentDiagnosticSeverity::Warning,
                    None,
                    parser_id,
                    Some("top-level capability declaration matching an extern binding requirement"),
                    excerpt_bounds(source, None),
                    Some("Add the missing top-level capability declaration or remove the extern requirement"),
                    &format!(
                        "extern {} fn {} requires missing capability {}",
                        binding.language, binding.name, required
                    ),
                ));
            }
        }
    }
    if let Some(package) = surface.package {
        effects.push(format!("package:{package}"));
    }
    if let Some(module_name) = surface.module {
        effects.push(format!("module:{module_name}"));
    }
    let package_manifest = crate::package_manifest::load_package_manifest_from_source(path)
        .ok()
        .map(|(_, manifest)| manifest);
    let semantic_imports = crate::package_manifest::resolve_semantic_imports(
        &surface.semantic_imports,
        package_manifest.as_ref(),
    );
    let semantic_bindings = crate::package_manifest::resolve_semantic_bindings(
        &surface.semantic_bindings,
        &semantic_imports,
    );
    package_symbol_index = if let Ok((root, manifest)) =
        crate::package_manifest::load_package_manifest_from_source(path)
    {
        let lock = crate::package_lock::discover_package_lock(&root.root).and_then(|lock_root| {
            crate::package_lock::load_package_lock(&lock_root.lock_path).ok()
        });
        crate::package_manifest::symbol_index_for_semantic_imports_with_context(
            &semantic_imports,
            Some(&root.root),
            Some(&manifest),
            lock.as_ref(),
        )
    } else {
        crate::package_manifest::symbol_index_for_semantic_imports(&semantic_imports)
    };
    package_symbol_index.extend(crate::package_manifest::symbol_index_for_semantic_bindings(
        &semantic_bindings,
    ));
    let package_diagnostics =
        crate::package_manifest::diagnostics_for_semantic_imports(&semantic_imports);
    for diagnostic_fact in package_diagnostics.iter() {
        diagnostics.push(diagnostic(
            &diagnostic_fact.code,
            AgentDiagnosticSeverity::Warning,
            None,
            parser_id,
            Some("top-level use declaration matching a dependency in the nearest inauguration.package"),
            excerpt_bounds(source, None),
            Some("Declare the dependency in inauguration.package or remove the semantic import"),
            &diagnostic_fact.message,
        ));
    }
    diagnostics.extend(dependency_symbol_call_diagnostics(
        module,
        &package_symbol_index,
        &semantic_bindings
            .iter()
            .filter(|binding| binding.status == "resolved")
            .map(|binding| binding.alias.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        parser_id,
        source,
    ));
    effects.extend(
        semantic_imports
            .into_iter()
            .map(|import| format!("use:{}:{}", import.import, import.status)),
    );
    effects.extend(semantic_bindings.into_iter().map(|binding| {
        format!(
            "bind:{}:{}:{}",
            binding.import, binding.alias, binding.status
        )
    }));
    effects.extend(
        surface
            .imports
            .into_iter()
            .map(|name| format!("import:{name}")),
    );
    effects.extend(extern_bindings.into_iter().map(|binding| {
        if binding.required_capabilities.is_empty() {
            format!("extern:{}:{}", binding.language, binding.name)
        } else {
            format!(
                "extern:{}:{}:requires={}",
                binding.language,
                binding.name,
                binding.required_capabilities.join(",")
            )
        }
    }));
    effects.extend(
        surface
            .orchestration
            .enabled_extensions
            .iter()
            .map(|name| format!("enable:{name}")),
    );
    effects.extend(
        surface
            .orchestration
            .distributed_functions
            .iter()
            .map(|name| format!("distributed:{name}")),
    );
    if surface.orchestration.parallel_regions > 0 {
        effects.push(format!(
            "parallel_regions:{}",
            surface.orchestration.parallel_regions
        ));
    }
    capabilities.extend(surface.capabilities);
    let orchestration = orchestration_facts_from_surface(surface.orchestration);

    SurfaceFacts {
        effects,
        capabilities,
        orchestration,
        package_symbol_index,
        package_diagnostics,
    }
}

fn io_error_report(path: &Path, config: &AgentModeConfig, err: AgentModeError) -> AgentReport {
    let parser_decision = parser_decision(
        path,
        &config.parser,
        parser_registry::resolve_parser_id(path, parser_cli(&config.parser)),
    );
    let diagnostic = diagnostic(
        "AGENT_IO_ERROR",
        AgentDiagnosticSeverity::Error,
        None,
        parser_decision.parser_id.as_deref(),
        Some("readable source file"),
        None,
        Some("Check that the source path exists and is readable"),
        &err.to_string(),
    );
    let repair_plans = repair_plan_for(&diagnostic, "").into_iter().collect();
    AgentReport {
        schema_version: 1,
        diagnostics: vec![diagnostic],
        parser_decision,
        language_level: language_level(parser_registry::resolve_parser_id(
            path,
            parser_cli(&config.parser),
        )),
        core_ir_summary: None,
        graph_facts: None,
        orchestration: OrchestrationFacts::default(),
        effects: Vec::new(),
        capabilities: Vec::new(),
        package_symbol_index: Vec::new(),
        package_diagnostics: Vec::new(),
        size_timing: SizeTiming {
            source_bytes: 0,
            source_lines: 0,
            core_decl_count: 0,
            textual_sil_bytes: 0,
            textual_sil_lines: 0,
            parse_micros: 0,
            lower_micros: 0,
            graph_micros: 0,
            total_micros: 0,
        },
        repair_plans,
    }
}

fn lower_textual_sil(module: &UnifiedModule, module_id: &str) -> String {
    let effective_module_id = module.effective_module_id(module_id);
    let sil = crate::compiler::driver::lower_unified_module(module, effective_module_id);
    debug_assert_eq!(
        sil,
        crate::lower_core::lower_to_textual_sil(module.clone(), effective_module_id)
    );
    sil
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_mode::types::{AgentModeConfig, AgentParserPreference};
    use std::fs;

    struct TempFile(std::path::PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_json_report() {
        let dir = std::env::temp_dir();
        let file_path = dir.join(format!("test_json_report_{}.in", std::process::id()));
        let temp_file = TempFile(file_path.clone());
        fs::write(&temp_file.0, "fn main() {}\n").unwrap();

        let config = AgentModeConfig {
            parser: AgentParserPreference::Auto,
            module_id: "App".to_string(),
        };

        let result = json_report(&temp_file.0, &config);
        assert!(result.is_ok());
        let report = result.unwrap();

        assert_eq!(report.schema_version, 1);
        assert_eq!(report.parser_decision.parser_id.as_deref(), Some("in"));

        let missing_path = dir.join(format!("missing_json_report_{}.in", std::process::id()));
        let result_missing = json_report(&missing_path, &config);
        assert!(result_missing.is_err());
        match result_missing {
            Err(AgentModeError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
            _ => panic!("Expected IO error for missing file"),
        }
    }
}
