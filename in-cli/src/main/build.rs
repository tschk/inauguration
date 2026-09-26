use crate::util::{run_cmd, run_cmd_silent};
use crate::{InError, Result};
use inauguration::owned_compile::{CompileTarget, OwnedCompileRequest, compile_owned};
use inauguration::parser_registry::ParserCli;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

pub(crate) fn cmd_build(
    invocation_cwd: &Path,
    path: &str,
    out: Option<String>,
    release: bool,
    profile: inauguration::emit_profile::EmitProfile,
    module_id: &str,
    verbose: bool,
    swiftpm: bool,
    allow_external_toolchain: bool,
    parser: ParserCli,
) -> Result<()> {
    let start = Instant::now();
    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        invocation_cwd.join(path)
    };
    let display_target = resolved.display();
    if !allow_external_toolchain
        && resolved
            .extension()
            .is_some_and(|ext| ext == "swift" || ext == "swiftpm")
        && inauguration::config::env_config().native_swift_sil_enabled
    {
        return Err(InError::Message(
            "in: owned build path rejects external Swift toolchain; pass --allow-external-toolchain to permit swiftc/SwiftPM fallback on `in build`"
                .to_string(),
        ));
    }
    let result =
        run_pipeline_for_path(&resolved, out, release, profile, module_id, verbose, parser);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let wall = format!("{elapsed_ms:.3}ms");
    let mut emit_note = String::new();
    if swiftpm && let Some(package_root) = find_package_root(&resolved) {
        let build_result = if verbose {
            run_cmd(
                Command::new("swift")
                    .arg("build")
                    .current_dir(&package_root),
            )
        } else {
            run_cmd_silent(
                Command::new("swift")
                    .arg("build")
                    .current_dir(&package_root),
            )
        };
        build_result?;
        let bin_dir = swift_bin_path(&package_root)?;
        #[cfg(unix)]
        {
            match stage_swift_products(&package_root, &bin_dir) {
                Ok(summary) => emit_note = staging_emit_note(&summary, Some(&bin_dir)),
                Err(e) => {
                    let executables = swift_executables_in_dir(&bin_dir);
                    emit_note = if executables.is_empty() {
                        format!(
                            " -> {} (no executable product; library artifacts built); staging failed: {}",
                            bin_dir.display(),
                            e
                        )
                    } else {
                        format!(
                            " -> {} [{}]; staging failed: {}",
                            bin_dir.display(),
                            executables.join(", "),
                            e
                        )
                    };
                }
            }
        }
        #[cfg(not(unix))]
        {
            let executables = swift_executables_in_dir(&bin_dir);
            emit_note = if executables.is_empty() {
                format!(
                    " -> {} (no executable product; library artifacts built)",
                    bin_dir.display()
                )
            } else {
                format!(" -> {} [{}]", bin_dir.display(), executables.join(", "))
            };
        }
    }

    if verbose {
        if result.is_ok() {
            println!("    Finished `in build` in {wall}{emit_note}");
        }
        println!("in.build_wall_ms={elapsed_ms:.3}");
    } else if result.is_err() {
        println!(
            "\x1b[31m✗\x1b[0m \x1b[36min build\x1b[0m {display_target} \x1b[2m({wall})\x1b[0m"
        );
    }
    result
}

fn resolve_source_path(path: &Path) -> Result<PathBuf> {
    if path.is_dir() {
        let cargo_toml = path.join("Cargo.toml");
        if cargo_toml.exists() {
            return resolve_cargo_root(&cargo_toml);
        }
        for candidate in &["src/main.rs", "src/lib.rs", "main.rs", "lib.rs"] {
            let candidate_path = path.join(candidate);
            if candidate_path.exists() {
                return Ok(candidate_path);
            }
        }
        return Err(InError::Message(format!(
            "no Rust source found in directory `{}`",
            path.display()
        )));
    }
    if path.file_name().and_then(|s| s.to_str()) == Some("Cargo.toml") {
        return resolve_cargo_root(path);
    }
    Ok(path.to_path_buf())
}

fn resolve_cargo_root(cargo_toml: &Path) -> Result<PathBuf> {
    let base = cargo_toml.parent().unwrap_or(Path::new("."));
    let mut path = base.to_path_buf();
    path.push("src");
    for candidate in &["main.rs", "lib.rs"] {
        path.push(candidate);
        if path.exists() {
            return Ok(path);
        }
        path.pop();
    }
    path.pop();

    for member_dir in &["in-cli", "inauguration"] {
        path.push(member_dir);
        path.push("src");
        for candidate in &["main.rs", "lib.rs"] {
            path.push(candidate);
            if path.exists() {
                return Ok(path);
            }
            path.pop();
        }
        path.pop();
        path.pop();
    }
    Err(InError::Message(format!(
        "no src/main.rs or src/lib.rs found for `{}`",
        cargo_toml.display()
    )))
}

fn run_pipeline_for_path(
    path: &Path,
    out: Option<String>,
    _release: bool,
    profile: inauguration::emit_profile::EmitProfile,
    module_id: &str,
    verbose: bool,
    parser: ParserCli,
) -> Result<()> {
    let source_path = resolve_source_path(path)?;
    let is_rust = source_path.extension().is_some_and(|e| e == "rs");

    if let Some(out_path_str) = out {
        let out_path = if Path::new(&out_path_str).is_absolute() {
            PathBuf::from(&out_path_str)
        } else {
            std::env::current_dir()
                .unwrap_or_default()
                .join(&out_path_str)
        };

        if is_rust && verbose {
            let parse_result = inauguration::compiler::rust_front::parse_rust_file(&source_path);
            if let Ok(m) = &parse_result {
                let n = m
                    .decls
                    .iter()
                    .filter(|d| matches!(d, inauguration::core_ir::Decl::Function { .. }))
                    .count();
                eprintln!("  in parse: {n} functions");
            }
        }

        let start = std::time::Instant::now();
        let request = OwnedCompileRequest {
            path: source_path.clone(),
            module_id: module_id.to_string(),
            parser,
            target: CompileTarget::Native,
            entry: Some("main".to_string()),
            out: Some(out_path.clone()),
            linkage: inauguration::native_emit::NativeLinkage::Executable,
            target_triple: None,
            jobs: 1,
            debug: false,
            profile,
            emit: None,
            base: None,
        };
        let report = compile_owned(&request);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        if !report.success {
            let reason = report.reason_code.as_deref().unwrap_or("unknown");
            // The reason code alone hides the cause; the report carries the
            // lowerer's or linker's own message.
            let detail = report
                .error
                .as_deref()
                .or(report.reason.as_deref())
                .unwrap_or("no detail reported");
            return Err(InError::Message(format!(
                "in build: native compilation failed ({reason}): {detail}"
            )));
        }
        if verbose {
            println!("in built {} in {:.1}ms", out_path.display(), elapsed_ms);
            println!("  backend: {}", report.backend_level);
        }
        return Ok(());
    }

    let start = std::time::Instant::now();

    if is_rust {
        let request = OwnedCompileRequest {
            path: source_path.clone(),
            module_id: module_id.to_string(),
            parser,
            target: CompileTarget::Jit,
            entry: Some("main".to_string()),
            out: None,
            linkage: inauguration::native_emit::NativeLinkage::Executable,
            target_triple: None,
            jobs: 1,
            debug: false,
            profile,
            emit: None,
            base: None,
        };
        let report = compile_owned(&request);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        if !report.success {
            let err = report.error.unwrap_or_else(|| "unknown error".into());
            return Err(InError::Message(format!("compile failed: {err}")));
        }
        if verbose {
            println!("Compiled {} in {:.3}ms", source_path.display(), elapsed_ms);
            println!("  target: analysis (jit path)");
            println!(
                "  functions: {} parsed, {} typed",
                report.parsed_function_count, report.typed_function_count
            );
            println!("  note: use --out <path> to compile to a native binary via cargo");
        }
        return Ok(());
    }

    let request = OwnedCompileRequest {
        path: source_path.clone(),
        module_id: module_id.to_string(),
        parser,
        target: CompileTarget::Jit,
        entry: Some("main".to_string()),
        out: None,
        linkage: inauguration::native_emit::NativeLinkage::Executable,
        target_triple: None,
        jobs: 1,
        debug: false,
        profile,
        emit: None,
        base: None,
    };
    let report = compile_owned(&request);
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    if !report.success {
        let err = report.error.unwrap_or_else(|| "unknown error".into());
        return Err(InError::Message(format!("compile failed: {err}")));
    }
    if verbose {
        println!("Compiled {} in {:.3}ms", source_path.display(), elapsed_ms);
        println!("  target: jit");
        println!(
            "  functions: {} parsed, {} typed",
            report.parsed_function_count, report.typed_function_count
        );
    }
    Ok(())
}

fn find_package_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        if current.join("Package.swift").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn swift_bin_path(package_root: &Path) -> Result<PathBuf> {
    let output = Command::new("swift")
        .env_clear()
        .arg("build")
        .arg("--show-bin-path")
        .current_dir(package_root)
        .output()?;
    if output.status.success() {
        Ok(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Err(InError::Message(format!(
            "swift build --show-bin-path failed with status {}",
            output.status
        )))
    }
}

fn swift_executables_in_dir(bin_dir: &Path) -> Vec<String> {
    let mut bins = Vec::new();
    let Ok(entries) = fs::read_dir(bin_dir) else {
        return bins;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = fs::metadata(&path)
                && metadata.permissions().mode() & 0o111 == 0
            {
                continue;
            }
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            bins.push(name.to_string());
        }
    }
    bins.sort();
    bins
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct StageSummary {
    bin_names: Vec<String>,
    artifact_names: Vec<String>,
}

#[cfg(unix)]
fn clear_dir_contents(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            fs::remove_file(&path)?;
        } else if meta.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn is_swift_products_internal_skip(name: &str, is_dir: bool) -> bool {
    if matches!(
        name,
        "Modules" | "ModuleCache" | "index" | "description.json" | "plugin-tools-description.json"
    ) {
        return true;
    }
    if is_dir && name.ends_with(".build") {
        return true;
    }
    name.starts_with("swift-version-") && name.ends_with(".txt")
}

#[cfg(unix)]
fn excluded_non_bin_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext,
        "a" | "dylib" | "swiftmodule" | "json" | "txt" | "swiftdoc" | "swiftsourceinfo"
    )
}

#[cfg(unix)]
fn is_unix_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    if excluded_non_bin_extension(path) {
        return false;
    }
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(unix)]
fn should_stage_as_bin(path: &Path, name: &str) -> bool {
    if is_swift_products_internal_skip(name, path.is_dir()) {
        return false;
    }
    if path.is_dir() && name.ends_with(".app") {
        return true;
    }
    is_unix_executable_file(path)
}

#[cfg(unix)]
fn should_stage_as_artifact(path: &Path, name: &str) -> bool {
    if is_swift_products_internal_skip(name, path.is_dir()) {
        return false;
    }
    if name.ends_with(".xctest")
        || name.ends_with(".dSYM")
        || name.ends_with(".bundle")
        || name.ends_with(".product")
    {
        return true;
    }
    path.is_file() && name.ends_with(".plist")
}

#[cfg(unix)]
fn stage_swift_products(package_root: &Path, products_dir: &Path) -> Result<StageSummary> {
    let bin_stage = package_root.join(".build/bin");
    let art_stage = package_root.join(".build/artifacts");
    fs::create_dir_all(&bin_stage)?;
    fs::create_dir_all(&art_stage)?;
    clear_dir_contents(&bin_stage)?;
    clear_dir_contents(&art_stage)?;

    let mut summary = StageSummary::default();
    let entries = fs::read_dir(products_dir)?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let target = path
            .canonicalize()
            .map_err(|e| InError::Message(format!("canonicalize {}: {e}", path.display())))?;

        if should_stage_as_bin(&path, name) {
            let link = bin_stage.join(name);
            std::os::unix::fs::symlink(&target, &link)?;
            summary.bin_names.push(name.to_string());
        } else if should_stage_as_artifact(&path, name) {
            let link = art_stage.join(name);
            std::os::unix::fs::symlink(&target, &link)?;
            summary.artifact_names.push(name.to_string());
        }
    }
    summary.bin_names.sort();
    summary.artifact_names.sort();
    Ok(summary)
}

#[cfg(unix)]
fn staging_emit_note(summary: &StageSummary, swift_products_dir: Option<&Path>) -> String {
    let mut parts = Vec::new();
    if !summary.bin_names.is_empty() {
        parts.push(format!(".build/bin [{}]", summary.bin_names.join(", ")));
    } else if let Some(p) = swift_products_dir {
        parts.push(format!(
            ".build/bin (empty); SwiftPM products {}",
            p.display()
        ));
    } else {
        parts.push(".build/bin (empty)".to_string());
    }
    if !summary.artifact_names.is_empty() {
        let hint = if summary.artifact_names.len() <= 4 {
            summary.artifact_names.join(", ")
        } else {
            format!(
                "{}, +{} more",
                summary.artifact_names[..4].join(", "),
                summary.artifact_names.len() - 4
            )
        };
        parts.push(format!(".build/artifacts [{hint}]"));
    }
    format!(" -> {}", parts.join("; "))
}
