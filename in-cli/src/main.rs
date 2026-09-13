#![allow(clippy::too_many_arguments)]

use clap::{Parser, Subcommand, ValueEnum};
use inauguration::parser_registry::ParserCli;
use thiserror::Error;

mod cli_test;

pub(crate) type Result<T> = std::result::Result<T, InError>;
const DEFAULT_BENCH_METRICS: &str = ".brisk/hotreload/metrics/latest.ndjson";

#[derive(Debug, Error)]
pub(crate) enum InError {
    #[error("{0}")]
    Message(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Parser, Debug)]
#[command(name = "in")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "inauguration v0.5.1")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum PackageCommands {
    #[command(about = "Install declared dependencies from cargo/npm registries or local paths")]
    Install {
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(
            long,
            default_value_t = false,
            help = "Reuse inauguration.lock install paths only"
        )]
        offline: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(about = "Write inauguration.lock for declared dependencies")]
    Lock {
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackendTargetCli {
    Native,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompileTargetCli {
    Native,
    Jit,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum EmitKindCli {
    Boot,
    /// Emit C source (Vlang-style C backend for optimization via zig cc).
    C,
    /// Emit Go source from Core IR (lossy subset).
    Go,
    /// Emit Rust source from Core IR (lossy subset).
    Rust,
    /// Emit a raw SCI component binary for a freestanding x86_64 target.
    Sci,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum NativeLinkageCli {
    Executable,
    Dylib,
    StaticLib,
}

#[derive(Clone, Copy, Debug, ValueEnum, Default)]
enum EmitProfileCli {
    #[default]
    Default,
    Harden,
    Lean,
}

impl EmitProfileCli {
    fn to_owned(self) -> inauguration::emit_profile::EmitProfile {
        match self {
            Self::Default => inauguration::emit_profile::EmitProfile::Default,
            Self::Harden => inauguration::emit_profile::EmitProfile::Harden,
            Self::Lean => inauguration::emit_profile::EmitProfile::Lean,
        }
    }
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = "Run hybrid compiler pipeline")]
    Build {
        #[arg(
            long,
            default_value = ".",
            help = "Source path: .in, .icore, .swift file, package directory, or Cargo.toml"
        )]
        path: String,
        #[arg(
            long,
            help = "Output binary path (for Rust sources, runs cargo build with stats)"
        )]
        out: Option<String>,
        #[arg(
            long,
            default_value_t = false,
            help = "Enable optimization passes (core_opt)"
        )]
        release: bool,
        #[arg(long, value_enum, default_value_t = EmitProfileCli::Default, help = "Emit profile: default|harden|lean")]
        profile: EmitProfileCli,
        #[arg(long, action = clap::ArgAction::SetTrue, help = "Shorthand for --profile harden")]
        harden: bool,
        #[arg(long, action = clap::ArgAction::SetTrue, help = "Shorthand for --profile lean")]
        lean: bool,
        #[arg(
            long,
            action = clap::ArgAction::SetTrue,
            help = "Emit runtime artifact to --out plus a separate harden sample (see --harden-out)"
        )]
        dual_emit: bool,
        #[arg(
            long,
            help = "Harden artifact path; with --out, implies --dual-emit. Default: insert -harden before the --out extension"
        )]
        harden_out: Option<String>,
        #[arg(long, default_value = "App")]
        module_id: String,
        #[arg(
            long,
            default_value_t = false,
            help = "Show detailed stage timing output"
        )]
        verbose: bool,
        #[arg(
            long,
            action = clap::ArgAction::SetTrue,
            help = "After the in-tree hybrid pipeline, run SwiftPM swift build and stage products (toolchain fallback)"
        )]
        swiftpm: bool,
        #[arg(
            long,
            action = clap::ArgAction::SetTrue,
            help = "Allow external Swift/swiftc toolchain fallback on build paths that would otherwise stay owned-only"
        )]
        allow_external_toolchain: bool,
        #[arg(
            long,
            value_enum,
            default_value_t = ParserCli::Auto,
            help = "`auto`: extension + `IN_PARSER` pick Core IR vs Swift; `in` / `icore` force `.in` or JSON icore"
        )]
        parser: ParserCli,
    },
    #[command(about = "Emit agent-first compiler facts as stable JSON")]
    Agent {
        #[arg(
            long,
            default_value = ".",
            help = "Source path: .in, .icore, .swift file, or supported frontend source"
        )]
        path: String,
        #[arg(long, default_value = "App")]
        module_id: String,
        #[arg(
            long,
            value_enum,
            default_value_t = ParserCli::Auto,
            help = "`auto`: extension + `IN_PARSER` pick Core IR vs Swift; `in` / `icore` force `.in` or JSON icore"
        )]
        parser: ParserCli,
    },
    #[command(about = "Explain a compiler diagnostic code")]
    Explain {
        diagnostic_code: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(about = "Emit typed repair plans for agents")]
    Fix {
        #[arg(long, action = clap::ArgAction::SetTrue)]
        plan: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(long, default_value = "App")]
        module_id: String,
        #[arg(long, value_enum, default_value_t = ParserCli::Auto)]
        parser: ParserCli,
    },
    #[command(about = "Canonicalize strict .in source")]
    Canonicalize {
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(long, default_value_t = false)]
        check: bool,
    },
    #[command(about = "Inspect parser, Core IR, and SIL graph facts")]
    Graph {
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(long, default_value = "App")]
        module_id: String,
        #[arg(long, value_enum, default_value_t = ParserCli::Auto)]
        parser: ParserCli,
        #[arg(long, default_value_t = false)]
        imports: bool,
        #[arg(long, default_value_t = false)]
        capabilities: bool,
        #[arg(long, default_value_t = false)]
        symbols: bool,
        #[arg(long, default_value_t = false)]
        calls: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(
        about = "Install package dependencies from registries or local paths",
        visible_aliases = ["get", "stall", "i"]
    )]
    Install {
        #[arg(
            value_name = "PACKAGE",
            help = "Ecosystem refs such as pip:flask or cargo:serde"
        )]
        packages: Vec<String>,
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(
            long,
            default_value_t = false,
            help = "Reuse inauguration.lock install paths only"
        )]
        offline: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(about = "Add packages to inauguration.package and install them")]
    Add {
        #[arg(
            value_name = "PACKAGE",
            help = "Ecosystem refs such as pip:flask or npm:hono"
        )]
        packages: Vec<String>,
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(long, default_value = "latest")]
        version: String,
        #[arg(
            long,
            default_value_t = false,
            help = "Reuse inauguration.lock install paths only"
        )]
        offline: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(about = "Package manifest report and dependency management")]
    Package {
        #[command(subcommand)]
        action: Option<PackageCommands>,
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(about = "List language front maturity, examples, and runtime boundaries")]
    Languages {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(about = "Run full local dev loop (daemon + Rust socket client)")]
    Dev,
    #[command(about = "Persistent compiler daemon")]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    #[command(about = "Run hotreload daemon only")]
    Run {
        #[arg(long, default_value = "apps/sample-swiftui")]
        watch_root: String,
        #[arg(long, default_value = ".brisk/hotreload/daemon.sock")]
        socket: String,
        #[arg(long, default_value = ".brisk/hotreload/metrics/latest.ndjson")]
        metrics: String,
        #[arg(long, default_value_t = 60)]
        debounce_ms: u64,
    },
    #[command(about = "Compile source through owned inauguration pipeline")]
    Compile {
        #[arg(long)]
        path: String,
        #[arg(long, value_enum, default_value_t = CompileTargetCli::Jit)]
        target: CompileTargetCli,
        #[arg(long)]
        out: String,
        #[arg(long, default_value = "App")]
        module_id: String,
        #[arg(long, value_enum, default_value_t = ParserCli::Auto)]
        parser: ParserCli,
        #[arg(long)]
        entry: Option<String>,
        #[arg(long)]
        target_triple: Option<String>,
        #[arg(long, value_enum, default_value_t = NativeLinkageCli::Executable)]
        linkage: NativeLinkageCli,
        #[arg(long, default_value = "1")]
        jobs: usize,
        #[arg(long, default_value_t = false)]
        json: bool,
        #[arg(long, value_enum)]
        emit: Option<EmitKindCli>,
        #[arg(long)]
        trampoline: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        metadata: Option<String>,
        #[arg(long, default_value_t = false, help = "Show detailed stage report")]
        debug: bool,
        #[arg(long, value_enum, default_value_t = EmitProfileCli::Default, help = "Emit profile: default|harden|lean")]
        profile: EmitProfileCli,
        #[arg(long, action = clap::ArgAction::SetTrue, help = "Shorthand for --profile harden")]
        harden: bool,
        #[arg(long, action = clap::ArgAction::SetTrue, help = "Shorthand for --profile lean")]
        lean: bool,
        #[arg(
            long,
            action = clap::ArgAction::SetTrue,
            help = "Emit runtime artifact to --out plus a separate harden sample (see --harden-out)"
        )]
        dual_emit: bool,
        #[arg(
            long,
            help = "Harden artifact path; with --out, implies --dual-emit. Default: insert -harden before the --out extension"
        )]
        harden_out: Option<String>,
    },
    #[command(about = "Compile and execute a source file via JIT")]
    Execute {
        #[arg(help = "Source file path (.in, .icore, .go, .v, .rs, etc.)")]
        path: String,
        #[arg(long, default_value = "App")]
        module_id: String,
        #[arg(long, default_value_t = false)]
        verbose: bool,
        #[arg(long, default_value_t = false, help = "Show detailed stage report")]
        debug: bool,
    },
    #[command(about = "Report owned backend status and compile-path facts")]
    Backend {
        #[arg(long, default_value = ".")]
        path: String,
        #[arg(long, default_value = "App")]
        module_id: String,
        #[arg(long, value_enum, default_value_t = ParserCli::Auto)]
        parser: ParserCli,
        #[arg(long, value_enum, default_value_t = BackendTargetCli::Native)]
        target: BackendTargetCli,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    #[command(about = "Run self-hosted compiler test suites")]
    Test {
        #[arg(
            long,
            default_value_t = false,
            help = "Print test group names and exit"
        )]
        list: bool,
        #[arg(long, default_value_t = false)]
        self_host: bool,
        #[arg(long, default_value_t = false)]
        toolchain: bool,
        #[arg(long, default_value_t = false)]
        external_parity: bool,
        #[arg(long, default_value_t = false)]
        owned_native: bool,
        #[arg(long, default_value_t = false)]
        all: bool,
        #[arg(long, default_value_t = false)]
        serial: bool,
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Reinstall the `in` CLI from the enclosing inauguration checkout (`cargo install --path in-cli`).
    #[command(visible_alias = "self-update")]
    Update,
    #[command(about = "Evaluate code or file (auto-detects file path vs inline code)")]
    Eval {
        #[arg(help = "Inline code, or path to .in/.poly/.rs/.py file")]
        source: Option<String>,
        #[arg(
            long,
            help = "Parser/language slug (auto-detected from extension or content if omitted)"
        )]
        parser: Option<String>,
        #[arg(long, default_value_t = false, help = "Show detailed output")]
        verbose: bool,
        #[arg(long, default_value_t = false, help = "Show detailed stage report")]
        debug: bool,
    },
    Doctor,
    #[command(about = "Summarize hotreload metrics")]
    Bench {
        #[arg(long, default_value = DEFAULT_BENCH_METRICS)]
        metrics: String,
    },
    #[command(about = "Manage installable optimization plugins")]
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
}

#[derive(Subcommand, Debug)]
enum PluginAction {
    #[command(about = "List built-in and installed plugins")]
    List,
    #[command(about = "Install plugin from built-in registry")]
    Install { name: String },
    #[command(about = "Run installed plugin against target path")]
    Run {
        name: String,
        #[arg(long, default_value = ".")]
        target: String,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    #[command(about = "Start the compiler daemon")]
    Start,
    #[command(about = "Stop the compiler daemon")]
    Stop,
    #[command(about = "Check daemon status")]
    Status,
}

#[path = "main/backend.rs"]
pub mod backend;
#[path = "main/bench.rs"]
pub mod bench;
#[path = "main/build.rs"]
pub mod build;
#[path = "main/compile.rs"]
pub mod compile;
#[path = "main/compiler_daemon.rs"]
pub mod compiler_daemon;
#[path = "main/daemon.rs"]
pub mod daemon;
#[path = "main/doctor.rs"]
pub mod doctor;
#[path = "main/eval.rs"]
pub mod eval;
#[path = "main/graph.rs"]
pub mod graph;
#[path = "main/package.rs"]
pub mod package;
#[path = "main/plugin.rs"]
pub mod plugin;
#[path = "main/tools.rs"]
pub mod tools;
#[path = "main/update.rs"]
pub mod update;
#[path = "main/util.rs"]
pub mod util;

fn main() {
    if let Err(err) = run() {
        eprintln!("in: {err}");
        std::process::exit(1);
    }
}

impl Commands {
    fn execute(self, invocation_cwd: &std::path::Path) -> Result<()> {
        use crate::backend::cmd_backend;
        use crate::bench::cmd_bench;
        use crate::build::cmd_build;
        use crate::compile::cmd_compile;
        use crate::compiler_daemon::{cmd_daemon_start, cmd_daemon_status, cmd_daemon_stop};
        use crate::daemon::{cmd_dev, cmd_run};
        use crate::doctor::cmd_doctor;
        use crate::eval::cmd_eval_source_or_path;
        use crate::graph::cmd_graph;
        use crate::package::{cmd_install, cmd_package, cmd_package_lock};
        use crate::plugin::cmd_plugin;
        use crate::tools::{cmd_agent, cmd_canonicalize, cmd_explain, cmd_fix, cmd_languages};
        use crate::update::{cmd_update, cmd_update_remote};
        use crate::util::workspace_root;

        match self {
            Commands::Build {
                path,
                out,
                release,
                profile,
                harden,
                lean,
                dual_emit,
                harden_out,
                module_id,
                verbose,
                swiftpm,
                allow_external_toolchain,
                parser,
            } => {
                let plan = inauguration::emit_profile::EmitProfile::resolve_dual_emit(
                    profile.to_owned(),
                    harden,
                    lean,
                    dual_emit,
                    out.as_deref(),
                    harden_out.as_deref(),
                )
                .map_err(InError::Message)?;
                let (profile, dual_harden_out) = match plan {
                    inauguration::emit_profile::ResolvedEmit::Single(profile) => (profile, None),
                    inauguration::emit_profile::ResolvedEmit::Dual {
                        runtime,
                        harden_out,
                    } => (runtime, Some(harden_out)),
                };
                cmd_build(
                    invocation_cwd,
                    &path,
                    out,
                    release,
                    profile,
                    dual_harden_out,
                    &module_id,
                    verbose,
                    swiftpm,
                    allow_external_toolchain,
                    parser,
                )
            }
            Commands::Agent {
                path,
                module_id,
                parser,
            } => cmd_agent(invocation_cwd, &path, &module_id, parser),
            Commands::Explain {
                diagnostic_code,
                json,
            } => cmd_explain(&diagnostic_code, json),
            Commands::Fix {
                plan,
                json,
                path,
                module_id,
                parser,
            } => cmd_fix(invocation_cwd, plan, json, &path, &module_id, parser),
            Commands::Canonicalize { path, check } => {
                cmd_canonicalize(invocation_cwd, &path, check)
            }
            Commands::Graph {
                path,
                module_id,
                parser,
                imports,
                capabilities,
                symbols,
                calls,
                json,
            } => cmd_graph(
                invocation_cwd,
                &path,
                &module_id,
                parser,
                inauguration::graph_report::GraphReportSelection {
                    imports,
                    capabilities,
                    symbols,
                    calls,
                },
                json,
            ),
            Commands::Install {
                packages,
                path,
                offline,
                json,
            } => cmd_install(invocation_cwd, &packages, &path, offline, json, "latest"),
            Commands::Add {
                packages,
                path,
                version,
                offline,
                json,
            } => cmd_install(invocation_cwd, &packages, &path, offline, json, &version),
            Commands::Package { action, path, json } => match action {
                Some(PackageCommands::Install {
                    path: install_path,
                    offline,
                    json: install_json,
                }) => cmd_install(
                    invocation_cwd,
                    &[],
                    &install_path,
                    offline,
                    install_json,
                    "latest",
                ),
                Some(PackageCommands::Lock {
                    path: lock_path,
                    json: lock_json,
                }) => cmd_package_lock(invocation_cwd, &lock_path, lock_json),
                None => cmd_package(invocation_cwd, &path, json),
            },
            Commands::Languages { json } => cmd_languages(json),
            Commands::Dev => cmd_dev(&workspace_root(invocation_cwd.to_path_buf())?),
            Commands::Daemon { action } => match action {
                DaemonAction::Start => cmd_daemon_start(),
                DaemonAction::Stop => cmd_daemon_stop(),
                DaemonAction::Status => cmd_daemon_status(),
            },
            Commands::Run {
                watch_root,
                socket,
                metrics,
                debounce_ms,
            } => cmd_run(
                &workspace_root(invocation_cwd.to_path_buf())?,
                &watch_root,
                &socket,
                &metrics,
                debounce_ms,
            ),
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
                emit,
                trampoline,
                base,
                metadata,
                debug,
                profile,
                harden,
                lean,
                dual_emit,
                harden_out,
            } => {
                let plan = inauguration::emit_profile::EmitProfile::resolve_dual_emit(
                    profile.to_owned(),
                    harden,
                    lean,
                    dual_emit,
                    Some(out.as_str()),
                    harden_out.as_deref(),
                )
                .map_err(InError::Message)?;
                let (profile, dual_harden_out) = match plan {
                    inauguration::emit_profile::ResolvedEmit::Single(profile) => (profile, None),
                    inauguration::emit_profile::ResolvedEmit::Dual {
                        runtime,
                        harden_out,
                    } => (runtime, Some(harden_out)),
                };
                cmd_compile(
                    invocation_cwd,
                    &path,
                    target,
                    &out,
                    &module_id,
                    parser,
                    entry.as_deref(),
                    target_triple.as_deref(),
                    linkage,
                    jobs,
                    json,
                    emit,
                    trampoline.as_deref(),
                    base.as_deref(),
                    metadata.as_deref(),
                    debug,
                    profile,
                    dual_harden_out.as_deref(),
                )
            }
            Commands::Execute {
                path,
                module_id,
                verbose,
                debug,
            } => crate::compile::cmd_execute(invocation_cwd, &path, &module_id, verbose, debug),
            Commands::Backend {
                path,
                module_id,
                parser,
                target,
                json,
            } => cmd_backend(invocation_cwd, &path, &module_id, parser, target, json),
            Commands::Test {
                list,
                self_host,
                toolchain,
                external_parity,
                owned_native,
                all,
                serial,
                paths,
            } => cli_test::cmd_test(
                &workspace_root(invocation_cwd.to_path_buf())?,
                cli_test::TestOptions {
                    list,
                    self_host,
                    toolchain,
                    external_parity,
                    owned_native,
                    all,
                    serial,
                    paths,
                },
            ),
            Commands::Update => match workspace_root(invocation_cwd.to_path_buf()) {
                Ok(root) => cmd_update(&root),
                Err(_) => cmd_update_remote(),
            },
            Commands::Eval {
                source,
                parser,
                verbose,
                debug,
            } => cmd_eval_source_or_path(invocation_cwd, source, parser, verbose, debug),
            Commands::Doctor => cmd_doctor(),
            Commands::Bench { metrics } => {
                cmd_bench(&workspace_root(invocation_cwd.to_path_buf())?, &metrics)
            }
            Commands::Plugin { action } => {
                cmd_plugin(&workspace_root(invocation_cwd.to_path_buf())?, action)
            }
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let invocation_cwd = crate::util::cwd()?;
    cli.command.execute(&invocation_cwd)
}
#[cfg(test)]
#[path = "main/tests.rs"]
mod tests;
