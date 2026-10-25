use super::decl::{
    has_empty_braces, parse_class_block, parse_component_block, parse_extern_fn_block,
    parse_fn_block, parse_interface_block, parse_struct_block,
};
use super::expr::parse_expr;
use super::lexer::{is_core_decl_item, normalize_human_in_source, split_top_level_items};
use super::surface::{
    binding_decl, in_standard_import_bindings, normalize_import_path, parse_in_surface_info,
    parse_in_surface_info_with_items,
};
use super::types::{parse_fn_header, parse_in_type};
use super::util::*;
use super::validate::{
    collect_top_level_type_names, desugar_method_calls, duplicate_top_level_names,
    infer_in_expr_type, inline_const_values, type_known, validate_class_contracts,
    validate_stmt_types,
};
use crate::core_ir::{CoreModuleIdentity, Decl, Typ, UnifiedModule};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

struct ParsedInModule {
    module: UnifiedModule,
    pending_globals: HashSet<String>,
    empty_brace_fns: HashSet<String>,
}

fn parse_module_from_blocks<'a>(
    blocks: impl IntoIterator<Item = &'a (usize, String)>,
    mut counts: Option<&mut Vec<usize>>,
) -> Result<ParsedInModule, String> {
    let mut decls = Vec::new();
    let mut pending_globals = HashSet::new();
    let mut empty_brace_fns = HashSet::new();
    for (start_line, block) in blocks {
        let before = decls.len();
        let line = trim(block);
        if line.is_empty() {
            continue;
        }
        if line.starts_with("interrupt fn ") {
            let blk = block[10..].to_string();
            let (name, params, ret, mut body) = parse_fn_block(&blk, *start_line as u32)?;
            if has_empty_braces(block) {
                body.clear();
            }
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
            if has_empty_braces(block) {
                empty_brace_fns.insert(name.clone());
            }
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
                let init = parse_expr(rhs.trim_end_matches(';').trim());
                if typ.is_none() {
                    pending_globals.insert(name.clone());
                }
                decls.push(Decl::Global {
                    name,
                    typ: typ.unwrap_or(crate::core_ir::Typ::Int),
                    init: Some(Box::new(init)),
                    mutable: true,
                    constant_value: false,
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
                let init = parse_expr(rhs.trim_end_matches(';').trim());
                if typ.is_none() {
                    pending_globals.insert(name.clone());
                }
                decls.push(Decl::Global {
                    name,
                    typ: typ.unwrap_or(crate::core_ir::Typ::Int),
                    init: Some(Box::new(init)),
                    mutable: false,
                    constant_value: false,
                });
            } else {
                return Err(".in: `const` needs `=` initializer".into());
            }
        } else {
            return Err(format!(
                ".in at line {start_line}: expected top-level `fn`, `interrupt fn`, `struct`, `class`, `interface`, `component`, `var`, or `const`"
            ));
        }
        if let Some(counts) = counts.as_deref_mut() {
            counts.push(decls.len() - before);
        }
    }
    Ok(ParsedInModule {
        module: UnifiedModule::new(decls),
        pending_globals,
        empty_brace_fns,
    })
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

fn parse_in_module_without_validation(
    source: &str,
    source_path: Option<&std::path::Path>,
) -> Result<ParsedInModule, String> {
    let items = split_top_level_items(source)?;
    parse_in_module_from_items(&items, source_path, None)
}

fn parse_in_module_from_items(
    items: &[(usize, String)],
    source_path: Option<&std::path::Path>,
    counts: Option<&mut Vec<usize>>,
) -> Result<ParsedInModule, String> {
    let surface = parse_in_surface_info_with_items(items)?;
    let identity = CoreModuleIdentity {
        package: surface.package.clone(),
        module: surface.module.clone(),
    };
    let ParsedInModule {
        mut module,
        pending_globals,
        empty_brace_fns,
    } = parse_module_from_blocks(
        items.iter().filter(|(_, text)| is_core_decl_item(text)),
        counts,
    )?;
    module.identity = identity;
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
    Ok(ParsedInModule {
        module,
        pending_globals,
        empty_brace_fns,
    })
}

/// Resolve private omitted-type metadata only after the complete import graph is available.
fn finalize_in_module(parsed: ParsedInModule, require_main: bool) -> Result<UnifiedModule, String> {
    let ParsedInModule {
        mut module,
        mut pending_globals,
        empty_brace_fns,
    } = parsed;
    if module
        .decls
        .iter()
        .any(|decl| matches!(decl, Decl::Component { .. }))
    {
        for decl in &mut module.decls {
            if let Decl::Function { name, body, .. } = decl
                && empty_brace_fns.contains(name)
                && name != "main"
            {
                body.clear();
            }
        }
    }
    if let Some(duplicate) = duplicate_top_level_names(&module).first() {
        return Err(format!(".in: duplicate top-level name: {duplicate}"));
    }
    let structs: HashMap<String, HashMap<String, Typ>> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Struct { name, fields, .. } | Decl::Class { name, fields, .. } => {
                Some((name.clone(), fields.iter().cloned().collect()))
            }
            _ => None,
        })
        .collect();
    let fn_rets: HashMap<String, Typ> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, ret, .. } => Some((name.clone(), ret.clone())),
            _ => None,
        })
        .collect();
    let mut env: HashMap<String, Typ> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } if !pending_globals.contains(name) => {
                Some((name.clone(), typ.clone()))
            }
            _ => None,
        })
        .collect();
    while !pending_globals.is_empty() {
        let previous = pending_globals.len();
        for decl in &mut module.decls {
            if let Decl::Global {
                name,
                typ,
                init: Some(init),
                ..
            } = decl
                && pending_globals.contains(name)
                && let Some(inferred) = infer_in_expr_type(init, &env, &structs, &fn_rets)
            {
                *typ = inferred.clone();
                env.insert(name.clone(), inferred);
                pending_globals.remove(name);
            }
        }
        if pending_globals.len() == previous {
            let name = module
                .decls
                .iter()
                .find_map(|decl| match decl {
                    Decl::Global { name, .. } if pending_globals.contains(name) => {
                        Some(name.as_str())
                    }
                    _ => None,
                })
                .unwrap_or("<unknown>");
            return Err(format!(
                ".in: cannot infer global `{name}`; add an explicit type annotation"
            ));
        }
    }
    desugar_method_calls(&mut module);
    inline_const_values(&mut module);
    validate_module(&module, require_main)?;
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
    let parsed = parse_in_module_without_validation(&normalize_human_in_source(source), None)?;
    finalize_in_module(parsed, true)
}

/// Preserve source-item order and the generated-binding prefix for canonical output.
/// Finalization changes expressions/types, but preserves user declaration order/count.
pub(crate) struct CanonicalSource {
    pub module: UnifiedModule,
    pub items: Vec<(usize, String)>,
    pub generated_count: usize,
    pub counts: Vec<usize>,
}

pub(crate) fn parse_in_source_for_canonical(source: &str) -> Result<CanonicalSource, String> {
    let normalized = normalize_human_in_source(source);
    let items = split_top_level_items(&normalized)?;
    let mut counts = Vec::new();
    let parsed = parse_in_module_from_items(&items, None, Some(&mut counts))?;
    let user_count: usize = counts.iter().sum();
    let generated_count = parsed
        .module
        .decls
        .len()
        .checked_sub(user_count)
        .ok_or(".in: inconsistent source declaration provenance")?;
    let module = finalize_in_module(parsed, true)?;
    Ok(CanonicalSource {
        module,
        items,
        generated_count,
        counts,
    })
}

/// Parse library source without requiring a `main` entry point.
pub fn parse_in_library_source(source: &str) -> Result<UnifiedModule, String> {
    let parsed = parse_in_module_without_validation(&normalize_human_in_source(source), None)?;
    finalize_in_module(parsed, false)
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

// Keep source-file provenance until the complete graph is merged. A declaration
// name is not an import identity: distinct files may declare the same name, and
// validation must still report that collision.
fn parse_in_file_inner(
    path: &Path,
    seen: &mut HashSet<PathBuf>,
) -> Result<Vec<(PathBuf, ParsedInModule)>, String> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !seen.insert(key.clone()) {
        return Ok(Vec::new());
    }
    let source = normalize_human_in_source(
        &fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?,
    );
    let surface = parse_in_surface_info(&source)?;
    let mut files = Vec::new();
    let mut emitted = HashSet::new();
    let mut scheduled = HashSet::new();
    let imports: Vec<_> = surface
        .imports
        .into_iter()
        .filter_map(|import| local_in_import_path(path, &import))
        .filter(|import| {
            let key = import.canonicalize().unwrap_or_else(|_| import.clone());
            !seen.contains(&key) && scheduled.insert(key)
        })
        .collect();
    if imports.len() > 1 {
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(imports.len());
            for import_path in imports {
                // Include ancestors so a cycle crossing a worker boundary stops.
                // Each worker keeps independent visitation; join order decides
                // which occurrence of a shared dependency is retained.
                let mut local_seen = seen.clone();
                handles.push(s.spawn(move || parse_in_file_inner(&import_path, &mut local_seen)));
            }
            for handle in handles {
                let imported = handle
                    .join()
                    .map_err(|e| format!("import parse thread panicked: {e:?}"))??;
                for (key, parsed) in imported {
                    seen.insert(key.clone());
                    if emitted.insert(key.clone()) {
                        files.push((key, parsed));
                    }
                }
            }
            Ok::<(), String>(())
        })?;
    } else {
        for import_path in imports {
            for (key, parsed) in parse_in_file_inner(&import_path, seen)? {
                if emitted.insert(key.clone()) {
                    files.push((key, parsed));
                }
            }
        }
    }
    files.push((
        key,
        parse_in_module_without_validation(&source, Some(path))?,
    ));
    Ok(files)
}

fn parse_in_file_with_imports(path: &Path) -> Result<ParsedInModule, String> {
    let mut files = parse_in_file_inner(path, &mut HashSet::new())?;
    // An initially empty visited set always includes the root file last.
    let (_, root) = files
        .pop()
        .ok_or_else(|| format!("{}: missing root module", path.display()))?;
    let identity = root.module.identity.clone();
    let mut decls = Vec::new();
    let mut pending_globals = HashSet::new();
    let mut empty_brace_fns = HashSet::new();
    for parsed in files
        .into_iter()
        .map(|(_, parsed)| parsed)
        .chain(std::iter::once(root))
    {
        decls.extend(parsed.module.decls);
        pending_globals.extend(parsed.pending_globals);
        empty_brace_fns.extend(parsed.empty_brace_fns);
    }
    Ok(ParsedInModule {
        module: UnifiedModule::with_identity(decls, identity),
        pending_globals,
        empty_brace_fns,
    })
}

/// Read a `.in` file and parse to core IR.
pub fn parse_in_file(path: &Path) -> Result<UnifiedModule, String> {
    finalize_in_module(parse_in_file_with_imports(path)?, true)
}

pub fn parse_in_library_file(path: &Path) -> Result<UnifiedModule, String> {
    finalize_in_module(parse_in_file_with_imports(path)?, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_in_library_source() {
        let valid_source = "fn hello() {}";
        let parsed = parse_in_library_source(valid_source);
        if let Err(e) = &parsed {
            println!("Valid source parsing error: {}", e);
        }
        assert!(parsed.is_ok());

        let invalid_source = "!!! INVALID SYNTAX !!!";
        let parsed_invalid = parse_in_library_source(invalid_source);
        assert!(parsed_invalid.is_err());
    }
}
