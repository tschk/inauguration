//! CLI test command implementation.

use crate::{InError, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Instant;

pub(crate) struct TestOptions {
    pub(crate) list: bool,
    pub(crate) self_host: bool,
    pub(crate) toolchain: bool,
    pub(crate) external_parity: bool,
    pub(crate) owned_native: bool,
    pub(crate) all: bool,
    pub(crate) serial: bool,
    pub(crate) paths: Vec<String>,
}

#[derive(Clone, Debug)]
struct TestCommand {
    program: String,
    args: Vec<String>,
    cwd: PathBuf,
}

#[derive(Clone, Debug)]
struct TestGroup {
    name: &'static str,
    commands: Vec<TestCommand>,
}

#[derive(Debug)]
struct TestGroupResult {
    name: &'static str,
    elapsed_ms: f64,
    output: String,
    error: Option<String>,
}

pub(crate) fn cmd_test(root: &Path, options: TestOptions) -> Result<()> {
    if options.list {
        for name in all_test_step_names() {
            println!("{name}");
        }
        return Ok(());
    }
    if !options.paths.is_empty() {
        return run_test_groups(
            vec![TestGroup {
                name: "selected conformance fixtures (scripts/run-conformance.sh)",
                commands: vec![TestCommand {
                    program: "bash".to_string(),
                    args: std::iter::once("scripts/run-conformance.sh".to_string())
                        .chain(options.paths)
                        .collect(),
                    cwd: root.to_path_buf(),
                }],
            }],
            true,
        );
    }
    let include_toolchain = options.all || options.toolchain;
    let include_external = options.all || options.external_parity;
    let include_owned_native = options.all || options.owned_native;
    let owned_native_only = options.owned_native && !options.all;
    let mut groups = Vec::new();

    if include_owned_native {
        if owned_native_only {
            eprintln!("running owned-native compiler gates");
        }
        groups.extend(owned_native_test_groups(root));
    }

    if !owned_native_only
        && (options.all || options.self_host || (!options.toolchain && !options.external_parity))
    {
        if options.self_host && !options.all && !include_toolchain && !include_external {
            eprintln!("running self-hosted compiler gates");
        }
        groups.extend(self_host_test_groups(root));
    }

    if include_external {
        groups.extend(external_parity_test_groups(root));
    }
    if include_toolchain {
        groups.extend(toolchain_test_groups(root));
    }
    run_test_groups(groups, options.serial)
}

pub(crate) fn owned_native_test_step_names() -> [&'static str; 11] {
    [
        "owned native compiler (scripts/check-owned-native-compiler.sh)",
        "native answer sample (scripts/check-native-answer-sample.sh)",
        "native artifact sample (scripts/check-native-artifact-sample.sh)",
        "native ARM Linux executables (scripts/check-native-arm-linux-executables.sh)",
        "native linkable objects (scripts/check-native-linkable-objects.sh)",
        "owned polyglot samples (scripts/check-polyglot-sample.sh)",
        "abi layouts (scripts/check-abi-layouts.sh)",
        "dynamic loader (scripts/check-dynamic-loader.sh)",
        "target matrix (scripts/check-target-matrix.sh)",
        "freestanding x86_64 (scripts/check-freestanding-x86_64.sh)",
        "component metadata (scripts/check-component-metadata.sh)",
    ]
}

fn owned_native_script_for_step(name: &str) -> &'static str {
    if name.contains("owned-native-compiler") {
        "scripts/check-owned-native-compiler.sh"
    } else if name.contains("native-answer") {
        "scripts/check-native-answer-sample.sh"
    } else if name.contains("native-artifact") {
        "scripts/check-native-artifact-sample.sh"
    } else if name.contains("native ARM") {
        "scripts/check-native-arm-linux-executables.sh"
    } else if name.contains("native linkable") {
        "scripts/check-native-linkable-objects.sh"
    } else if name.contains("polyglot") {
        "scripts/check-polyglot-sample.sh"
    } else if name.contains("abi layouts") {
        "scripts/check-abi-layouts.sh"
    } else if name.contains("dynamic loader") {
        "scripts/check-dynamic-loader.sh"
    } else if name.contains("freestanding x86_64") {
        "scripts/check-freestanding-x86_64.sh"
    } else if name.contains("component metadata") {
        "scripts/check-component-metadata.sh"
    } else {
        "scripts/check-target-matrix.sh"
    }
}

fn owned_native_test_groups(root: &Path) -> Vec<TestGroup> {
    owned_native_test_step_names()
        .into_iter()
        .map(|name| TestGroup {
            name,
            commands: vec![bash_command(root, owned_native_script_for_step(name))],
        })
        .collect()
}

pub(crate) fn test_step_names() -> [&'static str; 9] {
    [
        "polyglot samples (scripts/check-polyglot-sample.sh)",
        "polyglot graph samples (scripts/check-graph-polyglot-sample.sh)",
        "polyglot eval samples (scripts/check-eval-polyglot-sample.sh)",
        "native answer polyglot subset (scripts/check-native-answer-polyglot-subset.sh)",
        "jit compiler (scripts/check-jit-compiler.sh)",
        "orchestration compiler (scripts/check-orchestration-compiler.sh)",
        "static coverage (scripts/check-coverage.sh)",
        "backend differential (scripts/check-backend-differential.sh)",
        "conformance suite (scripts/run-conformance.sh)",
    ]
}

fn in_build_command(root: &Path, path: &str, parser: &str) -> TestCommand {
    let in_bin = std::env::var("IN_BIN").unwrap_or_else(|_| "in".to_string());
    TestCommand {
        program: in_bin,
        args: vec![
            "build".to_string(),
            "--parser".to_string(),
            parser.to_string(),
            "--path".to_string(),
            path.to_string(),
        ],
        cwd: root.to_path_buf(),
    }
}

fn in_compile_jit_command(root: &Path, path: &str) -> TestCommand {
    let in_bin = std::env::var("IN_BIN").unwrap_or_else(|_| "in".to_string());
    TestCommand {
        program: in_bin,
        args: vec![
            "compile".to_string(),
            "--path".to_string(),
            path.to_string(),
            "--target".to_string(),
            "jit".to_string(),
            "--out".to_string(),
            "/dev/null".to_string(),
        ],
        cwd: root.to_path_buf(),
    }
}

fn toolchain_test_step_names() -> [&'static str; 3] {
    [
        "protocol models (scripts/check-protocol-models.sh)",
        "compiler/rust-driver (cargo test --all)",
        "in-cli (cargo test)",
    ]
}

fn external_parity_test_step_names() -> [&'static str; 1] {
    ["external compiler parity (scripts/check-external-compiler-parity.sh)"]
}

fn in_lang_test_groups(root: &Path) -> Vec<TestGroup> {
    vec![
        TestGroup {
            name: ".in lang compile (hello.in)",
            commands: vec![in_build_command(root, "apps/in-sample/hello.in", "in")],
        },
        TestGroup {
            name: ".in lang compile (agent-native.in)",
            commands: vec![in_build_command(
                root,
                "apps/in-sample/agent-native.in",
                "in",
            )],
        },
        TestGroup {
            name: ".in lang compile (try_catch.in)",
            commands: vec![in_build_command(root, "apps/in-sample/try_catch.in", "in")],
        },
        TestGroup {
            name: ".icore compile",
            commands: vec![in_compile_jit_command(root, "apps/icore-sample/min.icore")],
        },
        TestGroup {
            name: "self-host compile",
            commands: vec![in_compile_jit_command(root, "in-cli/src/main.rs")],
        },
        TestGroup {
            name: "owned polyglot compile",
            commands: vec![in_build_command(
                root,
                "apps/polyglot-sample/sample.in",
                "in",
            )],
        },
        TestGroup {
            name: "native answer sample",
            commands: vec![in_compile_jit_command(
                root,
                "apps/native-artifact-sample/answer.in",
            )],
        },
    ]
}

fn self_host_test_groups(root: &Path) -> Vec<TestGroup> {
    let mut groups: Vec<TestGroup> = in_lang_test_groups(root);
    groups.extend(
        test_step_names()
            .into_iter()
            .map(|name| {
                let script = if name.contains("polyglot") {
                    if name.contains("graph") {
                        "scripts/check-graph-polyglot-sample.sh"
                    } else if name.contains("eval") {
                        "scripts/check-eval-polyglot-sample.sh"
                    } else if name.contains("native answer") {
                        "scripts/check-native-answer-polyglot-subset.sh"
                    } else {
                        "scripts/check-polyglot-sample.sh"
                    }
                } else if name.contains("jit") {
                    "scripts/check-jit-compiler.sh"
                } else if name.contains("conformance") {
                    "scripts/run-conformance.sh"
                } else {
                    "scripts/check-orchestration-compiler.sh"
                };
                TestGroup {
                    name,
                    commands: vec![bash_command(root, script)],
                }
            })
            .collect::<Vec<_>>(),
    );
    groups
}

fn external_parity_test_groups(root: &Path) -> Vec<TestGroup> {
    vec![TestGroup {
        name: external_parity_test_step_names()[0],
        commands: vec![bash_command(
            root,
            "scripts/check-external-compiler-parity.sh",
        )],
    }]
}

fn toolchain_test_groups(root: &Path) -> Vec<TestGroup> {
    vec![
        TestGroup {
            name: toolchain_test_step_names()[0],
            commands: vec![bash_command(root, "scripts/check-protocol-models.sh")],
        },
        TestGroup {
            name: toolchain_test_step_names()[1],
            commands: vec![TestCommand {
                program: "cargo".to_string(),
                args: vec!["test".to_string(), "--all".to_string()],
                cwd: root.join("compiler").join("rust-driver"),
            }],
        },
        TestGroup {
            name: toolchain_test_step_names()[2],
            commands: vec![TestCommand {
                program: "cargo".to_string(),
                args: vec!["test".to_string()],
                cwd: root.join("in-cli"),
            }],
        },
    ]
}

fn bash_command(root: &Path, script: &str) -> TestCommand {
    TestCommand {
        program: "bash".to_string(),
        args: vec![script.to_string()],
        cwd: root.to_path_buf(),
    }
}

fn run_test_groups(groups: Vec<TestGroup>, serial: bool) -> Result<()> {
    if serial {
        for group in groups {
            let result = run_test_group(group);
            print_test_group_result(&result);
            if let Some(error) = result.error {
                return Err(InError::Message(error));
            }
        }
        return Ok(());
    }

    let handles = groups
        .into_iter()
        .map(|group| thread::spawn(|| run_test_group(group)));
    let mut failures = Vec::new();
    for handle in handles {
        let result = handle
            .join()
            .map_err(|_| InError::Message("test worker panicked".to_string()))?;
        print_test_group_result(&result);
        if let Some(error) = result.error {
            failures.push(error);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(InError::Message(failures.join("\n")))
    }
}

fn run_test_group(group: TestGroup) -> TestGroupResult {
    let start = Instant::now();
    let mut output = String::new();
    for command in group.commands {
        let rendered = render_test_command(&command);
        output.push_str(&format!("$ {rendered}\n"));
        match Command::new(&command.program)
            .args(&command.args)
            .current_dir(&command.cwd)
            .stdin(Stdio::null())
            .output()
        {
            Ok(cmd_output) => {
                output.push_str(&String::from_utf8_lossy(&cmd_output.stdout));
                output.push_str(&String::from_utf8_lossy(&cmd_output.stderr));
                if !cmd_output.status.success() {
                    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                    return TestGroupResult {
                        name: group.name,
                        elapsed_ms,
                        output,
                        error: Some(format!(
                            "{}: `{}` exited with {}",
                            group.name, command.program, cmd_output.status
                        )),
                    };
                }
            }
            Err(e) => {
                let mut msg = format!(
                    "{}: failed to start `{}` (cwd={}): {e}",
                    group.name,
                    command.program,
                    command.cwd.display()
                );
                if e.kind() == std::io::ErrorKind::NotFound {
                    msg.push_str(
                        " — from an inauguration checkout run `in update` (or `cargo install --path in-cli --force`).",
                    );
                }
                let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                return TestGroupResult {
                    name: group.name,
                    elapsed_ms,
                    output,
                    error: Some(msg),
                };
            }
        }
    }
    TestGroupResult {
        name: group.name,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
        output,
        error: None,
    }
}

fn render_test_command(command: &TestCommand) -> String {
    let mut parts = vec![command.program.clone()];
    parts.extend(command.args.clone());
    format!("{} (cwd={})", parts.join(" "), command.cwd.display())
}

fn print_test_group_result(result: &TestGroupResult) {
    print!("{}", result.output);
    if result.error.is_some() {
        eprintln!("test failed: {} in {:.3}ms", result.name, result.elapsed_ms);
    } else {
        eprintln!("test ok: {} in {:.3}ms", result.name, result.elapsed_ms);
    }
}

pub(crate) fn in_lang_test_step_names() -> [&'static str; 7] {
    [
        ".in lang compile (hello.in)",
        ".in lang compile (agent-native.in)",
        ".in lang compile (try_catch.in)",
        ".icore compile",
        "self-host compile",
        "owned polyglot compile",
        "native answer sample",
    ]
}

pub(crate) fn all_test_step_names() -> Vec<&'static str> {
    let mut names = Vec::new();
    names.extend(owned_native_test_step_names());
    names.extend(test_step_names());
    names.extend(in_lang_test_step_names());
    names.extend(external_parity_test_step_names());
    names.extend(toolchain_test_step_names());
    names
}
