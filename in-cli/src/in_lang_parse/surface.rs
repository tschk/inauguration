use super::decl::parse_extern_fn_block;
use super::types::parse_fn_header;
use super::util::*;
use crate::core_ir::{Decl, Typ};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InSurfaceInfo {
    pub package: Option<String>,
    pub module: Option<String>,
    pub imports: Vec<String>,
    pub semantic_imports: Vec<String>,
    pub semantic_bindings: Vec<InSemanticBinding>,
    pub capabilities: Vec<String>,
    pub externs: Vec<InExternBinding>,
    pub orchestration: InOrchestrationFacts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InSemanticBinding {
    pub import: String,
    pub alias: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InExternBinding {
    pub language: String,
    pub name: String,
    pub params: Vec<(String, Typ)>,
    pub required_capabilities: Vec<String>,
    pub ret: Option<Typ>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InOrchestrationFacts {
    pub enabled_extensions: Vec<String>,
    pub annotations: Vec<InAnnotationFact>,
    pub distributed_functions: Vec<String>,
    pub parallel_regions: usize,
    pub parallel_tasks: Vec<InParallelTaskFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InAnnotationFact {
    pub name: String,
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InParallelTaskFact {
    pub region: usize,
    pub name: String,
}

pub(crate) fn std_binding(name: &str, caps: Vec<String>) -> InExternBinding {
    InExternBinding {
        language: "std".into(),
        name: name.into(),
        params: vec![],
        required_capabilities: caps,
        ret: None,
    }
}

pub fn in_standard_import_bindings(import: &str) -> Vec<InExternBinding> {
    match normalize_import_path(import) {
        "std.io" => vec![
            std_binding("print", vec!["process.stdout".into()]),
            std_binding("println", vec!["process.stdout".into()]),
            std_binding("eprintln", vec!["process.stderr".into()]),
        ],
        "std.fs" => vec![
            std_binding("read-file", vec!["fs.read".into()]),
            std_binding("write-file", vec!["fs.write".into()]),
            std_binding("fs-exists", vec!["fs.read".into()]),
            std_binding("create-dir", vec!["fs.write".into()]),
            std_binding("remove-file", vec!["fs.write".into()]),
        ],
        "std.http" => vec![std_binding("http-get", vec!["network.http".into()])],
        "std.json" => vec![
            std_binding("json-parse", Vec::new()),
            std_binding("json-stringify", Vec::new()),
        ],
        "std.process" => vec![std_binding("process-run", vec!["process.spawn".into()])],
        "std.cli" => vec![
            std_binding("arg-count", vec!["process.args".into()]),
            std_binding("arg", vec!["process.args".into()]),
        ],
        "std.env" => vec![
            std_binding("env-get", vec!["env.read".into()]),
            std_binding("env-set", vec!["env.write".into()]),
            std_binding("env-has", vec!["env.read".into()]),
            std_binding("env-temp-dir", vec!["env.read".into()]),
            std_binding("env-current-dir", vec!["env.read".into()]),
        ],
        "std.path" => vec![
            std_binding("path-join", Vec::new()),
            std_binding("path-dirname", Vec::new()),
            std_binding("path-basename", Vec::new()),
            std_binding("path-extname", Vec::new()),
            std_binding("path-normalize", Vec::new()),
        ],
        _ => Vec::new(),
    }
}

pub(crate) fn binding_decl(binding: &InExternBinding) -> Decl {
    // General extern with explicit params: use them directly
    if !binding.params.is_empty() {
        return Decl::Function {
            name: binding.name.clone(),
            params: binding.params.clone(),
            ret: binding.ret.clone().unwrap_or(Typ::Void),
            body: Vec::new(),
            type_params: vec![],
        };
    }
    // Standard library named bindings (legacy imports like "std.io", "std.fs")
    match binding.name.as_str() {
        "print" | "println" | "eprintln" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("text".into(), Typ::String)],
            ret: Typ::Void,
            body: Vec::new(),
            type_params: vec![],
        },
        "read-file" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("path".into(), Typ::String)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        "write-file" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("path".into(), Typ::String), ("text".into(), Typ::String)],
            ret: Typ::Bool,
            body: Vec::new(),
            type_params: vec![],
        },
        "fs-exists" | "create-dir" | "remove-file" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("path".into(), Typ::String)],
            ret: Typ::Bool,
            body: Vec::new(),
            type_params: vec![],
        },
        "http-get" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("url".into(), Typ::String)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        "json-parse" | "json-stringify" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("text".into(), Typ::String)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        "process-run" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("command".into(), Typ::String)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        "arg-count" => Decl::Function {
            name: binding.name.clone(),
            params: Vec::new(),
            ret: Typ::Int,
            body: Vec::new(),
            type_params: vec![],
        },
        "arg" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("index".into(), Typ::Int)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        "env-get" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("name".into(), Typ::String)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        "env-set" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("name".into(), Typ::String), ("value".into(), Typ::String)],
            ret: Typ::Void,
            body: Vec::new(),
            type_params: vec![],
        },
        "env-has" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("name".into(), Typ::String)],
            ret: Typ::Bool,
            body: Vec::new(),
            type_params: vec![],
        },
        "env-temp-dir" | "env-current-dir" => Decl::Function {
            name: binding.name.clone(),
            params: Vec::new(),
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        "path-join" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("base".into(), Typ::String), ("child".into(), Typ::String)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        "path-dirname" | "path-basename" | "path-extname" | "path-normalize" => Decl::Function {
            name: binding.name.clone(),
            params: vec![("path".into(), Typ::String)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        },
        _ => Decl::Function {
            name: binding.name.clone(),
            params: Vec::new(),
            ret: binding.ret.clone().unwrap_or(Typ::Void),
            body: Vec::new(),
            type_params: vec![],
        },
    }
}

pub(crate) fn parse_distributed_fn_name(line: &str) -> Result<String, String> {
    let rest = trim(line)
        .strip_prefix("distributed fn ")
        .ok_or_else(|| ".in: expected `distributed fn name(...)`".to_string())?;
    let (name, _, _) = parse_fn_header(rest)?;
    if name.is_empty() {
        return Err(".in: distributed function name missing".into());
    }
    Ok(name)
}

pub(crate) fn parse_annotation_name(line: &str) -> Result<String, String> {
    let name = trim(line)
        .strip_prefix('@')
        .ok_or_else(|| ".in: expected annotation".to_string())?
        .trim_end_matches(';')
        .trim();
    match name {
        "pure" | "gpu" | "parallel-safe" => Ok(name.to_string()),
        _ => Err(format!(".in: unsupported annotation `{name}`")),
    }
}

pub(crate) fn valid_package_or_module_name(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(is_ident_name)
}

pub(crate) fn parse_package_or_module_name(kind: &str, rest: &str) -> Result<String, String> {
    let name = trim(rest).trim_end_matches(';').trim();
    if name.is_empty() {
        return Err(format!(".in: {kind} name missing"));
    }
    if !valid_package_or_module_name(name) && !crate::package_ref::is_valid_semantic_import(name) {
        return Err(format!(".in: invalid {kind} name `{name}`"));
    }
    Ok(name.to_string())
}

pub(crate) fn parse_semantic_binding(rest: &str) -> Result<InSemanticBinding, String> {
    let t = trim(rest).trim_end_matches(';').trim();
    let Some((import, alias)) = t.split_once(" as ") else {
        return Err(".in: expected `bind <semantic.import> as <alias>`".into());
    };
    let import = parse_package_or_module_name("bind", import)?;
    let alias = trim(alias);
    if alias.is_empty() {
        return Err(".in: bind alias missing".into());
    }
    if !is_ident_name(alias) {
        return Err(format!(".in: invalid bind alias `{alias}`"));
    }
    Ok(InSemanticBinding {
        import,
        alias: alias.to_string(),
    })
}

pub(crate) fn next_function_name_after_annotation<'a, I>(lines: I) -> Option<String>
where
    I: Iterator<Item = &'a str>,
{
    for raw in lines {
        let line = trim(strip_line_comment_outside_strings(raw));
        if line.is_empty() || line.starts_with('@') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("fn ") {
            return parse_fn_header(rest).ok().map(|(n, _, _)| n);
        }
        if let Some(rest) = line.strip_prefix("distributed fn ") {
            return parse_fn_header(rest).ok().map(|(n, _, _)| n);
        }
        return None;
    }
    None
}

pub(crate) fn collect_parallel_tasks(
    lines: &[&str],
    start_idx: usize,
    region: usize,
) -> Vec<InParallelTaskFact> {
    let mut depth = 0i32;
    let mut started = false;
    let mut content = String::new();
    for raw in lines.iter().skip(start_idx) {
        let line = strip_line_comment_outside_strings(raw);
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    started = true;
                }
                '}' => {
                    depth -= 1;
                    if depth <= 0 {
                        return parallel_tasks_from_content(&content, region);
                    }
                }
                _ if started && depth > 0 => content.push(ch),
                _ => {}
            }
        }
        if started && depth > 0 {
            content.push('\n');
        }
    }
    parallel_tasks_from_content(&content, region)
}

pub(crate) fn parallel_tasks_from_content(content: &str, region: usize) -> Vec<InParallelTaskFact> {
    content
        .split([';', '\n'])
        .filter_map(|token| {
            let token = trim(token);
            let name = token.split_once('(')?.0.trim();
            if name.is_empty()
                || !name.split('.').all(|part| {
                    !part.is_empty()
                        && part.chars().enumerate().all(|(i, ch)| {
                            if i == 0 {
                                is_ident_start(ch)
                            } else {
                                is_ident_continue(ch)
                            }
                        })
                })
            {
                return None;
            }
            Some(InParallelTaskFact {
                region,
                name: name.to_string(),
            })
        })
        .collect()
}

pub fn parse_in_surface_info(source: &str) -> Result<InSurfaceInfo, String> {
    let mut info = InSurfaceInfo::default();
    let mut depth = 0i32;
    let lines: Vec<&str> = source.lines().collect();
    for (idx, raw_line) in lines.iter().enumerate() {
        let line = strip_line_comment_outside_strings(raw_line);
        let line = trim(line);
        if line.is_empty() || line.starts_with("//") {
            depth += brace_delta(raw_line);
            if depth < 0 {
                depth = 0;
            }
            continue;
        }
        if depth == 0 {
            if let Some(rest) = line.strip_prefix("package ") {
                let package = parse_package_or_module_name("package", rest)?;
                if info.package.replace(package).is_some() {
                    return Err(".in: duplicate package declaration".into());
                }
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("module ") {
                let module = parse_package_or_module_name("module", rest)?;
                if info.module.replace(module).is_some() {
                    return Err(".in: duplicate module declaration".into());
                }
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("import ") {
                let import = trim(rest).trim_end_matches(';').trim();
                if import.is_empty() {
                    return Err(".in: import path missing".into());
                }
                info.imports.push(import.to_string());
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("use ") {
                let import = parse_package_or_module_name("use", rest)?;
                info.semantic_imports.push(import);
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("bind ") {
                let binding = parse_semantic_binding(rest)?;
                info.semantic_bindings.push(binding);
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("capability ") {
                let capability = trim(rest).trim_end_matches(';').trim();
                if capability.is_empty() {
                    return Err(".in: capability name missing".into());
                }
                info.capabilities.push(capability.to_string());
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("enable ") {
                let extension = trim(rest).trim_end_matches(';').trim();
                if extension.is_empty() {
                    return Err(".in: enable extension missing".into());
                }
                info.orchestration
                    .enabled_extensions
                    .push(extension.to_string());
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if line.starts_with('@') {
                let name = parse_annotation_name(line)?;
                let target = next_function_name_after_annotation(lines[idx + 1..].iter().copied());
                info.orchestration
                    .annotations
                    .push(InAnnotationFact { name, target });
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if line.starts_with("distributed ") {
                let name = parse_distributed_fn_name(line)?;
                info.orchestration.distributed_functions.push(name);
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if line.starts_with("parallel") {
                if !(line == "parallel {" || line.starts_with("parallel {")) {
                    return Err(
                        ".in: `parallel` must be a top-level `parallel { ... }` region".into(),
                    );
                }
                info.orchestration.parallel_regions += 1;
                let region = info.orchestration.parallel_regions - 1;
                info.orchestration
                    .parallel_tasks
                    .extend(collect_parallel_tasks(&lines, idx, region));
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if line.starts_with("extern ") {
                info.externs.push(parse_extern_fn_block(line)?);
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if is_closed_world_topology_block(line) {
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            if line.starts_with("fn ")
                || line.starts_with("interrupt fn ")
                || line.starts_with("struct ")
                || line.starts_with("class ")
                || line.starts_with("interface ")
                || line.starts_with("component ")
                || line.starts_with("var ")
                || line.starts_with("const ")
            {
                depth += brace_delta(raw_line);
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }
            return Err(format!(".in: unknown top-level syntax `{line}`"));
        }
        depth += brace_delta(raw_line);
        if depth < 0 {
            depth = 0;
        }
    }
    Ok(info)
}

/// Closed-world topology blocks are product-owned (e.g. Subspace system graphs).
/// Inauguration skips them at the surface so `in graph` / `in compile` can still
/// inspect `.in` files that mix components with system/task/grant/port decls.
fn is_closed_world_topology_block(line: &str) -> bool {
    const KINDS: &[&str] = &[
        "system ",
        "domain ",
        "instance ",
        "task ",
        "grant ",
        "port ",
        "interrupt ",
        "startup ",
    ];
    KINDS.iter().any(|kind| line.starts_with(kind))
}

pub(crate) fn normalize_import_path(raw: &str) -> &str {
    raw.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches(';')
        .trim()
}
