use super::*;
use inauguration::parser_registry::ParserCli;

#[test]
fn parse_build_subcommand() {
    let cli = Cli::try_parse_from(["in", "build", "--path", "Foo.swift", "--module-id", "Foo"])
        .expect("cli parse");
    match cli.command {
        Commands::Build {
            path,
            out,
            release,
            module_id,
            verbose,
            swiftpm,
            allow_external_toolchain,
            parser,
            profile,
            harden,
            lean,
        } => {
            assert_eq!(path, "Foo.swift");
            assert_eq!(out, None);
            assert!(!release);
            assert_eq!(module_id, "Foo");
            assert!(!verbose);
            assert!(!swiftpm);
            assert!(!allow_external_toolchain);
            assert!(matches!(parser, ParserCli::Auto));
            assert!(matches!(profile, EmitProfileCli::Default));
            assert!(!harden);
            assert!(!lean);
        }
        _ => panic!("expected build command"),
    }
}

#[test]
fn parse_build_swiftpm_flag() {
    let cli = Cli::try_parse_from(["in", "build", "--path", "Foo.swift", "--swiftpm"])
        .expect("cli parse");
    match cli.command {
        Commands::Build { swiftpm, .. } => assert!(swiftpm),
        _ => panic!("expected build command"),
    }
}

#[test]
fn parse_build_parser_in_flag() {
    let cli = Cli::try_parse_from(["in", "build", "--path", "hello.in", "--parser", "in"])
        .expect("cli parse");
    match cli.command {
        Commands::Build { parser, .. } => assert!(matches!(parser, ParserCli::In)),
        _ => panic!("expected build command"),
    }
}

#[test]
fn parse_build_parser_icore_flag() {
    let cli = Cli::try_parse_from(["in", "build", "--path", "m.icore", "--parser", "icore"])
        .expect("cli parse");
    match cli.command {
        Commands::Build { parser, .. } => assert!(matches!(parser, ParserCli::Icore)),
        _ => panic!("expected build command"),
    }
}

#[test]
fn parse_agent_subcommand_defaults() {
    let cli = Cli::try_parse_from(["in", "agent", "--path", "hello.in"]).expect("cli parse");
    match cli.command {
        Commands::Agent {
            path,
            module_id,
            parser,
        } => {
            assert_eq!(path, "hello.in");
            assert_eq!(module_id, "App");
            assert!(matches!(parser, ParserCli::Auto));
        }
        _ => panic!("expected agent command"),
    }
}

#[test]
fn parse_explain_json_flag() {
    let cli = Cli::try_parse_from(["in", "explain", "INAGENT010", "--json"]).expect("cli parse");
    match cli.command {
        Commands::Explain {
            diagnostic_code,
            json,
        } => {
            assert_eq!(diagnostic_code, "INAGENT010");
            assert!(json);
        }
        _ => panic!("expected explain command"),
    }
}

#[test]
fn parse_fix_plan_json_flags() {
    let cli = Cli::try_parse_from([
        "in", "fix", "--plan", "--json", "--path", "bad.in", "--parser", "in",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Fix {
            plan,
            json,
            path,
            module_id,
            parser,
        } => {
            assert!(plan);
            assert!(json);
            assert_eq!(path, "bad.in");
            assert_eq!(module_id, "App");
            assert!(matches!(parser, ParserCli::In));
        }
        _ => panic!("expected fix command"),
    }
}

#[test]
fn parse_canonicalize_check_flag() {
    let cli = Cli::try_parse_from(["in", "canonicalize", "--path", "example.in", "--check"])
        .expect("cli parse");
    match cli.command {
        Commands::Canonicalize { path, check } => {
            assert_eq!(path, "example.in");
            assert!(check);
        }
        _ => panic!("expected canonicalize command"),
    }
}

#[test]
fn execute_propagates_nonzero_jit_exit() {
    let path = std::env::temp_dir().join(format!("in-cli-nonzero-{}.in", std::process::id()));
    std::fs::write(&path, "fn main() -> Int { return 7; }\n").expect("write source");
    let result = crate::compile::cmd_execute(
        std::path::Path::new("."),
        &path.to_string_lossy(),
        "Nonzero",
        false,
        false,
    );
    let _ = std::fs::remove_file(&path);
    let error = result.expect_err("nonzero JIT result should fail execute");
    assert!(error.to_string().contains("status 7"));
}

#[test]
fn parse_graph_flags() {
    let cli = Cli::try_parse_from([
        "in",
        "graph",
        "--path",
        "apps/in-sample/agent-native.in",
        "--imports",
        "--capabilities",
        "--symbols",
        "--calls",
        "--json",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Graph {
            path,
            imports,
            capabilities,
            symbols,
            calls,
            json,
            ..
        } => {
            assert_eq!(path, "apps/in-sample/agent-native.in");
            assert!(imports);
            assert!(capabilities);
            assert!(symbols);
            assert!(calls);
            assert!(json);
        }
        _ => panic!("expected graph command"),
    }
}

#[test]
fn parse_package_json_flag() {
    let cli = Cli::try_parse_from(["in", "package", "--path", "apps/package-sample", "--json"])
        .expect("cli parse");
    match cli.command {
        Commands::Package {
            action: None,
            path,
            json,
        } => {
            assert_eq!(path, "apps/package-sample");
            assert!(json);
        }
        _ => panic!("expected package command"),
    }
}

#[test]
fn parse_install_aliases_and_package_refs() {
    for alias in ["install", "get", "stall", "i"] {
        let cli =
            Cli::try_parse_from(["in", alias, "pip:flask", "--path", "."]).expect("cli parse");
        match cli.command {
            Commands::Install { packages, path, .. } => {
                assert_eq!(packages, vec!["pip:flask"]);
                assert_eq!(path, ".");
            }
            _ => panic!("expected install command for alias {alias}"),
        }
    }
}

#[test]
fn parse_add_package_refs() {
    let cli = Cli::try_parse_from(["in", "add", "pip:flask", "npm:hono", "--version", "^1.0.0"])
        .expect("cli parse");
    match cli.command {
        Commands::Add {
            packages, version, ..
        } => {
            assert_eq!(packages, vec!["pip:flask", "npm:hono"]);
            assert_eq!(version, "^1.0.0");
        }
        _ => panic!("expected add command"),
    }
}

#[test]
fn parse_run_subcommand_defaults() {
    let cli = Cli::try_parse_from(["in", "run"]).expect("cli parse");
    match cli.command {
        Commands::Run {
            watch_root,
            socket,
            metrics,
            debounce_ms,
        } => {
            assert_eq!(watch_root, "apps/sample-swiftui");
            assert_eq!(socket, ".brisk/hotreload/daemon.sock");
            assert_eq!(metrics, ".brisk/hotreload/metrics/latest.ndjson");
            assert_eq!(debounce_ms, 60);
        }
        _ => panic!("expected run command"),
    }
}

#[test]
fn parse_bench_subcommand_defaults() {
    let cli = Cli::try_parse_from(["in", "bench"]).expect("cli parse");
    match cli.command {
        Commands::Bench { metrics } => {
            assert_eq!(metrics, ".brisk/hotreload/metrics/latest.ndjson");
        }
        _ => panic!("expected bench command"),
    }
}

#[test]
fn parse_languages_json_flag() {
    let cli = Cli::try_parse_from(["in", "languages", "--json"]).expect("cli parse");
    match cli.command {
        Commands::Languages { json } => assert!(json),
        _ => panic!("expected languages command"),
    }
}

#[test]
fn parse_compile_subcommand() {
    let cli = Cli::try_parse_from([
        "in",
        "compile",
        "--path",
        "apps/in-sample/hello.in",
        "--target",
        "jit",
        "--out",
        "target/hello",
        "--module-id",
        "Hello",
        "--parser",
        "in",
        "--entry",
        "main",
        "--json",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Compile {
            path,
            target,
            out,
            module_id,
            parser,
            entry,
            target_triple,
            linkage,
            jobs,
            json,
            ..
        } => {
            assert_eq!(path, "apps/in-sample/hello.in");
            assert!(matches!(target, CompileTargetCli::Jit));
            assert_eq!(out, "target/hello");
            assert_eq!(module_id, "Hello");
            assert!(matches!(parser, ParserCli::In));
            assert_eq!(entry.as_deref(), Some("main"));
            assert!(target_triple.is_none());
            assert!(matches!(linkage, NativeLinkageCli::Executable));
            assert_eq!(jobs, 1);
            assert!(json);
        }
        _ => panic!("expected compile command"),
    }
}

#[test]
fn parse_compile_native_target_subcommand() {
    let cli = Cli::try_parse_from([
        "in",
        "compile",
        "--path",
        "apps/in-sample/hello.in",
        "--target",
        "native",
        "--out",
        "target/hello",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Compile { target, .. } => {
            assert!(matches!(target, CompileTargetCli::Native));
        }
        _ => panic!("expected compile command"),
    }
}

#[test]
fn parse_compile_native_dylib_linkage_subcommand() {
    let cli = Cli::try_parse_from([
        "in",
        "compile",
        "--path",
        "apps/in-sample/hello.in",
        "--target",
        "native",
        "--linkage",
        "dylib",
        "--out",
        "target/libhello.dylib",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Compile { linkage, .. } => {
            assert!(matches!(linkage, NativeLinkageCli::Dylib));
        }
        _ => panic!("expected compile command"),
    }
}

#[test]
fn parse_compile_native_target_triple_subcommand() {
    let cli = Cli::try_parse_from([
        "in",
        "compile",
        "--path",
        "apps/in-sample/hello.in",
        "--target",
        "native",
        "--target-triple",
        "x86_64-unknown-linux-gnu",
        "--linkage",
        "static-lib",
        "--out",
        "target/hello.o",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Compile {
            target_triple,
            linkage,
            ..
        } => {
            assert_eq!(target_triple.as_deref(), Some("x86_64-unknown-linux-gnu"));
            assert!(matches!(linkage, NativeLinkageCli::StaticLib));
        }
        _ => panic!("expected compile command"),
    }
}

#[test]
fn parse_backend_report_subcommand() {
    let cli = Cli::try_parse_from([
        "in",
        "backend",
        "--path",
        "apps/in-sample/hello.in",
        "--target",
        "native",
        "--json",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Backend {
            path, target, json, ..
        } => {
            assert_eq!(path, "apps/in-sample/hello.in");
            assert!(matches!(target, BackendTargetCli::Native));
            assert!(json);
        }
        _ => panic!("expected backend command"),
    }
}

#[test]
fn parse_backend_native_status_subcommand() {
    let cli = Cli::try_parse_from([
        "in",
        "backend",
        "--path",
        "apps/in-sample/hello.in",
        "--module-id",
        "Hello",
        "--parser",
        "in",
        "--target",
        "native",
        "--json",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Backend {
            path,
            module_id,
            parser,
            target,
            json,
        } => {
            assert_eq!(path, "apps/in-sample/hello.in");
            assert_eq!(module_id, "Hello");
            assert!(matches!(parser, ParserCli::In));
            assert!(matches!(target, BackendTargetCli::Native));
            assert!(json);
        }
        _ => panic!("expected backend command"),
    }
}

#[test]
fn parse_update_and_self_update_alias() {
    for argv in [["in", "update"], ["in", "self-update"]] {
        let cli = Cli::try_parse_from(argv).expect("cli parse");
        assert!(matches!(cli.command, Commands::Update));
    }
}

#[test]
fn parse_execute_subcommand() {
    let cli = Cli::try_parse_from([
        "in",
        "execute",
        "apps/in-sample/hello.in",
        "--module-id",
        "Hello",
        "--verbose",
    ])
    .expect("cli parse");
    match cli.command {
        Commands::Execute {
            path,
            module_id,
            verbose,
            debug,
        } => {
            assert_eq!(path, "apps/in-sample/hello.in");
            assert_eq!(module_id, "Hello");
            assert!(verbose);
            assert!(!debug);
        }
        _ => panic!("expected execute command"),
    }
}

#[test]
fn in_test_owned_native_gate_steps_exist() {
    let steps = crate::cli_test::owned_native_test_step_names();
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-owned-native-compiler.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-polyglot-sample.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-native-artifact-sample.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-native-arm-linux-executables.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-native-linkable-objects.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-abi-layouts.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-dynamic-loader.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-target-matrix.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-freestanding-x86_64.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-component-metadata.sh"))
    );
}

#[test]
fn in_test_includes_polyglot_sample_gate() {
    assert!(
        crate::cli_test::test_step_names()
            .iter()
            .any(|step| step.contains("check-polyglot-sample.sh"))
    );
    assert!(
        crate::cli_test::test_step_names()
            .iter()
            .any(|step| step.contains("check-graph-polyglot-sample.sh"))
    );
    assert!(
        crate::cli_test::test_step_names()
            .iter()
            .any(|step| step.contains("check-eval-polyglot-sample.sh"))
    );
    assert!(
        crate::cli_test::test_step_names()
            .iter()
            .any(|step| step.contains("check-native-answer-polyglot-subset.sh"))
    );
    assert!(
        crate::cli_test::test_step_names()
            .iter()
            .any(|step| step.contains("check-jit-compiler.sh"))
    );
    assert!(
        crate::cli_test::test_step_names()
            .iter()
            .any(|step| step.contains("check-orchestration-compiler.sh"))
    );
}

#[test]
fn in_test_defaults_to_self_hosted_compiler_gates() {
    let steps = crate::cli_test::test_step_names();
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-polyglot-sample.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-graph-polyglot-sample.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-eval-polyglot-sample.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-native-answer-polyglot-subset.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-jit-compiler.sh"))
    );
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-orchestration-compiler.sh"))
    );
    assert!(!steps.iter().any(|step| step.contains("cargo")));
    assert!(!steps.iter().any(|step| step.contains("swift")));
    assert!(!steps.iter().any(|step| step.contains("external compiler")));
}

#[test]
fn all_test_step_names_keep_toolchain_and_external_gates_available() {
    let steps = crate::cli_test::all_test_step_names();
    assert!(
        steps
            .iter()
            .any(|step| step.contains("check-owned-native-compiler.sh"))
    );
    assert!(steps.iter().any(|step| step.contains("cargo test")));
    assert!(
        steps
            .iter()
            .any(|step| step.contains("external compiler parity"))
    );
}

#[test]
fn parse_test_scope_flags() {
    for argv in [
        ["in", "test", "--self-host"],
        ["in", "test", "--toolchain"],
        ["in", "test", "--external-parity"],
        ["in", "test", "--owned-native"],
        ["in", "test", "--all"],
        ["in", "test", "--serial"],
    ] {
        assert!(Cli::try_parse_from(argv).is_ok(), "{argv:?}");
    }
}

#[test]
fn parse_test_owned_native_flag() {
    let cli = Cli::try_parse_from(["in", "test", "--owned-native"]).expect("cli parse");
    match cli.command {
        Commands::Test {
            owned_native, all, ..
        } => {
            assert!(owned_native);
            assert!(!all);
        }
        _ => panic!("expected test command"),
    }
}

#[test]
fn parse_test_accepts_fixture_path() {
    let cli = Cli::try_parse_from(["in", "test", "example.in"]).expect("cli parse");
    match cli.command {
        Commands::Test { paths, .. } => assert_eq!(paths, vec!["example.in"]),
        _ => panic!("expected test command"),
    }
}

#[test]
fn parse_eval_accepts_parser_flag() {
    let cli = Cli::try_parse_from(["in", "eval", "--parser", "js", "console.log(\"hi\")"])
        .expect("cli parse");
    match cli.command {
        Commands::Eval { parser, .. } => assert_eq!(parser.as_deref(), Some("js")),
        _ => panic!("expected eval command"),
    }
}

#[test]
fn parse_eval_defaults_to_no_source() {
    let cli = Cli::try_parse_from(["in", "eval"]).expect("cli parse");
    match cli.command {
        Commands::Eval { source, .. } => assert!(source.is_none()),
        _ => panic!("expected eval command"),
    }
}

#[test]
fn parse_plugin_run_subcommand() {
    let cli = Cli::try_parse_from(["in", "plugin", "run", "aurorality", "--target", "./foo"])
        .expect("cli parse");
    match cli.command {
        Commands::Plugin { action, .. } => match action {
            PluginAction::Run { name, target } => {
                assert_eq!(name, "aurorality");
                assert_eq!(target, "./foo");
            }
            _ => panic!("expected plugin run"),
        },
        _ => panic!("expected plugin command"),
    }
}

#[test]
fn parse_plugin_list_and_install() {
    let cli = Cli::try_parse_from(["in", "plugin", "list"]).expect("cli parse");
    assert!(matches!(
        cli.command,
        Commands::Plugin {
            action: PluginAction::List,
            ..
        }
    ));
    let cli = Cli::try_parse_from(["in", "plugin", "install", "aurorality"]).expect("cli parse");
    assert!(matches!(
        cli.command,
        Commands::Plugin {
            action: PluginAction::Install { name, .. },
            ..
        } if name == "aurorality"
    ));
}

#[test]
fn parse_package_subcommands() {
    let cli = Cli::try_parse_from(["in", "package", "install"]).expect("cli parse");
    assert!(matches!(
        cli.command,
        Commands::Package {
            action: Some(PackageCommands::Install { .. }),
            ..
        }
    ));
    let cli = Cli::try_parse_from(["in", "package", "lock"]).expect("cli parse");
    assert!(matches!(
        cli.command,
        Commands::Package {
            action: Some(PackageCommands::Lock { .. }),
            ..
        }
    ));
}

#[test]
fn parse_daemon_subcommands() {
    let cli = Cli::try_parse_from(["in", "daemon", "start"]).expect("cli parse");
    assert!(matches!(
        cli.command,
        Commands::Daemon {
            action: DaemonAction::Start,
            ..
        }
    ));
    let cli = Cli::try_parse_from(["in", "daemon", "stop"]).expect("cli parse");
    assert!(matches!(
        cli.command,
        Commands::Daemon {
            action: DaemonAction::Stop,
            ..
        }
    ));
    let cli = Cli::try_parse_from(["in", "daemon", "status"]).expect("cli parse");
    assert!(matches!(
        cli.command,
        Commands::Daemon {
            action: DaemonAction::Status,
            ..
        }
    ));
}

#[test]
fn eval_wraps_in_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::In, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "fn main() -> Int { return 1 + 2 }");
    assert!(plans[0].print_result);
    assert_eq!(plans[1].wrapped, "main:\n  1 + 2");
    assert!(!plans[1].print_result);
}

#[test]
fn eval_wraps_in_statement() {
    let plans =
        crate::eval::eval_plans(inauguration::parser_registry::ParserId::In, "print(\"hi\")");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "main:\n  print(\"hi\")");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_python_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Python, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "def main() -> int:\n    return 1 + 2");
    assert!(plans[0].print_result);
    assert_eq!(plans[1].wrapped, "def main() -> None:\n    1 + 2");
    assert!(!plans[1].print_result);
}

#[test]
fn eval_wraps_python_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Python,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "def main() -> None:\n    print(\"hi\")");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_rust_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Rust, "1 + 2");
    assert_eq!(plans[0].wrapped, "fn main() -> i64 { 1 + 2 }");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_rust_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Rust,
        "println!(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "fn main() -> i64 { print(\"hi\");\n0\n}");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_javascript_expression() {
    let plans =
        crate::eval::eval_plans(inauguration::parser_registry::ParserId::JavaScript, "1 + 2");
    assert_eq!(plans[0].wrapped, "function main() { return 1 + 2; }");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_javascript_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::JavaScript,
        "console.log(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "function main() { print(\"hi\") }");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_swift_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Swift, "1 + 2");
    assert_eq!(plans[0].wrapped, "func main() -> Int {\n  return 1 + 2\n}");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_swift_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Swift,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].wrapped,
        "func main() -> Void {\n  print(\"hi\")\n}"
    );
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_go_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Go, "1 + 2");
    assert_eq!(
        plans[0].wrapped,
        "package main\n\nfunc main() int {\n\treturn 1 + 2\n}"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_go_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Go,
        "println(\"hi\")",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "package main\n\nfunc main() int {\n\treturn println(\"hi\")\n}"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_v_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::V, "1 + 2");
    assert_eq!(
        plans[0].wrapped,
        "module main\n\nfn main() int {\n\treturn 1 + 2\n}"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_v_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::V,
        "println(\"hi\")",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "module main\n\nfn main() int {\n\treturn println(\"hi\")\n}"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_zig_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Zig, "1 + 2");
    assert_eq!(
        plans[0].wrapped,
        "pub fn main() i32 {\n    return 1 + 2;\n}"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_zig_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Zig,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].wrapped,
        "pub fn main() void {\n    print(\"hi\");\n}"
    );
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_dart_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Dart, "1 + 2");
    assert_eq!(plans[0].wrapped, "int main() {\n  return 1 + 2;\n}");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_dart_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Dart,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "void main() {\n  print(\"hi\");\n}");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_kotlin_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Kotlin, "1 + 2");
    assert_eq!(plans[0].wrapped, "fun main(): Int {\n    return 1 + 2\n}");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_kotlin_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Kotlin,
        "println(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "fun main() {\n    print(\"hi\")\n}");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_scala_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Scala, "1 + 2");
    assert_eq!(plans[0].wrapped, "def main(): Int = {\n  1 + 2\n}");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_scala_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Scala,
        "println(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "def main(): Unit = {\n  print(\"hi\")\n}");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_groovy_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Groovy, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "class App {\n  static int main(String[] args) {\n    return 1 + 2\n  }\n}"
    );
    assert!(plans[0].print_result);
    assert_eq!(
        plans[1].wrapped,
        "class App {\n  static void main(String[] args) {\n    1 + 2\n  }\n}"
    );
    assert!(!plans[1].print_result);
}

#[test]
fn eval_wraps_groovy_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Groovy,
        "println(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].wrapped,
        "class App {\n  static void main(String[] args) {\n    print(\"hi\")\n  }\n}"
    );
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_java_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Java, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "class App {\n  public static int main(String[] args) {\n    return 1 + 2;\n  }\n}"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_java_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Java,
        "System.out.println(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].wrapped,
        "class App {\n  public static void main(String[] args) {\n    print(\"hi\");\n  }\n}"
    );
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_csharp_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::CSharp, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "class App {\n    static int Main() {\n        return 1 + 2;\n    }\n}"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_csharp_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::CSharp,
        "Console.WriteLine(\"hi\")",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "class App {\n    static int Main() {\n        return Console.WriteLine(\"hi\");\n    }\n}"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_cpp_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Cpp, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "int main() { return 1 + 2; }");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_cpp_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Cpp,
        "std::cout << \"hi\"",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "int main() { return std::cout << \"hi\"; }"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_c_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::C, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "int main() { return 1 + 2; }");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_c_statement() {
    let plans =
        crate::eval::eval_plans(inauguration::parser_registry::ParserId::C, "printf(\"hi\")");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "int main() { return printf(\"hi\"); }");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_crystal_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Crystal, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "def main : Int32\n  1 + 2\nend");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_crystal_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Crystal,
        "puts \"hi\"",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "def main : Int32\n  puts \"hi\"\nend");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_nim_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Nim, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "proc main(): int =\n  1 + 2");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_nim_statement() {
    let plans =
        crate::eval::eval_plans(inauguration::parser_registry::ParserId::Nim, "echo \"hi\"");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "proc main(): int =\n  echo \"hi\"");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_haskell_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Haskell, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "main = 1 + 2");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_haskell_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Haskell,
        "putStrLn \"hi\"",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "main = putStrLn \"hi\"");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_fsharp_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::FSharp, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "let main _ : int = 1 + 2");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_fsharp_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::FSharp,
        "printfn \"hi\"",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "let main _ : int = printfn \"hi\"");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_odin_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Odin, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "package main\n\nmain :: proc() ->  {\n\treturn 1 + 2\n}\n"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_odin_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Odin,
        "println(\"hi\")",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "package main\n\nmain :: proc() ->  {\n\treturn println(\"hi\")\n}\n"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_d_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::D, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "int main() { return 1 + 2; }");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_d_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::D,
        "writeln(\"hi\")",
    );
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "int main() { return writeln(\"hi\"); }");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_ruby_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Ruby, "1 + 2");
    assert_eq!(plans[0].wrapped, "def main\n  1 + 2\nend");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_ruby_statement() {
    let plans =
        crate::eval::eval_plans(inauguration::parser_registry::ParserId::Ruby, "puts \"hi\"");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "def main\n  puts \"hi\"\nend");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_lua_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Lua, "1 + 2");
    assert_eq!(plans[0].wrapped, "function main()\n  return 1 + 2\nend");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_lua_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Lua,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "function main()\n  print(\"hi\")\nend");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_php_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Php, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "<?php\nfunction main() {\n    return 1 + 2;\n}\n"
    );
    assert!(plans[0].print_result);
    assert_eq!(
        plans[1].wrapped,
        "<?php\nfunction main() {\n    1 + 2;\n}\n"
    );
    assert!(!plans[1].print_result);
}

#[test]
fn eval_wraps_php_statement() {
    let plans =
        crate::eval::eval_plans(inauguration::parser_registry::ParserId::Php, "echo \"hi\"");
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].wrapped,
        "<?php\nfunction main() {\n    print(\"hi)\");\n}\n"
    );
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_perl_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Perl, "1 + 2");
    assert_eq!(plans[0].wrapped, "sub main {\n    return 1 + 2;\n}\n");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_perl_statement() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Perl,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "sub main {\n    print(\"hi\");\n}\n");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_prefers_printed_perl_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Perl, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "sub main {\n    return 1 + 2;\n}\n");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_perl_statement_in_main() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Perl,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "sub main {\n    print(\"hi\");\n}\n");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_prefers_printed_clojure_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Clojure, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "(defn main [] 1 + 2)\n");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_clojure_statement_in_main() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Clojure,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "(defn main [] print(\"hi\"))\n");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_prefers_printed_elixir_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Elixir, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].wrapped, "def main do\n  1 + 2\nend\n");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_elixir_statement_in_main() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Elixir,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "def main do\n  print(\"hi\")\nend\n");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_prefers_printed_erlang_expression() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::Erlang, "1 + 2");
    assert_eq!(plans.len(), 2);
    assert_eq!(
        plans[0].wrapped,
        "-module(app).\n-export([main/0]).\n\nmain() ->\n    1 + 2.\n"
    );
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_erlang_statement_in_main() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Erlang,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].wrapped,
        "-module(app).\n-export([main/0]).\n\nmain() ->\n    print(\"hi\").\n"
    );
    assert!(!plans[0].print_result);
}

#[test]
fn eval_normalizes_scala_println() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Scala,
        "println(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "def main(): Unit = {\n  print(\"hi\")\n}");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_treats_swift_source_as_declaration_input() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::Swift,
        "func main() -> Void {\n  return\n}",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "func main() -> Void {\n  return\n}");
    assert!(!plans[0].print_result);
}

#[test]
fn eval_wraps_holyc_expression_in_main() {
    let plans = crate::eval::eval_plans(inauguration::parser_registry::ParserId::HolyC, "1 + 2");
    assert_eq!(plans[0].wrapped, "I64 Main()\n{\n  return 1 + 2;\n}\nMain;");
    assert!(plans[0].print_result);
}

#[test]
fn eval_wraps_holyc_statement_in_main() {
    let plans = crate::eval::eval_plans(
        inauguration::parser_registry::ParserId::HolyC,
        "print(\"hi\")",
    );
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].wrapped, "U0 Main()\n{\n  print(\"hi\");\n}\nMain;");
    assert!(!plans[0].print_result);
}

#[test]
fn doctor_update_mode_reports_checkout_or_remote() {
    assert!(crate::doctor::doctor_update_mode_text(true).contains("checkout"));
    assert!(crate::doctor::doctor_update_mode_text(false).contains("remote install script"));
}

#[cfg(unix)]
#[test]
fn swift_products_internal_skip_patterns() {
    assert!(crate::build::is_swift_products_internal_skip(
        "Modules", true
    ));
    assert!(crate::build::is_swift_products_internal_skip(
        "ModuleCache",
        true
    ));
    assert!(crate::build::is_swift_products_internal_skip(
        "index", false
    ));
    assert!(crate::build::is_swift_products_internal_skip(
        "description.json",
        false
    ));
    assert!(crate::build::is_swift_products_internal_skip(
        "plugin-tools-description.json",
        false
    ));
    assert!(crate::build::is_swift_products_internal_skip(
        "foo.build",
        true
    ));
    assert!(!crate::build::is_swift_products_internal_skip(
        "foo.build",
        false
    ));
    assert!(crate::build::is_swift_products_internal_skip(
        "swift-version-5.9.txt",
        false
    ));
    assert!(!crate::build::is_swift_products_internal_skip(
        "MyApp.app",
        true
    ));
}

#[test]
fn in_test_includes_coverage_gate() {
    assert!(
        crate::cli_test::test_step_names()
            .iter()
            .any(|step| step.contains("check-coverage.sh"))
    );
}
