//! Stage foreign-language sources named by a project manifest into one JIT unit.
//!
//! Borrowed from Valen's `Valen.toml` orchestrator (`stage_workspace`): a
//! manifest lists sources the driver does not parse itself, and the build
//! copies them into a workspace the native backend can see. Here the sources
//! are any front this compiler already lowers (C, Go, Python, …), and the
//! backend is the in-process JIT rather than a staged rustc.

use std::fs;
use std::path::{Path, PathBuf};

use crate::boundary_ir::{UnaryMethodBindings, merge_unary_method_bindings};
use crate::core_ir::{Decl, UnifiedModule};
use crate::parser_registry::{self, ParserCli, ParserId};

pub const FOREIGN_STAGE_FILE: &str = "inauguration.foreign.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignStageManifest {
    pub project: ForeignProject,
    pub sources: Vec<ForeignSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignProject {
    pub name: String,
    pub entry: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignSource {
    pub path: String,
    /// Parser id (`c`, `go`, `python`, …). `None` means resolve from the extension.
    pub parser: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedModule {
    pub manifest: ForeignStageManifest,
    pub module: UnifiedModule,
    pub unary_methods: UnaryMethodBindings,
    pub staged_files: Vec<PathBuf>,
}

/// Load `inauguration.foreign.toml` from `dir` when it exists.
pub fn load_manifest(dir: &Path) -> Result<Option<ForeignStageManifest>, String> {
    let path = dir.join(FOREIGN_STAGE_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let text =
        fs::read_to_string(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let manifest =
        parse_manifest(&text).map_err(|err| format!("parse {}: {err}", path.display()))?;
    if manifest.project.name.is_empty() {
        return Err(format!("{}: project.name is empty", path.display()));
    }
    Ok(Some(manifest))
}

/// Parse every listed source with its own front and merge the declarations.
///
/// Later sources win on a duplicate function name, matching Valen's staged
/// workspace where a later input replaces an earlier one of the same symbol.
pub fn stage_sources(dir: &Path, manifest: &ForeignStageManifest) -> Result<StagedModule, String> {
    stage_sources_with_semantics(
        dir,
        manifest,
        crate::integer_semantics::IntegerSemantics::Word64,
    )
}

pub(crate) fn stage_sources_with_semantics(
    dir: &Path,
    manifest: &ForeignStageManifest,
    semantics: crate::integer_semantics::IntegerSemantics,
) -> Result<StagedModule, String> {
    let mut merged = UnifiedModule::new(Vec::new());
    merged.identity.package = Some(manifest.project.name.clone());
    let mut staged_files = Vec::new();
    let mut unary_methods = UnaryMethodBindings::new();
    for source in &manifest.sources {
        let path = dir.join(&source.path);
        if !path.is_file() {
            return Err(format!("foreign stage: {} is not a file", path.display()));
        }
        let parser = match source.parser.as_deref() {
            Some(id) => {
                let parsed = parser_registry::parser_id_from_cli_token(id).ok_or_else(|| {
                    format!(
                        "foreign stage: unknown parser `{id}` for {}",
                        path.display()
                    )
                })?;
                parser_cli_for(parsed)
            }
            None => ParserCli::Auto,
        };
        let resolved = parser_registry::resolve_parser_id(&path, parser);
        let mut artifact = parser_registry::parse_artifact_with_resolved(resolved, &path)
            .map_err(|err| format!("foreign stage: {}: {err}", path.display()))?
            .ok_or_else(|| format!("foreign stage: {} did not produce Core IR", path.display()))?;
        if let parser_registry::ResolvedBuildParser::CoreIr(id) = resolved
            && crate::typecheck::uses_family_normalization(id)
        {
            artifact.semantic = crate::typecheck::normalize_module_with_semantics(
                id,
                &artifact.semantic,
                semantics,
            );
        }
        merge_unary_method_bindings(&mut unary_methods, artifact.unary_methods)?;
        merge_module(&mut merged, artifact.semantic);
        staged_files.push(path);
    }
    Ok(StagedModule {
        manifest: manifest.clone(),
        module: merged,
        unary_methods,
        staged_files,
    })
}

fn parser_cli_for(id: ParserId) -> ParserCli {
    match id {
        ParserId::In => ParserCli::In,
        ParserId::Icore => ParserCli::Icore,
        other => ParserCli::Force(other),
    }
}

/// Minimal reader for the stage manifest. The file is small and fixed-shape,
/// so it does not pull in a TOML crate.
fn parse_manifest(text: &str) -> Result<ForeignStageManifest, String> {
    let mut name = None;
    let mut entry = "main".to_string();
    let mut sources = Vec::new();
    let mut in_project = false;
    let mut current: Option<ForeignSource> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line == "[project]" {
            finish_source(&mut sources, &mut current)?;
            in_project = true;
            continue;
        }
        if line == "[[sources]]" {
            finish_source(&mut sources, &mut current)?;
            in_project = false;
            current = Some(ForeignSource {
                path: String::new(),
                parser: None,
            });
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("foreign stage: expected key = value, got `{line}`"))?;
        let key = key.trim();
        let value = unquote(value.trim());
        if in_project {
            match key {
                "name" => name = Some(value),
                "entry" => entry = value,
                other => return Err(format!("foreign stage: unknown project key `{other}`")),
            }
        } else if let Some(source) = current.as_mut() {
            match key {
                "path" => source.path = value,
                "parser" => source.parser = Some(value),
                other => return Err(format!("foreign stage: unknown source key `{other}`")),
            }
        } else {
            return Err(format!(
                "foreign stage: `{key}` is outside [project] or [[sources]]"
            ));
        }
    }
    finish_source(&mut sources, &mut current)?;
    let name = name.ok_or("foreign stage: [project] name is required")?;
    Ok(ForeignStageManifest {
        project: ForeignProject { name, entry },
        sources,
    })
}

fn finish_source(
    sources: &mut Vec<ForeignSource>,
    current: &mut Option<ForeignSource>,
) -> Result<(), String> {
    if let Some(source) = current.take() {
        if source.path.is_empty() {
            return Err("foreign stage: [[sources]] is missing path".to_string());
        }
        sources.push(source);
    }
    Ok(())
}

fn unquote(value: &str) -> String {
    value.trim_matches('"').trim_matches('\'').to_string()
}

fn merge_module(into: &mut UnifiedModule, from: UnifiedModule) {
    into.regions.extend(from.regions);
    for decl in from.decls {
        if let Decl::Function { name, .. } = &decl
            && let Some(slot) = into.decls.iter_mut().find(|existing| match existing {
                Decl::Function {
                    name: existing_name,
                    ..
                } => existing_name == name,
                _ => false,
            })
        {
            *slot = decl;
            continue;
        }
        into.decls.push(decl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::RegionKind;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "in-foreign-stage-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn missing_manifest_is_none() {
        let dir = scratch();
        assert!(load_manifest(&dir).expect("load").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_manifest_parses_valid_manifest() {
        let dir = scratch();
        fs::write(
            dir.join(FOREIGN_STAGE_FILE),
            "[project]\nname = \"test\"\nentry = \"main\"\n[[sources]]\npath = \"a.c\"\n[[sources]]\npath = \"b.c\"\nparser = \"c\"\n",
        )
        .expect("manifest");
        let manifest = load_manifest(&dir).expect("load").expect("present");
        assert_eq!(manifest.project.name, "test");
        assert_eq!(manifest.project.entry, "main");
        assert_eq!(manifest.sources.len(), 2);
        assert_eq!(manifest.sources[0].path, "a.c");
        assert_eq!(manifest.sources[0].parser, None);
        assert_eq!(manifest.sources[1].path, "b.c");
        assert_eq!(manifest.sources[1].parser, Some("c".to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_manifest_returns_error_if_project_name_empty() {
        let dir = scratch();
        fs::write(dir.join(FOREIGN_STAGE_FILE), "[project]\nname = \"\"\n").expect("manifest");
        let res = load_manifest(&dir);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("project.name is empty"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stages_c_and_marks_noalias_across_the_merge() {
        let dir = scratch();
        fs::write(
            dir.join("answer.c"),
            "int answer(void) { return 42; }\nint main(void) { return 0; }\n",
        )
        .expect("c");
        fs::write(
            dir.join(FOREIGN_STAGE_FILE),
            "\
[project]
name = \"staged\"
entry = \"answer\"

[[sources]]
path = \"answer.c\"
parser = \"c\"
",
        )
        .expect("manifest");
        let manifest = load_manifest(&dir).expect("load").expect("present");
        let staged = stage_sources(&dir, &manifest).expect("stage");
        assert_eq!(staged.manifest.project.entry, "answer");
        assert!(staged.module.decls.iter().any(|decl| matches!(
            decl,
            Decl::Function { name, .. } if name == "answer"
        )));
        let mut module = staged.module;
        module.mark_noalias(&["left", "right"]);
        module.mark_noalias(&["left"]);
        assert_eq!(
            module.noalias_params(),
            vec!["left".to_string(), "right".to_string()]
        );
        assert_eq!(
            crate::region_facts::regions(&module)[0].kind,
            RegionKind::Mut
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn later_source_replaces_a_duplicate_function() {
        let dir = scratch();
        fs::write(dir.join("a.c"), "int answer(void) { return 1; }\n").expect("a");
        fs::write(dir.join("b.c"), "int answer(void) { return 2; }\n").expect("b");
        let manifest = ForeignStageManifest {
            project: ForeignProject {
                name: "dup".to_string(),
                entry: "answer".to_string(),
            },
            sources: vec![
                ForeignSource {
                    path: "a.c".to_string(),
                    parser: Some("c".to_string()),
                },
                ForeignSource {
                    path: "b.c".to_string(),
                    parser: Some("c".to_string()),
                },
            ],
        };
        let staged = stage_sources(&dir, &manifest).expect("stage");
        let answers: Vec<_> = staged
            .module
            .decls
            .iter()
            .filter(|decl| matches!(decl, Decl::Function { name, .. } if name == "answer"))
            .collect();
        assert_eq!(answers.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn driver_uses_manifest_entry_and_retains_rust_operator_proofs() {
        use crate::owned_compile::{CompileTarget, EvalValue, OwnedCompileRequest, compile_owned};
        let dir = scratch();
        fs::write(dir.join("extra.c"), "int unused(void){return 7;}").unwrap();
        fs::write(dir.join("answer.rs"), "struct Value{value:i64}impl std::ops::Neg for Value{type Output=i64;fn neg(self)->i64{self.value+22}}pub fn answer()->i64{-Value{value:20}}fn main()->i64{7}").unwrap();
        let path = dir.join(FOREIGN_STAGE_FILE);
        fs::write(&path, "[project]\nname=\"stage-test\"\nentry=\"answer\"\n[[sources]]\npath=\"extra.c\"\n[[sources]]\npath=\"answer.rs\"\n").unwrap();
        let request = OwnedCompileRequest {
            path,
            module_id: "App".into(),
            parser: ParserCli::Auto,
            target: CompileTarget::Jit,
            entry: None,
            out: None,
            linkage: crate::native_emit::NativeLinkage::Executable,
            target_triple: None,
            jobs: 1,
            debug: false,
            profile: crate::emit_profile::EmitProfile::Default,
            emit: None,
            base: None,
        };
        let report = compile_owned(&request);
        assert!(report.success && report.execution_started, "{report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
        fs::remove_dir_all(dir).unwrap();
    }
}
