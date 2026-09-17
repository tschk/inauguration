use crate::CompileTargetCli;
use crate::util::{
    compile_linkage_cli_to_owned, compile_target_cli_to_owned, extract_cargo_bin_path,
    resolve_invocation_path,
};
use crate::{EmitKindCli, InError, NativeLinkageCli, Result};
use inauguration::owned_compile::{OwnedCompileRequest, OwnedEmit, compile_owned, report_to_json};
use inauguration::parser_registry::ParserCli;
use std::fs;
use std::path::Path;
use std::time::Instant;

pub(crate) fn cmd_compile(
    cwd: &Path,
    path: &str,
    target: CompileTargetCli,
    out: &str,
    module_id: &str,
    parser: ParserCli,
    entry: Option<&str>,
    target_triple: Option<&str>,
    linkage: NativeLinkageCli,
    jobs: usize,
    json: bool,
    emit: Option<EmitKindCli>,
    trampoline: Option<&str>,
    base: Option<&str>,
    metadata: Option<&str>,
    debug: bool,
    profile: inauguration::emit_profile::EmitProfile,
    dual_harden_out: Option<&str>,
) -> Result<()> {
    if let Some(harden_out) = dual_harden_out {
        cmd_compile(
            cwd,
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
            None,
        )?;
        return cmd_compile(
            cwd,
            path,
            target,
            harden_out,
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
            inauguration::emit_profile::EmitProfile::Harden,
            None,
        );
    }
    let source_path = resolve_invocation_path(cwd, path);
    let out_path = resolve_invocation_path(cwd, out);
    let source_path = if source_path.is_dir() {
        let cargo_toml = source_path.join("Cargo.toml");
        if cargo_toml.exists() {
            let contents = fs::read_to_string(&cargo_toml)
                .map_err(|e| InError::Message(format!("read Cargo.toml: {e}")))?;
            extract_cargo_bin_path(&contents, &source_path)?
        } else {
            source_path
        }
    } else {
        source_path
    };
    let auto_module_id = if module_id == "App" {
        source_path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string()
    } else {
        module_id.to_string()
    };
    if !source_path.exists() {
        return Err(InError::Message(format!(
            "file not found: {}",
            source_path.display()
        )));
    }

    if matches!(emit, Some(EmitKindCli::Boot)) {
        return cmd_emit_boot(
            cwd,
            &source_path,
            &out_path,
            entry,
            trampoline,
            target_triple,
            metadata,
        );
    }
    if matches!(
        emit,
        Some(EmitKindCli::C) | Some(EmitKindCli::Go) | Some(EmitKindCli::Rust)
    ) {
        let kind = emit.expect("matched emit kind");
        return cmd_emit_source(&source_path, &out_path, parser, kind);
    }
    let parsed_base = base.map(parse_base).transpose()?;
    let owned_emit = match emit {
        Some(EmitKindCli::Sci) => {
            let base_val = parsed_base
                .ok_or_else(|| InError::Message("--base is required for --emit sci".to_string()))?;
            Some(OwnedEmit::Sci { base: base_val })
        }
        _ => None,
    };
    let request = OwnedCompileRequest {
        path: source_path,
        module_id: auto_module_id,
        parser,
        target: compile_target_cli_to_owned(target),
        entry: entry.map(str::to_string),
        out: Some(out_path),
        linkage: compile_linkage_cli_to_owned(linkage),
        target_triple: target_triple.map(str::to_string),
        jobs: jobs.max(1),
        debug,
        profile,
        emit: owned_emit,
        base: parsed_base,
    };
    let report = compile_owned(&request);

    if json {
        let raw = report_to_json(&report)
            .map_err(|err| InError::Message(format!("owned compile json: {err}")))?;
        println!("{raw}");
    } else if debug {
        println!("owned: {}", report.owned);
        println!("success: {}", report.success);
        if let Some(code) = &report.reason_code {
            println!("reason_code: {code}");
        }
        if let Some(reason) = &report.reason {
            println!("reason: {reason}");
        }
        println!("path: {}", report.path);
        println!("module_id: {}", report.module_id);
        if let Some(identity) = &report.module_identity {
            if let Some(package) = &identity.package {
                println!("package: {package}");
            }
            if let Some(module) = &identity.module {
                println!("module: {module}");
            }
            if identity.effective_module_id != report.module_id {
                println!("effective_module_id: {}", identity.effective_module_id);
            }
        }
        println!("target: {}", report.target);
        if let Some(target_triple) = &report.target_triple {
            println!("target_triple: {target_triple}");
        }
        if let Some(entry) = &report.entry {
            println!("entry: {entry}");
        }
        println!("linkage: {}", report.linkage);
        println!("frontend_level: {}", report.frontend_level);
        println!("semantic_level: {}", report.semantic_level);
        println!("backend_level: {}", report.backend_level);
        println!("runtime_level: {}", report.runtime_level);
        if !report.external_invocations.is_empty() {
            println!(
                "external_invocations: {}",
                report.external_invocations.join(", ")
            );
        }
        if let Some(path) = &report.artifact_path {
            println!("artifact_path: {path}");
        }
        if let Some(path) = &report.executable_path {
            println!("executable_path: {path}");
        }
        if let Some(path) = &report.abi_path {
            println!("abi_path: {path}");
        }
        println!("parsed_function_count: {}", report.parsed_function_count);
        println!("typed_function_count: {}", report.typed_function_count);
        println!("call_edge_count: {}", report.call_edge_count);
        println!("jobs: {}", report.jobs);
        println!("timing.total_us={}", report.timing_micros);
        if let Some(waves) = &report.timing_waves_us {
            println!("timing.waves_us={waves:?}");
        }
        if report.cache_hit {
            println!("cache_hit: true");
        }
        if let Some(hash) = &report.frontend_hash {
            println!("frontend_hash: {hash}");
        }
    }

    if !report.success && !json {
        return Err(InError::Message(
            report
                .reason
                .unwrap_or_else(|| "owned compile failed".to_string()),
        ));
    }
    Ok(())
}

fn parse_base(value: &str) -> Result<u64> {
    if let Some(stripped) = value.strip_prefix("0x") {
        u64::from_str_radix(stripped, 16)
            .map_err(|_| InError::Message(format!("invalid hex --base: {value}")))
    } else {
        value
            .parse::<u64>()
            .map_err(|_| InError::Message(format!("invalid --base: {value}")))
    }
}

fn cmd_emit_source(
    source_path: &Path,
    out_path: &Path,
    parser: ParserCli,
    kind: EmitKindCli,
) -> Result<()> {
    use inauguration::native_emit::source_emit::{SourceLang, emit_source_module};
    use inauguration::parser_registry;

    let resolved = parser_registry::resolve_parser_id(source_path, parser);
    let module = match parser_registry::parse_with_resolved(resolved, source_path) {
        Ok(Some(module)) => module,
        Ok(None) => {
            return Err(InError::Message(
                "--emit source needs a Core IR front (all polyglot langs / .in / .icore)".into(),
            ));
        }
        Err(err) => {
            // Library .in without `fn main` still useful for source dump.
            if source_path.extension().is_some_and(|e| e == "in") {
                inauguration::in_lang_parse::parse_in_library_file(source_path).map_err(|e| {
                    InError::Message(format!(
                        "parse {}: {e} (also: {err})",
                        source_path.display()
                    ))
                })?
            } else {
                return Err(InError::Message(format!(
                    "parse {}: {err}",
                    source_path.display()
                )));
            }
        }
    };
    let (label, text) = match kind {
        EmitKindCli::C => (
            "c",
            inauguration::native_emit::c_backend::emit_c_module(&module)
                .map_err(InError::Message)?,
        ),
        EmitKindCli::Go => ("go", emit_source_module(&module, SourceLang::Go)),
        EmitKindCli::Rust => ("rust", emit_source_module(&module, SourceLang::Rust)),
        _ => {
            return Err(InError::Message(format!(
                "internal: cmd_emit_source got {kind:?}"
            )));
        }
    };
    fs::write(out_path, &text)
        .map_err(|e| InError::Message(format!("write {}: {e}", out_path.display())))?;
    println!(
        "{label} source: {} → {} ({} bytes)",
        source_path.display(),
        out_path.display(),
        text.len()
    );
    Ok(())
}

fn cmd_emit_boot(
    cwd: &Path,
    source_path: &Path,
    out_path: &Path,
    entry: Option<&str>,
    trampoline: Option<&str>,
    target_triple: Option<&str>,
    _metadata: Option<&str>,
) -> Result<()> {
    let trampoline_path = trampoline
        .map(|t| resolve_invocation_path(cwd, t))
        .ok_or_else(|| InError::Message("--trampoline is required for --emit boot".to_string()))?;
    let entry_name =
        entry.ok_or_else(|| InError::Message("--entry is required for --emit boot".to_string()))?;

    let trampoline_bytes = fs::read(&trampoline_path)
        .map_err(|e| InError::Message(format!("read trampoline: {e}")))?;

    let mut module = inauguration::in_lang_parse::parse_in_library_file(source_path)
        .map_err(|e| InError::Message(format!("parse {}: {e}", source_path.display())))?;

    inauguration::core_opt::optimize_with_entry(&mut module.decls, Some(entry_name));

    let (_mir, code) =
        inauguration::compiler::mir_lower::lower_boot_image(&module, entry_name, target_triple)
            .map_err(|e| InError::Message(format!("lower: {e}")))?;

    let tramp_size = trampoline_bytes.len();
    if tramp_size != 0x1000 {
        return Err(InError::Message(format!(
            "trampoline size {tramp_size} != expected 4096 (0x1000)"
        )));
    }
    const SCI_HEADER_SIZE: usize = 256;
    const SCI_CODE_OFFSET: usize = 0x100;
    let mut sci_header = vec![0u8; SCI_HEADER_SIZE];

    let target_is_aarch64 = target_triple
        .map(|t| t.contains("aarch64") || t.contains("arm64"))
        .unwrap_or(false);
    if target_is_aarch64 {
        // Encode an AArch64 unconditional branch 'b' to SCI_CODE_OFFSET (which is 256 bytes = 0x100)
        let branch_inst = inauguration::native_emit::aarch64::b(SCI_CODE_OFFSET as i32);
        sci_header[0..4].copy_from_slice(&branch_inst.to_le_bytes());
    } else {
        let jmp_disp = SCI_CODE_OFFSET as i32 - 5;
        sci_header[0] = 0xE9;
        sci_header[1..5].copy_from_slice(&jmp_disp.to_le_bytes());
    }
    sci_header[8..16].copy_from_slice(b"SCI\0\0\0\0\x01");
    sci_header[16..24].copy_from_slice(&1u64.to_le_bytes());
    sci_header[24..32].copy_from_slice(&(SCI_CODE_OFFSET as u64).to_le_bytes());
    sci_header[32..40].copy_from_slice(&(code.len() as u64).to_le_bytes());
    sci_header[40..48].copy_from_slice(&0u64.to_le_bytes());
    let mut flags = 0u64;
    for decl in &module.decls {
        if let inauguration::core_ir::Decl::Component {
            deterministic: true,
            ..
        } = decl
        {
            flags |= 1;
        }
    }
    sci_header[48..56].copy_from_slice(&flags.to_le_bytes());
    let mut caps_mask = 0u64;
    let mut ci = 0u64;
    for decl in &module.decls {
        if let inauguration::core_ir::Decl::Component { capabilities, .. } = decl {
            for _ in capabilities {
                caps_mask |= 1u64 << ci;
                ci += 1;
            }
        }
    }
    sci_header[56..64].copy_from_slice(&caps_mask.to_le_bytes());

    let mut image = Vec::with_capacity(tramp_size + SCI_HEADER_SIZE + code.len());
    image.extend_from_slice(&trampoline_bytes);
    image.extend_from_slice(&sci_header);
    image.extend_from_slice(&code);

    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| InError::Message(format!("create output dir: {e}")))?;
    }
    fs::write(out_path, &image).map_err(|e| InError::Message(format!("write boot image: {e}")))?;

    let meta_path = out_path.with_extension("component-metadata.json");
    let mut schemas: Vec<serde_json::Value> = Vec::new();
    let mut component = serde_json::json!(null);
    let mut target = serde_json::json!(null);
    let mut deterministic = serde_json::json!(null);
    let mut checkpoint = serde_json::json!(null);
    let mut imports = serde_json::json!([]);
    let mut exports = serde_json::json!([]);
    let mut caps = serde_json::json!([]);
    for decl in &module.decls {
        match decl {
            inauguration::core_ir::Decl::Component {
                name,
                target: t,
                deterministic: det,
                checkpoint: chk,
                imports: imps,
                exports: exps,
                capabilities: capabs,
                ..
            } => {
                component = serde_json::json!(name);
                target = serde_json::json!(t);
                deterministic = serde_json::json!(det);
                checkpoint = serde_json::json!(chk);
                imports = serde_json::json!(
                    imps.iter()
                        .map(|i| { serde_json::json!({"name": i.name, "interface": i.interface}) })
                        .collect::<Vec<_>>()
                );
                exports = serde_json::json!(
                    exps.iter()
                        .map(|e| { serde_json::json!({"name": e.name, "interface": e.interface}) })
                        .collect::<Vec<_>>()
                );
                caps = serde_json::json!(
                    capabs
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "name": c.name,
                                "capability_type": c.capability_type,
                                "args": c.args,
                            })
                        })
                        .collect::<Vec<_>>()
                );
            }
            inauguration::core_ir::Decl::Struct { name, fields, .. } => {
                let schema_fields: Vec<serde_json::Value> = fields
                    .iter()
                    .map(
                        |(fn_, typ)| serde_json::json!({"name": fn_, "type": format!("{:?}", typ)}),
                    )
                    .collect();
                schemas.push(serde_json::json!({
                    "name": name,
                    "fields": schema_fields,
                }));
            }
            _ => {}
        }
    }
    let meta = serde_json::json!({
        "component": component,
        "target": target,
        "entry": entry_name,
        "imports": imports,
        "exports": exports,
        "capabilities_required": caps,
        "object_schemas": schemas,
        "deterministic": deterministic,
        "checkpoint": checkpoint,
        "code_size": code.len(),
        "provenance": {
            "compiler": "inauguration",
            "compiler_version": env!("CARGO_PKG_VERSION"),
        }
    });
    if let Err(e) = fs::write(
        &meta_path,
        serde_json::to_string_pretty(&meta).unwrap_or_else(|_| "{}".to_string()),
    ) {
        eprintln!(
            "[metadata] warning: failed to write {}: {e}",
            meta_path.display()
        );
    }

    eprintln!(
        "boot image: {} bytes (trampoline: {} + kernel: {})",
        image.len(),
        tramp_size,
        code.len()
    );
    Ok(())
}

#[derive(Debug)]
pub(crate) enum JitExecution {
    Int(i64),
    Bool(bool),
    Float(FloatValFmt),
    String(String),
}

pub(crate) struct FloatValFmt(pub f64);

impl std::fmt::Debug for FloatValFmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FloatVal({:?})", self.0)
    }
}

impl std::fmt::Display for FloatValFmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FloatVal({:?})", self.0)
    }
}

enum EntryReturnKind {
    Bool,
    Float,
    Other,
}

fn entry_return_kind(path: &Path) -> EntryReturnKind {
    let Ok(src) = std::fs::read_to_string(path) else {
        return EntryReturnKind::Other;
    };
    for line in src.lines() {
        let t = line.trim();
        if t.contains("fn main") && t.contains("->") {
            if t.contains("Bool") || t.contains("bool") {
                return EntryReturnKind::Bool;
            }
            if t.contains("Float") || t.contains("float") {
                return EntryReturnKind::Float;
            }
        }
    }
    EntryReturnKind::Other
}

pub(crate) fn compile_and_run_jit_report(
    source_path: &Path,
    module_id: &str,
    parser: ParserCli,
    debug: bool,
) -> Result<(
    inauguration::owned_compile::OwnedCompileReport,
    JitExecution,
)> {
    use inauguration::native_emit::NativeLinkage;
    use inauguration::owned_compile::{CompileTarget, OwnedCompileRequest};
    let request = OwnedCompileRequest {
        path: source_path.to_path_buf(),
        module_id: module_id.to_string(),
        parser,
        target: CompileTarget::Jit,
        entry: Some("main".to_string()),
        out: None,
        linkage: NativeLinkage::Executable,
        target_triple: None,
        jobs: 1,
        debug,
        profile: inauguration::emit_profile::EmitProfile::Default,
        emit: None,
        base: None,
    };
    let report = inauguration::owned_compile::compile_owned(&request);
    if !report.success {
        return Err(InError::Message(
            report
                .error
                .unwrap_or_else(|| "jit eval failed".to_string()),
        ));
    }
    let execution = if let Some(s) = report.eval_result_string.clone() {
        JitExecution::String(s)
    } else {
        match entry_return_kind(source_path) {
            EntryReturnKind::Bool => JitExecution::Bool(report.eval_result.unwrap_or(0) != 0),
            EntryReturnKind::Float => {
                JitExecution::Float(FloatValFmt(report.eval_result.unwrap_or(0) as f64))
            }
            EntryReturnKind::Other => JitExecution::Int(report.eval_result.unwrap_or(0)),
        }
    };
    Ok((report, execution))
}

pub(crate) fn compile_and_run_jit_source_path(
    source_path: &Path,
    module_id: &str,
    parser: ParserCli,
    debug: bool,
) -> Result<JitExecution> {
    compile_and_run_jit_report(source_path, module_id, parser, debug).map(|(_, exec)| exec)
}

pub(crate) fn cmd_execute(
    cwd: &Path,
    path: &str,
    module_id: &str,
    verbose: bool,
    debug: bool,
) -> Result<()> {
    let start = Instant::now();
    let source_path = resolve_invocation_path(cwd, path);

    if !source_path.exists() {
        return Err(InError::Message(format!(
            "file not found: {}",
            source_path.display()
        )));
    }

    if let Some(ext) = source_path.extension().and_then(|s| s.to_str()) {
        if verbose {
            eprintln!("[jit] Detected file extension: {}", ext);
        }
    } else {
        return Err(InError::Message(
            "unable to determine file type (no extension)".into(),
        ));
    }

    let (report, result) =
        compile_and_run_jit_report(&source_path, module_id, ParserCli::Auto, debug)?;

    if verbose {
        eprintln!("[jit] Execution completed with result: {:?}", result);
    }

    if !report.success {
        return Err(InError::Message(
            report
                .error
                .clone()
                .unwrap_or_else(|| "jit execution failed".to_string()),
        ));
    }

    if debug {
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        println!("[jit] Finished execution in {:.3}ms", elapsed_ms);
        print_owned_report(&report);
    }

    Ok(())
}

fn print_owned_report(report: &inauguration::owned_compile::OwnedCompileReport) {
    println!("owned: {}", report.owned);
    println!("success: {}", report.success);
    if let Some(code) = &report.reason_code {
        println!("reason_code: {code}");
    }
    if let Some(reason) = &report.reason {
        println!("reason: {reason}");
    }
    println!("path: {}", report.path);
    println!("module_id: {}", report.module_id);
    if let Some(identity) = &report.module_identity {
        if let Some(package) = &identity.package {
            println!("package: {package}");
        }
        if let Some(module) = &identity.module {
            println!("module: {module}");
        }
        if identity.effective_module_id != report.module_id {
            println!("effective_module_id: {}", identity.effective_module_id);
        }
    }
    println!("target: {}", report.target);
    if let Some(target_triple) = &report.target_triple {
        println!("target_triple: {target_triple}");
    }
    if let Some(entry) = &report.entry {
        println!("entry: {entry}");
    }
    println!("linkage: {}", report.linkage);
    println!("frontend_level: {}", report.frontend_level);
    println!("semantic_level: {}", report.semantic_level);
    println!("backend_level: {}", report.backend_level);
    println!("runtime_level: {}", report.runtime_level);
    if !report.external_invocations.is_empty() {
        println!(
            "external_invocations: {}",
            report.external_invocations.join(", ")
        );
    }
    if let Some(path) = &report.artifact_path {
        println!("artifact_path: {path}");
    }
    if let Some(path) = &report.executable_path {
        println!("executable_path: {path}");
    }
    if let Some(path) = &report.abi_path {
        println!("abi_path: {path}");
    }
    println!("parsed_function_count: {}", report.parsed_function_count);
    println!("typed_function_count: {}", report.typed_function_count);
    println!("call_edge_count: {}", report.call_edge_count);
    println!("jobs: {}", report.jobs);
    println!("timing.total_us={}", report.timing_micros);
    if let Some(waves) = &report.timing_waves_us {
        println!("timing.waves_us={waves:?}");
    }
    if report.cache_hit {
        println!("cache_hit: true");
    }
    if let Some(hash) = &report.frontend_hash {
        println!("frontend_hash: {hash}");
    }
}
