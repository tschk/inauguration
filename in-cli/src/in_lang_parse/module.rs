use super::decl::{
    parse_class_block, parse_component_block, parse_extern_fn_block, parse_fn_block,
    parse_interface_block, parse_struct_block,
};
use super::expr::parse_expr;
use super::lexer::{normalize_human_in_source, split_top_level_decl_blocks};
use super::surface::{
    binding_decl, in_standard_import_bindings, normalize_import_path, parse_in_surface_info,
};
use super::types::{parse_fn_header, parse_in_type};
use super::util::*;
use super::validate::{
    collect_top_level_type_names, desugar_method_calls, duplicate_top_level_names,
    inline_const_values, type_known, validate_class_contracts, validate_stmt_types,
};
use crate::core_ir::{CoreModuleIdentity, Decl, Typ, UnifiedModule};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn parse_module_from_blocks(
    blocks: &[(usize, String)],
) -> Result<UnifiedModule, String> {
    let mut decls = Vec::new();
    for (start_line, block) in blocks {
        let line = trim(block);
        if line.is_empty() {
            continue;
        }
        if line.starts_with("interrupt fn ") {
            let blk = block[10..].to_string();
            let (name, params, ret, body) = parse_fn_block(&blk, *start_line as u32)?;
            crate::core_ir::register_interrupt_fn(&name);
            decls.push(Decl::Function {
                name: name.clone(),
                params,
                ret,
                body,
                type_params: vec![],
            });
        } else if line.starts_with("fn ") {
            let (name, params, ret, body) = parse_fn_block(block, *start_line as u32)?;
            decls.push(Decl::Function {
                name,
                params,
                ret,
                body,
                type_params: vec![],
            });
        } else if line.starts_with("extern ") {
            let binding = parse_extern_fn_block(block)
                .map_err(|e| format!(".in at line {start_line}: extern parse: {e}"))?;
            let rest = trim(block)
                .trim_end_matches(';')
                .trim()
                .strip_prefix("extern ")
                .ok_or_else(|| format!(".in at line {start_line}: expected `extern`"))?;
            let (_, header) = rest.split_once(" fn ").ok_or_else(|| {
                format!(".in at line {start_line}: expected `extern <language> fn name(...)`")
            })?;
            let header = header
                .split_once(" requires ")
                .map(|(left, _)| left)
                .unwrap_or(header);
            let (name, params, ret) = parse_fn_header(header)?;
            if name != binding.name {
                return Err(format!(
                    ".in at line {start_line}: extern binding name mismatch"
                ));
            }
            decls.push(Decl::Function {
                name,
                params,
                ret,
                body: Vec::new(),
                type_params: vec![],
            });
        } else if line.starts_with("struct ") {
            let (name, fields, methods) = parse_struct_block(block)
                .map_err(|e| format!(".in at line {start_line}: struct parse: {e}"))?;
            decls.push(Decl::Struct {
                name,
                fields,
                type_params: vec![],
            });
            decls.extend(methods);
        } else if line.starts_with("class ") {
            decls.push(
                parse_class_block(block).map_err(|e| format!(".in at line {start_line}: {e}"))?,
            );
        } else if line.starts_with("interface ") {
            decls.push(
                parse_interface_block(block)
                    .map_err(|e| format!(".in at line {start_line}: {e}"))?,
            );
        } else if line.starts_with("component ") {
            decls.push(
                parse_component_block(block)
                    .map_err(|e| format!(".in at line {start_line}: {e}"))?,
            );
        } else if is_skipped_topology_block(line) {
            continue;
        } else if line.starts_with("var ") {
            let rest = trim(&line[4..]);
            if let Some(eq) = rest.find('=') {
                let lhs = trim(&rest[..eq]);
                let rhs = trim(&rest[eq + 1..]);
                let (name, typ) = if let Some(colon) = lhs.rfind(':') {
                    (
                        trim(&lhs[..colon]).to_string(),
                        Some(parse_in_type(trim(&lhs[colon + 1..]))),
                    )
                } else {
                    (lhs.to_string(), None)
                };
                let init = parse_expr(rhs);
                decls.push(Decl::Global {
                    name,
                    typ: typ.unwrap_or(crate::core_ir::Typ::Int),
                    init: Some(Box::new(init)),
                    mutable: true,
                });
            } else {
                return Err(format!(
                    ".in at line {start_line}: `var` needs `=` initializer"
                ));
            }
        } else if line.starts_with("const ") {
            let rest = trim(&line[6..]);
            if let Some(eq) = rest.find('=') {
                let lhs = trim(&rest[..eq]);
                let rhs = trim(&rest[eq + 1..]);
                let (name, typ) = if let Some(colon) = lhs.rfind(':') {
                    (
                        trim(&lhs[..colon]).to_string(),
                        Some(parse_in_type(trim(&lhs[colon + 1..]))),
                    )
                } else {
                    (lhs.to_string(), None)
                };
                let init = parse_expr(rhs);
                decls.push(Decl::Global {
                    name,
                    typ: typ.unwrap_or(crate::core_ir::Typ::Int),
                    init: Some(Box::new(init)),
                    mutable: false,
                });
            } else {
                return Err(".in: `const` needs `=` initializer".into());
            }
        } else {
            return Err(format!(
                ".in at line {start_line}: expected top-level `fn`, `interrupt fn`, `struct`, `class`, `interface`, `component`, `var`, or `const`"
            ));
        }
    }
    Ok(UnifiedModule::new(decls))
}

fn is_skipped_topology_block(line: &str) -> bool {
    line.starts_with("system ")
        || line.starts_with("domain ")
        || line.starts_with("instance ")
        || line.starts_with("task ")
        || line.starts_with("grant ")
        || line.starts_with("port ")
        || (line.starts_with("interrupt ") && !line.starts_with("interrupt fn "))
        || line.starts_with("startup ")
}

pub(crate) fn parse_in_module_without_validation(
    source: &str,
    source_path: Option<&std::path::Path>,
) -> Result<UnifiedModule, String> {
    let surface = parse_in_surface_info(source)?;
    let identity = CoreModuleIdentity {
        package: surface.package.clone(),
        module: surface.module.clone(),
    };
    let blocks = split_top_level_decl_blocks(source);
    let mut module = parse_module_from_blocks(&blocks)?;
    module.identity = identity;
    desugar_method_calls(&mut module);
    // Inline const values: replace all Expr::Ident references to consts with their init expressions.
    // This avoids type-checking and lowering complications for compile-time constants.
    inline_const_values(&mut module);
    let mut std_decls = Vec::new();
    for import in surface.imports {
        std_decls.extend(
            in_standard_import_bindings(&import)
                .into_iter()
                .map(|binding| binding_decl(&binding)),
        );
    }
    if let Some(path) = source_path
        && let Ok((root, manifest)) =
            crate::package_manifest::load_package_manifest_from_source(path)
    {
        let lock = crate::package_lock::discover_package_lock(&root.root).and_then(|lock_root| {
            crate::package_lock::load_package_lock(&lock_root.lock_path).ok()
        });
        for import in &surface.semantic_imports {
            std_decls.extend(
                crate::package_extern::package_import_bindings_for_semantic_import(
                    import,
                    &root.root,
                    &manifest,
                    lock.as_ref(),
                )
                .into_iter()
                .map(|binding| binding_decl(&binding)),
            );
        }
    }
    for binding in &surface.semantic_bindings {
        std_decls.push(Decl::Function {
            name: binding.alias.clone(),
            params: vec![("value".into(), Typ::String)],
            ret: Typ::String,
            body: Vec::new(),
            type_params: vec![],
        });
    }
    std_decls.extend(module.decls);
    module.decls = std_decls;
    Ok(module)
}

pub(crate) fn validate_module(module: &UnifiedModule, require_main: bool) -> Result<(), String> {
    if module.decls.is_empty() {
        return Err(
            ".in: no top-level struct, class, interface, component, or fn after filtering".into(),
        );
    }

    if let Some(dup) = duplicate_top_level_names(module).first() {
        return Err(format!(".in: duplicate top-level name: {dup}"));
    }

    let has_main = module
        .decls
        .iter()
        .any(|d| matches!(d, Decl::Function { name, .. } if name == "main"));
    if require_main && !has_main {
        return Err(".in: missing required `fn main`".into());
    }

    let struct_names = collect_top_level_type_names(module);
    let struct_set: HashSet<&str> = struct_names.iter().map(String::as_str).collect();
    let struct_fields: HashMap<String, Vec<String>> = module
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Struct { name, fields, .. } | Decl::Class { name, fields, .. } => Some((
                name.clone(),
                fields.iter().map(|(field, _)| field.clone()).collect(),
            )),
            _ => None,
        })
        .collect();

    for d in &module.decls {
        match d {
            Decl::Struct { name, fields, .. } => {
                for (field, ty) in fields {
                    if !type_known(&struct_set, ty) {
                        return Err(format!(".in: unknown type in struct {name} field {field}",));
                    }
                }
            }
            Decl::Function {
                name,
                params,
                ret,
                body,
                ..
            } => {
                for (param, ty) in params {
                    if !type_known(&struct_set, ty) {
                        return Err(format!(".in: unknown type in fn {name} parameter {param}",));
                    }
                }
                if !type_known(&struct_set, ret) {
                    return Err(format!(".in: unknown return type in fn {name}",));
                }
                for st in body {
                    validate_stmt_types(name, &struct_set, &struct_fields, st)?;
                }
            }
            Decl::Class { name, fields, .. } => {
                for (field, ty) in fields {
                    if !type_known(&struct_set, ty) {
                        return Err(format!(".in: unknown type in class {name} field {field}"));
                    }
                }
            }
            Decl::Interface { .. } => {}
            Decl::Component { .. } => {}
            Decl::Global { .. } => {}
        }
    }

    validate_class_contracts(module)?;

    Ok(())
}

/// Parse and validate `.in` v0.2 source; returns human-readable errors as strings.
pub fn parse_in_source(source: &str) -> Result<UnifiedModule, String> {
    let module = parse_in_module_without_validation(&normalize_human_in_source(source), None)?;
    validate_module(&module, true)?;
    Ok(module)
}

pub(crate) fn local_in_import_path(base: &Path, raw: &str) -> Option<PathBuf> {
    let import = normalize_import_path(raw);
    if !(import.ends_with(".in") || import.starts_with("./") || import.starts_with("../")) {
        return None;
    }
    let path = Path::new(import);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.parent().unwrap_or_else(|| Path::new(".")).join(path)
    };
    Some(resolved)
}

pub(crate) fn parse_in_file_inner(
    path: &Path,
    seen: &mut HashSet<PathBuf>,
) -> Result<UnifiedModule, String> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(key) {
        return Ok(UnifiedModule::new(Vec::new()));
    }
    let source = normalize_human_in_source(
        &fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
    );
    let surface = parse_in_surface_info(&source)?;
    let mut decls = Vec::new();
    let imports: Vec<_> = surface
        .imports
        .into_iter()
        .filter_map(|import| local_in_import_path(path, &import))
        .collect();
    if imports.len() > 1 {
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(imports.len());
            for import_path in imports {
                let key = import_path
                    .canonicalize()
                    .unwrap_or_else(|_| import_path.clone());
                if !seen.insert(key) {
                    continue;
                }
                handles.push(s.spawn(move || {
                    let mut local_seen = std::collections::HashSet::new();
                    parse_in_file_inner(&import_path, &mut local_seen)
                }));
            }
            let mut results = Vec::with_capacity(handles.len());
            for handle in handles {
                let imported = handle
                    .join()
                    .map_err(|e| format!("import parse thread panicked: {e:?}"))?;
                results.push(imported?);
            }
            for imported in results {
                decls.extend(imported.decls);
            }
            Ok::<(), String>(())
        })?;
    } else {
        for import_path in imports {
            let imported = parse_in_file_inner(&import_path, seen)?;
            decls.extend(imported.decls);
        }
    }
    let module = parse_in_module_without_validation(&source, Some(path))?;
    let identity = module.identity.clone();
    decls.extend(module.decls);
    let mut merged = UnifiedModule::with_identity(decls, identity);
    // Re-inline constants after merging imported modules so that constants
    // from imported files are available to functions in the current file.
    inline_const_values(&mut merged);
    Ok(merged)
}

/// Read a `.in` file and parse to core IR.
pub fn parse_in_file(path: &Path) -> Result<UnifiedModule, String> {
    let mut seen = HashSet::new();
    let module = parse_in_file_inner(path, &mut seen)?;
    validate_module(&module, true)?;
    Ok(module)
}

pub fn parse_in_library_file(path: &Path) -> Result<UnifiedModule, String> {
    let mut seen = HashSet::new();
    let module = parse_in_file_inner(path, &mut seen)?;
    validate_module(&module, false)?;
    Ok(module)
}
