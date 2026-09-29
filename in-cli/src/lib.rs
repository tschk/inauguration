#![allow(
    clippy::collapsible_if,
    clippy::comparison_chain,
    clippy::if_not_else,
    clippy::if_same_then_else,
    clippy::len_zero,
    clippy::len_without_is_empty,
    clippy::assertions_on_constants,
    clippy::manual_strip,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_range_loop,
    clippy::unnecessary_find_map,
    clippy::unnecessary_map_or,
    clippy::useless_format,
    clippy::needless_question_mark,
    clippy::never_loop,
    clippy::slow_vector_initialization,
    clippy::stable_sort_primitive,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::unnecessary_cast,
    clippy::useless_conversion
)]
//! Library crate backing the `in` CLI — hybrid compiler wave plus embedded hotreload daemon.

#[cfg(unix)]
pub mod preview_client;

pub mod agent_mode;
pub mod arena;
pub mod boundary_capability;
pub mod boundary_emit;
pub mod boundary_ir;
pub mod boundary_verify;
pub mod cargo_linker;
pub mod compile_cache;
pub mod compile_error;
pub mod compiler;
pub mod config;
pub mod core_ir;
pub mod core_ir_verifier;
pub mod coverage;
pub mod crate_db;
#[cfg(unix)]
pub mod daemon_client;
#[cfg(unix)]
pub mod daemon_server;
pub mod dep_resolver;
pub mod dynamic_module;
pub mod external_guard;
pub mod graph_report;
pub mod hotreload;
pub mod hybrid_sil;
pub mod in_canonical;
pub mod in_lang_parse;
pub mod inrt;
pub mod jit_runtime;
pub mod language_gates;
pub mod language_support;
pub mod lower_core;
pub mod module_resolver;
pub mod native_backend;
pub mod native_emit;
pub mod native_runtime;
pub mod native_stdlib;
pub mod owned_compile;
pub mod package_discover;
pub mod package_extern;
pub mod package_install;
pub mod package_lock;
pub mod package_manifest;
pub mod package_ref;
pub mod package_runtime;
pub mod parser_registry;
pub mod target;
pub mod typecheck;

#[cfg(test)]
mod in_pipeline_tests {
    use crate::compiler::{driver, icore, tree_front};
    use crate::core_ir::Decl;
    use crate::core_ir::{Expr, Stmt, Typ};
    use crate::hybrid_sil;
    use crate::in_lang_parse;
    use crate::lower_core;
    use crate::parser_registry::ParserId;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(path: PathBuf) -> Self {
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn minimal_in_source_to_sil_contains_main() {
        let src = "fn main() -> void\n";
        let module = in_lang_parse::parse_in_source(src).expect("parse .in");
        let sil = lower_core::lower_to_textual_sil(module, "App");
        assert!(
            sil.contains("sil @main"),
            "expected textual SIL to declare @main, got:\n{sil}"
        );
    }

    #[test]
    fn in_sample_shape_lowers_main_body() {
        let src = r#"
struct Session {
  Int id
  String label
}
fn note(text: String) -> void { return; }
fn main() -> void { let seed: Int = 0; note("ready"); return; }
"#;
        let module = in_lang_parse::parse_in_source(src).expect("parse .in");
        let sil = lower_core::lower_to_textual_sil(module, "App");
        assert!(sil.contains("integer_literal $Builtin.Int64, 0"));
        assert!(sil.contains("function_ref @note"));
        assert!(sil.contains("return %"));
    }

    #[test]
    fn in_extern_call_lowers_graph_edge() {
        let src = r#"
import host.log;
capability process.stdout;
extern rust fn host-log(text: String) -> void;
fn main() -> void { host-log("ready"); return; }
"#;
        let module = in_lang_parse::parse_in_source(src).expect("parse .in");
        let sil = lower_core::lower_to_textual_sil(module, "App");
        assert!(sil.contains("sil @host-log"), "sil:\n{sil}");
        assert!(sil.contains("sil @main"), "sil:\n{sil}");
        assert!(sil.contains("function_ref @host-log"), "sil:\n{sil}");
        let artifact = hybrid_sil::parse_textual_sil(&sil);
        let cleaned = hybrid_sil::remove_debug_insts(&artifact);
        let report = hybrid_sil::extract_call_graph(&cleaned);
        assert!(
            report
                .call_edges
                .contains(&("main".into(), "host-log".into())),
            "sil:\n{sil}"
        );
    }

    #[test]
    fn minimal_icore_json_to_sil_contains_main() {
        let j = r#"{
            "icoreVersion": 1,
            "decls": [
                { "kind": "struct", "name": "S", "fields": [{ "name": "x", "type": "Int" }] },
                { "kind": "function", "name": "main", "params": [], "return": "Void", "body": [] }
            ]
        }"#;
        let module = icore::parse_icore_source(j).expect("icore");
        let sil = driver::lower_unified_module(&module, "App");
        assert!(sil.contains("sil @main"), "sil:\n{sil}");
    }

    #[test]
    fn java_tree_front_lowers_to_textual_sil() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "inauguration-java-tree-front-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let _guard = TempDirGuard::new(temp_dir.clone());

        let path = temp_dir.join("Hello.java");
        fs::write(
            &path,
            r#"
public class Hello {
  private static int helper(int value) { return value; }
  private static int twice(int value) { return helper(value); }
  public static void main(String[] args) { twice(1); }
}
"#,
        )
        .expect("write Java source");

        let module = tree_front::parse_polyglot_file(ParserId::Java, &path).expect("parse Java");
        let sil = driver::lower_unified_module(&module, "App");
        assert!(sil.contains("sil @helper"), "sil:\n{sil}");
        assert!(sil.contains("sil @twice"), "sil:\n{sil}");
        assert!(sil.contains("sil @main"), "sil:\n{sil}");
        assert!(sil.contains("function_ref @helper"), "sil:\n{sil}");
        assert!(sil.contains("function_ref @twice"), "sil:\n{sil}");
        let artifact = hybrid_sil::parse_textual_sil(&sil);
        let cleaned = hybrid_sil::remove_debug_insts(&artifact);
        let report = hybrid_sil::extract_call_graph(&cleaned);
        assert!(
            report
                .call_edges
                .contains(&("twice".into(), "helper".into())),
            "sil:\n{sil}"
        );
        assert!(
            report.call_edges.contains(&("main".into(), "twice".into())),
            "sil:\n{sil}"
        );
        assert!(
            !artifact.instructions.is_empty() || !report.call_edges.is_empty(),
            "hybrid_sil should see instructions or call edges from lowered Java SIL; sil:\n{sil}"
        );
    }

    #[test]
    fn holyc_tree_front_extracts_main_and_bare_call() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "inauguration-holyc-tree-front-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let _guard = TempDirGuard::new(temp_dir.clone());

        let path = temp_dir.join("sample.hc");
        fs::write(
            &path,
            r#"U0 Main()
{
  "Hello\n";
}
Main;
"#,
        )
        .expect("write HolyC source");

        let module = tree_front::parse_polyglot_file(ParserId::HolyC, &path).expect("parse HolyC");
        let names: Vec<_> = module
            .decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"main"), "decls: {names:?}");
        let main = module
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, ret, .. } => {
                assert!(matches!(ret, Typ::Void));
                assert!(!body.is_empty(), "expected print stmt in Main body");
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn c_tree_front_lowers_trivial_return_and_helpers() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "inauguration-c-tree-front-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let _guard = TempDirGuard::new(temp_dir.clone());

        let path = temp_dir.join("sample.c");
        fs::write(
            &path,
            r#"int helper(void) { return 42; }
int main(void) { return 0; }
"#,
        )
        .expect("write C source");

        let module = tree_front::parse_polyglot_file(ParserId::C, &path).expect("parse C");
        let sil = driver::lower_unified_module(&module, "App");
        assert!(sil.contains("sil @helper"), "sil:\n{sil}");
        assert!(sil.contains("sil @main"), "sil:\n{sil}");
        assert!(
            sil.contains("integer_literal $Builtin.Int64, 42"),
            "helper return literal; sil:\n{sil}"
        );
        assert!(
            sil.contains("integer_literal $Builtin.Int64, 0"),
            "main return literal; sil:\n{sil}"
        );
        let artifact = hybrid_sil::parse_textual_sil(&sil);
        let cleaned = hybrid_sil::remove_debug_insts(&artifact);
        let report = hybrid_sil::extract_call_graph(&cleaned);
        assert!(
            !report
                .call_edges
                .contains(&("main".into(), "helper".into())),
            "C helper declarations should not synthesize call edges; sil:\n{sil}"
        );
        assert!(
            !artifact.instructions.is_empty() || !report.call_edges.is_empty(),
            "hybrid_sil should see instructions or call edges from lowered C SIL; sil:\n{sil}"
        );
    }

    #[test]
    fn c_tree_front_return_statement_uses_parameter_ident() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before UNIX_EPOCH")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "inauguration-c-tree-front-ident-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let _guard = TempDirGuard::new(temp_dir.clone());

        let path = temp_dir.join("echo.c");
        fs::write(
            &path,
            "int echo(int x) { return x; }\nint main(void) { return 0; }\n",
        )
        .expect("write C source");

        let module = tree_front::parse_polyglot_file(ParserId::C, &path).expect("parse C");
        let echo = module
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "echo"))
            .expect("echo fn");
        match echo {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body.as_slice(),
                    &[Stmt::Return(Some(Expr::Ident("x".into())))]
                );
            }
            _ => panic!("expected function"),
        }
        let sil = driver::lower_unified_module(&module, "App");
        assert!(sil.contains("sil @echo"), "sil:\n{sil}");
    }
}
pub mod core_opt;
pub mod emit_profile;
