use crate::boundary_emit::emit_abi_manifest;
use crate::boundary_verify::boundary_ir_verify;
use crate::compiler::driver;
use crate::core_ir::{Decl, UnifiedModule};
use crate::language_support::LanguageSupport;
use crate::parser_registry::{self, ParserId, ResolvedBuildParser};
use crate::typecheck;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const GATE_REPORTED: &str = "reported";
pub const GATE_CORE_IR_DECLS: &str = "core-ir-decls";
pub const GATE_CORE_IR_BODIES: &str = "core-ir-bodies";
pub const GATE_TEXTUAL_SIL: &str = "textual-sil";
pub const GATE_SEMANTIC_TYPECHECK: &str = "semantic-typecheck";
pub const GATE_BOUNDARY_IR_VERIFY: &str = "boundary-ir-verify";
pub const GATE_BOUNDARY_IR_ATTACH: &str = "boundary-ir-attach";
pub const GATE_BOUNDARY_EXTRACT: &str = "boundary-extract";
pub const GATE_ABI_LAYOUT_HASH: &str = "abi-layout-hash";
pub const GATE_ABI_EMIT: &str = "abi-emit";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LanguageGateReport {
    pub sample_path: Option<String>,
    pub passed_gates: Vec<&'static str>,
    pub blocking_gates: Vec<&'static str>,
}

pub fn polyglot_sample_for(entry: &LanguageSupport) -> Option<&'static str> {
    match entry.language {
        "in" => Some("apps/polyglot-sample/sample.in"),
        "icore" => Some("apps/polyglot-sample/sample.icore"),
        "Swift" => Some("apps/polyglot-sample/sample.swift"),
        "Rust" => Some("apps/polyglot-sample/sample.rs"),
        "Go" => Some("apps/polyglot-sample/sample.go"),
        #[cfg(feature = "parse-extended")]
        "V" => Some("apps/polyglot-sample/sample.v"),
        "C" => Some("apps/polyglot-sample/sample.c"),
        "C++" => Some("apps/polyglot-sample/sample.cpp"),
        #[cfg(feature = "parse-extended")]
        "Objective-C" => Some("apps/polyglot-sample/sample.m"),
        "Objective-C++" => Some("apps/polyglot-sample/sample.mm"),
        "Java" => Some("apps/polyglot-sample/Sample.java"),
        #[cfg(feature = "parse-extended")]
        "Groovy" => Some("apps/polyglot-sample/Sample.groovy"),
        "JavaScript" => Some("apps/polyglot-sample/sample.js"),
        "TypeScript" => Some("apps/polyglot-sample/sample.ts"),
        #[cfg(feature = "parse-extended")]
        "Kotlin" => Some("apps/polyglot-sample/Sample.kt"),
        #[cfg(feature = "parse-extended")]
        "Scala" => Some("apps/polyglot-sample/sample.scala"),
        #[cfg(feature = "parse-extended")]
        "C#" => Some("apps/polyglot-sample/Program.cs"),
        #[cfg(feature = "parse-extended")]
        "F#" => Some("apps/polyglot-sample/sample.fs"),
        "Python" => Some("apps/polyglot-sample/sample.py"),
        "Ruby" => Some("apps/polyglot-sample/sample.rb"),
        #[cfg(feature = "parse-extended")]
        "PHP" => Some("apps/polyglot-sample/sample.php"),
        "Perl" => Some("apps/polyglot-sample/sample.pl"),
        "Zig" => Some("apps/polyglot-sample/sample.zig"),
        #[cfg(feature = "parse-extended")]
        "Dart" => Some("apps/polyglot-sample/sample.dart"),
        "Lua" => Some("apps/polyglot-sample/sample.lua"),
        #[cfg(feature = "parse-extended")]
        "Elixir" => Some("apps/polyglot-sample/sample.ex"),
        #[cfg(feature = "parse-extended")]
        "Erlang" => Some("apps/polyglot-sample/sample.erl"),
        #[cfg(feature = "parse-extended")]
        "Haskell" => Some("apps/polyglot-sample/sample.hs"),
        "Nim" => Some("apps/polyglot-sample/sample.nim"),
        #[cfg(feature = "parse-extended")]
        "OCaml" => Some("apps/polyglot-sample/sample.ml"),
        #[cfg(feature = "parse-extended")]
        "Julia" => Some("apps/polyglot-sample/sample.jl"),
        #[cfg(feature = "parse-extended")]
        "R" => Some("apps/polyglot-sample/sample.r"),
        "Odin" => Some("apps/polyglot-sample/sample.odin"),
        "Hare" => Some("apps/polyglot-sample/sample.ha"),
        "HolyC" => Some("apps/polyglot-sample/sample.hc"),
        "D" => Some("apps/polyglot-sample/sample.d"),
        "Crystal" => Some("apps/polyglot-sample/sample.cr"),
        "Clojure" => Some("apps/polyglot-sample/sample.clj"),
        "VB.NET" => Some("apps/polyglot-sample/sample.vb"),
        _ => None,
    }
}

pub fn evaluate_language_gates(entry: &LanguageSupport, repo_root: &Path) -> LanguageGateReport {
    let Some(sample_rel) = polyglot_sample_for(entry) else {
        return LanguageGateReport {
            sample_path: None,
            passed_gates: vec![GATE_REPORTED],
            blocking_gates: vec![GATE_CORE_IR_DECLS],
        };
    };
    let sample_path = repo_root.join(sample_rel);
    if !sample_path.is_file() {
        return LanguageGateReport {
            sample_path: Some(sample_rel.to_string()),
            passed_gates: vec![GATE_REPORTED],
            blocking_gates: vec![GATE_CORE_IR_DECLS],
        };
    }
    evaluate_path(&sample_path, entry)
}

pub fn evaluate_path(path: &Path, _entry: &LanguageSupport) -> LanguageGateReport {
    let sample_path = path.display().to_string();
    let mut passed = vec![GATE_REPORTED];
    let mut blocking = Vec::new();

    let resolved = parser_registry::resolve_parser_id(path, parser_registry::ParserCli::Auto);

    let parse_result = parser_registry::parse_with_resolved(resolved, path);
    let module = match parse_result {
        Ok(Some(m)) => m,
        Ok(None) => {
            blocking.push(GATE_CORE_IR_DECLS);
            return finish(sample_path, passed, blocking);
        }
        Err(e) => {
            eprintln!("[gate] parse failed for {}: {e}", path.display());
            blocking.push(GATE_CORE_IR_DECLS);
            return finish(sample_path, passed, blocking);
        }
    };

    passed.push(GATE_CORE_IR_DECLS);
    if module_has_bodies(&module) {
        passed.push(GATE_CORE_IR_BODIES);
    } else {
        blocking.push(GATE_CORE_IR_BODIES);
        return finish(sample_path, passed, blocking);
    }

    let sil = driver::lower_unified_module(&module, "App");
    if sil.contains("sil @") {
        passed.push(GATE_TEXTUAL_SIL);
    } else {
        blocking.push(GATE_TEXTUAL_SIL);
        return finish(sample_path, passed, blocking);
    }

    if typecheck::typecheck_resolved(&resolved, &module).is_ok() {
        passed.push(GATE_SEMANTIC_TYPECHECK);
    } else {
        blocking.push(GATE_SEMANTIC_TYPECHECK);
        return finish(sample_path, passed, blocking);
    }

    if let Some(boundary) = load_boundary(path, &resolved) {
        passed.push(GATE_BOUNDARY_IR_ATTACH);
        let report = boundary_ir_verify(&boundary);
        if report.ok {
            passed.push(GATE_BOUNDARY_IR_VERIFY);
            if !boundary.layouts.is_empty() || !boundary.symbols.is_empty() {
                passed.push(GATE_BOUNDARY_EXTRACT);
            }
            if !boundary.layout_hash.is_empty() || !boundary.compute_layout_hash().is_empty() {
                passed.push(GATE_ABI_LAYOUT_HASH);
            }
            let manifest = emit_abi_manifest(&boundary);
            if !manifest.is_empty() {
                passed.push(GATE_ABI_EMIT);
            }
        } else {
            blocking.push(GATE_BOUNDARY_IR_VERIFY);
        }
    }

    finish(sample_path, passed, blocking)
}

fn load_boundary(
    path: &Path,
    resolved: &ResolvedBuildParser,
) -> Option<crate::boundary_ir::BoundaryModule> {
    let artifact = match resolved {
        ResolvedBuildParser::CoreIr(ParserId::Icore) => {
            crate::compiler::icore::parse_icore_artifact(path).ok()
        }
        ResolvedBuildParser::CoreIr(ParserId::Zig) => {
            crate::compiler::tree_front::parse_zig_artifact(path).ok()
        }
        ResolvedBuildParser::CoreIr(ParserId::Rust) => {
            crate::compiler::rust_front::parse_rust_artifact(path).ok()
        }
        _ => None,
    }?;
    artifact.boundary
}

fn module_has_bodies(module: &UnifiedModule) -> bool {
    module.decls.iter().any(|decl| match decl {
        Decl::Function { body, .. } => !body.is_empty(),
        Decl::Class { methods, .. } => methods.iter().any(|m| match m {
            Decl::Function { body, .. } => !body.is_empty(),
            _ => false,
        }),
        _ => false,
    })
}

fn finish(
    sample_path: String,
    passed: Vec<&'static str>,
    blocking: Vec<&'static str>,
) -> LanguageGateReport {
    LanguageGateReport {
        sample_path: Some(sample_path),
        passed_gates: dedup(passed),
        blocking_gates: dedup(blocking),
    }
}

fn dedup(gates: Vec<&'static str>) -> Vec<&'static str> {
    let mut out = Vec::new();
    for gate in gates {
        if !out.contains(&gate) {
            out.push(gate);
        }
    }
    out
}

#[must_use]
pub fn repo_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.pop();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_support::language_support_for_parser;

    fn repo_root() -> PathBuf {
        super::repo_root()
    }

    #[test]
    fn in_sample_reaches_high_gates() {
        let entry = language_support_for_parser("in").expect("in");
        let report = evaluate_language_gates(entry, &repo_root());
        assert!(
            report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK),
            "{report:?}"
        );
        assert!(report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK));
    }

    #[test]
    fn zig_sample_reaches_family_typecheck() {
        let entry = language_support_for_parser("zig").expect("zig");
        let report = evaluate_language_gates(entry, &repo_root());
        assert!(
            report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK),
            "{report:?}"
        );
        assert!(report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK));
    }

    #[test]
    fn lua_sample_reaches_family_typecheck() {
        let entry = language_support_for_parser("lua").expect("lua");
        let report = evaluate_language_gates(entry, &repo_root());
        assert!(
            report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK),
            "{report:?}"
        );
    }

    #[test]
    fn javascript_sample_reaches_typecheck() {
        let entry = language_support_for_parser("javascript").expect("javascript");
        let report = evaluate_language_gates(entry, &repo_root());
        assert!(report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK));
    }

    #[test]
    fn typescript_sample_reaches_typecheck() {
        let entry = language_support_for_parser("typescript").expect("typescript");
        let report = evaluate_language_gates(entry, &repo_root());
        assert!(report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK));
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn jvm_and_dotnet_samples_reach_family_typecheck() {
        let root = repo_root();
        for parser_id in ["java", "kotlin", "csharp"] {
            let entry = language_support_for_parser(parser_id).expect(parser_id);
            let report = evaluate_language_gates(entry, &root);
            assert!(
                report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK),
                "{parser_id}: {report:?}"
            );
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn promoted_tree_and_boundary_samples_reach_declared_gates() {
        let root = repo_root();
        for parser_id in [
            "php",
            "lua",
            "rust",
            "javascript",
            "typescript",
            "scala",
            "perl",
        ] {
            let entry = language_support_for_parser(parser_id).expect(parser_id);
            let report = evaluate_language_gates(entry, &root);
            assert!(
                report.passed_gates.contains(&GATE_CORE_IR_DECLS),
                "{parser_id} should pass core-ir-decls: {report:?}"
            );
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn mandatory_samples_pass_core_ir_decls() {
        let root = repo_root();
        for entry in crate::language_support::all_language_support() {
            if polyglot_sample_for(entry).is_none() {
                continue;
            }
            let sample = root.join(polyglot_sample_for(entry).unwrap());
            if !sample.is_file() {
                continue;
            }
            let report = evaluate_language_gates(entry, &root);
            assert!(
                report.passed_gates.contains(&GATE_CORE_IR_DECLS),
                "{} should pass core-ir-decls: {:?}",
                entry.language,
                report
            );
        }
    }

    #[test]
    fn javascript_inline_boundary_still_passes_typecheck() {
        let path = std::env::temp_dir().join(format!(
            "in-js-boundary-{}-{}.js",
            std::process::id(),
            "level-four"
        ));
        std::fs::write(
            &path,
            r#"function answer() {
  return 42;
}
function main() {}
"#,
        )
        .expect("write js fixture");
        let entry = language_support_for_parser("javascript").expect("javascript");
        let report = evaluate_path(&path, entry);
        let _ = std::fs::remove_file(&path);
        assert!(report.passed_gates.contains(&GATE_SEMANTIC_TYPECHECK));
        assert!(report.passed_gates.contains(&GATE_TEXTUAL_SIL));
    }
}
