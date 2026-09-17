//! Rust front powered by `syn` AST parsing.
//!
//! This is the first non-`.in` front that lowers real statement bodies (subset) into Core IR.

use crate::boundary_ir::{
    BoundaryField, BoundaryLayout, BoundaryModule, BoundaryOwnership, BoundaryRepr, BoundarySymbol,
    BoundaryTransfer, CompileArtifact, IN_ABI_VERSION,
};
use crate::boundary_verify::boundary_ir_verify;
use crate::core_ir::{CatchArm, Expr, LoopKind, MatchArm, Stmt, Typ};
use crate::core_ir::{Decl, UnifiedModule};
use quote::ToTokens;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use syn::parse::Parser;

type RustLayoutSpecs = HashMap<String, (BoundaryRepr, Vec<(String, syn::Type)>)>;

pub fn parse_rust_file(path: &Path) -> Result<UnifiedModule, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let module_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rust")
        .to_string();
    parse_rust_artifact_source_with_dir(&src, &module_id, path.parent().unwrap_or(Path::new(".")))
        .map(|artifact| artifact.semantic)
}

pub fn parse_rust_artifact(path: &Path) -> Result<CompileArtifact, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let module_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rust")
        .to_string();
    parse_rust_artifact_source(&src, &module_id)
}

pub fn parse_rust_source(src: &str) -> Result<UnifiedModule, String> {
    parse_rust_artifact_source(src, "rust").map(|artifact| artifact.semantic)
}

pub fn parse_rust_artifact_source(src: &str, module_id: &str) -> Result<CompileArtifact, String> {
    parse_rust_artifact_source_with_dir(src, module_id, Path::new("."))
}

pub fn parse_rust_artifact_source_with_dir(
    src: &str,
    module_id: &str,
    base_dir: &Path,
) -> Result<CompileArtifact, String> {
    let file = syn::parse_file(src).map_err(|e| format!("rust parse failed: {e}"))?;
    let semantic = lower_file_items_at(&file, base_dir)?;
    let boundary = extract_boundary_module(&file, module_id);
    Ok(match boundary {
        Some(boundary) => CompileArtifact::with_boundary(semantic, boundary),
        None => CompileArtifact::from_semantic(semantic),
    })
}

fn lower_file_items_at(file: &syn::File, base_dir: &Path) -> Result<UnifiedModule, String> {
    lower_file_items_at_visited(file, base_dir, &mut HashSet::new(), 0)
}

fn lower_file_items_at_visited(
    file: &syn::File,
    base_dir: &Path,
    visited: &mut HashSet<PathBuf>,
    depth: u32,
) -> Result<UnifiedModule, String> {
    if depth > 8 {
        return Err("rust front: module nest limit".into());
    }
    let mut decls = Vec::new();
    let mut external_modules: Vec<String> = Vec::new();
    let mut uses: Vec<syn::ItemUse> = Vec::new();

    for item in &file.items {
        match item {
            syn::Item::Struct(s) => {
                decls.push(Decl::Struct {
                    name: s.ident.to_string(),
                    fields: rust_struct_fields(&s.fields),
                    type_params: vec![],
                });
            }
            syn::Item::Fn(f) => decls.push(lower_fn(f.clone())),
            syn::Item::Impl(i) => {
                for method in lower_impl(i) {
                    decls.push(method);
                }
            }
            syn::Item::Enum(e) => {
                if let Some(decl) = lower_enum(e) {
                    decls.push(decl);
                }
            }
            syn::Item::Use(u) => uses.push(u.clone()),
            syn::Item::Const(c) => {
                if let Some(d) = lower_const_item(c) {
                    decls.push(d);
                }
            }
            syn::Item::Static(s) => {
                if let Some(d) = lower_static_item(s) {
                    decls.push(d);
                }
            }
            syn::Item::Type(t) => {
                decls.push(Decl::Struct {
                    name: t.ident.to_string(),
                    fields: vec![],
                    type_params: vec![],
                });
            }
            syn::Item::ForeignMod(fm) => {
                for item in &fm.items {
                    if let syn::ForeignItem::Fn(f) = item {
                        decls.push(Decl::Function {
                            name: f.sig.ident.to_string(),
                            params: f
                                .sig
                                .inputs
                                .iter()
                                .map(|arg| match arg {
                                    syn::FnArg::Typed(pat_ty) => (
                                        pattern_name(&pat_ty.pat).unwrap_or_else(|| "arg".into()),
                                        map_type(&pat_ty.ty),
                                    ),
                                    syn::FnArg::Receiver(_) => {
                                        ("self".into(), Typ::Named("Self".into()))
                                    }
                                })
                                .collect(),
                            ret: match &f.sig.output {
                                syn::ReturnType::Default => Typ::Void,
                                syn::ReturnType::Type(_, ty) => map_type(ty),
                            },
                            body: vec![],
                            type_params: vec![],
                        });
                    }
                }
            }
            syn::Item::Trait(tr) => {
                for item in &tr.items {
                    if let syn::TraitItem::Fn(method) = item {
                        if let Some(block) = &method.default {
                            let f = syn::ItemFn {
                                attrs: method.attrs.clone(),
                                vis: syn::Visibility::Inherited,
                                sig: method.sig.clone(),
                                block: Box::new(block.clone()),
                            };
                            decls.push(lower_fn(f));
                        }
                    }
                }
            }
            syn::Item::Union(u) => {
                decls.push(Decl::Struct {
                    name: u.ident.to_string(),
                    fields: rust_struct_fields(&syn::Fields::Named(u.fields.clone())),
                    type_params: vec![],
                });
            }
            syn::Item::TraitAlias(ta) => {
                decls.push(Decl::Struct {
                    name: ta.ident.to_string(),
                    fields: vec![],
                    type_params: vec![],
                });
            }
            syn::Item::Macro(m) => {
                if m.mac.path.is_ident("include") || m.mac.path.is_ident("include_str") {
                    // Keep a named hook so later DCE can see the include site.
                    decls.push(Decl::Function {
                        name: format!("__macro_{}", m.mac.path.to_token_stream()),
                        params: vec![],
                        ret: Typ::Void,
                        body: vec![Stmt::Expr(Expr::StringLit(m.mac.tokens.to_string()))],
                        type_params: vec![],
                    });
                }
            }
            syn::Item::ExternCrate(_) | syn::Item::Verbatim(_) => {}
            syn::Item::Mod(m) => {
                // Handle `mod foo;` (external file) and `mod foo { ... }` (inline)
                if let Some((_brace, items)) = &m.content {
                    if depth >= 6 {
                        // Bound inline-mod explosion (generated / re-export trees).
                    } else {
                        let inner_file = syn::File {
                            shebang: None,
                            attrs: vec![],
                            items: items.clone(),
                        };
                        if let Ok(inner) =
                            lower_file_items_at_visited(&inner_file, base_dir, visited, depth + 1)
                        {
                            decls.extend(prefix_module_decls(inner.decls, &m.ident.to_string()));
                        }
                    }
                } else {
                    // External module: record candidate path for parallel parsing
                    external_modules.push(m.ident.to_string());
                }
            }
            _ => {}
        }
    }

    // Parse external modules in parallel; they are independent subtrees.
    if external_modules.len() > 1 {
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(external_modules.len());
            for mod_name in external_modules {
                let base = base_dir.to_path_buf();
                handles.push(s.spawn(move || {
                    let candidate_rs = base.join(format!("{mod_name}.rs"));
                    let candidate_mod = base.join(format!("{mod_name}/mod.rs"));
                    let candidate_nested = base.join(&mod_name).join("mod.rs");

                    let candidate = if candidate_rs.exists() {
                        candidate_rs
                    } else if candidate_mod.exists() {
                        candidate_mod
                    } else if candidate_nested.exists() {
                        candidate_nested
                    } else {
                        return None;
                    };

                    std::fs::read_to_string(&candidate)
                        .ok()
                        .and_then(|src| syn::parse_file(&src).ok())
                        .and_then(|sub_file| {
                            let sub_dir = candidate.parent().unwrap_or(&base);
                            let mut v = HashSet::new();
                            lower_file_items_at_visited(&sub_file, sub_dir, &mut v, 1)
                                .ok()
                                .map(|m| {
                                    UnifiedModule::new(prefix_module_decls(m.decls, &mod_name))
                                })
                        })
                }));
            }
            for handle in handles {
                if let Ok(Some(inner)) = handle.join() {
                    decls.extend(inner.decls);
                }
            }
        });
    } else if let Some(mod_name) = external_modules.first() {
        let candidate_rs = base_dir.join(format!("{mod_name}.rs"));
        let candidate_mod = base_dir.join(format!("{mod_name}/mod.rs"));
        let candidate_nested = base_dir.join(mod_name).join("mod.rs");

        let candidate = if candidate_rs.exists() {
            Some(candidate_rs)
        } else if candidate_mod.exists() {
            Some(candidate_mod)
        } else if candidate_nested.exists() {
            Some(candidate_nested)
        } else {
            None
        };

        if let Some(candidate) = candidate {
            if let Ok(src) = std::fs::read_to_string(&candidate) {
                if let Ok(sub_file) = syn::parse_file(&src) {
                    let sub_dir = candidate.parent().unwrap_or(base_dir);
                    let canon = candidate.canonicalize().unwrap_or(candidate.clone());
                    if visited.insert(canon)
                        && let Ok(inner) =
                            lower_file_items_at_visited(&sub_file, sub_dir, visited, depth + 1)
                    {
                        decls.extend(prefix_module_decls(inner.decls, mod_name));
                    }
                }
            }
        }
    }

    for u in &uses {
        apply_use_tree(&mut decls, &u.tree, "");
    }
    if decls.is_empty() {
        return Err("rust front parsed file but found no top-level structs/functions".to_string());
    }
    Ok(UnifiedModule::new(decls))
}

fn prefix_module_decls(mut decls: Vec<Decl>, module: &str) -> Vec<Decl> {
    if module.is_empty() {
        return decls;
    }
    for d in &mut decls {
        match d {
            Decl::Function { name, .. } | Decl::Struct { name, .. } => {
                if !name.contains("::") || !name.starts_with(module) {
                    *name = format!("{module}::{name}");
                }
            }
            _ => {}
        }
    }
    decls
}

fn lower_const_item(c: &syn::ItemConst) -> Option<Decl> {
    let mut locals = HashMap::new();
    Some(Decl::Function {
        name: c.ident.to_string(),
        params: vec![],
        ret: map_type(&c.ty),
        body: vec![Stmt::Return(Some(lower_expr_with_types(
            &c.expr,
            &mut locals,
        )))],
        type_params: vec![],
    })
}

fn lower_static_item(s: &syn::ItemStatic) -> Option<Decl> {
    let mut locals = HashMap::new();
    Some(Decl::Function {
        name: s.ident.to_string(),
        params: vec![],
        ret: map_type(&s.ty),
        body: vec![Stmt::Return(Some(lower_expr_with_types(
            &s.expr,
            &mut locals,
        )))],
        type_params: vec![],
    })
}

fn apply_use_tree(decls: &mut Vec<Decl>, tree: &syn::UseTree, prefix: &str) {
    match tree {
        syn::UseTree::Path(p) => {
            let next = if prefix.is_empty() {
                p.ident.to_string()
            } else {
                format!("{prefix}::{}", p.ident)
            };
            apply_use_tree(decls, &p.tree, &next);
        }
        syn::UseTree::Name(n) => {
            let full = if prefix.is_empty() {
                n.ident.to_string()
            } else {
                format!("{prefix}::{}", n.ident)
            };
            alias_fn(decls, &full, &n.ident.to_string());
        }
        syn::UseTree::Rename(r) => {
            let full = if prefix.is_empty() {
                r.ident.to_string()
            } else {
                format!("{prefix}::{}", r.ident)
            };
            alias_fn(decls, &full, &r.rename.to_string());
        }
        syn::UseTree::Glob(_) => {}
        syn::UseTree::Group(g) => {
            for t in &g.items {
                apply_use_tree(decls, t, prefix);
            }
        }
    }
}

fn alias_fn(decls: &mut Vec<Decl>, full: &str, short: &str) {
    if full == short {
        return;
    }
    if decls
        .iter()
        .any(|d| matches!(d, Decl::Function { name, .. } if name == short))
    {
        return;
    }
    let Some((target, params, ret)) = decls.iter().find_map(|d| match d {
        Decl::Function {
            name, params, ret, ..
        } if name == full || name.ends_with(&format!("::{short}")) => {
            Some((name.clone(), params.clone(), ret.clone()))
        }
        _ => None,
    }) else {
        return;
    };
    let args: Vec<Expr> = params.iter().map(|(n, _)| Expr::Ident(n.clone())).collect();
    decls.push(Decl::Function {
        name: short.to_string(),
        params,
        ret,
        body: vec![Stmt::Return(Some(Expr::Call {
            callee: Box::new(Expr::Ident(target)),
            args,
        }))],
        type_params: vec![],
    });
}

fn extract_boundary_module(file: &syn::File, module_id: &str) -> Option<BoundaryModule> {
    let mut layouts = Vec::new();
    let mut symbols = Vec::new();
    let mut layout_specs: RustLayoutSpecs = HashMap::new();

    for item in &file.items {
        if let syn::Item::Struct(s) = item
            && let Some(repr) = repr_from_attrs(&s.attrs)
        {
            let fields = boundary_struct_fields(&s.fields);
            if !fields.is_empty() {
                layout_specs.insert(s.ident.to_string(), (repr, fields));
            }
        }
    }

    for (name, (repr, fields)) in &layout_specs {
        if let Some(layout) = compute_struct_layout(name, repr.clone(), fields, &layout_specs) {
            layouts.push(layout);
        }
    }

    for item in &file.items {
        if let syn::Item::Fn(f) = item
            && has_no_mangle(&f.attrs)
            && is_extern_c(&f.sig)
        {
            symbols.push(boundary_symbol_from_fn(f, &layout_specs));
        }
    }

    if layouts.is_empty() && symbols.is_empty() {
        return None;
    }

    let boundary = BoundaryModule {
        abi_version: IN_ABI_VERSION,
        module: format!("rust.{module_id}"),
        layouts,
        symbols,
        allocators: vec![],
        layout_hash: String::new(),
    }
    .with_layout_hash();
    let report = boundary_ir_verify(&boundary);
    if !report.ok {
        return None;
    }
    Some(boundary)
}

fn repr_from_attrs(attrs: &[syn::Attribute]) -> Option<BoundaryRepr> {
    for attr in attrs {
        if !attr.path().is_ident("repr") {
            continue;
        }
        let mut repr = None;
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("C") {
                repr = Some(BoundaryRepr::C);
            } else if meta.path.is_ident("transparent") {
                repr = Some(BoundaryRepr::Transparent);
            } else if meta.path.is_ident("packed") {
                repr = Some(BoundaryRepr::Packed);
            }
            Ok(())
        });
        if repr.is_some() {
            return repr;
        }
    }
    None
}

fn has_no_mangle(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| attr.path().is_ident("no_mangle"))
}

fn is_extern_c(sig: &syn::Signature) -> bool {
    sig.abi
        .as_ref()
        .and_then(|abi| abi.name.as_ref())
        .is_some_and(|name| name.value() == "C")
}

fn boundary_struct_fields(fields: &syn::Fields) -> Vec<(String, syn::Type)> {
    match fields {
        syn::Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                (
                    f.ident
                        .as_ref()
                        .map(std::string::ToString::to_string)
                        .unwrap_or_else(|| "field".to_string()),
                    f.ty.clone(),
                )
            })
            .collect(),
        syn::Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, f)| (format!("_{i}"), f.ty.clone()))
            .collect(),
        syn::Fields::Unit => vec![],
    }
}

#[derive(Clone)]
struct AbiType {
    boundary_type: String,
    size: u64,
    align: u64,
    transfer: Option<BoundaryTransfer>,
}

fn abi_type_for(ty: &syn::Type, layout_specs: &RustLayoutSpecs, packed: bool) -> Option<AbiType> {
    match ty {
        syn::Type::Path(tp) => {
            if let Some(seg) = tp.path.segments.last() {
                let ident = seg.ident.to_string();
                match ident.as_str() {
                    "i8" => return Some(scalar_abi("i8", 1, 1)),
                    "u8" => return Some(scalar_abi("u8", 1, 1)),
                    "i16" => return Some(scalar_abi("i16", 2, 2)),
                    "u16" => return Some(scalar_abi("u16", 2, 2)),
                    "i32" => return Some(scalar_abi("i32", 4, 4)),
                    "u32" => return Some(scalar_abi("u32", 4, 4)),
                    "f32" => return Some(scalar_abi("float", 4, 4)),
                    "i64" => return Some(scalar_abi("i64", 8, 8)),
                    "u64" => return Some(scalar_abi("u64", 8, 8)),
                    "f64" => return Some(scalar_abi("f64", 8, 8)),
                    "isize" => return Some(scalar_abi("i64", 8, 8)),
                    "usize" => return Some(scalar_abi("u64", 8, 8)),
                    "bool" => return Some(scalar_abi("bool", 1, 1)),
                    "InSliceU8" => {
                        return Some(AbiType {
                            boundary_type: "InSliceU8".to_string(),
                            size: 16,
                            align: 8,
                            transfer: Some(BoundaryTransfer::Borrow),
                        });
                    }
                    name => {
                        if let Some((repr, fields)) = layout_specs.get(name) {
                            let packed_layout = packed || matches!(repr, BoundaryRepr::Packed);
                            if let Some(layout) =
                                compute_struct_layout(name, repr.clone(), fields, layout_specs)
                            {
                                return Some(AbiType {
                                    boundary_type: name.to_string(),
                                    size: layout.size,
                                    align: if packed_layout { 1 } else { layout.align },
                                    transfer: Some(BoundaryTransfer::Copy),
                                });
                            }
                        }
                    }
                }
            }
            None
        }
        syn::Type::Ptr(_) | syn::Type::Reference(_) => Some(scalar_abi("u64", 8, 8)),
        syn::Type::Array(arr) => {
            let elem = abi_type_for(&arr.elem, layout_specs, packed)?;
            let len = match &arr.len {
                syn::Expr::Lit(expr_lit) => match &expr_lit.lit {
                    syn::Lit::Int(i) => i.base10_parse::<u64>().ok()?,
                    _ => return None,
                },
                _ => return None,
            };
            Some(AbiType {
                boundary_type: elem.boundary_type.clone(),
                size: elem.size.saturating_mul(len),
                align: if packed { 1 } else { elem.align },
                transfer: elem.transfer.clone(),
            })
        }
        _ => None,
    }
}

fn scalar_abi(boundary_type: &str, size: u64, align: u64) -> AbiType {
    AbiType {
        boundary_type: boundary_type.to_string(),
        size,
        align,
        transfer: Some(BoundaryTransfer::Copy),
    }
}

fn align_up(offset: u64, align: u64) -> u64 {
    if align == 0 {
        return offset;
    }
    let mask = align - 1;
    (offset + mask) & !mask
}

fn compute_struct_layout(
    name: &str,
    repr: BoundaryRepr,
    fields: &[(String, syn::Type)],
    layout_specs: &RustLayoutSpecs,
) -> Option<BoundaryLayout> {
    let packed = matches!(repr, BoundaryRepr::Packed);
    let mut offset = 0u64;
    let mut max_align = 1u64;
    let mut boundary_fields = Vec::new();

    for (field_name, field_ty) in fields {
        let abi = abi_type_for(field_ty, layout_specs, packed)?;
        let field_align = if packed { 1 } else { abi.align };
        offset = align_up(offset, field_align);
        boundary_fields.push(BoundaryField {
            name: field_name.clone(),
            offset,
            typ: abi.boundary_type.clone(),
            transfer: abi.transfer,
        });
        offset = offset.saturating_add(abi.size);
        max_align = max_align.max(field_align);
    }

    let struct_align = if packed { 1 } else { max_align };
    let size = if offset == 0 {
        struct_align
    } else {
        align_up(offset, struct_align)
    };

    Some(BoundaryLayout {
        name: name.to_string(),
        kind: "struct".to_string(),
        repr: Some(repr),
        size,
        align: struct_align,
        stride: size,
        fields: boundary_fields,
    })
}

fn boundary_type_name(ty: &syn::Type, layout_specs: &RustLayoutSpecs) -> String {
    abi_type_for(ty, layout_specs, false)
        .map(|abi| abi.boundary_type.clone())
        .unwrap_or_else(|| ty.to_token_stream().to_string())
}

fn boundary_symbol_from_fn(f: &syn::ItemFn, layout_specs: &RustLayoutSpecs) -> BoundarySymbol {
    let name = f.sig.ident.to_string();
    let mut parts = vec![name.clone()];
    for arg in &f.sig.inputs {
        if let syn::FnArg::Typed(pat_ty) = arg {
            parts.push(boundary_type_name(&pat_ty.ty, layout_specs));
        }
    }
    let ret = match &f.sig.output {
        syn::ReturnType::Default => "void".to_string(),
        syn::ReturnType::Type(_, ty) => boundary_type_name(ty, layout_specs),
    };
    parts.push(ret);
    let canonical = parts.join(";");
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&canonical, &mut h);
    let hash = h.finish();
    let ownership = match &f.sig.output {
        syn::ReturnType::Type(_, ty) if matches!(ty.as_ref(), syn::Type::Reference(_)) => {
            BoundaryOwnership::Borrowed
        }
        _ => BoundaryOwnership::ReturnsOwnedHandle,
    };
    BoundarySymbol {
        name,
        signature_hash: format!("siphash-{:016x}", hash),
        ownership,
        calling_convention: "c".to_string(),
    }
}

fn rust_struct_fields(fields: &syn::Fields) -> Vec<(String, Typ)> {
    match fields {
        syn::Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                (
                    f.ident
                        .as_ref()
                        .map(std::string::ToString::to_string)
                        .unwrap_or_else(|| "field".to_string()),
                    map_type(&f.ty),
                )
            })
            .collect(),
        syn::Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, f)| (format!("_{i}"), map_type(&f.ty)))
            .collect(),
        syn::Fields::Unit => vec![],
    }
}

fn lower_impl(i: &syn::ItemImpl) -> Vec<Decl> {
    // Strip generics from self type name to match collect_structs keys
    // e.g., "Bencher < 'a , M >" → "Bencher"
    let raw_type: String = i
        .self_ty
        .to_token_stream()
        .to_string()
        .chars()
        .filter(|&c| c != ' ')
        .collect();
    let self_type = if let Some(idx) = raw_type.find(" <") {
        raw_type[..idx].to_string()
    } else if let Some(idx) = raw_type.find(" (") {
        raw_type[..idx].to_string()
    } else {
        raw_type
    };
    let mut decls = Vec::new();
    for item in &i.items {
        if let syn::ImplItem::Fn(method) = item {
            let f = method.clone();
            let method_name = if i.trait_.is_some() {
                // Trait impl: use the original method name
                f.sig.ident.to_string()
            } else {
                // Inherent impl: `Type::method_name` (matches Rust path syntax)
                format!("{}::{}", self_type, f.sig.ident)
            };
            // Prepend self param if present
            let params: Vec<(String, Typ)> = f
                .sig
                .inputs
                .iter()
                .map(|arg| match arg {
                    syn::FnArg::Typed(pat_ty) => {
                        let pname = pattern_name(&pat_ty.pat).unwrap_or_else(|| format!("arg"));
                        (pname, map_type(&pat_ty.ty))
                    }
                    syn::FnArg::Receiver(_recv) => {
                        ("self".to_string(), Typ::Named(self_type.clone()))
                    }
                })
                .collect();
            let ret = match &f.sig.output {
                syn::ReturnType::Default => Typ::Void,
                syn::ReturnType::Type(_, ty) => map_type(ty),
            };
            let mut local_types = HashMap::new();
            // Add self type to local types for method dispatch
            for (pname, pty) in &params {
                local_types.insert(pname.clone(), type_to_string(pty));
            }
            let body = lower_block_with_types(&f.block, &mut local_types);
            decls.push(Decl::Function {
                name: method_name,
                params,
                ret,
                body,
                type_params: vec![],
            });
        }
    }
    decls
}

fn lower_enum(e: &syn::ItemEnum) -> Option<Decl> {
    // Lower enum to a struct with a discriminant field
    let enum_name = e.ident.to_string();
    let mut fields = vec![("__discriminant".to_string(), Typ::Int)];
    for variant in &e.variants {
        let variant_name = variant.ident.to_string();
        match &variant.fields {
            syn::Fields::Named(named) => {
                for f in &named.named {
                    if let Some(ident) = &f.ident {
                        fields.push((format!("{variant_name}_{}", ident), map_type(&f.ty)));
                    }
                }
            }
            syn::Fields::Unnamed(unnamed) => {
                for (i, f) in unnamed.unnamed.iter().enumerate() {
                    fields.push((format!("{variant_name}_{i}"), map_type(&f.ty)));
                }
            }
            syn::Fields::Unit => {}
        }
    }
    if fields.len() <= 1 {
        // Fieldless enum — skip (no lowering needed)
        return None;
    }
    Some(Decl::Struct {
        name: enum_name,
        fields,
        type_params: vec![],
    })
}

fn type_to_string(ty: &Typ) -> String {
    match ty {
        Typ::Int => "i64".to_string(),
        Typ::Bool => "bool".to_string(),
        Typ::String => "String".to_string(),
        Typ::Float => "f64".to_string(),
        Typ::Void => "()".to_string(),
        Typ::Named(n) => n.clone(),
        Typ::Array(e) => format!("Vec<{}>", type_to_string(e)),
        Typ::Vector(_) => "Vec".to_string(),
        Typ::Generic(g) => g.clone(),
    }
}

fn lower_fn(f: syn::ItemFn) -> Decl {
    lower_fn_with_types(f, &mut HashMap::new())
}

fn lower_fn_with_types(f: syn::ItemFn, local_types: &mut HashMap<String, String>) -> Decl {
    let name = f.sig.ident.to_string();
    let params = f
        .sig
        .inputs
        .iter()
        .map(|arg| match arg {
            syn::FnArg::Typed(pat_ty) => {
                let pname = pattern_name(&pat_ty.pat)
                    .unwrap_or_else(|| format!("arg_{}", params_fallback_idx(&pat_ty.pat)));
                // Track parameter types for method dispatch
                let ty = map_type(&pat_ty.ty);
                local_types.insert(pname.clone(), type_to_string(&ty));
                (pname, ty)
            }
            syn::FnArg::Receiver(_) => ("self".to_string(), Typ::Named("Self".to_string())),
        })
        .collect();
    let ret = match &f.sig.output {
        syn::ReturnType::Default => Typ::Void,
        syn::ReturnType::Type(_, ty) => map_type(ty),
    };
    let body = lower_block_with_types(&f.block, local_types);
    Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    }
}

fn params_fallback_idx(pat: &syn::Pat) -> usize {
    pat.to_token_stream().to_string().len()
}

fn pattern_name(pat: &syn::Pat) -> Option<String> {
    match pat {
        syn::Pat::Ident(pi) => Some(pi.ident.to_string()),
        syn::Pat::Reference(r) => pattern_name(&r.pat),
        syn::Pat::TupleStruct(ts) => Some(ts.path.to_token_stream().to_string()),
        syn::Pat::Struct(ps) => Some(ps.path.to_token_stream().to_string()),
        syn::Pat::Type(pt) => pattern_name(&pt.pat),
        _ => None,
    }
}

fn err_pattern_binding(pat: &syn::Pat) -> Option<String> {
    let syn::Pat::TupleStruct(tuple) = pat else {
        return None;
    };
    (tuple.path.segments.last()?.ident == "Err")
        .then(|| tuple.elems.first())
        .flatten()
        .and_then(pattern_name)
}

fn map_type(ty: &syn::Type) -> Typ {
    match ty {
        syn::Type::Path(tp) => {
            let last_segment = tp.path.segments.last();
            let last = last_segment.map(|s| s.ident.to_string());
            match last.as_deref() {
                Some("i8" | "i16" | "i32" | "i64" | "i128" | "isize") => Typ::Int,
                Some("u8" | "u16" | "u32" | "u64" | "u128" | "usize") => Typ::Int,
                Some("String" | "str") => Typ::String,
                Some("bool") => Typ::Bool,
                Some("Result") => last_segment
                    .and_then(|segment| match &segment.arguments {
                        syn::PathArguments::AngleBracketed(arguments) => {
                            arguments.args.iter().find_map(|arg| match arg {
                                syn::GenericArgument::Type(ty) => Some(map_type(ty)),
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| Typ::Named("Result".to_string())),
                Some("Vec") => last_segment
                    .and_then(|segment| match &segment.arguments {
                        syn::PathArguments::AngleBracketed(arguments) => {
                            arguments.args.iter().find_map(|arg| match arg {
                                syn::GenericArgument::Type(ty) => Some(map_type(ty)),
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .map(|elem| Typ::Vector(Box::new(elem)))
                    .unwrap_or_else(|| Typ::Vector(Box::new(Typ::Generic("_".to_string())))),
                Some(other) => Typ::Named(other.to_string()),
                None => Typ::Named(tp.path.to_token_stream().to_string()),
            }
        }
        syn::Type::Reference(r) => map_type(&r.elem),
        syn::Type::Array(array) => Typ::Array(Box::new(map_type(&array.elem))),
        syn::Type::Tuple(t) if t.elems.is_empty() => Typ::Void,
        _ => Typ::Named(ty.to_token_stream().to_string()),
    }
}

fn lower_block_with_types(
    block: &syn::Block,
    local_types: &mut HashMap<String, String>,
) -> Vec<Stmt> {
    lower_block_inner_with_types(block, true, local_types)
}

fn lower_block_no_implicit_return_with_types(
    block: &syn::Block,
    local_types: &mut HashMap<String, String>,
) -> Vec<Stmt> {
    lower_block_inner_with_types(block, false, local_types)
}

thread_local! {
    static BLOCK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn lower_block_inner_with_types(
    block: &syn::Block,
    wrap_implicit_return: bool,
    local_types: &mut HashMap<String, String>,
) -> Vec<Stmt> {
    let depth = BLOCK_DEPTH.with(|d| {
        let n = d.get();
        d.set(n.saturating_add(1));
        n
    });
    if depth > 32 {
        BLOCK_DEPTH.with(|d| d.set(depth));
        return vec![];
    }
    let out = lower_block_inner_with_types_go(block, wrap_implicit_return, local_types);
    BLOCK_DEPTH.with(|d| d.set(depth));
    out
}

fn lower_block_inner_with_types_go(
    block: &syn::Block,
    wrap_implicit_return: bool,
    local_types: &mut HashMap<String, String>,
) -> Vec<Stmt> {
    let mut out = Vec::new();
    for stmt in &block.stmts {
        match stmt {
            syn::Stmt::Local(local) => {
                if let Some(init) = &local.init {
                    // For destructuring patterns (Err(e), Some(x), etc.), use a temp name
                    let is_simple = pattern_name(&local.pat).is_some();
                    let name = if is_simple {
                        pattern_name(&local.pat).unwrap_or_else(|| "_pat".to_string())
                    } else {
                        "_pat".to_string()
                    };
                    // Infer type from annotation or constructor
                    if is_simple {
                        if let Some(ty) = local_decl_type(&local.pat) {
                            // ponytail: convert Typ to string name for method dispatch
                            let type_str = type_to_string(&ty);
                            local_types.insert(name.clone(), type_str);
                        } else if let syn::Expr::Call(call) = &*init.expr {
                            // Infer from Type::constructor() pattern
                            if let syn::Expr::Path(p) = &*call.func {
                                let path_str: String = p
                                    .path
                                    .to_token_stream()
                                    .to_string()
                                    .chars()
                                    .filter(|&c| c != ' ')
                                    .collect();
                                if let Some((prefix, _method)) = path_str.rsplit_once("::") {
                                    local_types.insert(name.clone(), prefix.to_string());
                                }
                            }
                        }
                    }
                    if let syn::Expr::Try(try_expr) = init.expr.as_ref() {
                        out.push(Stmt::Let(
                            name,
                            local_decl_type(&local.pat),
                            lower_expr_with_types(&try_expr.expr, local_types),
                        ));
                        out.push(Stmt::Propagate);
                        continue;
                    }
                    let expr = lower_expr_with_types(&init.expr, local_types);
                    let local_ty = local_decl_type(&local.pat).or_else(|| {
                        matches!(
                            &expr,
                            Expr::Call { callee, .. }
                                if matches!(
                                    callee.as_ref(),
                                    Expr::Ident(name) if name == "Vec::new"
                                )
                        )
                        .then(|| Typ::Vector(Box::new(Typ::Generic("_".to_string()))))
                    }).or_else(|| {
                        matches!(init.expr.as_ref(), syn::Expr::Macro(m) if m.mac.path.is_ident("vec"))
                            .then(|| Typ::Vector(Box::new(Typ::Generic("_".to_string()))))
                    });
                    if is_simple
                        && !local_types.contains_key(&name)
                        && let Some(typ) = local_ty.as_ref()
                    {
                        local_types.insert(name.clone(), type_to_string(typ));
                    }
                    out.push(Stmt::Let(name, local_ty, expr));
                }
            }
            syn::Stmt::Expr(expr, _) => {
                lower_expr_stmt(expr, &mut out, local_types);
            }
            syn::Stmt::Macro(m) => {
                let expr = syn::Expr::Macro(syn::ExprMacro {
                    attrs: vec![],
                    mac: m.mac.clone(),
                });
                lower_expr_stmt(&expr, &mut out, local_types);
            }
            syn::Stmt::Item(item) => {
                if let syn::Item::Fn(f) = item {
                    out.push(Stmt::Expr(Expr::Ident(format!(
                        "nested-fn:{}",
                        f.sig.ident
                    ))));
                }
            }
        }
    }
    // Only add implicit Return(None) if not all paths already return
    // and the function return type isn't void (no implicit wrapping for unit-like functions)
    if wrap_implicit_return && !all_paths_return(&out) {
        if let Some(Stmt::Expr(expr)) = out.last().cloned() {
            out.pop();
            out.push(Stmt::Return(Some(expr)));
        } else {
            out.push(Stmt::Return(None));
        }
    }
    out
}

/// Check if all code paths through the statements end with a return.
fn all_paths_return(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::Return(_) => return true,
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                // If present: both branches must return
                if else_body.is_empty() {
                    return false; // no else → if may fall through
                }
                if !all_paths_return(then_body) || !all_paths_return(else_body) {
                    return false;
                }
                // Check if there's more after this if (not a terminal if/else with returns)
                // For simplicity, if the if/else both return, consider this a terminal return
                // but we still need to check if it's the last stmt
            }
            _ => {
                // Non-return statement → maybe followed by a return later
                continue;
            }
        }
    }
    // Check if the last stmt(s) ensure all paths return
    stmts.last().map_or(false, stmt_ensures_return)
}

fn stmt_ensures_return(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::If {
            then_body,
            else_body,
            ..
        } => !else_body.is_empty() && all_paths_return(then_body) && all_paths_return(else_body),
        Stmt::Match { arms, .. } => {
            // ponytail: exhaustive match doesn't need implicit return
            !arms.is_empty()
        }
        _ => false,
    }
}

fn lower_expr_stmt(
    expr: &syn::Expr,
    out: &mut Vec<Stmt>,
    local_types: &mut HashMap<String, String>,
) {
    match expr {
        syn::Expr::Return(ret) => lower_return_expr(ret.expr.as_deref(), out, local_types),
        syn::Expr::Break(_) => out.push(Stmt::Break),
        syn::Expr::Continue(_) => out.push(Stmt::Continue),
        syn::Expr::Try(try_expr) => {
            out.push(Stmt::Expr(lower_expr_with_types(
                &try_expr.expr,
                local_types,
            )));
            out.push(Stmt::Propagate);
        }
        syn::Expr::If(eif) => {
            // Handle `if let` (condition is Expr::Let)
            if let syn::Expr::Let(expr_let) = &*eif.cond {
                if eif.else_branch.is_none()
                    && let Some(binding) = err_pattern_binding(&expr_let.pat)
                {
                    out.push(Stmt::Try {
                        body: vec![Stmt::Expr(lower_expr_with_types(
                            &expr_let.expr,
                            local_types,
                        ))],
                        catches: vec![CatchArm {
                            pattern: binding,
                            body: lower_block_no_implicit_return_with_types(
                                &eif.then_branch,
                                local_types,
                            ),
                        }],
                    });
                    return;
                }
                // `if let Pat = Expr { ... } else { ... }`
                // Lower to: let _pat = Expr; match _pat { Pat => ..., _ => else }
                let scrutinee = lower_expr_with_types(&expr_let.expr, local_types);
                let then_body =
                    lower_block_no_implicit_return_with_types(&eif.then_branch, local_types);
                let else_body = eif
                    .else_branch
                    .as_ref()
                    .map(|(_tok, else_branch)| lower_else_body_with_types(else_branch, local_types))
                    .unwrap_or_default();
                let arms = vec![
                    MatchArm {
                        pattern: expr_let.pat.to_token_stream().to_string(),
                        body: then_body,
                    },
                    MatchArm {
                        pattern: "_".to_string(),
                        body: else_body,
                    },
                ];
                out.push(Stmt::Match { scrutinee, arms });
            } else {
                let cond = lower_expr_with_types(&eif.cond, local_types);
                let then_body =
                    lower_block_no_implicit_return_with_types(&eif.then_branch, local_types);
                let else_body = eif
                    .else_branch
                    .as_ref()
                    .map(|(_tok, else_branch)| lower_else_body_with_types(else_branch, local_types))
                    .unwrap_or_default();
                out.push(Stmt::If {
                    cond,
                    then_body,
                    else_body,
                });
            }
        }
        syn::Expr::ForLoop(f) => {
            let body = lower_block_no_implicit_return_with_types(&f.body, local_types);
            out.push(Stmt::Loop {
                kind: LoopKind::For {
                    binding: f.pat.to_token_stream().to_string(),
                },
                cond: Some(lower_expr_with_types(&f.expr, local_types)),
                body,
            });
        }
        syn::Expr::While(w) => {
            out.push(Stmt::Loop {
                kind: LoopKind::While,
                cond: Some(lower_expr_with_types(&w.cond, local_types)),
                body: lower_block_no_implicit_return_with_types(&w.body, local_types),
            });
        }
        syn::Expr::Loop(l) => {
            out.push(Stmt::Loop {
                kind: LoopKind::Infinite,
                cond: None,
                body: lower_block_no_implicit_return_with_types(&l.body, local_types),
            });
        }
        syn::Expr::Match(m) => {
            let arms = m
                .arms
                .iter()
                .map(|arm| MatchArm {
                    pattern: arm.pat.to_token_stream().to_string(),
                    body: match arm.body.as_ref() {
                        syn::Expr::Block(b) => lower_block_with_types(&b.block, local_types),
                        body => vec![Stmt::Expr(lower_expr_with_types(body, local_types))],
                    },
                })
                .collect();
            out.push(Stmt::Match {
                scrutinee: lower_expr_with_types(&m.expr, local_types),
                arms,
            });
        }
        syn::Expr::Block(b) => out.extend(lower_block_with_types(&b.block, local_types)),
        syn::Expr::Assign(a) => {
            if let Some(name) = assign_lhs_name(&a.left) {
                out.push(Stmt::Assign(
                    name,
                    lower_expr_with_types(&a.right, local_types),
                ));
            } else if let syn::Expr::Field(f) = &*a.left {
                out.push(Stmt::FieldAssign {
                    base: lower_expr_with_types(&f.base, local_types),
                    name: match &f.member {
                        syn::Member::Unnamed(idx) => format!("_{}", idx.index),
                        _ => f.member.to_token_stream().to_string(),
                    },
                    value: lower_expr_with_types(&a.right, local_types),
                });
            }
        }
        syn::Expr::Binary(b) if assign_op_from_binop(&b.op).is_some() => {
            let op = assign_op_from_binop(&b.op).expect("checked");
            if let Some(name) = assign_lhs_name(&b.left) {
                out.push(Stmt::Assign(
                    name.clone(),
                    Expr::Binary {
                        op: op.into(),
                        lhs: Box::new(Expr::Ident(name)),
                        rhs: Box::new(lower_expr_with_types(&b.right, local_types)),
                    },
                ));
            } else {
                out.push(Stmt::Expr(Expr::Binary {
                    op: op.into(),
                    lhs: Box::new(lower_expr_with_types(&b.left, local_types)),
                    rhs: Box::new(lower_expr_with_types(&b.right, local_types)),
                }));
            }
        }
        _ => out.push(Stmt::Expr(lower_expr_with_types(expr, local_types))),
    }
}

fn lower_return_expr(
    expr: Option<&syn::Expr>,
    out: &mut Vec<Stmt>,
    local_types: &mut HashMap<String, String>,
) {
    let Some(expr) = expr else {
        out.push(Stmt::Return(None));
        return;
    };
    let syn::Expr::Call(call) = expr else {
        out.push(Stmt::Return(Some(lower_expr_with_types(expr, local_types))));
        return;
    };
    let Some(name) = rust_path_name(&call.func) else {
        out.push(Stmt::Return(Some(lower_expr_with_types(expr, local_types))));
        return;
    };
    match (name.as_str(), call.args.len()) {
        ("Ok", 1) => out.push(Stmt::Return(Some(lower_expr_with_types(
            call.args.first().expect("one Ok argument"),
            local_types,
        )))),
        ("Err", 1) => {
            out.push(Stmt::Throw(lower_expr_with_types(
                call.args.first().expect("one Err argument"),
                local_types,
            )));
            out.push(Stmt::Return(None));
        }
        _ => out.push(Stmt::Return(Some(lower_expr_with_types(expr, local_types)))),
    }
}

fn rust_path_name(expr: &syn::Expr) -> Option<String> {
    let syn::Expr::Path(path) = expr else {
        return None;
    };
    Some(
        path.path
            .to_token_stream()
            .to_string()
            .chars()
            .filter(|ch| *ch != ' ')
            .collect(),
    )
}

fn lower_else_body_with_types(
    else_branch: &syn::Expr,
    local_types: &mut HashMap<String, String>,
) -> Vec<Stmt> {
    match else_branch {
        syn::Expr::Block(b) => lower_block_with_types(&b.block, local_types),
        syn::Expr::If(e) => {
            let mut out = Vec::new();
            lower_expr_stmt(&syn::Expr::If(e.clone()), &mut out, local_types);
            out
        }
        other => vec![Stmt::Expr(lower_expr_with_types(other, local_types))],
    }
}

fn assign_op_from_binop(op: &syn::BinOp) -> Option<&'static str> {
    match op {
        syn::BinOp::AddAssign(_) => Some("+"),
        syn::BinOp::SubAssign(_) => Some("-"),
        syn::BinOp::MulAssign(_) => Some("*"),
        syn::BinOp::DivAssign(_) => Some("/"),
        syn::BinOp::RemAssign(_) => Some("%"),
        syn::BinOp::BitXorAssign(_) => Some("^"),
        syn::BinOp::BitAndAssign(_) => Some("&"),
        syn::BinOp::BitOrAssign(_) => Some("|"),
        syn::BinOp::ShlAssign(_) => Some("<<"),
        syn::BinOp::ShrAssign(_) => Some(">>"),
        _ => None,
    }
}

fn assign_lhs_name(lhs: &syn::Expr) -> Option<String> {
    match lhs {
        syn::Expr::Path(p) => Some(p.path.to_token_stream().to_string()),
        syn::Expr::Field(_) => None, // field assignments handled in Expr::Assign branch
        syn::Expr::Index(i) => Some(i.to_token_stream().to_string()),
        _ => None,
    }
}

fn local_decl_type(pat: &syn::Pat) -> Option<Typ> {
    match pat {
        syn::Pat::Type(pt) => Some(map_type(&pt.ty)),
        syn::Pat::Ident(_) => None,
        syn::Pat::Reference(r) => local_decl_type(&r.pat),
        _ => None,
    }
}

thread_local! {
    static EXPR_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn lower_expr_with_types(expr: &syn::Expr, local_types: &mut HashMap<String, String>) -> Expr {
    let depth = EXPR_DEPTH.with(|d| {
        let n = d.get();
        d.set(n.saturating_add(1));
        n
    });
    if depth > 64 {
        EXPR_DEPTH.with(|d| d.set(depth));
        return Expr::IntLit(0);
    }
    let out = lower_expr_with_types_inner(expr, local_types);
    EXPR_DEPTH.with(|d| d.set(depth));
    out
}

fn lower_expr_with_types_inner(
    expr: &syn::Expr,
    local_types: &mut HashMap<String, String>,
) -> Expr {
    match expr {
        syn::Expr::Lit(l) => match &l.lit {
            syn::Lit::Int(i) => i
                .base10_parse::<i64>()
                .map(Expr::IntLit)
                .unwrap_or_else(|_| Expr::Ident(i.to_token_stream().to_string())),
            syn::Lit::Bool(b) => Expr::BoolLit(b.value),
            syn::Lit::Str(s) => Expr::StringLit(s.value()),
            _ => Expr::Ident(l.lit.to_token_stream().to_string()),
        },
        syn::Expr::Path(p) => {
            let path = p.path.to_token_stream().to_string();
            // Strip spaces from path (token_stream produces "Point :: new" but table has "Point::new")
            Expr::Ident(path.chars().filter(|&c| c != ' ').collect())
        }
        syn::Expr::Reference(r) => lower_expr_with_types(&r.expr, local_types),
        syn::Expr::Paren(p) => lower_expr_with_types(&p.expr, local_types),
        syn::Expr::Tuple(tuple) if tuple.elems.is_empty() => Expr::IntLit(0),
        syn::Expr::Array(array) => Expr::ArrayLit(
            array
                .elems
                .iter()
                .map(|item| lower_expr_with_types(item, local_types))
                .collect(),
        ),
        syn::Expr::Macro(mac) if mac.mac.path.is_ident("vec") => {
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(mac.mac.tokens.clone())
                .map(|items| {
                    Expr::ArrayLit(
                        items
                            .iter()
                            .map(|item| lower_expr_with_types(item, local_types))
                            .collect(),
                    )
                })
                .unwrap_or_else(|_| Expr::Ident(mac.to_token_stream().to_string()))
        }
        syn::Expr::Macro(mac) if mac.mac.path.is_ident("env") => {
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(mac.mac.tokens.clone())
                .ok()
                .and_then(|items| {
                    let syn::Expr::Lit(lit) = items.into_iter().next()? else {
                        return None;
                    };
                    let syn::Lit::Str(key) = lit.lit else {
                        return None;
                    };
                    let key = key.value();
                    let value = if key == "CARGO_PKG_VERSION" {
                        std::env::var("CARGO_PKG_VERSION")
                            .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
                    } else {
                        std::env::var(&key).unwrap_or_default()
                    };
                    Some(Expr::StringLit(value))
                })
                .unwrap_or_else(|| Expr::StringLit(String::new()))
        }
        syn::Expr::Macro(mac)
            if mac.mac.path.is_ident("println") || mac.mac.path.is_ident("eprintln") =>
        {
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(mac.mac.tokens.clone())
                .map(|items| {
                    let args: Vec<Expr> = items
                        .iter()
                        .map(|item| lower_expr_with_types(item, local_types))
                        .collect();
                    Expr::Call {
                        callee: Box::new(Expr::Ident("print".into())),
                        args: if args.is_empty() {
                            vec![Expr::StringLit(String::new())]
                        } else {
                            args
                        },
                    }
                })
                .unwrap_or_else(|_| Expr::Call {
                    callee: Box::new(Expr::Ident("print".into())),
                    args: vec![Expr::StringLit(String::new())],
                })
        }
        syn::Expr::Macro(mac) if mac.mac.path.is_ident("format") => {
            syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
                .parse2(mac.mac.tokens.clone())
                .ok()
                .and_then(|items| {
                    let mut items = items.into_iter();
                    let syn::Expr::Lit(first) = items.next()? else {
                        return None;
                    };
                    let syn::Lit::Str(template) = first.lit else {
                        return None;
                    };
                    let template = template.value();
                    let segments: Vec<_> = template.split("{}").collect();
                    let args: Vec<_> = items.collect();
                    if segments.len() != args.len() + 1 {
                        return None;
                    }
                    let mut result = Expr::StringLit(segments[0].to_string());
                    for (arg, suffix) in args.into_iter().zip(segments.into_iter().skip(1)) {
                        result = Expr::Call {
                            callee: Box::new(Expr::Ident("str-concat".to_string())),
                            args: vec![result, lower_expr_with_types(&arg, local_types)],
                        };
                        if !suffix.is_empty() {
                            result = Expr::Call {
                                callee: Box::new(Expr::Ident("str-concat".to_string())),
                                args: vec![result, Expr::StringLit(suffix.to_string())],
                            };
                        }
                    }
                    Some(result)
                })
                .unwrap_or_else(|| Expr::Ident(mac.to_token_stream().to_string()))
        }
        syn::Expr::Closure(closure) => {
            let mut closure_types = local_types.clone();
            let params = closure
                .inputs
                .iter()
                .map(|input| {
                    let name = pattern_name(input).unwrap_or_else(|| "_".to_string());
                    let typ =
                        local_decl_type(input).unwrap_or_else(|| Typ::Generic("_".to_string()));
                    closure_types.insert(name.clone(), type_to_string(&typ));
                    (name, typ)
                })
                .collect();
            let body = match closure.body.as_ref() {
                syn::Expr::Block(block) => lower_block_with_types(&block.block, &mut closure_types),
                body => vec![Stmt::Return(Some(lower_expr_with_types(
                    body,
                    &mut closure_types,
                )))],
            };
            Expr::Closure {
                params,
                ret: Typ::Generic("_".to_string()),
                body,
                captures: vec![],
            }
        }
        syn::Expr::Call(c) => {
            let callee = lower_expr_with_types(&c.func, local_types);
            let args: Vec<Expr> = c
                .args
                .iter()
                .map(|a| lower_expr_with_types(a, local_types))
                .collect();
            if let Expr::Ident(name) = &callee {
                let short = name.rsplit("::").next().unwrap_or(name);
                if short == "exit"
                    || name.ends_with("process::exit")
                    || name == "std::process::exit"
                {
                    return Expr::Call {
                        callee: Box::new(Expr::Ident("process-exit".into())),
                        args,
                    };
                }
                if name == "std::env::args" || (short == "args" && name.contains("env")) {
                    return Expr::Call {
                        callee: Box::new(Expr::Ident("env-args".into())),
                        args,
                    };
                }
            }
            Expr::Call {
                callee: Box::new(callee),
                args,
            }
        }
        syn::Expr::MethodCall(m) => {
            let mut args = Vec::with_capacity(m.args.len() + 1);
            args.push(lower_expr_with_types(&m.receiver, local_types));
            args.extend(m.args.iter().map(|a| lower_expr_with_types(a, local_types)));
            // ponytail: qualify method name with receiver type when known
            let method = m.method.to_string();
            // Iterator adapters: `.iter()` / `.cloned()` / `.copied()` are identity
            // in the owned subset. `.collect()` yields the receiver.
            if matches!(
                method.as_str(),
                "iter" | "into_iter" | "cloned" | "copied" | "collect" | "to_vec" | "to_string"
            ) {
                return args.into_iter().next().unwrap_or(Expr::IntLit(0));
            }
            if method == "len" {
                return Expr::Call {
                    callee: Box::new(Expr::Ident("len".into())),
                    args,
                };
            }
            let method_name = if let Some(Expr::Ident(receiver_name)) = args.first() {
                if let Some(ty) = local_types.get(receiver_name) {
                    format!("{ty}::{method}")
                } else {
                    method
                }
            } else {
                method
            };
            Expr::Call {
                callee: Box::new(Expr::Ident(method_name)),
                args,
            }
        }
        syn::Expr::Unary(u) => Expr::Unary {
            op: u.op.to_token_stream().to_string(),
            expr: Box::new(lower_expr_with_types(&u.expr, local_types)),
        },
        syn::Expr::Binary(b) => Expr::Binary {
            op: b.op.to_token_stream().to_string(),
            lhs: Box::new(lower_expr_with_types(&b.left, local_types)),
            rhs: Box::new(lower_expr_with_types(&b.right, local_types)),
        },
        syn::Expr::Field(ef) => Expr::Field {
            base: Box::new(lower_expr_with_types(&ef.base, local_types)),
            name: match &ef.member {
                syn::Member::Unnamed(idx) => format!("_{}", idx.index),
                _ => ef.member.to_token_stream().to_string(),
            },
        },
        syn::Expr::Index(index) => Expr::Index {
            base: Box::new(lower_expr_with_types(&index.expr, local_types)),
            index: Box::new(lower_expr_with_types(&index.index, local_types)),
        },
        syn::Expr::Cast(cast) => {
            // ponytail: ignore integer casts, types all match at register width
            lower_expr_with_types(&cast.expr, local_types)
        }
        syn::Expr::Try(t) => lower_expr_with_types(&t.expr, local_types),
        syn::Expr::Await(a) => lower_expr_with_types(&a.base, local_types),
        syn::Expr::Async(a) => Expr::Closure {
            params: vec![],
            ret: Typ::Generic("_".into()),
            body: lower_block_with_types(&a.block, local_types),
            captures: vec![],
        },
        syn::Expr::If(eif) => {
            let then_body = lower_block_with_types(&eif.then_branch, local_types);
            let else_body = eif
                .else_branch
                .as_ref()
                .map(|(_, e)| lower_else_body_with_types(e, local_types))
                .unwrap_or_default();
            Expr::Closure {
                params: vec![],
                ret: Typ::Generic("_".into()),
                body: vec![Stmt::If {
                    cond: lower_expr_with_types(&eif.cond, local_types),
                    then_body,
                    else_body,
                }],
                captures: vec![],
            }
        }
        syn::Expr::Block(b) => {
            let body = lower_block_with_types(&b.block, local_types);
            Expr::Closure {
                params: vec![],
                ret: Typ::Generic("_".into()),
                body,
                captures: vec![],
            }
        }
        syn::Expr::Repeat(r) => Expr::ArrayLit(vec![lower_expr_with_types(&r.expr, local_types)]),
        syn::Expr::Range(r) => Expr::Call {
            callee: Box::new(Expr::Ident("range".into())),
            args: vec![
                r.start
                    .as_ref()
                    .map(|s| lower_expr_with_types(s, local_types))
                    .unwrap_or(Expr::IntLit(0)),
                r.end
                    .as_ref()
                    .map(|s| lower_expr_with_types(s, local_types))
                    .unwrap_or(Expr::IntLit(0)),
            ],
        },
        syn::Expr::Tuple(t) => Expr::ArrayLit(
            t.elems
                .iter()
                .map(|e| lower_expr_with_types(e, local_types))
                .collect(),
        ),
        syn::Expr::Unsafe(u) => {
            let body = lower_block_with_types(&u.block, local_types);
            Expr::Closure {
                params: vec![],
                ret: Typ::Generic("_".into()),
                body,
                captures: vec![],
            }
        }
        syn::Expr::Match(m) => {
            let arms = m
                .arms
                .iter()
                .map(|arm| MatchArm {
                    pattern: arm.pat.to_token_stream().to_string(),
                    body: match arm.body.as_ref() {
                        syn::Expr::Block(b) => lower_block_with_types(&b.block, local_types),
                        body => vec![Stmt::Return(Some(lower_expr_with_types(body, local_types)))],
                    },
                })
                .collect();
            Expr::Closure {
                params: vec![],
                ret: Typ::Generic("_".into()),
                body: vec![Stmt::Match {
                    scrutinee: lower_expr_with_types(&m.expr, local_types),
                    arms,
                }],
                captures: vec![],
            }
        }
        syn::Expr::Group(g) => lower_expr_with_types(&g.expr, local_types),
        syn::Expr::Const(c) => Expr::Closure {
            params: vec![],
            ret: Typ::Generic("_".into()),
            body: lower_block_with_types(&c.block, local_types),
            captures: vec![],
        },
        syn::Expr::Return(r) => r
            .expr
            .as_ref()
            .map(|inner| lower_expr_with_types(inner, local_types))
            .unwrap_or(Expr::IntLit(0)),
        syn::Expr::Assign(a) => lower_expr_with_types(&a.right, local_types),
        syn::Expr::Break(b) => b
            .expr
            .as_ref()
            .map(|inner| lower_expr_with_types(inner, local_types))
            .unwrap_or(Expr::IntLit(0)),
        syn::Expr::Continue(_) | syn::Expr::Infer(_) | syn::Expr::Yield(_) => Expr::IntLit(0),
        syn::Expr::ForLoop(f) => Expr::Closure {
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::Loop {
                kind: LoopKind::For {
                    binding: f.pat.to_token_stream().to_string(),
                },
                cond: Some(lower_expr_with_types(&f.expr, local_types)),
                body: lower_block_no_implicit_return_with_types(&f.body, local_types),
            }],
            captures: vec![],
        },
        syn::Expr::Loop(l) => Expr::Closure {
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::Loop {
                kind: LoopKind::Infinite,
                cond: None,
                body: lower_block_no_implicit_return_with_types(&l.body, local_types),
            }],
            captures: vec![],
        },
        syn::Expr::Let(l) => lower_expr_with_types(&l.expr, local_types),
        syn::Expr::While(w) => Expr::Closure {
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::Loop {
                kind: LoopKind::While,
                cond: Some(lower_expr_with_types(&w.cond, local_types)),
                body: lower_block_no_implicit_return_with_types(&w.body, local_types),
            }],
            captures: vec![],
        },
        syn::Expr::Struct(s) => Expr::StructInit {
            name: s.path.to_token_stream().to_string(),
            fields: s
                .fields
                .iter()
                .map(|f| {
                    (
                        match &f.member {
                            syn::Member::Unnamed(idx) => format!("_{}", idx.index),
                            _ => f.member.to_token_stream().to_string(),
                        },
                        lower_expr_with_types(&f.expr, local_types),
                    )
                })
                .collect(),
        },
        other => {
            if let syn::Expr::Verbatim(ts) = other {
                let text: String = ts
                    .to_string()
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                if !text.is_empty() {
                    return Expr::Ident(text);
                }
            }
            Expr::Call {
                callee: Box::new(Expr::Ident(other.to_token_stream().to_string())),
                args: vec![],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary_ir::{BoundaryRepr, BoundaryTransfer};

    #[test]
    fn parses_struct_and_function_with_body() {
        let src = r#"
struct Point { x: i64, y: i64 }
fn main() { let v = 7; return; }
"#;
        let module = parse_rust_source(src).expect("parse rust");
        assert!(
            module
                .decls
                .iter()
                .any(|d| matches!(d, Decl::Struct { name, .. } if name == "Point"))
        );
        assert!(module.decls.iter().any(
            |d| matches!(d, Decl::Function { name, body, .. } if name == "main" && !body.is_empty())
        ));
    }

    #[test]
    fn lowers_unit_tuple_to_zero() {
        let expr: syn::Expr = syn::parse_str("()").expect("parse unit tuple");
        assert_eq!(
            lower_expr_with_types(&expr, &mut HashMap::new()),
            Expr::IntLit(0)
        );
    }

    #[test]
    fn lowers_result_return_and_propagation() {
        let module = parse_rust_source(
            r#"
fn leaf() -> Result<i64, i64> { return Err(4); }
fn main() -> Result<i64, i64> {
    let value: i64 = leaf()?;
    return Ok(value + 1);
}
"#,
        )
        .expect("parse result source");
        let Decl::Function { ret, body, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function { name, .. } if name == "main"))
            .expect("main function")
        else {
            panic!("main must be a function");
        };
        assert_eq!(*ret, Typ::Int);
        assert!(matches!(body.get(1), Some(Stmt::Propagate)));
        assert!(matches!(body.last(), Some(Stmt::Return(Some(_)))));
    }

    #[test]
    fn lowers_if_let_err_to_try() {
        let module = parse_rust_source(
            "fn run() -> Result<(), i64> { Ok(()) } fn main() { if let Err(err) = run() { report(err); } }",
        )
        .expect("parse if let err");
        let Decl::Function { body, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function { name, .. } if name == "main"))
            .expect("main function")
        else {
            panic!("main must be a function");
        };
        assert!(
            matches!(body.first(), Some(Stmt::Try { catches, .. }) if catches[0].pattern == "err")
        );
    }

    #[test]
    fn lowers_vec_macro_literal() {
        let module = parse_rust_source("fn main() { let values: Vec<i64> = vec![1, 2]; }")
            .expect("parse Vec literal");
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!("main must be a function");
        };
        assert!(matches!(
            body.first(),
            Some(Stmt::Let(_, Some(Typ::Vector(elem)), Expr::ArrayLit(items)))
                if **elem == Typ::Int && items.len() == 2
        ));
    }

    #[test]
    fn lowers_fixed_string_array_return() {
        let module = parse_rust_source("fn names() -> [&'static str; 1] { [\"step\"] }")
            .expect("parse fixed string array");
        let Decl::Function { ret, body, .. } = &module.decls[0] else {
            panic!("main must be a function");
        };
        assert_eq!(*ret, Typ::Array(Box::new(Typ::String)));
        assert!(
            matches!(body.last(), Some(Stmt::Return(Some(Expr::ArrayLit(items)))) if items.len() == 1)
        );
    }

    #[test]
    fn lowers_closure_body() {
        let module = parse_rust_source("fn main() { let f = |value: i64| value + 1; }")
            .expect("parse closure");
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!("main must be a function");
        };
        assert!(
            matches!(body.first(), Some(Stmt::Let(_, _, Expr::Closure { params, body, .. })) if params == &vec![("value".to_string(), Typ::Int)] && matches!(body.last(), Some(Stmt::Return(Some(Expr::Binary { .. })))))
        );
    }

    #[test]
    fn lowers_simple_format_macro() {
        let module = parse_rust_source("fn main() -> String { format!(\"{}:{}\", \"a\", \"b\") }")
            .expect("parse format macro");
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!("main must be a function");
        };
        assert!(
            matches!(body.last(), Some(Stmt::Return(Some(Expr::Call { callee, .. }))) if matches!(callee.as_ref(), Expr::Ident(name) if name == "str-concat"))
        );
    }

    #[test]
    fn preserves_vec_struct_element_type() {
        let module = parse_rust_source(
            "struct Item { value: i64 } fn main() { let values: Vec<Item> = vec![Item { value: 1 }]; }",
        )
        .expect("parse Vec struct literal");
        let Decl::Function { body, .. } = &module.decls[1] else {
            panic!("main must be a function");
        };
        assert!(matches!(
            body.first(),
            Some(Stmt::Let(_, Some(Typ::Vector(elem)), Expr::ArrayLit(items)))
                if **elem == Typ::Named("Item".into()) && items.len() == 1
        ));
    }

    #[test]
    fn extracts_repr_c_layout_and_extern_c_symbol() {
        let src = r#"
#[repr(C)]
struct Person {
    name: InSliceU8,
    age: u32,
}

#[no_mangle]
pub extern "C" fn person_new(age: u32) -> Person {
    let p = Person { name: InSliceU8 { ptr: 0 as *const u8, len: 0 }, age };
    return p;
}

fn main() { return; }
"#;
        let artifact = parse_rust_artifact_source(src, "person").expect("parse rust artifact");
        let boundary = artifact.boundary.expect("boundary module");
        assert_eq!(boundary.module, "rust.person");
        assert_eq!(boundary.layouts.len(), 1);
        let layout = &boundary.layouts[0];
        assert_eq!(layout.name, "Person");
        assert_eq!(layout.repr, Some(BoundaryRepr::C));
        assert_eq!(layout.size, 24);
        assert_eq!(layout.align, 8);
        assert_eq!(layout.fields.len(), 2);
        assert_eq!(layout.fields[0].typ, "InSliceU8");
        assert_eq!(layout.fields[0].transfer, Some(BoundaryTransfer::Borrow));
        assert_eq!(layout.fields[1].typ, "u32");
        assert_eq!(boundary.symbols.len(), 1);
        assert_eq!(boundary.symbols[0].name, "person_new");
        assert_eq!(boundary.symbols[0].calling_convention, "c");
        assert!(!boundary.symbols[0].signature_hash.is_empty());
        assert!(!boundary.layout_hash.is_empty());
    }

    #[test]
    fn artifact_without_boundary_markers_has_no_boundary() {
        let src = r#"
struct Point { x: i64, y: i64 }
fn main() { return; }
"#;
        let artifact = parse_rust_artifact_source(src, "point").expect("parse rust artifact");
        assert!(artifact.boundary.is_none());
    }

    #[test]
    fn lowers_structured_control_flow_in_main() {
        let src = r#"
fn main() {
    let mut x: i32 = 1;
    if x > 0 { x = 2; } else { x = 3; }
    for _i in 0..2 { x = x + 1; }
    while x < 10 { x = x + 1; }
    match x { 1 => { return; }, _ => { return; } }
}
"#;
        let module = parse_rust_source(src).expect("parse rust");
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main body");
        assert!(body.iter().any(|s| matches!(s, Stmt::If { .. })));
        assert!(body.iter().any(|s| matches!(
            s,
            Stmt::Loop {
                kind: LoopKind::For { .. },
                ..
            }
        )));
        assert!(body.iter().any(|s| matches!(s, Stmt::Match { .. })));
    }

    #[test]
    fn pub_use_forwards_to_prefixed_fn() {
        let dir = std::env::temp_dir().join(format!(
            "in-pub-use-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cli.rs"), "pub fn run() -> i64 { 7 }\n").unwrap();
        std::fs::write(dir.join("lib.rs"), "mod cli;\npub use cli::run;\n").unwrap();
        let module = parse_rust_file(&dir.join("lib.rs")).expect("parse");
        let _ = std::fs::remove_dir_all(&dir);
        let run = module.decls.iter().find_map(|d| match d {
            Decl::Function { name, body, .. } if name == "run" => Some(body),
            _ => None,
        });
        let src = format!("{run:?}");
        assert!(
            src.contains("cli::run"),
            "pub use run should call cli::run: {src}"
        );
    }

    #[test]
    fn prefixes_submodule_functions() {
        let dir = std::env::temp_dir().join(format!(
            "in-mod-prefix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cli.rs"), "pub fn run() -> i64 { 7 }\n").unwrap();
        std::fs::write(
            dir.join("lib.rs"),
            "mod cli;\npub use cli::run;\nfn helper() {}\n",
        )
        .unwrap();
        let module = parse_rust_file(&dir.join("lib.rs")).expect("parse crate");
        let names: Vec<_> = module
            .decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            names
                .iter()
                .any(|n| *n == "cli::run" || n.ends_with("::run")),
            "submodule fns should be prefixed: {names:?}"
        );
    }

    #[test]
    fn lowers_expr_match_to_stmt_match() {
        let module = parse_rust_source(
            r#"
fn main() -> i64 {
    let x = 1;
    let y = match x { 1 => 2, _ => 3 };
    y
}
"#,
        )
        .expect("parse match expr");
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main");
        let src = format!("{body:?}");
        assert!(
            !src.contains("match-expr"),
            "expr match must not be a fake callee: {src}"
        );
        assert!(src.contains("Match"), "expected Stmt::Match: {src}");
    }

    #[test]
    fn lowers_break_in_loop() {
        let module = parse_rust_source(
            r#"
fn main() {
    loop { break; }
}
"#,
        )
        .expect("parse break");
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main");
        let src = format!("{body:?}");
        assert!(src.contains("Break"), "expected Stmt::Break: {src}");
    }

    #[test]
    fn lowers_await_and_const() {
        let module = parse_rust_source(
            r#"
const N: i64 = 4;
async fn go() -> i64 { 1.await }
fn main() -> i64 { N }
"#,
        )
        .expect("parse await/const");
        assert!(
            module
                .decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "N"))
        );
        assert!(
            module
                .decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "go"))
        );
    }

    #[test]
    fn lowers_collect_as_identity() {
        let module = parse_rust_source(
            r#"
fn main() {
    let args: Vec<String> = std::env::args().collect();
}
"#,
        )
        .expect("parse collect");
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main");
        let src = format!("{body:?}");
        assert!(
            !src.contains("Ident(\"collect\")"),
            "collect should not remain a call: {src}"
        );
    }

    #[test]
    fn lowers_println_to_print() {
        let module = parse_rust_source(r#"fn main() { println!("tk"); }"#).expect("parse println");
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main");
        let src = format!("{body:?}");
        assert!(
            src.contains("print"),
            "println should lower to print: {src}"
        );
    }

    #[test]
    fn infers_vec_new_and_qualifies_extend() {
        let src = r#"
fn produced() -> Vec<i64> { Vec::new() }

fn main() {
    let mut values = Vec::new();
    values.extend(produced());
}
"#;
        let module = parse_rust_source(src).expect("parse rust");
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main body");
        assert!(matches!(
            body.first(),
            Some(Stmt::Let(name, Some(Typ::Vector(_)), _)) if name == "values"
        ));
        assert!(matches!(
            body.get(1),
            Some(Stmt::Return(Some(Expr::Call { callee, args })))
                if matches!(callee.as_ref(), Expr::Ident(name) if name == "Vec::extend")
                    && matches!(args.as_slice(), [Expr::Ident(receiver), Expr::Call { callee, .. }]
                        if receiver == "values"
                            && matches!(callee.as_ref(), Expr::Ident(name) if name == "produced"))
        ));
    }

    #[test]
    fn lowers_trait_alias_and_grouped_expr() {
        let module = parse_rust_source(
            r#"
pub trait Sharable = Clone + Send;
fn main() -> i64 { (1 + 2) }
"#,
        )
        .expect("parse trait alias");
        assert!(
            module
                .decls
                .iter()
                .any(|d| matches!(d, Decl::Struct { name, .. } if name == "Sharable"))
        );
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main");
        let src = format!("{body:?}");
        assert!(src.contains("Binary") || src.contains("IntLit"), "{src}");
    }

    #[test]
    fn lowers_for_range_to_for_loop_kind() {
        let module = parse_rust_source(
            r#"
fn main() {
    for i in 0..3 { let _ = i; }
}
"#,
        )
        .expect("parse for");
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main");
        assert!(
            body.iter().any(|s| matches!(
                s,
                Stmt::Loop {
                    kind: LoopKind::For { .. },
                    ..
                }
            )),
            "expected For loop: {body:?}"
        );
    }

    #[test]
    fn lowers_add_assign() {
        let module =
            parse_rust_source(r#"fn main() { let mut acc = 0; acc += 1; }"#).expect("parse +=");
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main");
        let src = format!("{body:?}");
        assert!(src.contains("Assign"), "expected Assign from +=: {src}");
        assert!(src.contains('+') || src.contains("\"+\""), "{src}");
    }
}
