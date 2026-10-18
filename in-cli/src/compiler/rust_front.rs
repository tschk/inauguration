//! Rust front powered by `syn` AST parsing.
//!
//! This is the first non-`.in` front that lowers real statement bodies (subset) into Core IR.

use crate::boundary_ir::{
    BoundaryField, BoundaryLayout, BoundaryModule, BoundaryOwnership, BoundaryRepr, BoundarySymbol,
    BoundaryTransfer, CompileArtifact, IN_ABI_VERSION,
};
use crate::boundary_verify::boundary_ir_verify;
use crate::core_ir::{CatchArm, Expr, LoopKind, MatchArm, MatchValueArm, Stmt, Typ};
use crate::core_ir::{Decl, UnifiedModule};
use quote::ToTokens;
use std::collections::{HashMap, HashSet};
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use syn::parse::Parser;

mod function_access;
mod imports;
mod type_aliases;
mod unary_methods;

type RustLayoutSpecs = HashMap<String, (BoundaryRepr, Vec<(String, syn::Type)>)>;

pub fn parse_rust_file(path: &Path) -> Result<UnifiedModule, String> {
    parse_rust_artifact(path).map(CompileArtifact::into_semantic)
}

pub fn parse_rust_artifact(path: &Path) -> Result<CompileArtifact, String> {
    parse_rust_path_artifact(path, true)
}

/// Parse a lazily discovered module without claiming that its source file owns `crate::`.
pub(crate) fn parse_rust_module_artifact(path: &Path) -> Result<CompileArtifact, String> {
    parse_rust_path_artifact(path, false)
}

fn parse_rust_path_artifact(path: &Path, owns_crate_root: bool) -> Result<CompileArtifact, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let module_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rust")
        .to_string();
    parse_rust_source_unit(
        &src,
        &module_id,
        path.parent().unwrap_or(Path::new(".")),
        owns_crate_root,
    )
}

pub fn parse_rust_source(src: &str) -> Result<UnifiedModule, String> {
    parse_rust_artifact_source(src, "rust").map(CompileArtifact::into_semantic)
}

pub fn parse_rust_artifact_source(src: &str, module_id: &str) -> Result<CompileArtifact, String> {
    parse_rust_artifact_source_with_dir(src, module_id, Path::new("."))
}

pub fn parse_rust_artifact_source_with_dir(
    src: &str,
    module_id: &str,
    base_dir: &Path,
) -> Result<CompileArtifact, String> {
    parse_rust_source_unit(src, module_id, base_dir, true)
}

fn parse_rust_source_unit(
    src: &str,
    module_id: &str,
    base_dir: &Path,
    owns_crate_root: bool,
) -> Result<CompileArtifact, String> {
    let file = syn::parse_file(src).map_err(|e| format!("rust parse failed: {e}"))?;
    let catalog = if owns_crate_root {
        type_aliases::Catalog::new(&file)
    } else {
        type_aliases::Catalog::module_source(&file)
    };
    let failures = type_aliases::validate_declarations(&file, catalog.clone());
    let frame = type_aliases::Frame::root(catalog);
    let _aliases = frame.enter();
    let mut artifact = lower_file_items_at(&file, base_dir, owns_crate_root)?;
    retain_alias_declaration_failures(&mut artifact, failures);
    artifact.boundary = extract_boundary_module(&file, module_id);
    Ok(artifact)
}

fn retain_alias_declaration_failures(
    artifact: &mut CompileArtifact,
    failures: Vec<crate::core_ir::type_proofs::ResolutionFailure>,
) {
    let failure = failures
        .into_iter()
        .filter(crate::core_ir::type_proofs::ResolutionFailure::is_invalid)
        .fold(None, |previous, failure| {
            crate::core_ir::type_proofs::prefer_failure(previous, Some(failure))
        });
    if let Some(failure) = failure {
        // This private typed site is rejected before entry pruning or executable emission.
        artifact.semantic.decls.push(Decl::Global {
            name: "#rust-source-alias-validation".into(),
            typ: failure.typ(),
            init: None,
            mutable: false,
            constant_value: false,
        });
    }
}

fn lower_file_items_at(
    file: &syn::File,
    base_dir: &Path,
    owns_crate_root: bool,
) -> Result<CompileArtifact, String> {
    let LoweredRustArtifact {
        mut artifact,
        access,
    } = lower_file_items_at_visited(file, base_dir, &mut HashSet::new(), 0)?;
    let module = &mut artifact.semantic;
    let references = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, .. } => Some([
                (name.clone(), resolved_global(name)),
                (format!("crate::{name}"), resolved_global(name)),
                (format!("self::{name}"), resolved_global(name)),
                (resolved_global(name), resolved_global(name)),
            ]),
            _ => None,
        })
        .flatten()
        .collect();
    rewrite_global_references(&mut module.decls, &references, file);
    let references = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, .. } => Some((resolved_global(name), name.clone())),
            _ => None,
        })
        .collect();
    rewrite_global_references(&mut module.decls, &references, file);
    let function_names = crate::core_bindings::module_names(&module.decls);
    for declaration in &mut module.decls {
        let caller = match declaration {
            Decl::Function { name, .. } | Decl::Global { name, .. } => name.clone(),
            _ => String::new(),
        };
        let mut referenced = HashSet::new();
        match declaration {
            Decl::Function { body, .. } => {
                crate::core_bindings::reserve_body(body, &mut referenced)
            }
            Decl::Global {
                init: Some(init), ..
            } => crate::core_bindings::reserve_expr(init, &mut referenced),
            _ => {}
        }
        let failures = referenced
            .iter()
            .filter_map(|target| {
                access
                    .validate(target, &caller)
                    .map(|failure| (target.clone(), failure.symbol()))
            })
            .collect::<HashMap<_, _>>();
        if !failures.is_empty() {
            match declaration {
                Decl::Function { params, body, .. } => {
                    crate::core_bindings::normalize_function_with_references(
                        params,
                        body,
                        &function_names,
                        &failures,
                    );
                }
                Decl::Global {
                    init: Some(init), ..
                } => {
                    crate::core_bindings::normalize_initializer_with_references(
                        init,
                        &function_names,
                        &failures,
                    );
                }
                _ => {}
            }
        }
        crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(declaration), &mut |expr| {
            if let Expr::Ident(name) = expr {
                let target = name.strip_prefix("crate::").unwrap_or(name);
                if name.contains("::")
                    && (owns_crate_root || !name.starts_with("crate::"))
                    && let Some(failure) = access.validate(target, &caller)
                {
                    *name = failure.symbol();
                }
            }
            if let Expr::Ident(name) = expr
                && let Some(canonical) = name.strip_prefix("crate::")
                && owns_crate_root
                && access.contains_function(canonical)
            {
                *name = canonical.to_owned();
            }
            if let Expr::Call { callee, .. } = expr
                && let Expr::Ident(name) = callee.as_ref()
                && name.rsplit_once("::").is_some_and(|(owner, member)| {
                    crate::core_ir::rust_integer_type_name(owner).is_some()
                        && matches!(member, "MIN" | "MAX" | "BITS")
                })
            {
                **callee = Expr::Ident(
                    crate::core_ir::type_proofs::ResolutionFailure::Invalid(format!(
                        "integer constant `{name}` is not callable"
                    ))
                    .symbol(),
                );
            }
        });
    }
    register_explicit_vector_constructors(module);
    refuse_static_array_value_uses(module);
    crate::typecheck::rust_integers::mark_declaration_origins(&mut module.decls);
    crate::typecheck::TypeChecker::new().resolve_unary_methods(module, &artifact.unary_methods)?;
    Ok(artifact)
}

fn register_explicit_vector_constructors(module: &mut UnifiedModule) {
    // Scoped aliases were normalized at source lowering; do not reinterpret generic leftovers.
    let _aliases = type_aliases::suspend();
    let names = crate::core_bindings::module_names(&module.decls);
    let shadowed = module.decls.iter().any(|decl| {
        matches!(decl, Decl::Struct {name, ..} | Decl::Class {name, ..}
            if name == "Vec" || name.ends_with("::Vec"))
    });
    let mut constructors = std::collections::BTreeMap::new();
    crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
        let Expr::Call { callee, args } = expr else {
            return;
        };
        let Expr::Ident(name) = callee.as_ref() else {
            return;
        };
        if names.contains(name) {
            return;
        }
        if matches!(
            name.as_str(),
            "Vec::with_capacity"
                | "std::vec::Vec::with_capacity"
                | "alloc::vec::Vec::with_capacity"
        ) {
            if args.len() == 1 && !(name == "Vec::with_capacity" && shadowed) {
                **callee = Expr::Ident(crate::core_ir::VECTOR_ALLOCATION.into());
            }
            return;
        }
        if !name.contains("::<") {
            return;
        }
        let Ok(path) = syn::parse_str::<syn::ExprPath>(name) else {
            return;
        };
        if path.qself.is_some() {
            return;
        }
        let segments: Vec<_> = path.path.segments.iter().collect();
        let owner = segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        if !matches!(
            owner.as_str(),
            "Vec::new"
                | "std::vec::Vec::new"
                | "alloc::vec::Vec::new"
                | "Vec::with_capacity"
                | "std::vec::Vec::with_capacity"
                | "alloc::vec::Vec::with_capacity"
        ) || args.len() != usize::from(owner.ends_with("::with_capacity"))
            || names.contains(&owner)
            || (owner.starts_with("Vec::") && shadowed)
            || !matches!(segments.last().unwrap().arguments, syn::PathArguments::None)
            || segments[..segments.len() - 2]
                .iter()
                .any(|s| !matches!(s.arguments, syn::PathArguments::None))
        {
            return;
        }
        let syn::PathArguments::AngleBracketed(generics) = &segments[segments.len() - 2].arguments
        else {
            return;
        };
        if generics.args.len() != 1 {
            return;
        }
        let Some(syn::GenericArgument::Type(item)) = generics.args.first() else {
            return;
        };
        if matches!(item, syn::Type::Infer(_)) {
            **callee = Expr::Ident(if owner.ends_with("::with_capacity") {
                crate::core_ir::VECTOR_ALLOCATION.into()
            } else {
                owner
            });
            return;
        }
        // References need an ownership model; do not erase them into scalar elements.
        let syn::Type::Path(item_path) = item else {
            return;
        };
        if item_path.qself.is_some()
            || item_path.path.segments.len() != 1
            || !matches!(
                item_path.path.segments[0].arguments,
                syn::PathArguments::None
            )
        {
            return;
        }
        let typ = map_type(item);
        if matches!(typ.canonical(), Typ::Int | Typ::Float | Typ::Bool) {
            constructors.insert(
                name.clone(),
                (
                    Typ::Vector(Box::new(typ)),
                    owner.ends_with("::with_capacity"),
                ),
            );
        }
    });
    module
        .decls
        .extend(constructors.into_iter().map(|(name, (ret, reserved))| {
            let params = if reserved {
                vec![("capacity".into(), Typ::Int)]
            } else {
                vec![]
            };
            let value = if reserved {
                Expr::Call {
                    callee: Box::new(Expr::Ident(
                        ret.vector_allocation_intrinsic().unwrap().into(),
                    )),
                    args: vec![Expr::Ident("capacity".into())],
                }
            } else {
                Expr::ArrayLit(vec![])
            };
            Decl::Function {
                name,
                params,
                ret,
                body: vec![Stmt::Return(Some(value))],
                type_params: vec![],
            }
        }));
}

fn refuse_static_array_value_uses(module: &mut UnifiedModule) {
    let arrays: HashMap<_, _> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global {
                name,
                typ,
                mutable,
                constant_value: false,
                ..
            } if matches!(typ.canonical(), Typ::Array(_)) => {
                Some((name.clone(), (*mutable, typ.canonical())))
            }
            _ => None,
        })
        .collect();
    if arrays.is_empty() {
        return;
    }
    let functions: HashSet<_> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    fn static_index_type<'a>(
        expr: &Expr,
        arrays: &'a HashMap<String, (bool, Typ)>,
    ) -> Option<&'a Typ> {
        match expr {
            Expr::Ident(name) => arrays.get(name).map(|(_, typ)| typ),
            Expr::Index { base, .. } => match static_index_type(base, arrays)? {
                Typ::Array(elem) => Some(elem),
                _ => None,
            },
            _ => None,
        }
    }
    fn expr_uses_value(
        expr: &Expr,
        arrays: &HashMap<String, (bool, Typ)>,
        functions: &HashSet<String>,
        borrow: bool,
    ) -> bool {
        match expr {
            Expr::Ident(name) => !borrow && arrays.contains_key(name),
            Expr::Index { base, index } => {
                (!borrow && matches!(static_index_type(expr, arrays), Some(Typ::Array(_))))
                    || expr_uses_value(base, arrays, functions, true)
                    || expr_uses_value(index, arrays, functions, false)
            }
            Expr::Call { callee, args } => {
                let name = match callee.as_ref() {
                    Expr::Ident(name) => name.as_str(),
                    _ => "",
                };
                let read = !functions.contains(name)
                    && !functions.contains(name.rsplit("::").next().unwrap_or(name))
                    && (matches!(
                        name,
                        "len"
                            | "array-len"
                            | "array_len"
                            | "is_empty"
                            | "__method__len"
                            | "__method__is_empty"
                            | "#receiver-method:len"
                            | "#receiver-method:is_empty"
                    ) || name.ends_with("::is_empty"));
                let store = matches!(
                    name,
                    crate::core_ir::INDEX_ASSIGN_VALUE_FIRST
                        | crate::core_ir::INDEX_MODIFY_VALUE_FIRST
                        | crate::core_ir::INDEX_MODIFY_PLACE_FIRST
                );
                if store
                    && matches!(args.first(), Some(Expr::Ident(name)) if arrays.get(name).is_some_and(|(mutable, _)| !mutable))
                {
                    return true;
                }
                args.iter().enumerate().any(|(index, arg)| {
                    expr_uses_value(arg, arrays, functions, index == 0 && (read || store))
                })
            }
            Expr::Block { body, result } => {
                body_uses_value(body, arrays, functions)
                    || result
                        .as_ref()
                        .is_some_and(|value| expr_uses_value(value, arrays, functions, borrow))
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => {
                expr_uses_value(cond, arrays, functions, false)
                    || expr_uses_value(then_expr, arrays, functions, borrow)
                    || expr_uses_value(else_expr, arrays, functions, borrow)
            }
            Expr::Closure { body, .. } => body_uses_value(body, arrays, functions),
            _ => {
                let mut value = false;
                crate::core_ir::for_each_expr_child(expr, &mut |child| {
                    value |= expr_uses_value(child, arrays, functions, false)
                });
                value
            }
        }
    }
    fn body_uses_value(
        body: &[Stmt],
        arrays: &HashMap<String, (bool, Typ)>,
        functions: &HashSet<String>,
    ) -> bool {
        body.iter().any(|stmt| match stmt {
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::Return(Some(expr))
            | Stmt::Throw(expr)
            | Stmt::Expr(expr) => expr_uses_value(expr, arrays, functions, false),
            Stmt::IndexAssign { base, index, value } => {
                expr_uses_value(base, arrays, functions, true)
                    || expr_uses_value(index, arrays, functions, false)
                    || expr_uses_value(value, arrays, functions, false)
            }
            Stmt::FieldAssign { base, value, .. } => {
                expr_uses_value(base, arrays, functions, false)
                    || expr_uses_value(value, arrays, functions, false)
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                expr_uses_value(cond, arrays, functions, false)
                    || body_uses_value(then_body, arrays, functions)
                    || body_uses_value(else_body, arrays, functions)
            }
            Stmt::Loop { cond, body, .. } => {
                cond.as_ref()
                    .is_some_and(|cond| expr_uses_value(cond, arrays, functions, false))
                    || body_uses_value(body, arrays, functions)
            }
            Stmt::Match { scrutinee, arms } => {
                expr_uses_value(scrutinee, arrays, functions, false)
                    || arms.iter().any(|arm| {
                        arm.guard
                            .as_ref()
                            .is_some_and(|guard| expr_uses_value(guard, arrays, functions, false))
                            || body_uses_value(&arm.body, arrays, functions)
                    })
            }
            Stmt::Try { body, catches } => {
                body_uses_value(body, arrays, functions)
                    || catches
                        .iter()
                        .any(|catch| body_uses_value(&catch.body, arrays, functions))
            }
            _ => false,
        })
    }
    for decl in &mut module.decls {
        if let Decl::Function { body, .. } = decl
            && body_uses_value(body, &arrays, &functions)
        {
            // shortcut: preserve refusal until Rust array copy and borrow types survive lowering.
            *body = vec![Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident("__unsupported_rust_static_array_value".into())),
                args: vec![],
            })];
        }
    }
}

#[path = "rust_front/assignment.rs"]
mod assignment;
#[path = "rust_front/atomics.rs"]
mod atomics;

// Primitive-shadow resolution needs a type namespace. Refuse these ambiguous
// declarations explicitly until the Rust front carries that namespace.
fn reject_integer_primitive_shadowing(items: &[syn::Item]) -> Result<(), String> {
    let check_name = |name: &syn::Ident| -> Result<(), String> {
        let name = name.to_string();
        if matches!(
            name.as_str(),
            "i8" | "u8"
                | "i16"
                | "u16"
                | "i32"
                | "u32"
                | "i64"
                | "u64"
                | "isize"
                | "usize"
                | "i128"
                | "u128"
        ) {
            return Err(format!(
                "rust front: shadowed primitive integer type `{name}` requires type namespace resolution"
            ));
        }
        Ok(())
    };
    let check_generics = |generics: &syn::Generics| -> Result<(), String> {
        for parameter in &generics.params {
            if let syn::GenericParam::Type(parameter) = parameter {
                check_name(&parameter.ident)?;
            }
        }
        Ok(())
    };
    fn nested_blocks(mut cursor: syn::buffer::Cursor<'_>) -> Result<(), String> {
        while !cursor.eof() {
            if let Some((inside, _, _, after)) = cursor.any_group() {
                let tokens = inside.token_stream();
                if let Ok(block) = syn::parse2::<syn::Block>(quote::quote!({ #tokens })) {
                    for statement in &block.stmts {
                        if let syn::Stmt::Item(item) = statement {
                            reject_integer_primitive_shadowing(std::slice::from_ref(item))?;
                        }
                    }
                }
                nested_blocks(inside)?;
                cursor = after;
            } else if let Some((_, after)) = cursor.token_tree() {
                cursor = after;
            } else {
                break;
            }
        }
        Ok(())
    }
    for item in items {
        if type_aliases::item_presence(item) == type_aliases::Presence::Inactive {
            continue;
        }
        match item {
            syn::Item::ExternCrate(value) => {
                let alias = value
                    .rename
                    .as_ref()
                    .map(|(_, name)| name)
                    .unwrap_or(&value.ident);
                check_name(alias)?;
                // shortcut: external aliases need crate identity resolution before primitive folding.
                if (alias == "std" || alias == "core")
                    && value.ident != "std"
                    && value.ident != "core"
                {
                    return Err(format!(
                        "rust front: external primitive root `{alias}` requires crate identity resolution"
                    ));
                }
            }
            syn::Item::Use(value) => {
                fn imported(tree: &syn::UseTree, prefix: &str) -> Vec<(String, String)> {
                    match tree {
                        syn::UseTree::Path(path) => {
                            imported(&path.tree, &format!("{prefix}{}::", path.ident))
                        }
                        syn::UseTree::Name(name) => {
                            vec![(name.ident.to_string(), format!("{prefix}{}", name.ident))]
                        }
                        syn::UseTree::Rename(name) => {
                            vec![(name.rename.to_string(), format!("{prefix}{}", name.ident))]
                        }
                        syn::UseTree::Group(group) => group
                            .items
                            .iter()
                            .flat_map(|tree| imported(tree, prefix))
                            .collect(),
                        // Glob namespaces require resolution outside this bounded guard.
                        syn::UseTree::Glob(_) => vec![],
                    }
                }
                for (binding, path) in imported(&value.tree, "") {
                    if let Err(error) = check_name(&syn::Ident::new(&binding, value.use_token.span))
                    {
                        let primitive = path
                            .strip_prefix("core::primitive::")
                            .or_else(|| path.strip_prefix("std::primitive::"));
                        if primitive != Some(binding.as_str()) {
                            return Err(error);
                        }
                    }
                }
            }
            syn::Item::Struct(value) => {
                check_name(&value.ident)?;
                check_generics(&value.generics)?;
            }
            syn::Item::Enum(value) => {
                check_name(&value.ident)?;
                check_generics(&value.generics)?;
            }
            syn::Item::Union(value) => {
                check_name(&value.ident)?;
                check_generics(&value.generics)?;
            }
            syn::Item::Type(value) => {
                check_name(&value.ident)?;
                check_generics(&value.generics)?;
            }
            syn::Item::Fn(value) => {
                check_generics(&value.sig.generics)?;
                for statement in &value.block.stmts {
                    if let syn::Stmt::Item(item) = statement {
                        reject_integer_primitive_shadowing(std::slice::from_ref(item))?;
                    }
                }
            }
            syn::Item::Mod(value) => {
                if let Some((_, items)) = &value.content {
                    reject_integer_primitive_shadowing(items)?;
                }
            }
            syn::Item::Impl(value) => {
                check_generics(&value.generics)?;
                for item in &value.items {
                    match item {
                        syn::ImplItem::Fn(value) => check_generics(&value.sig.generics)?,
                        syn::ImplItem::Type(value) => check_name(&value.ident)?,
                        _ => {}
                    }
                }
            }
            syn::Item::Trait(value) => {
                check_name(&value.ident)?;
                check_generics(&value.generics)?;
                for item in &value.items {
                    match item {
                        syn::TraitItem::Fn(value) => check_generics(&value.sig.generics)?,
                        syn::TraitItem::Type(value) => check_name(&value.ident)?,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let tokens =
        syn::buffer::TokenBuffer::new2(items.iter().map(ToTokens::to_token_stream).collect());
    nested_blocks(tokens.begin())?;
    Ok(())
}

struct LoweredRustArtifact {
    artifact: CompileArtifact,
    access: function_access::FunctionAccessFacts,
}

fn lower_file_items_at_visited(
    file: &syn::File,
    base_dir: &Path,
    visited: &mut HashSet<PathBuf>,
    depth: u32,
) -> Result<LoweredRustArtifact, String> {
    if depth > 8 {
        return Err("rust front: module nest limit".into());
    }
    let _primitive_roots =
        PrimitiveRootScope::new(primitive_root_bindings(file.items.iter()), false);
    reject_integer_primitive_shadowing(&file.items)?;
    let mut decls = Vec::new();
    let mut access = function_access::FunctionAccessFacts::collect(file);
    let mut unary_methods = unary_methods::collect(file);
    let mut external_modules: Vec<(String, type_aliases::Presence)> = Vec::new();
    let mut uses: Vec<syn::ItemUse> = Vec::new();

    for item in &file.items {
        if type_aliases::item_presence(item) == type_aliases::Presence::Inactive {
            continue;
        }
        let generics = match item {
            syn::Item::Struct(item) => Some(&item.generics),
            syn::Item::Enum(item) => Some(&item.generics),
            syn::Item::Union(item) => Some(&item.generics),
            syn::Item::Trait(item) => Some(&item.generics),
            syn::Item::TraitAlias(item) => Some(&item.generics),
            _ => None,
        };
        let _type_parameters = generics.and_then(type_aliases::generic_scope);
        match item {
            syn::Item::Struct(s) => {
                let mut declaration = Decl::Struct {
                    name: s.ident.to_string(),
                    fields: rust_struct_fields(&s.fields),
                    type_params: vec![],
                };
                retain_generic_scope(&mut declaration, declared_type_parameters(&s.generics));
                decls.push(declaration);
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
                if let Some(mut d) = lower_const_item(c) {
                    if c.ident == "_" {
                        if let Decl::Global { name, .. } = &mut d {
                            *name = format!("$anonymous_const${}", decls.len());
                        }
                    }
                    decls.push(d);
                }
            }
            syn::Item::Static(s) => {
                if let Some(d) = lower_static_item(s) {
                    decls.push(d);
                }
            }
            syn::Item::Type(t) => {
                let ident = &t.ident;
                let path: syn::Path = syn::parse_quote!(#ident);
                // Only proven primitive aliases disappear; nominal declarations keep existing layout ownership.
                if matches!(type_aliases::resolve(&path), Ok(Some(Typ::Named(name))) if crate::core_ir::rust_integer_type_name(&name).is_some())
                {
                    continue;
                }
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
                let _primitive_roots =
                    PrimitiveRootScope::new(primitive_root_generics(&tr.generics), true);
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
                        let _aliases = type_aliases::current()
                            .map(|frame| frame.module(&m.ident.to_string()).enter());
                        let inner =
                            lower_file_items_at_visited(&inner_file, base_dir, visited, depth + 1)?;
                        access.merge_prefixed(&m.ident.to_string(), inner.access);
                        let inner = unary_methods::prefix(
                            inner.artifact,
                            &m.ident.to_string(),
                            &inner_file,
                        )?;
                        crate::boundary_ir::merge_unary_method_bindings(
                            &mut unary_methods,
                            inner.unary_methods,
                        )?;
                        decls.extend(inner.semantic.decls);
                    }
                } else {
                    // External module: record candidate path for parallel parsing
                    let presence =
                        if type_aliases::current().is_some_and(|frame| !frame.source_is_active()) {
                            type_aliases::Presence::Conditional
                        } else {
                            type_aliases::item_presence(item)
                        };
                    external_modules.push((m.ident.to_string(), presence));
                }
            }
            _ => {}
        }
    }

    // Missing optional discovery remains optional; a loaded malformed source
    // must not discard authoritative operator proof or its refusal reason.
    if external_modules.len() > 1 {
        std::thread::scope(|scope| -> Result<(), String> {
            let handles: Vec<_> = external_modules
                .into_iter()
                .map(|(name, presence)| {
                    let mut ancestors = visited.clone();
                    scope.spawn(move || {
                        load_external_module(base_dir, &name, &mut ancestors, depth + 1, presence)
                    })
                })
                .collect();
            for handle in handles {
                let parsed = handle
                    .join()
                    .map_err(|_| "Rust external module parser panicked".to_string())??;
                if let Some(inner) = parsed {
                    access.merge(inner.access);
                    let inner = inner.artifact;
                    crate::boundary_ir::merge_unary_method_bindings(
                        &mut unary_methods,
                        inner.unary_methods,
                    )?;
                    decls.extend(inner.semantic.decls);
                }
            }
            Ok(())
        })?;
    } else if let Some((name, presence)) = external_modules.first()
        && let Some(inner) = load_external_module(base_dir, name, visited, depth + 1, *presence)?
    {
        access.merge(inner.access);
        let inner = inner.artifact;
        crate::boundary_ir::merge_unary_method_bindings(&mut unary_methods, inner.unary_methods)?;
        decls.extend(inner.semantic.decls);
    }

    for u in &uses {
        apply_use_tree(&mut decls, &u.tree, "", file, depth == 0);
    }
    if decls.is_empty() && depth == 0 {
        return Err("rust front parsed file but found no top-level structs/functions".to_string());
    }
    atomics::normalize(&mut decls, file);
    let mut artifact = CompileArtifact::from_semantic(UnifiedModule::new(decls));
    artifact.unary_methods = unary_methods;
    Ok(LoweredRustArtifact { artifact, access })
}

fn load_external_module(
    base_dir: &Path,
    name: &str,
    visited: &mut HashSet<PathBuf>,
    depth: u32,
    presence: type_aliases::Presence,
) -> Result<Option<LoweredRustArtifact>, String> {
    let Some(path) = [
        base_dir.join(format!("{name}.rs")),
        base_dir.join(name).join("mod.rs"),
    ]
    .into_iter()
    .find(|path| path.exists()) else {
        return Ok(None);
    };
    let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
    if !visited.insert(canonical) {
        return Ok(None);
    }
    let source = std::fs::read_to_string(&path)
        .map_err(|error| format!("read Rust module {}: {error}", path.display()))?;
    let file = syn::parse_file(&source)
        .map_err(|error| format!("parse Rust module {}: {error}", path.display()))?;
    let catalog = type_aliases::Catalog::module_source_with_presence(&file, presence);
    let failures = type_aliases::validate_declarations(&file, catalog.clone());
    let frame = type_aliases::Frame::root(catalog);
    let _aliases = frame.enter();
    let mut artifact =
        lower_file_items_at_visited(&file, path.parent().unwrap_or(base_dir), visited, depth)?;
    retain_alias_declaration_failures(&mut artifact.artifact, failures);
    let mut access = function_access::FunctionAccessFacts::default();
    access.merge_prefixed(name, artifact.access);
    Ok(Some(LoweredRustArtifact {
        artifact: unary_methods::prefix(artifact.artifact, name, &file)?,
        access,
    }))
}

fn prefix_module_decls(mut decls: Vec<Decl>, module: &str, file: &syn::File) -> Vec<Decl> {
    if module.is_empty() {
        return decls;
    }
    let globals: HashMap<_, _> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, .. } => Some([
                (name.clone(), resolved_global(&format!("{module}::{name}"))),
                (
                    format!("self::{name}"),
                    resolved_global(&format!("{module}::{name}")),
                ),
                (
                    resolved_global(name),
                    resolved_global(&format!("{module}::{name}")),
                ),
            ]),
            _ => None,
        })
        .flatten()
        .collect();
    rewrite_global_references(&mut decls, &globals, file);
    // Prefix old lexical names bottom-up; crate paths stay deferred until the root.
    let nominal_names: HashMap<_, _> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Struct { name, .. } | Decl::Class { name, .. } | Decl::Interface { name, .. } => {
                Some((name.clone(), format!("{module}::{name}")))
            }
            _ => None,
        })
        .collect();
    let associated_names: HashMap<_, _> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } if name.contains("::") => {
                Some((name.clone(), format!("{module}::{name}")))
            }
            _ => None,
        })
        .collect();
    for decl in &mut decls {
        let associated = matches!(decl, Decl::Function { .. });
        let name = match decl {
            Decl::Function { name, .. } | Decl::Struct { name, .. } | Decl::Global { name, .. } => {
                name.as_str()
            }
            _ => continue,
        };
        let mut scope = name.rsplit_once("::").map(|(scope, _)| scope).unwrap_or("");
        if associated {
            let mut owner = scope;
            loop {
                if nominal_names.contains_key(owner) {
                    scope = owner
                        .rsplit_once("::")
                        .map(|(scope, _)| scope)
                        .unwrap_or("");
                    break;
                }
                let Some((parent, _)) = owner.rsplit_once("::") else {
                    break;
                };
                owner = parent;
            }
        }
        let scope = scope.to_string();
        let resolve = |name: &str, mapping: &HashMap<String, String>| {
            let local = if scope.is_empty() {
                name.to_string()
            } else {
                format!("{scope}::{name}")
            };
            mapping
                .get(&local)
                .or_else(|| {
                    (scope.is_empty() || name.starts_with(&format!("{scope}::")))
                        .then(|| mapping.get(name))
                        .flatten()
                })
                .cloned()
        };
        crate::core_bindings::rewrite_nominal_names(
            std::slice::from_mut(decl),
            &|name| resolve(name, &nominal_names),
            &|name| resolve(name, &associated_names),
        );
    }
    for decl in &mut decls {
        match decl {
            Decl::Function { name, .. } | Decl::Struct { name, .. } | Decl::Global { name, .. } => {
                *name = format!("{module}::{name}");
            }
            _ => {}
        }
    }
    decls
}

// Mark resolved names until every enclosing module has applied its prefix.
fn resolved_global(name: &str) -> String {
    format!("$global${name}")
}

fn rewrite_global_references(
    decls: &mut [Decl],
    references: &HashMap<String, String>,
    file: &syn::File,
) {
    if references.is_empty() {
        return;
    }
    let child_modules: HashSet<_> = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Mod(module) => Some(module.ident.to_string()),
            _ => None,
        })
        .collect();
    let nested_references = references
        .iter()
        .map(|(name, target)| {
            let target = if name.starts_with("$global$") || name.starts_with("crate::") {
                target.clone()
            } else {
                format!("$unresolved_global${name}")
            };
            (name.clone(), target)
        })
        .collect();
    let mut names = crate::core_bindings::module_names(decls);
    names.extend(references.keys().cloned());
    for decl in decls.iter_mut() {
        let nested = match decl {
            Decl::Function { name, .. } | Decl::Global { name, .. } => name
                .split("::")
                .next()
                .is_some_and(|prefix| child_modules.contains(prefix)),
            _ => false,
        };
        let references = if nested {
            &nested_references
        } else {
            references
        };
        if let Decl::Function { params, body, .. } = decl {
            crate::core_bindings::normalize_function_with_references(
                params, body, &names, references,
            );
        }
        if let Decl::Global {
            init: Some(init), ..
        } = decl
        {
            crate::core_bindings::normalize_initializer_with_references(init, &names, references);
        }
        crate::core_opt::rewrite_constant_references(std::slice::from_mut(decl), references);
    }
}

fn lower_initializer(expr: &syn::Expr, typ: &Typ, locals: &mut HashMap<String, String>) -> Expr {
    if let syn::Expr::Block(block) = expr {
        return Expr::Call {
            callee: Box::new(Expr::Closure {
                params: vec![],
                ret: typ.clone(),
                body: lower_block_inner_with_types(&block.block, true, locals),
                captures: vec![],
            }),
            args: vec![],
        };
    }
    lower_expr_with_types(expr, locals)
}

fn lower_const_item(c: &syn::ItemConst) -> Option<Decl> {
    let mut locals = HashMap::new();
    Some(Decl::Global {
        name: c.ident.to_string(),
        typ: map_type(&c.ty),
        init: Some(Box::new(lower_initializer(
            &c.expr,
            &map_type(&c.ty),
            &mut locals,
        ))),
        mutable: false,
        constant_value: true,
    })
}

fn lower_static_item(s: &syn::ItemStatic) -> Option<Decl> {
    let mut locals = HashMap::new();
    Some(Decl::Global {
        name: s.ident.to_string(),
        typ: map_type(&s.ty),
        init: Some(Box::new(lower_initializer(
            &s.expr,
            &map_type(&s.ty),
            &mut locals,
        ))),
        mutable: matches!(s.mutability, syn::StaticMutability::Mut(_)),
        constant_value: false,
    })
}

fn apply_use_tree(
    decls: &mut Vec<Decl>,
    tree: &syn::UseTree,
    prefix: &str,
    file: &syn::File,
    root: bool,
) {
    match tree {
        syn::UseTree::Path(p) => {
            let next = if prefix.is_empty() {
                p.ident.to_string()
            } else {
                format!("{prefix}::{}", p.ident)
            };
            apply_use_tree(decls, &p.tree, &next, file, root);
        }
        syn::UseTree::Name(n) => {
            let full = if prefix.is_empty() {
                n.ident.to_string()
            } else {
                format!("{prefix}::{}", n.ident)
            };
            alias_fn(decls, &full, &n.ident.to_string(), file, root);
        }
        syn::UseTree::Rename(r) => {
            let full = if prefix.is_empty() {
                r.ident.to_string()
            } else {
                format!("{prefix}::{}", r.ident)
            };
            alias_fn(decls, &full, &r.rename.to_string(), file, root);
        }
        syn::UseTree::Glob(_) => {}
        syn::UseTree::Group(g) => {
            for t in &g.items {
                apply_use_tree(decls, t, prefix, file, root);
            }
        }
    }
}

fn alias_fn(decls: &mut Vec<Decl>, full: &str, short: &str, file: &syn::File, root: bool) {
    let full = full.strip_prefix("self::").unwrap_or(full);
    let full = if root {
        full.strip_prefix("crate::").unwrap_or(full)
    } else {
        full
    };
    if full == short {
        return;
    }
    if decls
        .iter()
        .any(|decl| matches!(decl, Decl::Global { name, .. } if name == full))
    {
        if !decls.iter().any(|decl| matches!(decl, Decl::Global { name, .. } | Decl::Function { name, .. } if name == short)) {
            rewrite_global_references(decls, &HashMap::from([(short.to_owned(), resolved_global(full))]), file);
        }
        return;
    }
    if decls
        .iter()
        .any(|d| matches!(d, Decl::Function { name, .. } if name == short))
    {
        return;
    }
    let Some((target, params, ret, type_params, source_template)) = decls.iter().find_map(|d| match d {
        Decl::Function {
            name, params, ret, type_params, body,
        } if name == full || name.ends_with(&format!("::{short}")) => {
            let owned = name == full && body.iter().any(|stmt|
                matches!(stmt, Stmt::Expr(Expr::Unary { op, .. }) if op == crate::core_ir::rust_generics::TEMPLATE));
            Some((name.clone(), params.clone(), ret.clone(), type_params.clone(), owned))
        }
        _ => None,
    }) else {
        return;
    };
    let args: Vec<Expr> = params.iter().map(|(n, _)| Expr::Ident(n.clone())).collect();
    let callee = Expr::Ident(target);
    let mut body = Vec::new();
    if source_template && !type_params.is_empty() {
        body.push(Stmt::Expr(Expr::Unary {
            op: crate::core_ir::rust_generics::ALIAS.into(),
            expr: Box::new(callee.clone()),
        }));
        body.push(Stmt::Expr(Expr::Unary {
            op: crate::core_ir::rust_generics::TEMPLATE.into(),
            expr: Box::new(Expr::UnitLit),
        }));
    }
    body.push(Stmt::Return(Some(Expr::Call {
        callee: Box::new(callee),
        args,
    })));
    decls.push(Decl::Function {
        name: short.to_string(),
        params,
        ret,
        body,
        type_params,
    });
}

fn extract_boundary_module(file: &syn::File, module_id: &str) -> Option<BoundaryModule> {
    let mut layouts = Vec::new();
    let mut symbols = Vec::new();
    let mut layout_specs: RustLayoutSpecs = HashMap::new();

    for item in &file.items {
        if type_aliases::item_presence(item) == type_aliases::Presence::Inactive {
            continue;
        }
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
        if type_aliases::item_presence(item) == type_aliases::Presence::Inactive {
            continue;
        }
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
            if tp.qself.is_none()
                && let Some(resolved) = type_aliases::resolve(&tp.path).ok()?
                && let Some(kind) = crate::core_ir::rust_floats::kind(&resolved)
            {
                return Some(match kind {
                    crate::core_ir::rust_floats::Kind::F32 => scalar_abi("float", 4, 4),
                    crate::core_ir::rust_floats::Kind::F64 => scalar_abi("f64", 8, 8),
                });
            }
            if tp.qself.is_none()
                && let Some(Typ::Named(name)) = type_aliases::resolve(&tp.path).ok()?
                && let Some(kind) = crate::core_ir::rust_integer_type_name(&name)
            {
                let (name, size) = match kind {
                    "i8" => ("i8", 1),
                    "u8" => ("u8", 1),
                    "i16" => ("i16", 2),
                    "u16" => ("u16", 2),
                    "i32" => ("i32", 4),
                    "u32" => ("u32", 4),
                    "i64" | "isize" => ("i64", 8),
                    "u64" | "usize" => ("u64", 8),
                    _ => return None,
                };
                return Some(scalar_abi(name, size, size));
            }
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

/// Fixed-array fields need value-copy storage, not the dynamic Array pointer/length ABI.
/// Borrowed slices keep their existing representation; unused schemas remain available.
fn rust_struct_field_type(typ: &syn::Type) -> Typ {
    fn fixed_value(typ: &syn::Type) -> bool {
        match typ {
            syn::Type::Array(_) => true,
            syn::Type::Tuple(tuple) => tuple.elems.iter().any(fixed_value),
            syn::Type::Paren(typ) => fixed_value(&typ.elem),
            syn::Type::Group(typ) => fixed_value(&typ.elem),
            _ => false,
        }
    }
    if fixed_value(typ) {
        Typ::Named("#unsupported-rust-fixed-array-struct-value".into())
    } else {
        map_type(typ)
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
                    rust_struct_field_type(&f.ty),
                )
            })
            .collect(),
        syn::Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, f)| (format!("_{i}"), rust_struct_field_type(&f.ty)))
            .collect(),
        syn::Fields::Unit => vec![],
    }
}

thread_local! {
    static PRIMITIVE_ROOT_SHADOWS: std::cell::RefCell<HashSet<String>> = std::cell::RefCell::new(HashSet::new());
}

struct PrimitiveRootScope(HashSet<String>);
impl PrimitiveRootScope {
    fn for_block(block: &syn::Block) -> Self {
        Self::new(
            primitive_root_bindings(block.stmts.iter().filter_map(|statement| {
                if let syn::Stmt::Item(item) = statement {
                    Some(item)
                } else {
                    None
                }
            })),
            true,
        )
    }

    fn new(mut bindings: HashSet<String>, inherit: bool) -> Self {
        Self(PRIMITIVE_ROOT_SHADOWS.with(|roots| {
            let mut roots = roots.borrow_mut();
            if inherit {
                bindings.extend(roots.iter().cloned());
            }
            std::mem::replace(&mut *roots, bindings)
        }))
    }
}
impl Drop for PrimitiveRootScope {
    fn drop(&mut self) {
        PRIMITIVE_ROOT_SHADOWS.with(|roots| *roots.borrow_mut() = std::mem::take(&mut self.0));
    }
}

// Like the Neg root proof, unresolved imports/globs cannot establish an external identity.
fn primitive_root_bindings<'a>(items: impl Iterator<Item = &'a syn::Item>) -> HashSet<String> {
    fn binding(name: &syn::Ident, roots: &mut HashSet<String>) {
        if name == "std" || name == "core" {
            roots.insert(name.to_string());
        }
    }
    fn imported(tree: &syn::UseTree, roots: &mut HashSet<String>) {
        match tree {
            syn::UseTree::Path(path) => imported(&path.tree, roots),
            syn::UseTree::Name(name) => binding(&name.ident, roots),
            syn::UseTree::Rename(name) => binding(&name.rename, roots),
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    imported(tree, roots);
                }
            }
            syn::UseTree::Glob(_) => {
                roots.extend(["std".into(), "core".into()]);
            }
        }
    }
    let mut roots = HashSet::new();
    for item in items {
        if type_aliases::item_presence(item) == type_aliases::Presence::Inactive {
            continue;
        }
        match item {
            syn::Item::Mod(value) => binding(&value.ident, &mut roots),
            syn::Item::Struct(value) => binding(&value.ident, &mut roots),
            syn::Item::Enum(value) => binding(&value.ident, &mut roots),
            syn::Item::Union(value) => binding(&value.ident, &mut roots),
            syn::Item::Type(value) => binding(&value.ident, &mut roots),
            syn::Item::Trait(value) => binding(&value.ident, &mut roots),
            syn::Item::TraitAlias(value) => binding(&value.ident, &mut roots),
            syn::Item::Use(value) => imported(&value.tree, &mut roots),
            syn::Item::ExternCrate(value) => {
                let alias = value
                    .rename
                    .as_ref()
                    .map(|(_, alias)| alias)
                    .unwrap_or(&value.ident);
                if alias != &value.ident || (value.ident != "std" && value.ident != "core") {
                    binding(alias, &mut roots);
                }
            }
            _ => {}
        }
    }
    roots
}

fn primitive_root_generics(generics: &syn::Generics) -> HashSet<String> {
    generics
        .type_params()
        .filter(|parameter| parameter.ident == "std" || parameter.ident == "core")
        .map(|parameter| parameter.ident.to_string())
        .collect()
}

fn ambiguous_primitive_root(path: &syn::Path) -> bool {
    path.leading_colon.is_none()
        && path.segments.first().is_some_and(|root| {
            PRIMITIVE_ROOT_SHADOWS.with(|roots| roots.borrow().contains(&root.ident.to_string()))
        })
}

fn compact_rust_tokens(value: &impl ToTokens) -> String {
    value
        .to_token_stream()
        .to_string()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// Resolve associated type projections only in their defining impl scope.
/// Other traits' qualified projections and generic associated types stay unsupported.
fn resolve_impl_type(
    value: Typ,
    owner: &Typ,
    implementation: &syn::ItemImpl,
) -> Result<Typ, crate::core_ir::type_proofs::ResolutionFailure> {
    use crate::core_ir::type_proofs::ResolutionFailure::{Invalid, Unsupported};
    fn resolve(
        value: Typ,
        owner: &Typ,
        implementation: &syn::ItemImpl,
        active: &mut HashSet<String>,
    ) -> Result<Typ, crate::core_ir::type_proofs::ResolutionFailure> {
        match value {
            Typ::Named(name) if name == "Self" => Ok(owner.clone()),
            Typ::Named(name) => {
                if let Some(member) = name.strip_prefix("Self::") {
                    if !active.insert(member.to_string()) {
                        return Err(Invalid(format!("cyclic associated type `{name}`")));
                    }
                    let mut definitions =
                        implementation.items.iter().filter_map(|item| match item {
                            syn::ImplItem::Type(item) if item.ident == member => Some(item),
                            _ => None,
                        });
                    let item = definitions.next().ok_or_else(|| {
                        Unsupported(format!("unresolved associated type `{name}`"))
                    })?;
                    if definitions.next().is_some() {
                        return Err(Invalid(format!(
                            "duplicate associated type definition `{name}`"
                        )));
                    }
                    if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
                        return Err(Unsupported(format!(
                            "unsupported associated type definition `{name}`"
                        )));
                    }
                    let result = resolve(map_type(&item.ty), owner, implementation, active);
                    active.remove(member);
                    return result;
                }
                if implementation
                    .generics
                    .type_params()
                    .any(|parameter| parameter.ident == name)
                {
                    return Ok(Typ::Generic(name));
                }
                if let Some(array) = crate::core_ir::rust_arrays::decode(&name) {
                    return Ok(crate::core_ir::rust_arrays::typ(
                        resolve(array.element, owner, implementation, active)?,
                        array.length,
                    ));
                }
                if let Some(mut reference) = crate::core_ir::rust_references::decode(&name) {
                    reference.referent =
                        resolve(reference.referent, owner, implementation, active)?;
                    reference.runtime = resolve(reference.runtime, owner, implementation, active)?;
                    return Ok(crate::core_ir::rust_references::typ(reference));
                }
                if let Some(fields) = crate::core_ir::tuples::fields(&name) {
                    let items = fields
                        .into_iter()
                        .map(|(_, field)| resolve(field, owner, implementation, active))
                        .collect::<Result<Vec<_>, _>>()?;
                    return Ok(crate::core_ir::tuples::typ(&items));
                }
                if name.contains('<') || name.contains('>') || name.contains("Self::") {
                    return Err(Unsupported(format!(
                        "unsupported associated type projection `{name}`"
                    )));
                }
                Ok(Typ::Named(name))
            }
            Typ::Array(item) => Ok(Typ::Array(Box::new(resolve(
                *item,
                owner,
                implementation,
                active,
            )?))),
            Typ::Vector(item) => Ok(Typ::Vector(Box::new(resolve(
                *item,
                owner,
                implementation,
                active,
            )?))),
            value => Ok(value),
        }
    }
    resolve(value, owner, implementation, &mut HashSet::new())
}

fn lower_impl(i: &syn::ItemImpl) -> Vec<Decl> {
    let _type_parameters = type_aliases::generic_scope(&i.generics);
    let _primitive_roots = PrimitiveRootScope::new(primitive_root_generics(&i.generics), true);
    // Retain generic arguments so distinct impl receivers retain distinct identities.
    let self_type = compact_rust_tokens(&i.self_ty);
    let mut decls = Vec::new();
    for item in &i.items {
        if let syn::ImplItem::Fn(method) = item {
            let _type_parameters = type_aliases::generic_scope(&method.sig.generics);
            let _primitive_roots =
                PrimitiveRootScope::new(primitive_root_generics(&method.sig.generics), true);
            let f = method.clone();
            let signature_locals = signature_length_locals(&f.sig);
            let _length_shadows = LengthShadowScope::new(&signature_locals);
            let method_name = if let Some((_, trait_path, _)) = &i.trait_ {
                // Trait methods occupy their receiver/trait namespace, never the free-function namespace.
                format!(
                    "{}::{}::{}",
                    self_type,
                    compact_rust_tokens(trait_path),
                    f.sig.ident
                )
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
                    syn::FnArg::Receiver(recv) => {
                        let proof = map_type(&i.self_ty);
                        let runtime = Typ::Named(self_type.clone());
                        let typ = if recv.reference.is_some() {
                            crate::core_ir::rust_references::typ(
                                crate::core_ir::rust_references::ReferenceType {
                                    mutable: recv.mutability.is_some(),
                                    referent: proof,
                                    runtime,
                                },
                            )
                        } else if let Typ::Named(name) = &proof
                            && let Some(mut reference) =
                                crate::core_ir::rust_references::decode(name)
                        {
                            reference.runtime = runtime;
                            crate::core_ir::rust_references::typ(reference)
                        } else {
                            runtime
                        };
                        ("self".to_string(), typ)
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
                local_types.insert(format!("{LENGTH_LOCAL}{pname}"), String::new());
            }
            let body = lower_block_with_types(&f.block, &mut local_types);
            let mut declaration = Decl::Function {
                name: method_name,
                params,
                ret,
                body,
                type_params: vec![],
            };
            let mut parameters = declared_type_parameters(&i.generics);
            parameters.extend(declared_type_parameters(&f.sig.generics));
            retain_generic_scope(&mut declaration, parameters);
            decls.push(declaration);
        }
    }
    let mut self_proof = map_type(&i.self_ty);
    if let Typ::Named(name) = &self_proof
        && let Some(mut reference) = crate::core_ir::rust_references::decode(name)
    {
        reference.runtime = Typ::Named(self_type.clone());
        self_proof = crate::core_ir::rust_references::typ(reference);
    }
    crate::core_bindings::rewrite_type_annotations(&mut decls, &|name| {
        (name == "Self" || name.starts_with("Self::")).then(|| {
            resolve_impl_type(Typ::Named(name.into()), &self_proof, i)
                .unwrap_or_else(crate::core_ir::type_proofs::ResolutionFailure::typ)
        })
    });
    crate::core_bindings::rewrite_named_types(&mut decls, &|name| {
        // This walker also visits constructor/pattern names. Resolve only an
        // actual associated type here; other Self::member value paths keep the
        // existing nominal/value rewrite below.
        let projection = name.strip_prefix("Self::").is_some_and(|member| {
            i.items
                .iter()
                .any(|item| matches!(item, syn::ImplItem::Type(item) if item.ident == member))
        });
        (name == "Self" || projection).then(|| {
            resolve_impl_type(Typ::Named(name.into()), &self_proof, i)
                .unwrap_or_else(crate::core_ir::type_proofs::ResolutionFailure::typ)
        })
    });
    let resolve_self_type = |name: &str| {
        if name == "Self" {
            Some(self_type.clone())
        } else {
            name.strip_prefix("Self::")
                .map(|member| format!("{self_type}::{member}"))
        }
    };
    let resolve_self_value = |name: &str| {
        if let Some((head, member)) = name
            .strip_prefix("Self::")
            .and_then(|path| path.split_once("::"))
            && i.items
                .iter()
                .any(|item| matches!(item, syn::ImplItem::Type(item) if item.ident == head))
        {
            // Resolve only a declared impl-associated type, never a same-spelled
            // type or trait from another scope. The target member stays exact.
            let resolved = resolve_impl_type(Typ::Named(format!("Self::{head}")), &self_proof, i);
            return Some(match resolved {
                Ok(Typ::Named(owner))
                    if crate::core_ir::rust_integer_type_name(&owner).is_some()
                        && (matches!(member, "MIN" | "MAX" | "BITS")
                            || crate::typecheck::rust_integers::wrapping_operator(member)
                                .is_some()) =>
                {
                    format!("{owner}::{member}")
                }
                Ok(Typ::Named(owner))
                    if crate::core_ir::rust_references::decode(&owner).is_none()
                        && crate::core_ir::rust_integer_type_name(&owner).is_none()
                        && crate::core_ir::integer_type_name(&owner).is_none()
                        && !owner.starts_with('#')
                        && !owner.contains('<')
                        && !owner.contains('>')
                        && crate::core_ir::tuples::fields(&owner).is_none() =>
                {
                    format!("{owner}::{member}")
                }
                Err(proof) => proof.symbol(),
                Ok(owner) => crate::core_ir::type_proofs::ResolutionFailure::Unsupported(format!(
                    "associated value path `{name}` has unsupported owner `{owner:?}`"
                ))
                .symbol(),
            });
        }
        resolve_self_type(name)
    };
    crate::core_bindings::rewrite_nominal_names(
        &mut decls,
        &resolve_self_type,
        &resolve_self_value,
    );
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
    if let Some(kind) = crate::core_ir::rust_floats::kind(ty) {
        return kind.name().into();
    }
    if let Typ::Named(name) = ty
        && let Some(reference) = crate::core_ir::rust_references::decode(name)
    {
        return type_to_string(&reference.runtime);
    }
    match ty {
        Typ::Int => "i64".to_string(),
        Typ::Bool => "bool".to_string(),
        Typ::String => "String".to_string(),
        Typ::Float => "f64".to_string(),
        Typ::Void => "()".to_string(),
        Typ::Named(n) => n.strip_prefix("#rust-integer-").unwrap_or(n).to_string(),
        Typ::Array(e) => format!("Vec<{}>", type_to_string(e)),
        Typ::Vector(_) => "Vec".to_string(),
        Typ::Generic(g) => g.clone(),
    }
}

fn lower_fn(f: syn::ItemFn) -> Decl {
    lower_fn_with_types(f, &mut HashMap::new())
}

fn declared_type_parameters(generics: &syn::Generics) -> Vec<String> {
    generics
        .type_params()
        .map(|parameter| parameter.ident.to_string())
        .collect()
}

fn retain_generic_scope(decl: &mut Decl, parameters: Vec<String>) {
    if parameters.is_empty() {
        return;
    }
    let names: HashSet<_> = parameters.iter().cloned().collect();
    // Reuse the existing scoped type-position walker before setting the
    // declaration's generic-name guard. Values and nominal member names stay intact.
    crate::core_bindings::rewrite_named_types(std::slice::from_mut(decl), &|name| {
        names.contains(name).then(|| Typ::Generic(name.into()))
    });
    match decl {
        Decl::Function { type_params, .. } | Decl::Struct { type_params, .. } => {
            *type_params = parameters;
        }
        _ => {}
    }
}

fn lower_fn_with_types(f: syn::ItemFn, local_types: &mut HashMap<String, String>) -> Decl {
    let _type_parameters = type_aliases::generic_scope(&f.sig.generics);
    let _primitive_roots = PrimitiveRootScope::new(primitive_root_generics(&f.sig.generics), true);
    let name = f.sig.ident.to_string();
    let signature_locals = signature_length_locals(&f.sig);
    let _length_shadows = LengthShadowScope::new(&signature_locals);
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
                local_types.insert(format!("{LENGTH_LOCAL}{pname}"), String::new());
                (pname, ty)
            }
            syn::FnArg::Receiver(_) => ("self".to_string(), Typ::Named("Self".to_string())),
        })
        .collect();
    let ret = match &f.sig.output {
        syn::ReturnType::Default => Typ::Void,
        syn::ReturnType::Type(_, ty) => map_type(ty),
    };
    let mut body = lower_block_with_types(&f.block, local_types);
    if !f.sig.generics.params.is_empty() {
        body.insert(
            0,
            Stmt::Expr(Expr::Unary {
                op: crate::core_ir::rust_generics::TEMPLATE.into(),
                expr: Box::new(Expr::UnitLit),
            }),
        );
    }
    if f.sig.generics.where_clause.is_some()
        || f.sig
            .generics
            .params
            .iter()
            .any(|parameter| match parameter {
                syn::GenericParam::Type(parameter) => {
                    !parameter.bounds.is_empty() || parameter.default.is_some()
                }
                _ => true,
            })
    {
        body.insert(
            0,
            Stmt::Expr(if f.sig.generics.params.is_empty() {
                Expr::Ident(
                    crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                        "concrete where-clause obligations require constraint resolution".into(),
                    )
                    .symbol(),
                )
            } else {
                Expr::Unary {
                    op: crate::core_ir::rust_generics::CONSTRAINTS.into(),
                    expr: Box::new(Expr::UnitLit),
                }
            }),
        );
    }
    let mut declaration = Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    };
    retain_generic_scope(&mut declaration, declared_type_parameters(&f.sig.generics));
    declaration
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

fn nominal_path_name(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| {
            let arguments = match &segment.arguments {
                syn::PathArguments::None => String::new(),
                syn::PathArguments::AngleBracketed(arguments) => {
                    let args = arguments
                        .args
                        .iter()
                        .map(|arg| match arg {
                            syn::GenericArgument::Type(syn::Type::Path(ty))
                                if ty.qself.is_none() =>
                            {
                                nominal_path_name(&ty.path)
                            }
                            _ => arg.to_token_stream().to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("<{args}>")
                }
                other => other.to_token_stream().to_string(),
            };
            format!("{}{arguments}", segment.ident)
        })
        .collect::<Vec<_>>()
        .join("::")
}

fn generic_argument_type(
    typ: &syn::Type,
) -> Result<Typ, crate::core_ir::type_proofs::ResolutionFailure> {
    use crate::core_ir::type_proofs::ResolutionFailure::Unsupported;
    match typ {
        syn::Type::Path(path) if path.qself.is_none() => {
            let mapped = map_type(typ);
            if let Some(failure) = crate::core_ir::type_proofs::failure_in_type(&mapped) {
                return Err(failure);
            }
            for segment in &path.path.segments {
                if let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments {
                    for argument in &arguments.args {
                        match argument {
                            syn::GenericArgument::Type(typ) => {
                                generic_argument_type(typ)?;
                            }
                            _ => {
                                return Err(Unsupported(
                                    "generic type arguments require owned type identities".into(),
                                ));
                            }
                        }
                    }
                }
            }
            let owned = type_aliases::source_type_owned(&path.path)?;
            let spelling = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            let builtin = !ambiguous_primitive_root(&path.path)
                && (matches!(
                    spelling.as_str(),
                    "bool"
                        | "f32"
                        | "f64"
                        | "str"
                        | "String"
                        | "std::string::String"
                        | "alloc::string::String"
                        | "Vec"
                        | "std::vec::Vec"
                        | "alloc::vec::Vec"
                ) || matches!(&mapped, Typ::Generic(_))
                    || matches!(&mapped, Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some()));
            if builtin {
                let supplied =
                    path.path
                        .segments
                        .last()
                        .map_or(0, |segment| match &segment.arguments {
                            syn::PathArguments::AngleBracketed(arguments) => arguments.args.len(),
                            _ => 0,
                        });
                let expected = usize::from(matches!(mapped, Typ::Vector(_)));
                if supplied != expected {
                    return Err(crate::core_ir::type_proofs::ResolutionFailure::Invalid(
                        format!(
                            "type argument `{spelling}` expects {expected} type arguments, found {supplied}"
                        ),
                    ));
                }
            }
            if !builtin && !owned {
                return Err(Unsupported(format!(
                    "explicit type argument `{}` is not owned by source",
                    compact_rust_tokens(typ)
                )));
            }
            Ok(mapped)
        }
        syn::Type::Reference(reference) => {
            if reference
                .lifetime
                .as_ref()
                .is_some_and(|lifetime| lifetime.ident != "static" && lifetime.ident != "_")
            {
                return Err(Unsupported(
                    "explicit type argument lifetime is not resolved".into(),
                ));
            }
            generic_argument_type(&reference.elem)?;
            Ok(map_type(typ))
        }
        syn::Type::Array(array) => {
            generic_argument_type(&array.elem)?;
            if !matches!(&array.len, syn::Expr::Lit(literal) if matches!(literal.lit, syn::Lit::Int(_)))
            {
                return Err(Unsupported(
                    "explicit array length requires const resolution".into(),
                ));
            }
            Ok(map_type(typ))
        }
        syn::Type::Tuple(tuple) => {
            for element in &tuple.elems {
                generic_argument_type(element)?;
            }
            Ok(map_type(typ))
        }
        syn::Type::Paren(paren) => generic_argument_type(&paren.elem),
        syn::Type::Group(group) => generic_argument_type(&group.elem),
        _ => Err(Unsupported(
            "explicit type argument requires a supported source type".into(),
        )),
    }
}

fn rust_integer_path(path: &syn::Path) -> Option<&str> {
    if path
        .segments
        .iter()
        .any(|part| !matches!(part.arguments, syn::PathArguments::None))
    {
        return None;
    }
    let names: Vec<_> = path
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect();
    let primitive = match names.as_slice() {
        [name] => name,
        [root, module, name]
            if matches!(root.as_str(), "core" | "std") && module == "primitive" =>
        {
            name
        }
        _ => return None,
    };
    match primitive.as_str() {
        "i8" => Some("i8"),
        "u8" => Some("u8"),
        "i16" => Some("i16"),
        "u16" => Some("u16"),
        "i32" => Some("i32"),
        "u32" => Some("u32"),
        "i64" => Some("i64"),
        "u64" => Some("u64"),
        "isize" => Some("isize"),
        "usize" => Some("usize"),
        "i128" => Some("i128"),
        "u128" => Some("u128"),
        _ => None,
    }
}

fn rust_integer_constant(path: &syn::Path, call_target: bool) -> Option<String> {
    let mut owner = path.clone();
    let member = owner.segments.pop()?.into_value();
    let known_constant = matches!(member.arguments, syn::PathArguments::None)
        && matches!(member.ident.to_string().as_str(), "MIN" | "MAX" | "BITS");
    if rust_integer_path(&owner).is_some() && ambiguous_primitive_root(&owner) {
        return Some(
            crate::core_ir::type_proofs::ResolutionFailure::Unsupported(format!(
                "shadowed primitive root in `{}` requires namespace resolution",
                compact_rust_tokens(path)
            ))
            .symbol(),
        );
    }
    let known_wrapping = call_target
        && matches!(member.arguments, syn::PathArguments::None)
        && crate::typecheck::rust_integers::wrapping_operator(&member.ident.to_string()).is_some();
    let owner_type = type_aliases::resolve_value_owner(&owner);
    let resolved = match owner_type {
        Ok(Some(Typ::Named(name))) => Some(name),
        Ok(Some(Typ::Generic(_))) => {
            return Some(
                crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                    "generic associated value requires monomorphization".into(),
                )
                .symbol(),
            );
        }
        Err(proof) => return Some(proof.symbol()),
        _ => None,
    };
    let kind = resolved
        .as_deref()
        .and_then(crate::core_ir::rust_integer_type_name)
        .or_else(|| rust_integer_path(&owner))?;
    if owner.leading_colon.is_some() && owner.segments.len() == 1 {
        return Some(
            crate::core_ir::type_proofs::ResolutionFailure::Unsupported(format!(
                "absolute path `{}` does not resolve a primitive owner",
                compact_rust_tokens(path)
            ))
            .symbol(),
        );
    }
    if matches!(kind, "i128" | "u128") {
        return Some(
            crate::core_ir::type_proofs::ResolutionFailure::Unsupported(format!(
                "integer constant owner `{kind}` exceeds supported native width"
            ))
            .symbol(),
        );
    }

    if !known_constant && !known_wrapping {
        return Some(
            crate::core_ir::type_proofs::ResolutionFailure::Unsupported(format!(
                "unresolved primitive associated value `{}`",
                compact_rust_tokens(path)
            ))
            .symbol(),
        );
    }
    Some(format!("#rust-integer-{kind}::{}", member.ident))
}

// Resolve explicit vector type arguments while their source lexical scope is still active.
fn scoped_vector_path(
    path: &syn::Path,
) -> Result<syn::Path, crate::core_ir::type_proofs::ResolutionFailure> {
    let mut result = path.clone();
    let owner = path
        .segments
        .iter()
        .map(|part| part.ident.to_string())
        .collect::<Vec<_>>()
        .join("::");
    if !matches!(
        owner.as_str(),
        "Vec::new"
            | "Vec::with_capacity"
            | "std::vec::Vec::new"
            | "std::vec::Vec::with_capacity"
            | "alloc::vec::Vec::new"
            | "alloc::vec::Vec::with_capacity"
    ) {
        return Ok(result);
    }
    for segment in &mut result.segments {
        if let syn::PathArguments::AngleBracketed(arguments) = &mut segment.arguments {
            for argument in &mut arguments.args {
                if let syn::GenericArgument::Type(syn::Type::Path(ty)) = argument
                    && ty.qself.is_none()
                    && let Some(Typ::Named(name)) = type_aliases::resolve(&ty.path)?
                    && let Some(kind) = crate::core_ir::rust_integer_type_name(&name)
                {
                    ty.path = syn::parse_str(kind).map_err(|_| {
                        crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                            "unresolved primitive vector element".into(),
                        )
                    })?;
                }
            }
        }
    }
    Ok(result)
}

fn rust_float_path(path: &syn::Path) -> Option<crate::core_ir::rust_floats::Kind> {
    let spelling = compact_rust_tokens(path);
    let qualified = spelling.strip_prefix("::").unwrap_or(&spelling);
    let name = match spelling.as_str() {
        "f32" | "f64" => spelling.as_str(),
        _ => qualified
            .strip_prefix("core::primitive::")
            .or_else(|| qualified.strip_prefix("std::primitive::"))?,
    };
    crate::core_ir::rust_floats::Kind::parse(name)
}

fn rust_integer_literal(value: i64, suffix: &str) -> Expr {
    Expr::Unary {
        op: if suffix.is_empty() {
            "rust-integer-literal".into()
        } else {
            format!("rust-integer-literal-{suffix}")
        },
        expr: Box::new(Expr::IntLit(value)),
    }
}

fn map_type(ty: &syn::Type) -> Typ {
    match ty {
        syn::Type::Path(tp) if tp.qself.is_some() => Typ::Named(ty.to_token_stream().to_string()),
        syn::Type::Path(tp)
            if rust_integer_path(&tp.path).is_some()
                && tp.path.leading_colon.is_some()
                && tp.path.segments.len() == 1 =>
        {
            crate::core_ir::type_proofs::ResolutionFailure::Unsupported(format!(
                "absolute path `{}` does not resolve a primitive owner",
                compact_rust_tokens(&tp.path)
            ))
            .typ()
        }
        syn::Type::Path(tp)
            if rust_integer_path(&tp.path).is_some() && ambiguous_primitive_root(&tp.path) =>
        {
            crate::core_ir::type_proofs::ResolutionFailure::Unsupported(format!(
                "shadowed primitive root in `{}` requires namespace resolution",
                compact_rust_tokens(&tp.path)
            ))
            .typ()
        }
        syn::Type::Path(tp) => {
            match type_aliases::resolve(&tp.path) {
                Ok(Some(typ)) => return typ,
                Err(proof) => return proof.typ(),
                Ok(None) => {}
            }
            let last_segment = tp.path.segments.last();
            let last = last_segment.map(|s| s.ident.to_string());
            match last.as_deref() {
                Some(
                    "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64" | "usize",
                ) if rust_integer_path(&tp.path).is_some() => Typ::Named(format!(
                    "#rust-integer-{}",
                    last.as_deref().unwrap_or_default()
                )),
                Some("i128" | "u128") if rust_integer_path(&tp.path).is_some() => {
                    Typ::Named(format!(
                        "#unsupported-rust-integer-{}",
                        last.as_deref().unwrap_or_default()
                    ))
                }
                Some("String" | "str") => Typ::String,
                Some("bool") => Typ::Bool,
                Some("f32" | "f64") if type_aliases::current().is_none() => {
                    rust_float_path(&tp.path)
                        .map(crate::core_ir::rust_floats::typ)
                        .unwrap_or_else(|| Typ::Named(nominal_path_name(&tp.path)))
                }
                Some("AtomicU32" | "AtomicU64" | "AtomicUsize" | "Ordering") => Typ::Named(
                    tp.path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::"),
                ),
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
                Some(_) => Typ::Named(nominal_path_name(&tp.path)),
                None => Typ::Named(tp.path.to_token_stream().to_string()),
            }
        }
        syn::Type::Infer(_) => Typ::Generic("_".into()),
        syn::Type::Reference(r) => {
            let typ = map_type(&r.elem);
            if matches!(&typ, Typ::Vector(_)) {
                // shortcut: vector borrows need an address ABI before header mutations can be shared.
                Typ::Named("#unsupported-borrowed-vector".into())
            } else if matches!(&typ, Typ::Named(name) if crate::core_ir::tuples::fields(name).is_some())
            {
                // shortcut: tuple borrows need reference types and an address ABI before native execution.
                Typ::Named("#unsupported-borrowed-tuple".into())
            } else {
                let runtime = crate::core_ir::rust_references::erase_type(typ.clone());
                crate::core_ir::rust_references::typ(
                    crate::core_ir::rust_references::ReferenceType {
                        mutable: r.mutability.is_some(),
                        referent: typ,
                        runtime,
                    },
                )
            }
        }
        syn::Type::Array(array) => match fixed_array_length(&array.len) {
            Ok(Some(length)) => crate::core_ir::rust_arrays::typ(map_type(&array.elem), length),
            Ok(None) => crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                "fixed array length requires owned usize constant resolution".into(),
            )
            .typ(),
            Err(failure) => failure.typ(),
        },
        syn::Type::Tuple(t) => {
            crate::core_ir::tuples::typ(&t.elems.iter().map(map_type).collect::<Vec<_>>())
        }
        _ => Typ::Named(ty.to_token_stream().to_string()),
    }
}

const LENGTH_LOCAL: &str = "#rust-length-local:";
thread_local! {
    static LENGTH_SHADOWS: std::cell::RefCell<HashSet<String>> = std::cell::RefCell::new(HashSet::new());
}
fn signature_length_locals(signature: &syn::Signature) -> HashMap<String, String> {
    signature
        .inputs
        .iter()
        .fold(HashMap::new(), |mut locals, argument| match argument {
            syn::FnArg::Typed(argument) => length_pattern_locals(&argument.pat, &locals),
            syn::FnArg::Receiver(_) => {
                locals.insert(format!("{LENGTH_LOCAL}self"), String::new());
                locals
            }
        })
}
fn length_pattern_locals(
    pattern: &syn::Pat,
    locals: &HashMap<String, String>,
) -> HashMap<String, String> {
    fn add(pattern: &syn::Pat, locals: &mut HashMap<String, String>) {
        match pattern {
            syn::Pat::Ident(value) => {
                locals.insert(format!("{LENGTH_LOCAL}{}", value.ident), String::new());
                if let Some((_, pattern)) = &value.subpat {
                    add(pattern, locals);
                }
            }
            syn::Pat::Tuple(value) => {
                for item in &value.elems {
                    add(item, locals);
                }
            }
            syn::Pat::TupleStruct(value) => {
                for item in &value.elems {
                    add(item, locals);
                }
            }
            syn::Pat::Struct(value) => {
                for field in &value.fields {
                    add(&field.pat, locals);
                }
            }
            syn::Pat::Slice(value) => {
                for item in &value.elems {
                    add(item, locals);
                }
            }
            syn::Pat::Or(value) => {
                for item in &value.cases {
                    add(item, locals);
                }
            }
            syn::Pat::Reference(value) => add(&value.pat, locals),
            syn::Pat::Type(value) => add(&value.pat, locals),
            syn::Pat::Paren(value) => add(&value.pat, locals),
            _ => {}
        }
    }
    let mut inner = locals.clone();
    add(pattern, &mut inner);
    inner
}
struct LengthShadowScope(HashSet<String>);
impl LengthShadowScope {
    fn new(locals: &HashMap<String, String>) -> Self {
        let names = locals
            .keys()
            .filter_map(|name| name.strip_prefix(LENGTH_LOCAL).map(str::to_string))
            .collect();
        Self(LENGTH_SHADOWS.with(|slot| slot.replace(names)))
    }
}
impl Drop for LengthShadowScope {
    fn drop(&mut self) {
        LENGTH_SHADOWS.with(|slot| {
            slot.replace(std::mem::take(&mut self.0));
        });
    }
}
fn fixed_array_length(
    expression: &syn::Expr,
) -> Result<Option<usize>, crate::core_ir::type_proofs::ResolutionFailure> {
    let shadows = LENGTH_SHADOWS.with(|shadows| shadows.borrow().clone());
    type_aliases::usize_length(expression, &shadows)
}

fn opaque_macro_dependencies(mac: &syn::Macro, local_types: &HashMap<String, String>) -> Expr {
    let locals: HashSet<String> = local_types
        .keys()
        .filter_map(|name| name.strip_prefix(LENGTH_LOCAL).map(str::to_owned))
        .collect();
    // Retain missing inference edges without expanding or evaluating the macro.
    let mut temporary = if mac
        .path
        .segments
        .last()
        .is_some_and(|s| s.ident == "matches")
    {
        let parser = |input: syn::parse::ParseStream<'_>| {
            let value = input.parse::<syn::Expr>()?;
            input.parse::<syn::Token![,]>()?;
            let pattern = syn::Pat::parse_multi_with_leading_vert(input)?;
            let guard = if input.peek(syn::Token![if]) {
                input.parse::<syn::Token![if]>()?;
                Some(input.parse::<syn::Expr>()?)
            } else {
                None
            };
            if input.peek(syn::Token![,]) {
                input.parse::<syn::Token![,]>()?;
            }
            Ok((value, pattern, guard))
        };
        parser
            .parse2(mac.tokens.clone())
            .map(|(value, pattern, guard)| {
                let mut body = vec![Stmt::Expr(lower_expr_with_types(
                    &value,
                    &mut local_types.clone(),
                ))];
                if let Some(guard) = guard {
                    let mut params: Vec<_> = length_pattern_locals(&pattern, &HashMap::new())
                        .keys()
                        .filter_map(|name| name.strip_prefix(LENGTH_LOCAL))
                        .map(|name| (name.to_owned(), Typ::Generic("_".into())))
                        .collect();
                    params.sort_by(|left, right| left.0.cmp(&right.0));
                    body.push(Stmt::Expr(Expr::Closure {
                        params,
                        ret: Typ::Void,
                        body: vec![Stmt::Expr(lower_expr_with_types(
                            &guard,
                            &mut length_pattern_locals(&pattern, local_types),
                        ))],
                        captures: vec![],
                    }));
                }
                body
            })
            .unwrap_or_default()
    } else {
        syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
            .parse2(mac.tokens.clone())
            .map(|items| {
                items
                    .iter()
                    .map(|item| Stmt::Expr(lower_expr_with_types(item, &mut local_types.clone())))
                    .collect()
            })
            .unwrap_or_default()
    };
    // The runtime read collector skips unlowered closures. Analyze their scoped
    // bodies too, while keeping these dependency-only expressions non-executable.
    for stmt in &mut temporary {
        crate::core_opt::map_stmt_exprs_mut(stmt, &mut |expr| {
            if let Expr::Closure { params, body, .. } = expr {
                let mut scoped: Vec<_> = std::mem::take(params)
                    .into_iter()
                    .map(|(name, typ)| Stmt::Declare(name, typ))
                    .collect();
                scoped.extend(std::mem::take(body));
                *expr = Expr::Block {
                    body: scoped,
                    result: None,
                };
            }
        });
    }
    crate::core_bindings::normalize_function(&mut [], &mut temporary, &locals);
    let dependencies = crate::lower_core::collect_free_vars(&temporary, &[])
        .into_iter()
        .filter(|name| locals.contains(name))
        .map(Expr::Ident)
        .collect();
    Expr::Call {
        callee: Box::new(Expr::Ident(crate::core_ir::UNSUPPORTED_DEFINED_BODY.into())),
        args: dependencies,
    }
}

fn vector_value_initializer(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Macro(value) => value.mac.path.is_ident("vec"),
        syn::Expr::If(value) => value.then_branch.stmts.last().is_some_and(
            |stmt| matches!(stmt, syn::Stmt::Expr(value, None) if vector_value_initializer(value)),
        ) || value
            .else_branch
            .as_ref()
            .is_some_and(|(_, value)| vector_value_initializer(value)),
        syn::Expr::Block(value) => value.block.stmts.last().is_some_and(
            |stmt| matches!(stmt, syn::Stmt::Expr(value, None) if vector_value_initializer(value)),
        ),
        syn::Expr::Match(value) => value
            .arms
            .iter()
            .any(|arm| vector_value_initializer(&arm.body)),
        syn::Expr::Paren(value) => vector_value_initializer(&value.expr),
        syn::Expr::Group(value) => vector_value_initializer(&value.expr),
        _ => false,
    }
}

fn lower_match_arm_value(expr: &syn::Expr, local_types: &HashMap<String, String>) -> Expr {
    if let syn::Expr::Block(value) = expr {
        return lower_block_value(&value.block, local_types);
    }
    lower_block_value(
        &syn::Block {
            brace_token: Default::default(),
            stmts: vec![syn::Stmt::Expr(expr.clone(), None)],
        },
        local_types,
    )
}

fn lower_block_value(block: &syn::Block, local_types: &HashMap<String, String>) -> Expr {
    let _type_aliases = type_aliases::block_scope(block);
    let _primitive_roots = PrimitiveRootScope::for_block(block);
    let mut inner = local_types.clone();
    let mut prefix = block.clone();
    let tail = match prefix.stmts.last() {
        Some(syn::Stmt::Expr(_, None)) => match prefix.stmts.pop().unwrap() {
            syn::Stmt::Expr(expr, None) => Some(expr),
            _ => unreachable!(),
        },
        _ => None,
    };
    let mut body = lower_block_inner_with_types_mode(&prefix, false, &mut inner, false);
    let result = match tail {
        Some(syn::Expr::Return(value)) => {
            lower_return_expr(value.expr.as_deref(), &mut body, &mut inner);
            None
        }
        Some(syn::Expr::Break(value)) => {
            lower_expr_stmt(&syn::Expr::Break(value), &mut body, &mut inner);
            None
        }
        Some(syn::Expr::Continue(value)) => {
            lower_expr_stmt(&syn::Expr::Continue(value), &mut body, &mut inner);
            None
        }
        Some(syn::Expr::Tuple(value)) if value.elems.is_empty() => None,
        Some(value) => Some(Box::new(lower_expr_with_types(&value, &mut inner))),
        None => None,
    };
    let mut value = Expr::Block { body, result };
    let references = block_import_references(block);
    if !references.is_empty() {
        let names = references
            .keys()
            .chain(references.values())
            .cloned()
            .collect();
        crate::core_bindings::normalize_initializer_with_references(
            &mut value,
            &names,
            &references,
        );
    }
    value
}

/// Resolve block imports in the value namespace; type aliases remain in their own catalog.
fn block_import_references(block: &syn::Block) -> HashMap<String, String> {
    let mut leaves = HashMap::new();
    for statement in &block.stmts {
        let syn::Stmt::Item(item @ syn::Item::Use(_)) = statement else {
            continue;
        };
        if type_aliases::item_presence(item) == type_aliases::Presence::Inactive {
            continue;
        }
        let syn::Item::Use(item) = item else {
            unreachable!()
        };
        imports::import_leaves(&item.tree, &[], item.leading_colon.is_some(), &mut leaves);
    }
    fn target(
        name: &str,
        leaves: &HashMap<String, imports::Import>,
        visiting: &mut HashSet<String>,
    ) -> Result<(Vec<String>, bool), crate::core_ir::type_proofs::ResolutionFailure> {
        use crate::core_ir::type_proofs::ResolutionFailure;
        if !visiting.insert(name.to_owned()) {
            return Err(ResolutionFailure::Unsupported(format!(
                "cyclic block callable import `{name}`"
            )));
        }
        let (mut parts, mut absolute) =
            leaves.get(name).and_then(Clone::clone).ok_or_else(|| {
                ResolutionFailure::Unsupported(format!("ambiguous block callable import `{name}`"))
            })?;
        if !absolute && parts.len() > 1 {
            // A projected path selects a module in the type namespace. A same-named
            // value import must not redirect it through an unrelated function.
            let prefix: syn::Path =
                syn::parse_str(&parts[..parts.len() - 1].join("::")).map_err(|_| {
                    ResolutionFailure::Unsupported("unresolved block import prefix".into())
                })?;
            if let Some(mut module) = type_aliases::source_module_path(&prefix)? {
                module.push(parts.pop().unwrap());
                parts = module;
            }
        } else if !absolute
            && let Some(first) = parts.first()
            && leaves.contains_key(first)
        {
            let (prefix, root) = target(first, leaves, visiting)?;
            parts = prefix;
            absolute = root;
        }
        visiting.remove(name);
        Ok((parts, absolute))
    }
    leaves
        .keys()
        .filter_map(|name| {
            let path: syn::Path = syn::parse_str(name).ok()?;
            // The catalog checks source visibility and namespace proof before expansion.
            let resolved = type_aliases::resolve(&path)
                .and_then(|_| target(name, &leaves, &mut HashSet::new()));
            let target = match resolved {
                Err(failure) => failure.symbol(),
                Ok((parts, absolute)) => {
                    format!("{}{}", if absolute { "::" } else { "" }, parts.join("::"))
                }
            };
            Some((name.clone(), target))
        })
        .collect()
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

fn lower_scoped_block(
    block: &syn::Block,
    wrap_implicit_return: bool,
    out: &mut Vec<Stmt>,
    local_types: &HashMap<String, String>,
) {
    out.push(Stmt::If {
        cond: Expr::BoolLit(true),
        then_body: lower_block_inner_with_types(
            block,
            wrap_implicit_return,
            &mut local_types.clone(),
        ),
        else_body: vec![],
    });
}

thread_local! {
    static BLOCK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn lower_block_inner_with_types(
    block: &syn::Block,
    wrap_implicit_return: bool,
    local_types: &mut HashMap<String, String>,
) -> Vec<Stmt> {
    lower_block_inner_with_types_mode(block, wrap_implicit_return, local_types, true)
}

fn lower_block_inner_with_types_mode(
    block: &syn::Block,
    wrap_implicit_return: bool,
    local_types: &mut HashMap<String, String>,
    resolve_imports: bool,
) -> Vec<Stmt> {
    let _type_aliases = type_aliases::block_scope(block);
    let _primitive_roots = PrimitiveRootScope::for_block(block);
    let depth = BLOCK_DEPTH.with(|d| {
        let n = d.get();
        d.set(n.saturating_add(1));
        n
    });
    if depth > 32 {
        BLOCK_DEPTH.with(|d| d.set(depth));
        return vec![];
    }
    let mut out = lower_block_inner_with_types_go(block, wrap_implicit_return, local_types);
    if resolve_imports {
        let references = block_import_references(block);
        if !references.is_empty() {
            let names = references
                .keys()
                .chain(references.values())
                .cloned()
                .collect();
            crate::core_bindings::normalize_function_with_references(
                &mut [],
                &mut out,
                &names,
                &references,
            );
        }
    }
    BLOCK_DEPTH.with(|d| d.set(depth));
    out
}

fn tuple_let_pattern(pattern: &syn::Pat) -> bool {
    match pattern {
        syn::Pat::Tuple(_) => true,
        syn::Pat::Type(pattern) => tuple_let_pattern(&pattern.pat),
        syn::Pat::Paren(pattern) => tuple_let_pattern(&pattern.pat),
        syn::Pat::Reference(pattern) => tuple_let_pattern(&pattern.pat),
        _ => false,
    }
}

fn tuple_let_names(pattern: &syn::Pat, names: &mut HashSet<String>) -> bool {
    match pattern {
        syn::Pat::Ident(pattern) => names.insert(pattern.ident.to_string()),
        syn::Pat::Tuple(pattern) => pattern
            .elems
            .iter()
            .all(|pattern| tuple_let_names(pattern, names)),
        syn::Pat::Type(pattern) => tuple_let_names(&pattern.pat, names),
        syn::Pat::Paren(pattern) => tuple_let_names(&pattern.pat, names),
        _ => true,
    }
}

fn borrowed_tuple_initializer(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Reference(_) => true,
        syn::Expr::Paren(expr) => borrowed_tuple_initializer(&expr.expr),
        syn::Expr::Group(expr) => borrowed_tuple_initializer(&expr.expr),
        _ => false,
    }
}

fn unit_tuple_pattern(pattern: &syn::Pat) -> bool {
    match pattern {
        syn::Pat::Tuple(pattern) => pattern.elems.is_empty(),
        syn::Pat::Type(pattern) => unit_tuple_pattern(&pattern.pat),
        syn::Pat::Paren(pattern) => unit_tuple_pattern(&pattern.pat),
        _ => false,
    }
}

fn lower_tuple_let_bindings(pattern: &syn::Pat, value: Expr, out: &mut Vec<Stmt>) {
    match pattern {
        syn::Pat::Type(pattern) => lower_tuple_let_bindings(&pattern.pat, value, out),
        syn::Pat::Paren(pattern) => lower_tuple_let_bindings(&pattern.pat, value, out),
        syn::Pat::Wild(_) => {}
        syn::Pat::Ident(pattern) if pattern.by_ref.is_none() && pattern.subpat.is_none() => {
            out.push(Stmt::Let(pattern.ident.to_string(), None, value));
        }
        syn::Pat::Tuple(pattern) => {
            let rest = pattern
                .elems
                .iter()
                .position(|pattern| matches!(pattern, syn::Pat::Rest(_)));
            if pattern
                .elems
                .iter()
                .filter(|pattern| matches!(pattern, syn::Pat::Rest(_)))
                .count()
                > 1
            {
                out.push(Stmt::Expr(Expr::Ident(
                    "#unsupported-tuple-let-rest".into(),
                )));
                return;
            }
            out.push(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::tuples::LET_CHECK.into())),
                args: vec![
                    value.clone(),
                    Expr::IntLit((pattern.elems.len() - usize::from(rest.is_some())) as i64),
                    Expr::BoolLit(rest.is_some()),
                ],
            }));
            let len = pattern.elems.len();
            for (index, pattern) in pattern.elems.iter().enumerate() {
                if matches!(pattern, syn::Pat::Rest(_)) {
                    continue;
                }
                let from_end = rest.is_some_and(|rest| index > rest);
                let index = if from_end { len - index - 1 } else { index };
                let field = Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::tuples::LET_FIELD.into())),
                    args: vec![
                        value.clone(),
                        Expr::IntLit(index as i64),
                        Expr::BoolLit(from_end),
                    ],
                };
                lower_tuple_let_bindings(pattern, field, out);
            }
        }
        _ => out.push(Stmt::Expr(Expr::Ident(
            "#unsupported-tuple-let-pattern".into(),
        ))),
    }
}

fn lower_block_inner_with_types_go(
    block: &syn::Block,
    wrap_implicit_return: bool,
    local_types: &mut HashMap<String, String>,
) -> Vec<Stmt> {
    let mut out = Vec::new();
    for (index, stmt) in block.stmts.iter().enumerate() {
        match stmt {
            syn::Stmt::Local(local) => {
                if let Some(init) = &local.init {
                    if tuple_let_pattern(&local.pat) {
                        let mut names = HashSet::new();
                        if !tuple_let_names(&local.pat, &mut names)
                            || borrowed_tuple_initializer(&init.expr)
                        {
                            out.push(Stmt::Expr(Expr::Ident(
                                "#unsupported-tuple-let-pattern".into(),
                            )));
                            continue;
                        }
                        if unit_tuple_pattern(&local.pat)
                            && matches!(init.expr.as_ref(), syn::Expr::Tuple(tuple) if tuple.elems.is_empty())
                        {
                            continue;
                        }
                        let name = format!("#tuple-let-{}", out.len());
                        let value = if let syn::Expr::Try(value) = init.expr.as_ref() {
                            lower_expr_with_types(&value.expr, local_types)
                        } else {
                            lower_expr_with_types(&init.expr, local_types)
                        };
                        out.push(Stmt::Let(
                            name.clone(),
                            local_decl_type(&local.pat, local_types),
                            value,
                        ));
                        if matches!(init.expr.as_ref(), syn::Expr::Try(_)) {
                            out.push(Stmt::Propagate);
                        }
                        // A new binding must not inherit an earlier receiver type hint.
                        for name in names {
                            local_types.remove(&name);
                            local_types.insert(format!("{LENGTH_LOCAL}{name}"), String::new());
                        }
                        lower_tuple_let_bindings(&local.pat, Expr::Ident(name), &mut out);
                        continue;
                    }
                    // For destructuring patterns (Err(e), Some(x), etc.), use a temp name
                    let is_simple = pattern_name(&local.pat).is_some();
                    let name = if is_simple {
                        pattern_name(&local.pat).unwrap_or_else(|| "_pat".to_string())
                    } else {
                        "_pat".to_string()
                    };
                    // Infer type from annotation or constructor
                    if is_simple {
                        if let Some(ty) = local_decl_type(&local.pat, local_types) {
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
                        let typ = local_decl_type(&local.pat, local_types);
                        let value = lower_expr_with_types(&try_expr.expr, local_types);
                        local_types.insert(format!("{LENGTH_LOCAL}{name}"), String::new());
                        out.push(Stmt::Let(name, typ, value));
                        out.push(Stmt::Propagate);
                        continue;
                    }
                    let expr = lower_expr_with_types(&init.expr, local_types);
                    let local_ty = local_decl_type(&local.pat, local_types).or_else(|| {
                        matches!(
                            &expr,
                            Expr::Call { callee, .. }
                                if matches!(
                                    callee.as_ref(),
                                    Expr::Ident(name) if matches!(name.as_str(), "Vec::new" | "std::vec::Vec::new" | "alloc::vec::Vec::new" | "Vec::<_>::new" | "std::vec::Vec::<_>::new" | "alloc::vec::Vec::<_>::new" | "Vec::with_capacity" | "std::vec::Vec::with_capacity" | "alloc::vec::Vec::with_capacity" | "Vec::<_>::with_capacity" | "std::vec::Vec::<_>::with_capacity" | "alloc::vec::Vec::<_>::with_capacity")
                                )
                        )
                        .then(|| Typ::Vector(Box::new(Typ::Generic("_".to_string()))))
                    }).or_else(|| {
                        vector_value_initializer(&init.expr)
                            .then(|| Typ::Vector(Box::new(Typ::Generic("_".to_string()))))
                    });
                    if is_simple
                        && !local_types.contains_key(&name)
                        && let Some(typ) = local_ty.as_ref()
                    {
                        local_types.insert(name.clone(), type_to_string(typ));
                    }
                    local_types.insert(format!("{LENGTH_LOCAL}{name}"), String::new());
                    out.push(Stmt::Let(name, local_ty, expr));
                } else if let syn::Pat::Type(typed) = &local.pat
                    && let syn::Pat::Ident(binding) = typed.pat.as_ref()
                    && binding.by_ref.is_none()
                    && binding.subpat.is_none()
                {
                    let name = binding.ident.to_string();
                    let _length_shadows = LengthShadowScope::new(local_types);
                    let typ = map_type(&typed.ty);
                    local_types.insert(name.clone(), type_to_string(&typ));
                    local_types.insert(format!("{LENGTH_LOCAL}{name}"), String::new());
                    out.push(Stmt::Declare(name, typ));
                } else {
                    out.push(Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident(
                            crate::core_ir::UNSUPPORTED_DEFINED_BODY.into(),
                        )),
                        args: vec![],
                    }));
                }
            }
            syn::Stmt::Expr(expr, semi) => {
                if wrap_implicit_return && semi.is_none() && index + 1 == block.stmts.len() {
                    lower_tail_expr_with_types(expr, &mut out, local_types);
                } else {
                    lower_expr_stmt(expr, &mut out, local_types);
                }
            }
            syn::Stmt::Macro(m) => {
                let expr = syn::Expr::Macro(syn::ExprMacro {
                    attrs: vec![],
                    mac: m.mac.clone(),
                });
                if wrap_implicit_return && m.semi_token.is_none() && index + 1 == block.stmts.len()
                {
                    lower_tail_expr_with_types(&expr, &mut out, local_types);
                } else {
                    lower_expr_stmt(&expr, &mut out, local_types);
                }
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
    // A discarded expression does not provide the block's return value.
    if wrap_implicit_return && !all_paths_return(&out) {
        out.push(Stmt::Return(None));
    }
    out
}

/// Check if all code paths through the statements end with a return.
fn all_paths_return(stmts: &[Stmt]) -> bool {
    // Paths that fall through a conditional can still reach a later return.
    stmts.iter().any(stmt_ensures_return)
}

fn stmt_ensures_return(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return(_) | Stmt::Throw(_) => true,
        Stmt::If {
            cond: Expr::BoolLit(true),
            then_body,
            else_body,
        } if else_body.is_empty() => all_paths_return(then_body),
        Stmt::If {
            then_body,
            else_body,
            ..
        } => !else_body.is_empty() && all_paths_return(then_body) && all_paths_return(else_body),
        Stmt::Match { arms, .. } => {
            let irrefutable = arms.iter().filter(|arm| arm.guard.is_none()).any(|arm| {
                crate::core_ir::MatchPattern::parse(&arm.pattern)
                    .is_ok_and(|pattern| pattern.irrefutable_after_shape_check())
            });
            let bool_complete = ["true", "false"].iter().all(|value| {
                arms.iter()
                    .any(|arm| arm.guard.is_none() && arm.pattern.trim() == *value)
            });
            !arms.is_empty()
                && (irrefutable || bool_complete)
                && arms.iter().all(|arm| all_paths_return(&arm.body))
        }
        _ => false,
    }
}

fn lower_tail_expr_with_types(
    expr: &syn::Expr,
    out: &mut Vec<Stmt>,
    local_types: &mut HashMap<String, String>,
) {
    match expr {
        syn::Expr::If(value) => lower_if_with_types(value, true, out, local_types),
        syn::Expr::Match(value) => lower_match_with_types(value, true, out, local_types),
        syn::Expr::Block(value) => lower_scoped_block(&value.block, true, out, local_types),
        syn::Expr::Unsafe(value) => lower_scoped_block(&value.block, true, out, local_types),
        syn::Expr::Paren(value) => lower_tail_expr_with_types(&value.expr, out, local_types),
        syn::Expr::Group(value) => lower_tail_expr_with_types(&value.expr, out, local_types),
        syn::Expr::Return(value) => lower_return_expr(value.expr.as_deref(), out, local_types),
        syn::Expr::Tuple(value) if value.elems.is_empty() => out.push(Stmt::Return(None)),
        syn::Expr::ForLoop(_) | syn::Expr::While(_) | syn::Expr::Loop(_) | syn::Expr::Assign(_) => {
            lower_expr_stmt(expr, out, local_types);
            out.push(Stmt::Return(None));
        }
        syn::Expr::Binary(value) if assign_op_from_binop(&value.op).is_some() => {
            lower_expr_stmt(expr, out, local_types);
            out.push(Stmt::Return(None));
        }
        _ => lower_return_expr(Some(expr), out, local_types),
    }
}

fn lower_match_with_types(
    value: &syn::ExprMatch,
    tail: bool,
    out: &mut Vec<Stmt>,
    local_types: &mut HashMap<String, String>,
) {
    let scrutinee = lower_expr_with_types(&value.expr, local_types);
    let arms = value
        .arms
        .iter()
        .map(|arm| {
            let mut body = Vec::new();
            let mut inner = length_pattern_locals(&arm.pat, local_types);
            let guard = arm
                .guard
                .as_ref()
                .map(|(_, guard)| lower_expr_with_types(guard, &mut inner));
            if tail {
                lower_tail_expr_with_types(&arm.body, &mut body, &mut inner);
            } else {
                lower_expr_stmt(&arm.body, &mut body, &mut inner);
            }
            MatchArm {
                guard,
                pattern: arm.pat.to_token_stream().to_string(),
                body,
            }
        })
        .collect();
    out.push(Stmt::Match { scrutinee, arms });
}

fn lower_if_with_types(
    eif: &syn::ExprIf,
    tail: bool,
    out: &mut Vec<Stmt>,
    local_types: &mut HashMap<String, String>,
) {
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
                    body: lower_block_no_implicit_return_with_types(&eif.then_branch, local_types),
                }],
            });
            return;
        }
        // `if let Pat = Expr { ... } else { ... }`
        // Lower to: let _pat = Expr; match _pat { Pat => ..., _ => else }
        let scrutinee = lower_expr_with_types(&expr_let.expr, local_types);
        let then_body = lower_block_inner_with_types(
            &eif.then_branch,
            tail,
            &mut length_pattern_locals(&expr_let.pat, local_types),
        );
        let else_body = eif
            .else_branch
            .as_ref()
            .map(|(_tok, else_branch)| lower_else_body_with_types(else_branch, local_types, tail))
            .unwrap_or_default();
        let arms = vec![
            MatchArm {
                guard: None,
                pattern: expr_let.pat.to_token_stream().to_string(),
                body: then_body,
            },
            MatchArm {
                guard: None,
                pattern: "_".to_string(),
                body: else_body,
            },
        ];
        out.push(Stmt::Match { scrutinee, arms });
    } else {
        let cond = lower_expr_with_types(&eif.cond, local_types);
        let then_body =
            lower_block_inner_with_types(&eif.then_branch, tail, &mut local_types.clone());
        let else_body = eif
            .else_branch
            .as_ref()
            .map(|(_tok, else_branch)| lower_else_body_with_types(else_branch, local_types, tail))
            .unwrap_or_default();
        out.push(Stmt::If {
            cond,
            then_body,
            else_body,
        });
    }
}

fn lower_expr_stmt(
    expr: &syn::Expr,
    out: &mut Vec<Stmt>,
    local_types: &mut HashMap<String, String>,
) {
    match expr {
        syn::Expr::Paren(value) => lower_expr_stmt(&value.expr, out, local_types),
        syn::Expr::Group(value) => lower_expr_stmt(&value.expr, out, local_types),
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
        syn::Expr::If(eif) => lower_if_with_types(eif, false, out, local_types),
        syn::Expr::ForLoop(f) => {
            let mut inner = length_pattern_locals(&f.pat, local_types);
            let mut prefix = Vec::new();
            let binding = if tuple_let_pattern(&f.pat) {
                let mut names = HashSet::new();
                if !tuple_let_names(&f.pat, &mut names) {
                    out.push(Stmt::Expr(Expr::Ident(
                        "#unsupported-tuple-let-pattern".into(),
                    )));
                    return;
                }
                let binding = format!("#tuple-for-{}", out.len());
                lower_tuple_let_bindings(&f.pat, Expr::Ident(binding.clone()), &mut prefix);
                binding
            } else {
                match f.pat.as_ref() {
                    syn::Pat::Ident(pattern) => pattern.ident.to_string(),
                    pattern => pattern.to_token_stream().to_string(),
                }
            };
            prefix.extend(lower_block_no_implicit_return_with_types(
                &f.body, &mut inner,
            ));
            out.push(Stmt::Loop {
                kind: LoopKind::For { binding },
                cond: Some(lower_expr_with_types(&f.expr, local_types)),
                body: prefix,
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
        syn::Expr::Match(value) => lower_match_with_types(value, false, out, local_types),
        syn::Expr::Block(value) => lower_scoped_block(&value.block, false, out, local_types),
        syn::Expr::Unsafe(value) => lower_scoped_block(&value.block, false, out, local_types),
        syn::Expr::Assign(assign) => assignment::lower_statement(assign, out, local_types),
        syn::Expr::Binary(assign) if assign_op_from_binop(&assign.op).is_some() => {
            assignment::lower_compound(assign, out, local_types)
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
    if matches!(
        expr,
        syn::Expr::If(_)
            | syn::Expr::Match(_)
            | syn::Expr::Block(_)
            | syn::Expr::Unsafe(_)
            | syn::Expr::Paren(_)
            | syn::Expr::Group(_)
    ) || matches!(expr, syn::Expr::Tuple(value) if value.elems.is_empty())
    {
        lower_tail_expr_with_types(expr, out, local_types);
        return;
    }
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
    tail: bool,
) -> Vec<Stmt> {
    let mut out = Vec::new();
    let mut inner = local_types.clone();
    if tail {
        lower_tail_expr_with_types(else_branch, &mut out, &mut inner);
    } else {
        lower_expr_stmt(else_branch, &mut out, &mut inner);
    }
    out
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
        syn::Expr::Path(_) => rust_path_name(lhs),
        syn::Expr::Field(_) => None, // field assignments handled in Expr::Assign branch
        syn::Expr::Index(i) => Some(i.to_token_stream().to_string()),
        _ => None,
    }
}

fn local_decl_type(pat: &syn::Pat, locals: &HashMap<String, String>) -> Option<Typ> {
    let _length_shadows = LengthShadowScope::new(locals);
    match pat {
        syn::Pat::Type(pt) => Some(map_type(&pt.ty)),
        syn::Pat::Ident(_) => None,
        syn::Pat::Reference(r) => local_decl_type(&r.pat, locals),
        _ => None,
    }
}

thread_local! {
    static EXPR_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn lower_expr_with_types(expr: &syn::Expr, local_types: &mut HashMap<String, String>) -> Expr {
    lower_expr_in_position(expr, local_types, false)
}

fn lower_expr_in_position(
    expr: &syn::Expr,
    local_types: &mut HashMap<String, String>,
    call_target: bool,
) -> Expr {
    let _length_shadows = LengthShadowScope::new(local_types);
    // Transparent syntax wrappers do not consume structural recursion budget.
    let mut expr = expr;
    loop {
        expr = match expr {
            syn::Expr::Paren(value) => &value.expr,
            syn::Expr::Group(value) => &value.expr,
            _ => break,
        };
    }
    let depth = EXPR_DEPTH.with(|d| {
        let n = d.get();
        d.set(n.saturating_add(1));
        n
    });
    if depth > 64 {
        EXPR_DEPTH.with(|d| d.set(depth));
        return Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::RUST_EXPRESSION_DEPTH.into())),
            args: Vec::new(),
        };
    }
    let out = lower_expr_with_types_inner(expr, local_types, call_target);
    EXPR_DEPTH.with(|d| d.set(depth));
    out
}

/// Rust directly cast decimal literals retain target precision, including
/// parentheses and unary negation. An intervening binding or arithmetic
/// expression instead has its independently inferred source width.
fn direct_float_literal_cast(
    source: &syn::Expr,
    target: crate::core_ir::rust_floats::Kind,
) -> Option<Expr> {
    fn unparenthesized(mut source: &syn::Expr) -> &syn::Expr {
        loop {
            source = match source {
                syn::Expr::Paren(value) => &value.expr,
                syn::Expr::Group(value) => &value.expr,
                _ => return source,
            };
        }
    }
    let source = unparenthesized(source);
    let (source, negative) = match source {
        syn::Expr::Unary(value) if matches!(value.op, syn::UnOp::Neg(_)) => {
            (unparenthesized(&value.expr), true)
        }
        _ => (source, false),
    };
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Float(value),
        ..
    }) = source
    else {
        return None;
    };
    if !value.suffix().is_empty() {
        return None;
    }
    let value = crate::core_ir::rust_floats::literal(value.base10_digits(), Some(target));
    Some(if negative {
        Expr::Unary {
            op: crate::core_ir::rust_references::NEGATE_VALUE.into(),
            expr: Box::new(value),
        }
    } else {
        value
    })
}

fn lower_expr_with_types_inner(
    expr: &syn::Expr,
    local_types: &mut HashMap<String, String>,
    call_target: bool,
) -> Expr {
    match expr {
        syn::Expr::Lit(l) => match &l.lit {
            syn::Lit::Int(i) if matches!(i.suffix(), "f32" | "f64") => {
                let token = i.to_token_stream().to_string();
                if ["0x", "0o", "0b"]
                    .iter()
                    .any(|prefix| token.starts_with(prefix))
                {
                    return Expr::Ident(token);
                }
                crate::core_ir::rust_floats::literal(
                    i.base10_digits(),
                    crate::core_ir::rust_floats::Kind::parse(i.suffix()),
                )
            }
            syn::Lit::Int(i) => {
                let suffix = i.suffix();
                if suffix.is_empty()
                    && i.base10_parse::<i64>().is_err()
                    && let Ok(magnitude) = i.base10_parse::<u64>()
                {
                    // Retain the positive magnitude without assigning an unsigned source type.
                    return Expr::Unary {
                        op: "rust-integer-literal-magnitude".into(),
                        expr: Box::new(Expr::IntLit(magnitude as i64)),
                    };
                }
                let value = if suffix == "u64" {
                    i.base10_parse::<u64>().map(|value| value as i64).ok()
                } else {
                    i.base10_parse::<i64>().ok()
                };
                match value {
                    Some(value)
                        if suffix.is_empty()
                            || matches!(
                                suffix,
                                "i8" | "u8"
                                    | "i16"
                                    | "u16"
                                    | "i32"
                                    | "u32"
                                    | "i64"
                                    | "u64"
                                    | "isize"
                                    | "usize"
                            ) =>
                    {
                        rust_integer_literal(value, suffix)
                    }
                    _ => Expr::Ident(format!(
                        "#unsupported-rust-integer-literal:{}",
                        i.to_token_stream()
                    )),
                }
            }
            syn::Lit::Float(value) => {
                if !matches!(value.suffix(), "" | "f32" | "f64") {
                    return Expr::Ident(value.to_token_stream().to_string());
                }
                crate::core_ir::rust_floats::literal(
                    value.base10_digits(),
                    crate::core_ir::rust_floats::Kind::parse(value.suffix()),
                )
            }
            syn::Lit::Bool(b) => Expr::BoolLit(b.value),
            syn::Lit::Str(s) => Expr::StringLit(s.value()),
            _ => Expr::Ident(l.lit.to_token_stream().to_string()),
        },
        syn::Expr::Path(p) => {
            let scoped_path = match scoped_vector_path(&p.path) {
                Ok(path) => path,
                Err(proof) => return Expr::Ident(proof.symbol()),
            };
            if p.qself.is_none()
                && let Some(last) = scoped_path.segments.last()
                && let syn::PathArguments::AngleBracketed(arguments) = &last.arguments
            {
                let types = arguments.args.iter().map(|argument| match argument {
                    syn::GenericArgument::Type(typ) => generic_argument_type(typ),
                    _ => Err(crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                        "explicit lifetime and const arguments require generic constraint resolution".into(),
                    )),
                }).collect::<Result<Vec<_>, _>>();
                let types = match types {
                    Ok(types) => types,
                    Err(failure) => return Expr::Ident(failure.symbol()),
                };
                let mut owner = scoped_path.clone();
                owner.segments.last_mut().unwrap().arguments = syn::PathArguments::None;
                return Expr::Unary {
                    op: crate::core_ir::rust_generics::arguments(&types),
                    expr: Box::new(Expr::Ident(compact_rust_tokens(&owner))),
                };
            }
            let path = compact_rust_tokens(&scoped_path);
            Expr::Ident(if let Some(receiver) = &p.qself {
                // Match the receiver/trait identity used for impl declarations, including UFCS.
                format!("{}::{path}", compact_rust_tokens(&receiver.ty))
            } else {
                rust_integer_constant(&p.path, call_target).unwrap_or(path)
            })
        }
        syn::Expr::Reference(r) => Expr::Unary {
            op: if r.mutability.is_some() {
                crate::core_ir::rust_references::MUTABLE_VALUE
            } else {
                crate::core_ir::rust_references::SHARED_VALUE
            }
            .into(),
            // Preserve the existing place/spill preparation, independently of
            // the source proof which must survive argument substitution.
            expr: Box::new(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::BORROW.into())),
                args: vec![lower_expr_with_types(&r.expr, local_types)],
            }),
        },
        syn::Expr::Paren(p) => lower_expr_with_types(&p.expr, local_types),
        syn::Expr::Tuple(tuple) if tuple.elems.is_empty() => Expr::UnitLit,
        syn::Expr::Array(array) => crate::core_ir::rust_arrays::value(
            Expr::ArrayLit(
                array
                    .elems
                    .iter()
                    .map(|item| lower_expr_with_types(item, local_types))
                    .collect(),
            ),
            array.elems.len(),
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
        syn::Expr::Macro(mac) => opaque_macro_dependencies(&mac.mac, local_types),
        syn::Expr::Closure(closure) => {
            let mut closure_types = local_types.clone();
            let params = closure
                .inputs
                .iter()
                .map(|input| {
                    let name = pattern_name(input).unwrap_or_else(|| "_".to_string());
                    let typ = local_decl_type(input, &closure_types)
                        .unwrap_or_else(|| Typ::Generic("_".to_string()));
                    closure_types.insert(name.clone(), type_to_string(&typ));
                    closure_types.insert(format!("{LENGTH_LOCAL}{name}"), String::new());
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
                ret: match &closure.output {
                    syn::ReturnType::Default => Typ::Generic("_".to_string()),
                    syn::ReturnType::Type(_, ty) => map_type(ty),
                },
                body,
                captures: vec![],
            }
        }
        syn::Expr::Call(c) => {
            let callee = lower_expr_in_position(&c.func, local_types, true);
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
            let method_name = format!("{}{}", crate::core_ir::METHOD_PREFIX, m.method);
            Expr::Call {
                callee: Box::new(Expr::Ident(method_name)),
                args,
            }
        }
        syn::Expr::Unary(u)
            if matches!(u.op, syn::UnOp::Neg(_))
                && matches!(
                    u.expr.as_ref(),
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Int(_),
                        ..
                    })
                ) =>
        {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(literal),
                ..
            }) = u.expr.as_ref()
            else {
                unreachable!()
            };
            let suffix = literal.suffix();
            if suffix.is_empty() || matches!(suffix, "i8" | "i16" | "i32" | "i64" | "isize") {
                literal
                    .base10_parse::<i128>()
                    .ok()
                    .and_then(|value| value.checked_neg())
                    .and_then(|value| i64::try_from(value).ok())
                    .map(|value| rust_integer_literal(value, suffix))
                    .unwrap_or_else(|| {
                        Expr::Ident("#unsupported-rust-negative-integer-literal".into())
                    })
            } else {
                Expr::Unary {
                    op: crate::core_ir::rust_references::NEGATE_VALUE.into(),
                    expr: Box::new(lower_expr_with_types(&u.expr, local_types)),
                }
            }
        }
        syn::Expr::Unary(u) => Expr::Unary {
            op: match u.op {
                syn::UnOp::Not(_) => "complement".into(),
                syn::UnOp::Neg(_) => crate::core_ir::rust_references::NEGATE_VALUE.into(),
                _ => u.op.to_token_stream().to_string(),
            },
            expr: Box::new(lower_expr_with_types(&u.expr, local_types)),
        },
        syn::Expr::Binary(b) if assign_op_from_binop(&b.op).is_some() => {
            assignment::expression(expr, local_types)
        }
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
            let target_type = map_type(&cast.ty);
            if let Some(failure) = crate::core_ir::type_proofs::failure_in_type(&target_type) {
                return Expr::Ident(failure.symbol());
            }
            let floating_target = crate::core_ir::rust_floats::kind(&target_type);
            let target = cast.ty.to_token_stream().to_string().replace(' ', "");
            let target = target.strip_prefix("::").unwrap_or(&target);
            let target = target
                .strip_prefix("core::primitive::")
                .or_else(|| target.strip_prefix("std::primitive::"))
                .unwrap_or(target);
            let value = floating_target
                .and_then(|kind| direct_float_literal_cast(&cast.expr, kind))
                .unwrap_or_else(|| lower_expr_with_types(&cast.expr, local_types));
            let op = format!("int-cast-{target}");
            if crate::core_ir::integer_cast_spec(&op).is_some() {
                Expr::Unary {
                    // Retain the source integer result kind even when generic
                    // cast normalization erases its numeric-cast operation.
                    op: format!("rust-integer-value-{target}"),
                    expr: Box::new(Expr::Unary {
                        op: format!("numeric-cast-{target}"),
                        expr: Box::new(value),
                    }),
                }
            } else if let Some(kind) = floating_target {
                crate::core_ir::rust_floats::cast(value, kind)
            } else {
                Expr::Call {
                    callee: Box::new(Expr::Ident("unsupported-rust-cast".into())),
                    args: vec![value],
                }
            }
        }
        syn::Expr::Try(t) => lower_expr_with_types(&t.expr, local_types),
        syn::Expr::Await(a) => lower_expr_with_types(&a.base, local_types),
        syn::Expr::Async(a) => Expr::Closure {
            params: vec![],
            ret: Typ::Generic("_".into()),
            body: lower_block_with_types(&a.block, local_types),
            captures: vec![],
        },
        syn::Expr::If(eif) if !matches!(eif.cond.as_ref(), syn::Expr::Let(_)) => {
            Expr::Conditional {
                cond: Box::new(lower_expr_with_types(&eif.cond, local_types)),
                then_expr: Box::new(lower_block_value(&eif.then_branch, local_types)),
                else_expr: Box::new(
                    eif.else_branch
                        .as_ref()
                        .map(|(_, branch)| lower_expr_with_types(branch, &mut local_types.clone()))
                        .unwrap_or(Expr::Block {
                            body: vec![],
                            result: None,
                        }),
                ),
            }
        }
        syn::Expr::If(value) => {
            let syn::Expr::Let(binding) = value.cond.as_ref() else {
                unreachable!()
            };
            Expr::MatchValue {
                scrutinee: Box::new(lower_expr_with_types(&binding.expr, local_types)),
                arms: vec![
                    MatchValueArm {
                        pattern: binding.pat.to_token_stream().to_string(),
                        guard: None,
                        value: lower_block_value(&value.then_branch, local_types),
                    },
                    MatchValueArm {
                        pattern: "_".into(),
                        guard: None,
                        value: value
                            .else_branch
                            .as_ref()
                            .map(|(_, expr)| lower_match_arm_value(expr, local_types))
                            .unwrap_or(Expr::Block {
                                body: vec![],
                                result: None,
                            }),
                    },
                ],
            }
        }
        syn::Expr::Block(value) => lower_block_value(&value.block, local_types),
        syn::Expr::Unsafe(value) => lower_block_value(&value.block, local_types),
        syn::Expr::Repeat(repeat) => match fixed_array_length(&repeat.len) {
            Ok(Some(length)) => crate::core_ir::rust_arrays::value(
                Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::ARRAY_REPEAT.into())),
                    args: vec![
                        lower_expr_with_types(&repeat.expr, local_types),
                        Expr::IntLit(length as i64),
                    ],
                },
                length,
            ),
            Ok(None) => Expr::Unary {
                op: crate::core_ir::rust_arrays::PENDING_REPEAT.into(),
                expr: Box::new(Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::ARRAY_REPEAT.into())),
                    args: vec![
                        lower_expr_with_types(&repeat.expr, local_types),
                        lower_expr_with_types(&repeat.len, local_types),
                    ],
                }),
            },
            Err(failure) => Expr::Ident(failure.symbol()),
        },
        syn::Expr::Range(range) => {
            let inclusive = matches!(range.limits, syn::RangeLimits::Closed(_));
            let name = match (range.start.is_some(), range.end.is_some(), inclusive) {
                (true, true, false) => "range-exclusive",
                (true, true, true) => "range-inclusive",
                (true, false, false) => "range-from",
                (false, true, false) => "range-to",
                (false, true, true) => "range-to-inclusive",
                (false, false, false) => "range-full",
                _ => "range-invalid",
            };
            Expr::Call {
                callee: Box::new(Expr::Ident(name.into())),
                args: range
                    .start
                    .iter()
                    .chain(range.end.iter())
                    .map(|value| lower_expr_with_types(value, local_types))
                    .collect(),
            }
        }
        syn::Expr::Tuple(t) => Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::tuples::VALUE.into())),
            args: t
                .elems
                .iter()
                .map(|e| lower_expr_with_types(e, local_types))
                .collect(),
        },
        syn::Expr::Match(value) => Expr::MatchValue {
            scrutinee: Box::new(lower_expr_with_types(&value.expr, local_types)),
            arms: value
                .arms
                .iter()
                .map(|arm| {
                    let mut inner = length_pattern_locals(&arm.pat, local_types);
                    MatchValueArm {
                        pattern: arm.pat.to_token_stream().to_string(),
                        guard: arm
                            .guard
                            .as_ref()
                            .map(|(_, value)| lower_expr_with_types(value, &mut inner)),
                        value: lower_match_arm_value(&arm.body, &inner),
                    }
                })
                .collect(),
        },
        syn::Expr::Group(g) => lower_expr_with_types(&g.expr, local_types),
        syn::Expr::Const(c) => Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::INLINE_CONST.into())),
            args: vec![lower_block_value(&c.block, &HashMap::new())],
        },
        syn::Expr::Return(r) => r
            .expr
            .as_ref()
            .map(|inner| lower_expr_with_types(inner, local_types))
            .unwrap_or(Expr::IntLit(0)),
        syn::Expr::Assign(_) => assignment::expression(expr, local_types),
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
                    binding: match f.pat.as_ref() {
                        syn::Pat::Ident(pattern) => pattern.ident.to_string(),
                        pattern => pattern.to_token_stream().to_string(),
                    },
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
            name: nominal_path_name(&s.path),
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
    use crate::core_ir::FloatVal;

    #[test]
    fn unused_alias_declaration_failures_survive_source_artifact_assembly() {
        for source in [
            "type A=B;type B=A;fn main()->i64{42}",
            "fn unused(){fn nested(){type A=A;}}fn main()->i64{42}",
            "fn unused(){mod child{type A=A;}}fn main()->i64{42}",
            "type A=u8;type A=u16;",
        ] {
            let artifact = parse_rust_artifact_source(source, "aliases").unwrap();
            let error = crate::owned_compile::reject_invalid_type_proofs(&artifact.semantic)
                .expect_err(source);
            assert!(
                error.contains("type alias") || error.contains("duplicate type binding"),
                "{source}: {error}"
            );
        }
        let artifact = parse_rust_artifact_source(
            "#[cfg(any())]mod disabled{type A=A;}type A=u8;fn main()->i64{42}",
            "aliases",
        )
        .unwrap();
        assert!(crate::owned_compile::reject_invalid_type_proofs(&artifact.semantic).is_ok());
        assert!(!artifact.semantic.decls.iter().any(|decl| matches!(decl, Decl::Global { name, .. } if name == "#rust-source-alias-validation")));
    }

    #[test]
    fn unused_alias_declaration_failures_survive_parallel_external_modules() {
        let directory =
            std::env::temp_dir().join(format!("in-unused-alias-proof-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("nested")).unwrap();
        std::fs::write(directory.join("bad.rs"), "type A=B;type B=A;").unwrap();
        std::fs::write(directory.join("nested/mod.rs"), "pub mod child;").unwrap();
        std::fs::write(directory.join("nested/child.rs"), "fn unused(){type A=A;}").unwrap();
        let artifact = parse_rust_artifact_source_with_dir(
            "mod bad;mod nested;fn main()->i64{42}",
            "aliases",
            &directory,
        )
        .unwrap();
        let proofs =
            crate::typecheck::TypeChecker::new().typed_proof_nodes(&artifact.semantic.decls);
        assert!(
            proofs
                .get("bad::#rust-source-alias-validation")
                .is_some_and(crate::core_ir::type_proofs::ResolutionFailure::is_invalid),
            "{proofs:?}"
        );
        assert!(
            proofs
                .get("nested::child::#rust-source-alias-validation")
                .is_some_and(crate::core_ir::type_proofs::ResolutionFailure::is_invalid),
            "{proofs:?}"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn conditional_external_alias_declarations_preserve_all_ancestor_presence() {
        let directory =
            std::env::temp_dir().join(format!("in-conditional-alias-proof-{}", std::process::id()));
        std::fs::create_dir_all(directory.join("nested")).unwrap();
        std::fs::write(directory.join("child.rs"), "type A=A;").unwrap();
        std::fs::write(directory.join("nested/mod.rs"), "pub mod child;").unwrap();
        std::fs::write(directory.join("nested/child.rs"), "type A=A;").unwrap();
        std::fs::write(directory.join("good.rs"), "pub type A=u8;").unwrap();
        for source in [
            "#[cfg(feature=\"unknown\")]mod child;fn main()->i64{42}",
            "#![cfg(feature=\"unknown\")]mod child;fn main()->i64{42}",
            "#[cfg(feature=\"unknown\")]mod parent{mod child;}fn main()->i64{42}",
            "#[cfg(feature=\"unknown\")]mod nested;mod good;fn main()->i64{42}",
        ] {
            let artifact =
                parse_rust_artifact_source_with_dir(source, "aliases", &directory).unwrap();
            assert!(
                crate::owned_compile::reject_invalid_type_proofs(&artifact.semantic).is_ok(),
                "{source}: {:?}",
                artifact.semantic
            );
        }
        std::fs::write(directory.join("child.rs"), "#![cfg(any())]type A=A;").unwrap();
        let artifact = parse_rust_artifact_source_with_dir(
            "mod child;fn main()->i64{42}",
            "aliases",
            &directory,
        )
        .unwrap();
        assert!(crate::owned_compile::reject_invalid_type_proofs(&artifact.semantic).is_ok());
        std::fs::write(directory.join("child.rs"), "type A=A;").unwrap();
        let artifact = parse_rust_artifact_source_with_dir(
            "mod child;fn main()->i64{42}",
            "aliases",
            &directory,
        )
        .unwrap();
        assert!(crate::owned_compile::reject_invalid_type_proofs(&artifact.semantic).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn block_value_imports_resolve_calls_without_losing_tail_bindings() {
        let module = parse_rust_source(
            "mod values{pub fn Byte()->i64{7}}type Byte=u8;fn main()->i64{{use values::Byte;let answer=Byte();if answer==7{Byte::wrapping_add(255,43) as i64}else{0}}}",
        ).unwrap();
        let mut calls = Vec::new();
        for declaration in &module.decls {
            if let Decl::Function { name, body, .. } = declaration {
                if name == "main" {
                    let mut expression = Expr::Block {
                        body: body.clone(),
                        result: None,
                    };
                    crate::core_opt::map_expr_mut(&mut expression, &mut |value| {
                        if let Expr::Call { callee, .. } = value {
                            if let Expr::Ident(name) = callee.as_ref() {
                                calls.push(name.clone());
                            }
                        }
                    });
                }
            }
        }
        assert!(calls.iter().any(|name| name == "values::Byte"), "{calls:?}");
        assert!(!calls.iter().any(|name| name == "Byte"), "{calls:?}");
        assert!(
            crate::typecheck::TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty()
        );
    }

    #[test]
    fn conditional_source_function_calls_retain_unsupported_proof() {
        let module = parse_rust_source(
            "#[cfg(feature=\"unknown\")]fn helper()->i64{42}fn main()->i64{helper()}",
        )
        .unwrap();
        let proofs = crate::typecheck::TypeChecker::new().typed_proof_nodes(&module.decls);
        assert!(!proofs.is_empty(), "{module:?}");
        assert!(
            proofs.iter().all(|(_, proof)| !proof.is_invalid()),
            "{proofs:?}"
        );
    }

    #[test]
    fn source_function_access_preserves_private_descendant_calls_and_local_values() {
        for source in [
            "mod hidden{fn helper()->i64{42}pub mod child{pub fn answer()->i64{crate::hidden::helper()}}}fn main()->i64{hidden::child::answer()}",
            "fn helper()->i64{1}fn main()->i64{let helper=42;helper}",
        ] {
            let module = parse_rust_source(source).unwrap();
            assert!(
                crate::typecheck::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{module:?}"
            );
        }
    }

    #[test]
    fn known_crate_root_resolves_exact_function_paths_without_rebinding_module_units() {
        let source = "mod shared{pub fn helper()->i64{42}}fn main()->i64{crate::shared::helper()}";
        let mut root = parse_rust_source(source).unwrap();
        let mut calls = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut root.decls, &mut |expr| {
            if let Expr::Call { callee, .. } = expr
                && let Expr::Ident(name) = callee.as_ref()
            {
                calls.push(name.clone());
            }
        });
        assert_eq!(calls, ["shared::helper"]);
        let isolated = parse_rust_source_unit(source, "module", Path::new("."), false).unwrap();
        assert!(format!("{:?}", isolated.semantic).contains("crate::shared::helper"));
    }

    #[test]
    fn lazily_parsed_module_calls_keep_dependency_paths_and_type_owner_proofs() {
        let directory = std::env::temp_dir().join(format!("in-value-owner-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("alpha.rs");
        std::fs::write(&path, "pub fn answer()->i64{crate::shared::helper()}").unwrap();
        let mut module = parse_rust_module_artifact(&path).unwrap().semantic;
        let mut calls = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Call { callee, .. } = expr {
                if let Expr::Ident(name) = callee.as_ref() {
                    calls.push(name.clone());
                }
            }
        });
        assert_eq!(calls, vec!["crate::shared::helper"]);
        std::fs::write(&path, "type Unknown=crate::other::Value;type Cycle=Cycle;type Duplicate=u8;type Duplicate=u16;pub fn a()->i64{Unknown::ordinary()}pub fn b()->i64{Cycle::ordinary()}pub fn c()->i64{Duplicate::ordinary()}pub fn d()->i64{u64::rotate_left(1,2) as i64}").unwrap();
        let mut module = parse_rust_module_artifact(&path).unwrap().semantic;
        let mut proofs = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Call { callee, .. } = expr {
                if let Expr::Ident(name) = callee.as_ref() {
                    if let Some(proof) = crate::core_ir::type_proofs::decode(name) {
                        proofs.push(proof);
                    }
                }
            }
        });
        assert_eq!(proofs.len(), 4, "{proofs:?}");
        assert_eq!(proofs.iter().filter(|proof| proof.is_invalid()).count(), 2);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn block_callable_prefixes_use_module_not_value_namespace() {
        for source in [
            "mod values{pub fn answer()->i64{42}}mod functions{pub fn values(){}}fn main()->i64{{use functions::values;use values::answer as Second;Second()}}",
            "mod values{pub fn answer()->i64{42}}fn main()->i64{{use values as First;use First as Alias;use Alias::answer as Second;Second()}}",
        ] {
            let module = parse_rust_source(source).unwrap();
            let Decl::Function { body, .. } = module.decls.iter().find(|declaration| {
                matches!(declaration, Decl::Function { name, .. } if name == "main")
            }).unwrap() else { panic!("main") };
            let mut expression = Expr::Block {
                body: body.clone(),
                result: None,
            };
            let mut calls = Vec::new();
            crate::core_opt::map_expr_mut(&mut expression, &mut |value| {
                if let Expr::Call { callee, .. } = value {
                    if let Expr::Ident(name) = callee.as_ref() {
                        calls.push(name.clone());
                    }
                }
            });
            assert_eq!(calls, vec!["values::answer"], "{source}");
        }
    }

    #[test]
    fn block_callable_import_chains_resolve_before_value_rewriting() {
        let source = "mod values{pub fn answer()->i64{42}}fn First()->i64{7}fn main()->i64{use values::answer as First;use First as Second;Second()}";
        let module = parse_rust_source(source).unwrap();
        let Decl::Function { body, .. } = module
            .decls
            .iter()
            .find(
                |declaration| matches!(declaration, Decl::Function { name, .. } if name == "main"),
            )
            .unwrap()
        else {
            panic!("main")
        };
        let mut expression = Expr::Block {
            body: body.clone(),
            result: None,
        };
        let mut calls = Vec::new();
        crate::core_opt::map_expr_mut(&mut expression, &mut |value| {
            if let Expr::Call { callee, .. } = value {
                if let Expr::Ident(name) = callee.as_ref() {
                    calls.push(name.clone());
                }
            }
        });
        assert_eq!(calls, vec!["values::answer"]);
        let absolute: syn::Block = syn::parse_quote!({
            use ::values::answer as First;
            use First as Second;
        });
        assert_eq!(
            block_import_references(&absolute)["Second"],
            "::values::answer"
        );
        let cycle: syn::Block = syn::parse_quote!({
            use First as Second;
            use Second as First;
        });
        let references = block_import_references(&cycle);
        assert!(
            references
                .values()
                .all(|name| { crate::core_ir::type_proofs::decode(name).is_some() })
        );
        assert!(
            crate::typecheck::TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty()
        );
    }

    #[test]
    fn block_callable_imports_retain_private_target_failures() {
        let module = parse_rust_source(
            "mod values{fn Byte()->i64{7}}fn main()->i64{{use values::Byte;Byte()}}",
        )
        .unwrap();
        let proofs = crate::typecheck::TypeChecker::new().typed_proof_nodes(&module.decls);
        assert!(
            proofs.values().any(|proof| proof.is_invalid()),
            "{proofs:?}"
        );
    }

    #[test]
    fn block_import_rewriting_obeys_local_shadowing_and_nested_scope() {
        let source: syn::Block = syn::parse_quote!({
            use values::Byte;
            let Byte = 9;
            Byte
        });
        let value = lower_block_value(&source, &HashMap::new());
        let Expr::Block {
            body,
            result: Some(result),
        } = value
        else {
            panic!("block value")
        };
        let Stmt::Let(name, _, _) = &body[0] else {
            panic!("local binding")
        };
        assert_eq!(result.as_ref(), &Expr::Ident(name.clone()));
        assert_ne!(name, "Byte");
        let sibling: syn::Block = syn::parse_quote!({ Byte() });
        let sibling = lower_block_value(&sibling, &HashMap::new());
        assert!(format!("{sibling:?}").contains("Byte"));
        assert!(!format!("{sibling:?}").contains("values::Byte"));
        let closure: syn::Block = syn::parse_quote!({
            use values::Byte;
            let callback = |Byte: i64| Byte;
            callback(9)
        });
        let closure = lower_block_value(&closure, &HashMap::new());
        let Expr::Block { body, .. } = closure else {
            panic!("closure block")
        };
        let Stmt::Let(_, _, Expr::Closure { params, body, .. }) = &body[0] else {
            panic!("closure binding")
        };
        assert_eq!(
            body,
            &vec![Stmt::Return(Some(Expr::Ident(params[0].0.clone())))]
        );
        assert_ne!(params[0].0, "Byte");
    }

    #[test]
    fn known_cfg_aliases_and_value_namespace_imports_preserve_source_integer_facts() {
        for source in [
            "#[cfg(any())]type Byte=u16;#[cfg(not(any()))]type Byte=u8;fn main()->i64{if Byte::wrapping_add(255u8,1u8)==0u8{42}else{7}}",
            "mod values{pub type Byte=u16;}#[cfg(any())]use values::Byte;type Byte=u8;fn main()->i64{if Byte::MAX==255u8{42}else{7}}",
            "mod values{pub fn Byte()->i64{7}}use values::Byte;type Byte=u8;fn main()->i64{if Byte::wrapping_add(255u8,1u8)==0u8 && Byte()==7{42}else{7}}",
            "mod values{pub fn Byte()->i64{7}}type Byte=u8;fn main()->i64{use crate::values::Byte;if Byte::MAX==255u8 && Byte()==7{42}else{7}}",
        ] {
            let module = parse_rust_source(source).unwrap();
            let module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
            let proofs = crate::typecheck::TypeChecker::new().typed_proof_nodes(&module.decls);
            assert!(proofs.is_empty(), "{source}: {proofs:?}");
        }
        let module = parse_rust_source(
            "#[cfg(any())]mod disabled{type A=A;}type Byte=u8;fn main()->Byte{Byte::MAX}",
        )
        .unwrap();
        assert!(
            !module
                .decls
                .iter()
                .any(|decl| matches!(decl,Decl::Struct{name,..} if name.starts_with("disabled::")))
        );
    }

    #[test]
    fn external_module_imports_do_not_claim_an_independent_crate_root() {
        let directory =
            std::env::temp_dir().join(format!("in-alias-module-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("algorithm.rs"), "pub struct Algorithm;").unwrap();
        let consumer = "use crate::Algorithm;pub fn from(value:Algorithm)->i64{42}";
        let consumer_path = directory.join("consumer.rs");
        std::fs::write(&consumer_path, consumer).unwrap();
        let root = "mod algorithm;mod consumer;pub use algorithm::Algorithm;fn main()->i64{42}";
        let artifact = parse_rust_artifact_source_with_dir(root, "root", &directory).unwrap();
        let proofs =
            crate::typecheck::TypeChecker::new().typed_proof_nodes(&artifact.semantic.decls);
        assert!(!proofs.is_empty());
        assert!(
            proofs.values().all(|proof| !proof.is_invalid()),
            "{proofs:?}"
        );
        let isolated = parse_rust_module_artifact(&consumer_path).unwrap();
        let isolated_proofs =
            crate::typecheck::TypeChecker::new().typed_proof_nodes(&isolated.semantic.decls);
        assert!(
            isolated_proofs
                .values()
                .any(|proof| proof.message().contains("crate root is outside")),
            "{isolated_proofs:?}"
        );
        assert!(isolated_proofs.values().all(|proof| !proof.is_invalid()));
        let rooted =
            parse_rust_source("type A=crate::B;type B=A;fn main()->i64{let value:A;42}").unwrap();
        assert!(
            crate::typecheck::TypeChecker::new()
                .typed_proof_nodes(&rooted.decls)
                .values()
                .any(|proof| proof.is_invalid())
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn primitive_aliases_share_annotation_and_value_identity_without_fake_structs() {
        let module = parse_rust_source("type Byte=Next;type Next=u8;fn main()->Byte{let value:Byte;Byte::wrapping_add(Byte::MAX,43u8)}").unwrap();
        assert!(
            !module
                .decls
                .iter()
                .any(|decl| matches!(decl,Decl::Struct{name,..} if name=="Byte"||name=="Next"))
        );
        let Decl::Function { ret, body, .. } = &module.decls[0] else {
            panic!()
        };
        assert_eq!(ret, &Typ::Named("#rust-integer-u8".into()));
        assert!(matches!(&body[0],Stmt::Declare(_,Typ::Named(name)) if name=="#rust-integer-u8"));
        let mut calls = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.clone().decls, &mut |expr| {
            if let Expr::Ident(name) = expr {
                calls.push(name.clone());
            }
        });
        assert!(
            calls
                .iter()
                .any(|name| name == "#rust-integer-u8::wrapping_add")
        );
        assert!(calls.iter().any(|name| name == "#rust-integer-u8::MAX"));
    }
    #[test]
    fn used_alias_cycles_stay_on_real_uninitialized_declarations() {
        let module =
            parse_rust_source("type A=B;type B=A;fn main()->i64{let unused:A;42}").unwrap();
        let declaration = module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function { body, .. } => Some(&body[0]),
                _ => None,
            })
            .unwrap();
        assert!(
            matches!(declaration,Stmt::Declare(_,Typ::Named(name)) if matches!(crate::core_ir::type_proofs::decode(name),Some(crate::core_ir::type_proofs::ResolutionFailure::Invalid(_))))
        );
    }
    #[test]
    fn generic_vector_type_arguments_do_not_rebind_to_file_aliases_in_postpass() {
        let module=parse_rust_source("type Byte=u8;fn generic<Byte>(){Vec::<Byte>::new();}fn concrete(){Vec::<Byte>::new();}").unwrap();
        let mut identities = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.clone().decls, &mut |expr| {
            if let Expr::Ident(name) = expr {
                identities.push(name.clone());
            }
        });
        assert!(
            identities
                .iter()
                .any(|name| name.contains("Vec::<Byte>::new")),
            "{identities:?}"
        );
        assert!(
            !identities
                .iter()
                .any(|name| name.contains("Vec::<Byte>::new") && name.contains("u8"))
        );
    }
    #[test]
    fn terminal_result_errors_do_not_add_void_returns() {
        let module = parse_rust_source("fn fail()->Result<i64,i64> { Err(1) }").unwrap();
        let Decl::Function { body, .. } = &module.decls[0] else {
            unreachable!()
        };
        assert_eq!(body.as_slice(), &[Stmt::Throw(rust_integer_literal(1, ""))]);
    }

    #[test]
    fn nominal_types_preserve_paths_and_generic_arguments() {
        let int: syn::Type = syn::parse_str("OnceLock<i64>").unwrap();
        let boolean: syn::Type = syn::parse_str("OnceLock<bool>").unwrap();
        assert_ne!(map_type(&int), map_type(&boolean));
        let left: syn::Type = syn::parse_str("left::State").unwrap();
        let right: syn::Type = syn::parse_str("right::State").unwrap();
        assert_ne!(map_type(&left), map_type(&right));
        let nested: syn::Type = syn::parse_str(
            "std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, State>>>",
        )
        .unwrap();
        assert_eq!(
            map_type(&nested),
            Typ::Named(
                "std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, State>>>"
                    .into()
            )
        );
    }

    #[test]
    fn verifier_rejects_erased_generic_argument_mismatches() {
        let parsed = parse_rust_source("fn take(value:OnceLock<i64>)->i64 { 42 } fn wrong(value:OnceLock<bool>)->i64 { take(value) }").unwrap();
        let module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        let report = crate::core_ir_verifier::verify_for_entry(&module, "wrong");
        assert!(
            !report.ok,
            "distinct generic arguments must not share a call ABI"
        );
        assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
    }

    #[test]
    fn qualified_struct_initializers_match_parameter_types() {
        let module = parse_rust_source("mod alpha { pub struct Point { pub value:i64 } } fn answer(point:alpha::Point)->i64 { point.value } fn main()->i64 { answer(alpha::Point { value:42 }) }").unwrap();
        let Decl::Function { params, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..} if name=="answer"))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(params[0].1, Typ::Named("alpha::Point".into()));
        let Decl::Function { body, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..} if name=="main"))
            .unwrap()
        else {
            unreachable!()
        };
        let Stmt::Return(Some(Expr::Call { args, .. })) = body.last().unwrap() else {
            panic!("expected constructor argument");
        };
        assert!(matches!(&args[0], Expr::StructInit{name,..} if name=="alpha::Point"));
    }

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
    fn lowers_unit_tuple_without_numeric_coercion() {
        let expr: syn::Expr = syn::parse_str("()").expect("parse unit tuple");
        assert_eq!(
            lower_expr_with_types(&expr, &mut HashMap::new()),
            Expr::UnitLit
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
        assert_eq!(*ret, Typ::Named("#rust-integer-i64".into()));
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
                if **elem == Typ::Named("#rust-integer-i64".into()) && items == &vec![rust_integer_literal(1, ""), rust_integer_literal(2, "")]
        ));
    }

    #[test]
    fn lowers_fixed_string_array_return() {
        let module = parse_rust_source("fn names() -> [&'static str; 1] { [\"step\"] }")
            .expect("parse fixed string array");
        let Decl::Function { ret, body, .. } = &module.decls[0] else {
            panic!("main must be a function");
        };
        assert_eq!(*ret, crate::core_ir::rust_arrays::typ(Typ::String, 1));
        assert!(
            matches!(body.last(), Some(Stmt::Return(Some(Expr::Unary { op, expr })))
            if crate::core_ir::rust_arrays::value_length(op) == Some(1)
                && matches!(expr.as_ref(), Expr::ArrayLit(items) if items.len() == 1))
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
            matches!(body.first(), Some(Stmt::Let(_, _, Expr::Closure { params, body, .. })) if params == &vec![("value".to_string(), Typ::Named("#rust-integer-i64".into()))] && matches!(body.last(), Some(Stmt::Return(Some(Expr::Binary { .. })))))
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
    fn primitive_aliases_preserve_source_boundary_widths_and_signatures() {
        let direct = parse_rust_artifact_source(
            "#[repr(C)]struct Row{first:u8,second:u8}#[no_mangle]extern \"C\" fn byte(value:u8)->u8{value}",
            "alias-boundary",
        ).unwrap().boundary.unwrap();
        let aliased = parse_rust_artifact_source(
            "type Byte=Next;type Next=core::primitive::u8;#[repr(C)]struct Row{first:u8,second:Byte}#[no_mangle]extern \"C\" fn byte(value:Byte)->Byte{value}",
            "alias-boundary",
        ).unwrap().boundary.unwrap();
        assert_eq!(direct.layout_hash, aliased.layout_hash);
        assert_eq!(
            direct.symbols[0].signature_hash,
            aliased.symbols[0].signature_hash
        );
        let row = &aliased.layouts[0];
        assert_eq!((row.size, row.align), (2, 1));
        assert_eq!(
            row.fields
                .iter()
                .map(|field| field.offset)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
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
    fn inline_constants_keep_a_reserved_scoped_evaluation_boundary() {
        for source in [
            "const { 40 + 2 }",
            "const { return 42; }",
            "const { break; }",
            "const { runtime }",
        ] {
            let parsed: syn::Expr = syn::parse_str(source).unwrap();
            let value = lower_expr_with_types(
                &parsed,
                &mut HashMap::from([("runtime".into(), "Int".into())]),
            );
            let Expr::Call { callee, args } = value else {
                panic!("expected inline constant marker");
            };
            assert_eq!(*callee, Expr::Ident(crate::core_ir::INLINE_CONST.into()));
            assert!(matches!(args.as_slice(), [Expr::Block { .. }]));
            if source.contains("return") {
                assert!(
                    matches!(&args[0], Expr::Block { body, result: None } if body.as_slice() == [Stmt::Return(Some(rust_integer_literal(42, "")))])
                );
            }
            if source.contains("runtime") {
                assert!(
                    matches!(&args[0], Expr::Block { result: Some(result), .. } if matches!(result.as_ref(), Expr::Ident(name) if name == "runtime"))
                );
            }
        }
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
                .any(|d| matches!(d, Decl::Global { name, typ: Typ::Named(typ), mutable: false, constant_value: true, init: Some(value) } if name == "N" && typ == "#rust-integer-i64" && value.as_ref() == &rust_integer_literal(4, "")))
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
    fn tuple_let_patterns_reject_wrong_shapes_and_reference_bindings() {
        for source in [
            "fn main()->i64 {let(a,b)=(42,);a+b}",
            "fn main()->i64 {let(a,b)=[20,22];a+b}",
            "fn main()->i64 {let(ref a,b)=(20,22);*a+b}",
            "fn main()->i64 {let(a,a)=(20,22);a}",
            "fn main()->i64 {let(a,b)=&(20,22);a+b}",
            "fn main()->i64 {let pair=(20,22);let borrowed=&pair;let(a,b)=borrowed;*a+*b}",
            "fn read(pair:&(i64,i64))->i64 {let(a,b)=pair;*a+*b} fn main()->i64 {read(&(20,22))}",
            "fn read(pair:&(i64,i64))->i64 {pair.0+pair.1} fn main()->i64 {read(&(20,22))}",
            "fn main()->i64 {let pair=(20,22);let borrow=|pair| &pair;let(a,b)=borrow(pair);*a+*b}",
            "static PAIR:(i64,i64)=(20,22); fn borrowed()->&'static (i64,i64) {&PAIR} fn main()->i64 {let(a,b)=borrowed();*a+*b}",
            "fn main()->i64 {let &(a,b)=&(20,22);42}",
            "fn main()->i64 {let ()=0;42}",
            "fn main()->i64 {let(a,b):(i64,bool)=(20,22);a}",
        ] {
            let parsed = parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn vector_references_do_not_silently_copy_mutable_headers() {
        for source in [
            "fn fill(items:&mut Vec<i64>){items.push(22);} fn main()->i64{let mut items=vec![20];fill(&mut items);if items.len()==2{items[0]+items[1]}else{0}}",
            "fn read(items:&Vec<i64>)->i64{items.len()} fn main()->i64{let items=vec![42];read(&items)}",
            "fn main()->i64{let mut items=vec![20];let borrowed=&mut items;borrowed.push(22);if items.len()==2{42}else{0}}",
            "fn main()->i64{let mut items=vec![20];let borrowed=&items;items.push(22);if borrowed.len()==2{42}else{0}}",
        ] {
            let parsed = parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn explicit_vector_constructors_preserve_types_and_do_not_erase_borrows() {
        for source in [
            "fn main()->i64{let items=Vec::with_capacity(10);items.capacity()+42}",
            "fn main()->i64{let items:Vec<i64>=Vec::with_capacity(true);42}",
            "fn main()->i64{let items:Vec<bool>=Vec::<i64>::with_capacity(10);42}",
            "fn main()->i64{let items=Vec::<&i64>::with_capacity(10);42}",
            "fn main()->i64{let items=Vec::<i64,bool>::with_capacity(10);42}",
            "struct Vec{value:i64} fn main()->i64{let items:Vec<i64>=Vec::with_capacity(10);42}",
            "fn values()->Vec<bool>{Vec::<i64>::new()} fn main()->i64{values().len()+42}",
            "fn main()->i64{let mut items=Vec::<i64>::new();items.push(true);42}",
            "fn main()->i64{let items:Vec<bool>=Vec::<i64>::new();items.len()+42}",
            "fn take(items:Vec<bool>)->i64{42} fn main()->i64{take(Vec::<i64>::new())}",
            "fn main()->i64{let mut items=Vec::<&i64>::new();items.push(&42);42}",
            "fn main()->i64{let items=Vec::<i64,bool>::new();items.len()+42}",
            "struct Vec{value:i64} fn main()->i64{let items=Vec::<i64>::new();42}",
            "mod Vec{pub fn new<T>()->i64{0}} fn main()->i64{let items=Vec::<i64>::new();42}",
            "mod Vec{pub fn new<T>()->i64{0}} fn main()->i64{let items=Vec::<_>::new();42}",
            "mod geo{pub struct Vec{value:i64}} use geo::Vec; fn main()->i64{let items=Vec::<i64>::new();42}",
        ] {
            let parsed = parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn typed_tuples_do_not_guess_unknown_elements_or_erase_shape_mismatches() {
        for source in [
            "fn main()->i64 {let pair=(missing(),true);42}",
            "fn main()->i64 {let items=vec![20,true];42}",
            "fn main()->i64 {let mut items=Vec::new();if true {items.push(42);} else {items.push(false);} 42}",
            "fn main()->i64 {let mut items=std::vec::Vec::new();items.push(42);items.push(false);42}",
            "fn main()->i64 {let items=alloc::vec::Vec::new();items.len()+42}",
            "fn main()->i64 {let items=Vec::<_>::new();items.len()+42}",
            "fn main()->i64{let items=Vec::new();items.capacity()+42}",
            "fn main()->i64{let items=[42];items.capacity()}",
            "fn main()->i64 {let mut items=Vec::new();items.extend(vec![]);42}",
            "fn main()->i64 {let mut items=vec![42];items.extend(vec![false]);42}",
            "fn main()->i64 {let mut items=vec![42];items.extend(42);42}",
            "fn main()->i64 {let mut items=std::vec::Vec::<_>::new();items.push(42);items.push(false);42}",
            "fn main()->i64 {let mut items=Vec::new();items.push(missing());42}",
            "fn main()->i64 {let items=Vec::new();items.len()+42}",
            "mod geo {pub struct Vec<T> {value:T} impl Vec<i64> {pub fn new()->Self {Self{value:0}} pub fn is_empty(&self)->bool {false}}} use geo::Vec; fn main()->i64 {let items:Vec<i64>=Vec::new();if items.is_empty() {42} else {0}}",
            "fn main()->i64 {let items=vec![missing(),42];items[1]}",
            "fn pair()->(i64,bool) {(42,7)} fn main()->i64 {pair().0}",
        ] {
            let parsed = parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn receiver_methods_do_not_guess_free_functions_or_static_receivers() {
        for source in [
            "fn answer(_value:i64)->i64 {42} fn main()->i64 {let value=1;value.answer()}",
            "struct Point {x:i64} fn main()->i64 {let point=Point{x:42};let lost=point.iter();point.x}",
            "struct Bucket {value:i64} impl Bucket {fn answer(bucket:Bucket)->i64 {bucket.value}} fn main()->i64 {let bucket=Bucket{value:42};bucket.answer()}",
        ] {
            let parsed = parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn tuple_match_return_proof_preserves_guarded_and_literal_paths() {
        for pattern in ["(1,b)", "(a,b) if a>0"] {
            let source =
                format!("fn main()->i64 {{let pair=(20,22);match pair {{{pattern}=>42}}}}");
            let module = parse_rust_source(&source).unwrap();
            let body = module
                .decls
                .iter()
                .find_map(|decl| match decl {
                    Decl::Function { name, body, .. } if name == "main" => Some(body),
                    _ => None,
                })
                .unwrap();
            assert!(
                matches!(body.last(), Some(Stmt::Return(None))),
                "{pattern}: {body:?}"
            );
        }
        let parsed =
            parse_rust_source("fn main()->i64 {let values=[20,22];match values {(a,b)=>a+b}}")
                .unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        assert!(!crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
    }

    #[test]
    fn struct_match_return_proof_preserves_refutable_paths() {
        for pattern in ["Point { x: 1 }", "Point { x: value } if value > 0"] {
            let source = format!(
                "struct Point {{ x:i64 }} fn main()->i64 {{ let p=Point{{x:42}}; match p {{ {pattern}=>42 }} }}"
            );
            let module = parse_rust_source(&source).unwrap();
            let body = module
                .decls
                .iter()
                .find_map(|decl| match decl {
                    Decl::Function { name, body, .. } if name == "main" => Some(body),
                    _ => None,
                })
                .unwrap();
            assert!(
                matches!(body.last(), Some(Stmt::Return(None))),
                "{pattern}: {body:?}"
            );
        }
        let mut module = parse_rust_source("struct Point { x:i64 } struct Other { x:i64 } fn main()->i64 { let p=Point{x:42}; match p { Other{x:value}=>value } }").unwrap();
        crate::lower_core::desugar_module(&mut module);
        assert!(!crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
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
            Some(Stmt::Expr(Expr::Call { callee, args }))
                if matches!(callee.as_ref(), Expr::Ident(name) if name == "#receiver-method:extend")
                    && matches!(args.as_slice(), [Expr::Ident(receiver), Expr::Call { callee, .. }]
                        if receiver == "values"
                            && matches!(callee.as_ref(), Expr::Ident(name) if name == "produced"))
        ));
        assert!(matches!(body.last(), Some(Stmt::Return(None))));
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
    #[test]
    fn integer_cast_source_acceptance_is_host_independent() {
        for (source, expected) in [
            (
                "fn main() -> i64 { let value: i64 = 298; return (value as u8) as i64; }",
                Some(42),
            ),
            (
                "fn main() -> i64 { let value: bool = true; return value as i32 as i64; }",
                Some(1),
            ),
            (
                "fn main() -> i64 { return (170 as ::core::primitive::i8) as i64; }",
                Some(-86),
            ),
            ("fn main() -> i64 { return (42 as i128) as i64; }", None),
            (
                "fn main() -> i64 { return (42 as *const u8) as i64; }",
                None,
            ),
            ("fn main() -> i64 { return (42u32 as char) as i64; }", None),
        ] {
            let module = crate::typecheck::normalize_module(
                crate::parser_registry::ParserId::Rust,
                &parse_rust_source(source).unwrap(),
            );
            let accepted = expected.is_some();
            let typed = crate::typecheck::TypeChecker::new().check_module(&module);
            assert_eq!(typed.is_ok(), accepted, "{source}: {typed:?}");
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert_eq!(verified.ok, accepted, "{source}: {verified:?}");
            if !accepted {
                assert_eq!(verified.reason_code.as_deref(), Some("unresolved-symbol"));
            }
            if let Some(expected) = expected {
                let mut optimized = module.clone();
                crate::core_opt::optimize_with_profile(
                    &mut optimized.decls,
                    Some("main"),
                    crate::emit_profile::EmitProfile::Default,
                );
                let body = optimized
                    .decls
                    .iter()
                    .find_map(|decl| match decl {
                        Decl::Function { name, body, .. } if name == "main" => Some(body),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(
                    body.last(),
                    Some(&Stmt::Return(Some(Expr::IntLit(expected)))),
                    "{source}: {body:?}"
                );
            }
        }
    }
    #[test]
    fn floating_width_source_and_literal_suffixes_are_host_independent() {
        for (literal, expected) in [
            ("16777217.0", 16777216.0),
            ("0.1f32", f64::from(0.1f32)),
            ("42f32", 42.0),
            ("42f64", 42.0),
        ] {
            let source = format!("fn main() -> f64 {{ return ({literal} as f32) as f64; }}");
            let mut module = crate::typecheck::normalize_module(
                crate::parser_registry::ParserId::Rust,
                &parse_rust_source(&source).unwrap(),
            );
            crate::typecheck::TypeChecker::new()
                .check_module(&module)
                .unwrap();
            assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
            crate::core_opt::optimize_with_profile(
                &mut module.decls,
                Some("main"),
                crate::emit_profile::EmitProfile::Default,
            );
            let Decl::Function { body, .. } = &module.decls[0] else {
                panic!("function");
            };
            assert_eq!(
                body.last(),
                Some(&Stmt::Return(Some(Expr::FloatLit(FloatVal(expected))))),
                "{source}: {body:?}"
            );
        }
        for literal in ["1e39f32", "1e400f64", "0b1f32"] {
            let source = format!("fn main() -> f64 {{ return {literal}; }}");
            let module = parse_rust_source(&source).unwrap();
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}"
            );
        }
    }
    #[test]
    fn nested_nominal_prefixing_resolves_only_known_root_paths() {
        let module = parse_rust_source("mod outer{pub struct Parent{value:i64} mod inner{fn invalid()->Parent{Parent{value:0}} fn absolute()->i64{crate::root()}}} fn root()->i64{42}").unwrap();
        let Decl::Function { ret, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..} if name=="outer::inner::invalid"))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(ret, &Typ::Named("Parent".into()));
        let Decl::Function { body, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..} if name=="outer::inner::absolute"))
            .unwrap()
        else {
            unreachable!()
        };
        assert!(
            matches!(body.last(), Some(Stmt::Return(Some(Expr::Call{callee,..}))) if matches!(callee.as_ref(),Expr::Ident(name) if name=="root")),
            "{body:?}"
        );
    }

    #[test]
    fn self_names_reach_vector_types_and_macro_lowered_initializers() {
        let module = parse_rust_source(
            "struct Thing{value:i64} impl Thing{fn wrap()->Vec<Self>{vec![Self{value:42}]}}",
        )
        .unwrap();
        let Decl::Function { ret, body, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..} if name=="Thing::wrap"))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(ret, &Typ::Vector(Box::new(Typ::Named("Thing".into()))));
        assert!(
            matches!(body.last(), Some(Stmt::Return(Some(Expr::ArrayLit(items)))) if matches!(items.as_slice(), [Expr::StructInit{name, ..}] if name=="Thing")),
            "{body:?}"
        );
    }

    #[test]
    fn anonymous_constants_keep_distinct_internal_names_and_initializers() {
        let module = parse_rust_source("const _:i64=1; const _:i64=2; fn main()->i64{42}").unwrap();
        let globals: Vec<_> = module
            .decls
            .iter()
            .filter_map(|decl| match decl {
                Decl::Global {
                    name,
                    init,
                    constant_value,
                    ..
                } => Some((name, init, constant_value)),
                _ => None,
            })
            .collect();
        assert_eq!(globals.len(), 2);
        assert_ne!(globals[0].0, globals[1].0);
        for (index, (name, init, constant)) in globals.iter().enumerate() {
            assert!(name.starts_with("$anonymous_const$"));
            assert!(**constant);
            assert_eq!(
                init.as_deref(),
                Some(&rust_integer_literal(index as i64 + 1, ""))
            );
        }
    }

    #[test]
    fn trait_impl_identities_preserve_free_functions_receivers_traits_and_ufcs() {
        let module = parse_rust_source(
            r#"
            struct Left {} struct Right {} struct Holder<T> { value:T }
            trait First { fn eq(&self)->i64; } trait Second { fn eq(&self)->i64; }
            fn eq()->i64 { 42 }
            impl First for Left { fn eq(&self)->i64 { 1 } }
            impl First for Right { fn eq(&self)->i64 { 2 } }
            impl Second for Left { fn eq(&self)->i64 { 3 } }
            impl First for Holder<i64> { fn eq(&self)->i64 { 4 } }
            impl First for Holder<bool> { fn eq(&self)->i64 { 5 } }
            fn main()->i64 { <Left as First>::eq(Left{}) + eq() }
        "#,
        )
        .unwrap();
        let names: Vec<_> = module
            .decls
            .iter()
            .filter_map(|decl| match decl {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        for name in [
            "eq",
            "Left::First::eq",
            "Right::First::eq",
            "Left::Second::eq",
            "Holder<i64>::First::eq",
            "Holder<bool>::First::eq",
        ] {
            assert_eq!(
                names.iter().filter(|actual| **actual == name).count(),
                1,
                "{names:?}"
            );
        }
        let Decl::Function { body, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function {name, ..} if name == "main"))
            .unwrap()
        else {
            unreachable!()
        };
        let mut calls = Vec::new();
        let Stmt::Return(Some(expr)) = body.last().unwrap() else {
            panic!("{body:?}")
        };
        {
            fn visit(expr: &Expr, calls: &mut Vec<String>) {
                if let Expr::Call { callee, .. } = expr {
                    if let Expr::Ident(name) = callee.as_ref() {
                        calls.push(name.clone());
                    }
                }
                crate::core_ir::for_each_expr_child(expr, &mut |child| visit(child, calls));
            }
            visit(expr, &mut calls);
        }
        assert!(calls.contains(&"Left::First::eq".into()), "{calls:?}");
        assert!(calls.contains(&"eq".into()), "{calls:?}");
    }

    #[test]
    fn fixed_array_struct_values_preserve_native_refusal() {
        for source in [
            "#[derive(Clone,Copy)] struct Holder{items:[i64;2]} fn main()->i64{let mut a=Holder{items:[42,0]};let b=a;a.items[0]=0;b.items[0]}",
            "struct Inner{items:[i64;2]} struct Outer{inner:Inner} fn take(value:Outer)->i64{value.inner.items[0]} fn main()->i64{take(Outer{inner:Inner{items:[42,0]}})}",
            "struct Holder([i64;2]); fn main()->i64{let h=Holder([42,0]);h.0[0]}",
        ] {
            let mut module = parse_rust_source(source).unwrap();
            assert!(module.decls.iter().any(|decl| matches!(decl,
                Decl::Struct {fields,..} if fields.iter().any(|(_,typ)| matches!(typ,
                    Typ::Named(name) if name=="#unsupported-rust-fixed-array-struct-value")))));
            crate::lower_core::desugar_module(&mut module);
            assert!(
                crate::native_emit::lower::lower_module(
                    &module,
                    "main",
                    crate::native_emit::NativeLinkage::Executable
                )
                .is_err(),
                "{source}"
            );
            assert!(
                crate::native_emit::x86_64_lower::lower_module(&module, "main").is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn unused_fixed_array_struct_schema_and_borrowed_slice_schema_remain_parsable() {
        let mut module = parse_rust_source("mod shapes{pub struct Fixed{pub items:[i64;2]} pub struct Slice<'a>{pub items:&'a [i64]}} fn main()->i64{42}").unwrap();
        assert!(module.decls.iter().any(|decl| matches!(decl, Decl::Struct{name,fields,..}
            if name=="shapes::Fixed" && matches!(&fields[0].1,Typ::Named(name) if name=="#unsupported-rust-fixed-array-struct-value"))));
        assert!(
            module
                .decls
                .iter()
                .any(|decl| matches!(decl, Decl::Struct{name,fields,..}
            if name=="shapes::Slice" && fields[0].1 == crate::core_ir::rust_references::erase_type(map_type(&syn::parse_str::<syn::Type>("&[i64]").unwrap()))))
        );
        module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
        crate::lower_core::desugar_module(&mut module);
        assert!(
            crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .is_ok()
        );
        assert!(crate::native_emit::x86_64_lower::lower_module(&module, "main").is_ok());
    }
}

#[cfg(test)]
mod integer_metadata_tests {
    #[test]
    fn integer_casts_retain_source_kind_around_generic_conversion() {
        for target in [
            "i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "isize", "usize",
        ] {
            let expression: syn::Expr = syn::parse_str(&format!("next() as {target}")).unwrap();
            let actual = lower_expr_with_types(&expression, &mut HashMap::new());
            let expected = Expr::Unary {
                op: format!("rust-integer-value-{target}"),
                expr: Box::new(Expr::Unary {
                    op: format!("numeric-cast-{target}"),
                    expr: Box::new(Expr::Call {
                        callee: Box::new(Expr::Ident("next".into())),
                        args: vec![],
                    }),
                }),
            };
            assert_eq!(actual, expected);
        }
        let expression: syn::Expr = syn::parse_str("next() as f32").unwrap();
        assert!(
            matches!(lower_expr_with_types(&expression, &mut HashMap::new()), Expr::Unary {op, expr} if crate::core_ir::rust_floats::cast_kind(&op)==Some(crate::core_ir::rust_floats::Kind::F32) && matches!(expr.as_ref(), Expr::Call {..}))
        );
    }

    #[test]
    fn typed_vector_constructors_keep_private_integer_element_metadata() {
        for item in ["u8", "i64"] {
            let source = format!("fn main(){{let values=Vec::<{item}>::with_capacity(2);}}");
            let module = parse_rust_source(&source).unwrap();
            assert!(module.decls.iter().any(|decl| matches!(decl,
                Decl::Function {name, ret: Typ::Vector(element), ..}
                if name.contains("with_capacity") && **element == Typ::Named(format!("#rust-integer-{item}"))
            )), "{source}: {module:?}");
        }
    }

    #[test]
    fn ambiguous_primitive_type_shadows_refuse_explicitly() {
        for source in [
            "struct u32; fn main(){}",
            "type u8 = bool; fn main(){}",
            "mod names{pub struct Word;} use names::Word as u32; fn main(){}",
            "use names::{u8, Other as u16}; fn main(){}",
            "fn identity<u16>(value:u16)->u16{value}",
            "struct Holder<i32>{value:i32}",
            "mod inner { type usize = bool; } fn main(){}",
            "fn main(){if true {type u32=bool;} let f=||{struct u8;};}",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                reject_integer_primitive_shadowing(&file.items)
                    .unwrap_err()
                    .contains("shadowed primitive integer type")
            );
        }
        let file =
            syn::parse_file("fn main(value:core::primitive::u32)->std::primitive::u32{value}")
                .unwrap();
        assert!(reject_integer_primitive_shadowing(&file.items).is_ok());
        let file =
            syn::parse_file("use core::primitive::u32; use std::primitive::{u8,i16}; fn main(){}")
                .unwrap();
        assert!(reject_integer_primitive_shadowing(&file.items).is_ok());
    }

    use super::*;

    #[test]
    fn primitive_integer_paths_preserve_exact_rust_kind() {
        for spelling in ["u32", "core::primitive::u32", "std::primitive::u32"] {
            let typ: syn::Type = syn::parse_str(spelling).unwrap();
            let mapped = map_type(&typ);
            assert_eq!(mapped, Typ::Named("#rust-integer-u32".into()));
            assert_eq!(type_to_string(&mapped), "u32");
        }
        let nominal: syn::Type = syn::parse_str("foo::u32").unwrap();
        assert_eq!(map_type(&nominal), Typ::Named("foo::u32".into()));
        for name in ["i128", "u128"] {
            let typ: syn::Type = syn::parse_str(name).unwrap();
            assert_eq!(
                map_type(&typ),
                Typ::Named(format!("#unsupported-rust-integer-{name}"))
            );
        }
    }

    #[test]
    fn literal_metadata_preserves_suffix_bits_and_signed_minimum() {
        for (source, marker, value) in [
            ("42", "rust-integer-literal", 42),
            ("42u32", "rust-integer-literal-u32", 42),
            ("18446744073709551615", "rust-integer-literal-magnitude", -1),
            ("0xffffffffffffffffu64", "rust-integer-literal-u64", -1),
            (
                "-9223372036854775808i64",
                "rust-integer-literal-i64",
                i64::MIN,
            ),
            ("-128i8", "rust-integer-literal-i8", -128),
        ] {
            let expr: syn::Expr = syn::parse_str(source).unwrap();
            assert_eq!(
                lower_expr_with_types(&expr, &mut HashMap::new()),
                Expr::Unary {
                    op: marker.into(),
                    expr: Box::new(Expr::IntLit(value))
                }
            );
        }
        for source in ["1u128", "-9223372036854775809i64"] {
            let expr: syn::Expr = syn::parse_str(source).unwrap();
            assert!(
                matches!(lower_expr_with_types(&expr, &mut HashMap::new()), Expr::Ident(name) if name.starts_with("#unsupported-rust-"))
            );
        }
    }
}

#[cfg(test)]
mod private_ordered_operation_tests {
    use super::*;
    #[test]
    fn generated_rust_operations_have_compiler_owned_names() {
        let mut module = parse_rust_source("struct Row{x:i64} fn main()->i64{let mut values=[20i64;2];values[0]=21;values[0]+=1;let mut row=Row{x:20};row.x+=2;values[0]+row.x}").unwrap();
        let mut callees = HashSet::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Call { callee, .. } = expr
                && let Expr::Ident(name) = callee.as_ref()
            {
                callees.insert(name.clone());
            }
        });
        for expected in [
            crate::core_ir::ARRAY_REPEAT,
            crate::core_ir::INDEX_ASSIGN_VALUE_FIRST,
            crate::core_ir::INDEX_MODIFY_VALUE_FIRST,
            crate::core_ir::FIELD_MODIFY_VALUE_FIRST,
        ] {
            assert!(
                callees.contains(expected),
                "missing {expected}: {callees:?}"
            );
        }
    }
    #[test]
    fn private_indexed_store_keeps_static_array_mutation_refused() {
        let mut module =
            parse_rust_source("static VALUES:[i64;1]=[42]; fn main()->i64{VALUES[0]=0;VALUES[0]}")
                .unwrap();
        let mut refused = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            refused |= matches!(expr,Expr::Call{callee,..} if matches!(callee.as_ref(),Expr::Ident(name) if name=="__unsupported_rust_static_array_value"));
        });
        assert!(refused);
    }
}

#[cfg(test)]
mod expression_depth_tests {
    use super::*;
    // Proposed insertion in rust_front's existing test module, separate from production patch.
    #[test]
    fn transparent_wrappers_do_not_replace_nested_values_with_zero() {
        for depth in [60, 65, 70, 90] {
            let nested = format!("{}42i64{}", "(".repeat(depth), ")".repeat(depth));
            for (deep, shallow) in [
                (nested.clone(), "42i64".to_owned()),
                (format!("{nested}+0"), "42i64+0".to_owned()),
                (format!("({nested},0i64)"), "(42i64,0i64)".to_owned()),
            ] {
                let deep: syn::Expr = syn::parse_str(&deep).unwrap();
                let shallow: syn::Expr = syn::parse_str(&shallow).unwrap();
                assert_eq!(
                    lower_expr_with_types(&deep, &mut HashMap::new()),
                    lower_expr_with_types(&shallow, &mut HashMap::new()),
                    "depth {depth}"
                );
                assert_eq!(EXPR_DEPTH.with(|value| value.get()), 0);
            }
        }
        let mut grouped: syn::Expr = syn::parse_str("42i64").unwrap();
        for _ in 0..90 {
            grouped = syn::Expr::Group(syn::ExprGroup {
                attrs: vec![],
                group_token: Default::default(),
                expr: Box::new(grouped),
            });
        }
        let shallow: syn::Expr = syn::parse_str("42i64").unwrap();
        assert_eq!(
            lower_expr_with_types(&grouped, &mut HashMap::new()),
            lower_expr_with_types(&shallow, &mut HashMap::new())
        );
        assert_eq!(EXPR_DEPTH.with(|value| value.get()), 0);
    }

    #[test]
    fn real_structural_depth_produces_explicit_unsupported_marker_and_resets_budget() {
        // Build syntax nodes directly: this tests frontend depth, not syn parser limits.
        let mut deep: syn::Expr = syn::parse_str("42i64").unwrap();
        for _ in 0..90 {
            deep = syn::Expr::Unary(syn::ExprUnary {
                attrs: vec![],
                op: syn::UnOp::Neg(Default::default()),
                expr: Box::new(deep),
            });
        }
        let lowered = lower_expr_with_types(&deep, &mut HashMap::new());
        fn contains_depth_marker(expr: &Expr) -> bool {
            if matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(),Expr::Ident(name) if name == crate::core_ir::RUST_EXPRESSION_DEPTH))
            {
                return true;
            }
            let mut found = false;
            crate::core_ir::for_each_expr_child(expr, &mut |child| {
                found |= contains_depth_marker(child)
            });
            found
        }
        assert!(contains_depth_marker(&lowered));
        assert_eq!(EXPR_DEPTH.with(|value| value.get()), 0);
        let shallow: syn::Expr = syn::parse_str("42i64").unwrap();
        assert!(!contains_depth_marker(&lower_expr_with_types(
            &shallow,
            &mut HashMap::new()
        )));
        assert_eq!(EXPR_DEPTH.with(|value| value.get()), 0);
    }

    #[test]
    fn structural_depth_refusal_survives_zero_product_and_unused_binding_optimization() {
        let mut deep: syn::Expr = syn::parse_str("42i64").unwrap();
        for _ in 0..90 {
            deep = syn::Expr::Unary(syn::ExprUnary {
                attrs: vec![],
                op: syn::UnOp::Neg(Default::default()),
                expr: Box::new(deep),
            });
        }
        let lowered = lower_expr_with_types(&deep, &mut HashMap::new());
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for body in [
                vec![Stmt::Return(Some(lowered.clone()))],
                vec![Stmt::Return(Some(Expr::Binary {
                    op: "*".into(),
                    lhs: Box::new(lowered.clone()),
                    rhs: Box::new(Expr::IntLit(0)),
                }))],
                vec![
                    Stmt::Let("unused".into(), Some(Typ::Int), lowered.clone()),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ] {
                let mut module = parse_rust_source("fn main()->i64{return 42;}").unwrap();
                let Decl::Function { body: main, .. } = &mut module.decls[0] else {
                    panic!("missing main")
                };
                *main = body;
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
                assert!(!verified.ok, "{profile:?}");
                assert!(
                    verified.reason.as_deref().is_some_and(
                        |reason| reason.contains(crate::core_ir::RUST_EXPRESSION_DEPTH)
                    ),
                    "{verified:?}"
                );
                assert_eq!(EXPR_DEPTH.with(|value| value.get()), 0);
            }
        }
    }
}

#[cfg(test)]
mod generic_provenance_tests {
    use super::*;

    #[test]
    fn declared_generics_are_distinct_from_same_named_nominals_and_nested_containers() {
        let module = parse_rust_source(
            "struct T{x:i64}fn nominal(value:T)->T{value}fn generic<T>(value:T,values:Vec<T>,array:[T;2],tuple:(T,i64))->T{let local:T=value;local}"
        ).unwrap();
        let generic = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..}if name=="generic"))
            .unwrap();
        let Decl::Function {
            params,
            ret,
            body,
            type_params,
            ..
        } = generic
        else {
            unreachable!()
        };
        assert_eq!(type_params, &["T"]);
        assert_eq!(params[0].1, Typ::Generic("T".into()));
        assert_eq!(params[1].1, Typ::Vector(Box::new(Typ::Generic("T".into()))));
        assert_eq!(
            params[2].1,
            crate::core_ir::rust_arrays::typ(Typ::Generic("T".into()), 2)
        );
        assert_eq!(
            crate::core_ir::tuples::fields(match &params[3].1 {
                Typ::Named(name) => name,
                _ => panic!(),
            })
            .unwrap()[0]
                .1,
            Typ::Generic("T".into())
        );
        assert_eq!(*ret, Typ::Generic("T".into()));
        assert!(body.iter().any(
            |statement| matches!(statement,Stmt::Let(_,Some(Typ::Generic(name)),_)if name=="T")
        ));
        let Decl::Function {
            params,
            ret,
            type_params,
            ..
        } = module
            .decls
            .iter()
            .find(|decl| matches!(decl,Decl::Function{name,..}if name=="nominal"))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(params[0].1, Typ::Named("T".into()));
        assert_eq!(*ret, Typ::Named("T".into()));
        assert!(type_params.is_empty());
    }
}

#[cfg(test)]
mod retained_unary_artifact_tests {
    use super::*;
    use crate::core_ir::rust_references;
    #[test]
    fn artifact_keeps_exact_owned_shared_mutable_bindings_and_reference_producers() {
        let artifact = parse_rust_artifact_source(
            r#"
            struct Row{x:i64}
            impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}
            impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}
            impl std::ops::Neg for &mut Row{type Output=i64;fn neg(self)->i64{99}}
            fn negate<T:std::ops::Neg<Output=i64>>(value:T)->i64{-value}
            fn main()->i64{let mut row=Row{x:42};negate(Row{x:42})+negate(&row)+negate(&mut row)}
        "#,
            "proof",
        )
        .unwrap();
        for (receiver, target) in [
            ("Row", "Row::std::ops::Neg::neg"),
            ("&Row", "&Row::std::ops::Neg::neg"),
            ("&mut Row", "&mutRow::std::ops::Neg::neg"),
        ] {
            assert_eq!(
                artifact.unary_methods.get(&(receiver.into(), "-".into())),
                Some(&Ok(target.into()))
            );
        }
        let mut semantic = artifact.semantic.clone();
        let mut producers = Vec::new();
        let mut generic_minus = 0;
        crate::core_opt::map_decl_exprs_mut(&mut semantic.decls, &mut |expression| {
            if let Expr::Unary { op, .. } = expression {
                if let Some(mutable) = rust_references::value_kind(op) {
                    producers.push(mutable);
                }
                if op == rust_references::NEGATE_VALUE {
                    generic_minus += 1;
                }
            }
        });
        assert_eq!(producers, [false, true]);
        assert_eq!(generic_minus, 1);
        crate::lower_core::desugar_module(&mut semantic);
        crate::typecheck::TypeChecker::new().resolve_complements(&mut semantic);
        let mut borrow_calls = 0;
        crate::core_opt::map_decl_exprs_mut(&mut semantic.decls, &mut |expression| {
            if matches!(expression,Expr::Call{callee,..}if matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::BORROW))
            {
                borrow_calls += 1;
            }
        });
        assert_eq!(
            borrow_calls, 0,
            "old BORROW preparation completes before inlining while producer proof remains"
        );
    }
    #[test]
    fn concrete_semantic_api_resolves_neg_and_restores_reference_representations() {
        let semantic=parse_rust_source("struct Row{x:i64}impl std::ops::Neg for &Row{type Output=i64;fn neg(self)->i64{88}}fn apply(row:&Row)->i64{-row}").unwrap();
        let Decl::Function { params, body, .. } = semantic
            .decls
            .iter()
            .find(|declaration| matches!(declaration,Decl::Function{name,..}if name=="apply"))
            .unwrap()
        else {
            unreachable!()
        };
        assert_eq!(params[0].1, Typ::Named("Row".into()));
        assert!(
            matches!(&body[0],Stmt::Return(Some(Expr::Call{callee,..}))if matches!(callee.as_ref(),Expr::Ident(name)if name=="&Row::std::ops::Neg::neg"))
        );
    }
    #[test]
    fn inline_child_keeps_exact_target_and_loaded_external_errors_propagate() {
        let artifact=parse_rust_artifact_source("mod child{struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}pub fn entry()->i64{-Row{x:42}}}fn main()->i64{child::entry()}","proof").unwrap();
        assert_eq!(
            artifact
                .unary_methods
                .get(&("child::Row".into(), "-".into())),
            Some(&Ok("child::Row::std::ops::Neg::neg".into()))
        );
        let directory = std::env::temp_dir().join(format!("in-neg-proof-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("broken.rs"), "fn broken( {").unwrap();
        let error = parse_rust_artifact_source_with_dir(
            "mod broken;fn main()->i64{42}",
            "proof",
            &directory,
        )
        .unwrap_err();
        assert!(error.contains("parse Rust module"));
        std::fs::remove_file(directory.join("broken.rs")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}

#[cfg(test)]
mod nested_module_proof_boundaries {
    use super::*;
    #[test]
    fn empty_shadow_module_does_not_hide_absolute_standard_trait_proof() {
        let artifact = parse_rust_artifact_source(
            "mod std{}struct Row{x:i64}impl ::std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}fn main()->i64{-Row{x:42}}", "absolute"
        ).unwrap();
        assert_eq!(
            artifact.unary_methods.get(&("Row".into(), "-".into())),
            Some(&Ok("Row::::std::ops::Neg::neg".into()))
        );
        assert!(parse_rust_source("").is_err());
    }
    #[test]
    fn parallel_external_modules_keep_ancestor_depth_limit() {
        let directory = std::env::temp_dir().join(format!("in-neg-depth-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut level = directory.clone();
        for _ in 0..10 {
            std::fs::create_dir_all(level.join("next")).unwrap();
            std::fs::write(
                level.join("next/mod.rs"),
                "mod next;mod empty;pub fn value()->i64{42}",
            )
            .unwrap();
            std::fs::write(level.join("empty.rs"), "").unwrap();
            level = level.join("next");
        }
        let error = parse_rust_artifact_source_with_dir(
            "mod next;mod empty;fn main()->i64{42}",
            "depth",
            &directory,
        )
        .unwrap_err();
        assert!(error.contains("module nest limit"), "{error}");
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod impl_associated_type_tests {
    use super::*;
    fn implementation(source: &str) -> syn::ItemImpl {
        syn::parse_str(source).unwrap()
    }
    #[test]
    fn associated_output_is_impl_local_exact_width_and_generic_identity() {
        for (source, expected) in [
            (
                "impl First for Row{type Output=i8;}",
                Typ::Named("#rust-integer-i8".into()),
            ),
            (
                "impl Second for Row{type Output=u64;}",
                Typ::Named("#rust-integer-u64".into()),
            ),
            (
                "impl<T> Other for Row<T>{type Output=T;}",
                Typ::Generic("T".into()),
            ),
        ] {
            let item = implementation(source);
            assert_eq!(
                resolve_impl_type(
                    Typ::Named("Self::Output".into()),
                    &Typ::Named("Row".into()),
                    &item
                ),
                Ok(expected)
            );
        }
    }
    #[test]
    fn output_projection_reaches_reference_and_container_types_without_erasing_proof() {
        let item = implementation("impl Other for Row{type Output=i64;}");
        let source: syn::Type = syn::parse_str("(&Self::Output, Vec<Self::Output>)").unwrap();
        let resolved =
            resolve_impl_type(map_type(&source), &Typ::Named("Row".into()), &item).unwrap();
        let fields = crate::core_ir::tuples::fields(match &resolved {
            Typ::Named(name) => name,
            _ => panic!(),
        })
        .unwrap();
        let Typ::Named(reference) = &fields[0].1 else {
            panic!()
        };
        let reference = crate::core_ir::rust_references::decode(reference).unwrap();
        assert_eq!(reference.referent, Typ::Named("#rust-integer-i64".into()));
        assert_eq!(reference.runtime, Typ::Named("#rust-integer-i64".into()));
        assert_eq!(
            fields[1].1,
            Typ::Vector(Box::new(Typ::Named("#rust-integer-i64".into())))
        );
        assert_eq!(fields[1].1.canonical(), Typ::Vector(Box::new(Typ::Int)));
    }
    #[test]
    fn unsupported_associated_contracts_refuse_instead_of_guessing_row_member() {
        for source in [
            "impl Other for Row{type Output=Self::Output;}",
            "impl Other for Row{type Output=Self::Other;type Other=Self::Output;}",
            "impl Other for Row{type Output=i64;type Output=i32;}",
            "impl Other for Row{type Output<T>=T;}",
            "impl Other for Row{}",
            "impl Other for Row{type Output=<Row as Other>::Output;}",
            "impl Other for Row{type Output=Holder<Self::Output>;}",
            "impl Other for Row{type Output=fn()->Self::Output;}",
        ] {
            assert!(
                resolve_impl_type(
                    Typ::Named("Self::Output".into()),
                    &Typ::Named("Row".into()),
                    &implementation(source)
                )
                .is_err(),
                "{source}"
            );
        }
    }
    #[test]
    fn unresolved_neg_output_projections_never_establish_dispatch_proof() {
        for output in [
            "<Row as Other>::Output",
            "Holder<Self::Output>",
            "fn()->Self::Output",
            "Self::Output",
        ] {
            let file = syn::parse_file(&format!("struct Row{{x:i64}}impl std::ops::Neg for Row{{type Output={output};fn neg(self)->Self::Output{{77}}}}" )).unwrap();
            let bindings = unary_methods::collect(&file);
            assert_eq!(bindings.len(), 1, "{output}: {bindings:?}");
            assert!(
                bindings.values().all(Result::is_err),
                "{output}: {bindings:?}"
            );
        }
    }

    #[test]
    fn neg_self_output_uses_the_same_resolved_type_in_binding_and_lowered_return() {
        for (receiver, output, method_body, expected) in [
            ("Row", "i64", "77", Typ::Named("#rust-integer-i64".into())),
            ("&Row", "i64", "88", Typ::Named("#rust-integer-i64".into())),
            ("Row", "Self", "self", Typ::Named("Row".into())),
        ] {
            let source = format!(
                "struct Row{{x:i64}}impl std::ops::Neg for {receiver}{{type Output={output};fn neg(self)->Self::Output{{let value:Self::Output={method_body};value}}}}"
            );
            let file = syn::parse_file(&source).unwrap();
            let bindings = unary_methods::collect(&file);
            assert!(
                bindings.values().all(Result::is_ok),
                "{source}: {bindings:?}"
            );
            let item = file
                .items
                .iter()
                .find_map(|item| {
                    if let syn::Item::Impl(item) = item {
                        Some(item)
                    } else {
                        None
                    }
                })
                .unwrap();
            let lowered = lower_impl(item);
            assert!(
                matches!(&lowered[0], Decl::Function {ret,body,..} if ret == &expected && matches!(&body[0], Stmt::Let(_,Some(typ),_) if typ==&expected)),
                "{lowered:?}"
            );
        }
    }
}

#[cfg(test)]
mod associated_failure_classification_tests {
    use super::*;
    use crate::core_ir::type_proofs::ResolutionFailure;
    #[test]
    fn cycles_and_duplicates_are_invalid_but_unowned_resolution_stays_unsupported() {
        for source in [
            "impl Other for Row {type Output=Self::Output;}",
            "impl Other for Row {type Output=Self::Other;type Other=Self::Output;}",
            "impl Other for Row {type Output=i64;type Output=i32;}",
        ] {
            let implementation: syn::ItemImpl = syn::parse_str(source).unwrap();
            assert!(
                matches!(
                    resolve_impl_type(
                        Typ::Named("Self::Output".into()),
                        &Typ::Named("Row".into()),
                        &implementation
                    ),
                    Err(ResolutionFailure::Invalid(_))
                ),
                "{source}"
            );
        }
        for source in [
            "impl Other for Row {}",
            "impl Other for Row {type Output<T>=T;}",
            "impl Other for Row {type Output=<Row as Other>::Output;}",
        ] {
            let implementation: syn::ItemImpl = syn::parse_str(source).unwrap();
            assert!(
                matches!(
                    resolve_impl_type(
                        Typ::Named("Self::Output".into()),
                        &Typ::Named("Row".into()),
                        &implementation
                    ),
                    Err(ResolutionFailure::Unsupported(_))
                ),
                "{source}"
            );
        }
    }
    #[test]
    fn missing_associated_annotations_keep_unsupported_proof_without_rewriting_self_value_paths() {
        let implementation: syn::ItemImpl =
            syn::parse_str("impl Other for Row {fn make()->Self::Output{Self::new()}}").unwrap();
        let declarations = lower_impl(&implementation);
        let Decl::Function { ret, body, .. } = &declarations[0] else {
            panic!()
        };
        let Typ::Named(ret) = ret else { panic!() };
        assert!(matches!(
            crate::core_ir::type_proofs::decode(ret),
            Some(ResolutionFailure::Unsupported(_))
        ));

        assert!(format!("{body:?}").contains("Row::new"));
    }
}

#[cfg(test)]
mod uninitialized_declaration_tests {
    use super::*;
    #[test]
    fn typed_identifier_declarations_have_no_initializer() {
        let module = parse_rust_source(
            "fn main(){let mut value:i64;value=42;let closure=||{let inner:bool;};}",
        )
        .unwrap();
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!()
        };
        assert!(
            matches!(&body[0],Stmt::Declare(name,Typ::Named(typ)) if name=="value" && typ=="#rust-integer-i64")
        );
        assert!(matches!(&body[1],Stmt::Assign(name,_) if name=="value"));
        let Stmt::Let(_, _, Expr::Closure { body, .. }) = &body[2] else {
            panic!()
        };
        assert!(matches!(&body[0],Stmt::Declare(name,Typ::Bool) if name=="inner"));
    }
    #[test]
    fn unsupported_uninitialized_patterns_are_not_dropped() {
        for source in [
            "fn main(){let value;}",
            "fn main(){let (a,b):(i64,i64);}",
            "fn main(){let _:i64;}",
            "fn main(){let ref value:i64;}",
        ] {
            let module = parse_rust_source(source).unwrap();
            let Decl::Function { body, .. } = &module.decls[0] else {
                panic!()
            };
            assert!(
                matches!(&body[0],Stmt::Expr(Expr::Call{callee,args}) if args.is_empty() && matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::UNSUPPORTED_DEFINED_BODY)),
                "{source}"
            );
        }
    }
    #[test]
    fn impl_declaration_annotation_retains_associated_cycle_proof() {
        let module = parse_rust_source(
            "struct Row{} trait Other{type Output;fn work();} impl Other for Row{type Output=Self::Output;fn work(){let value:Self::Output;}} ",
        )
        .unwrap();
        assert!(crate::core_ir::type_proofs::present(&module.decls));
    }
}

#[cfg(test)]
mod associated_value_path_tests {
    use super::*;
    fn callees(source: &str) -> HashSet<String> {
        let implementation: syn::ItemImpl = syn::parse_str(source).unwrap();
        let mut declarations = lower_impl(&implementation);
        let mut names = HashSet::new();
        crate::core_opt::map_decl_exprs_mut(&mut declarations, &mut |expr| {
            if let Expr::Call { callee, .. } = expr
                && let Expr::Ident(name) = callee.as_ref()
            {
                names.insert(name.clone());
            }
        });
        names
    }
    #[test]
    fn associated_value_path_matches_explicit_nominal_control() {
        for (annotation, target) in [
            ("Self::Output", "Self::Output::identity"),
            ("Boxed", "Boxed::identity"),
        ] {
            let source = format!(
                "impl std::ops::Neg for Row{{type Output=Boxed;fn neg(self)->Self::Output{{let value:{annotation}=Boxed{{x:42}};{target}(value)}}}}"
            );
            let names = callees(&source);
            assert!(names.contains("Boxed::identity"), "{names:?}");
            assert!(!names.contains("Row::Output::identity"));
        }
    }
    #[test]
    fn associated_type_owner_is_impl_scoped_and_ordinary_self_calls_stay_ordinary() {
        for (receiver, output) in [("First", "one::Boxed"), ("Second", "two::Boxed")] {
            let names = callees(&format!(
                "impl Other for {receiver}{{type Output={output};fn create()->Self::Output{{Self::Output::new()}}}}"
            ));
            assert!(names.contains(&format!("{output}::new")), "{names:?}");
        }
        assert!(callees("impl Row{fn forward()->Self{Self::new()}}").contains("Row::new"));
    }
    #[test]
    fn unsupported_projections_never_use_contextual_reference_or_integer_symbols() {
        for output in ["Self::Output", "&Row", "i64", "<Self as Other>::Output"] {
            let names = callees(&format!(
                "impl Other for Row{{type Output={output};fn create()->Self::Output{{Self::Output::new()}}}}"
            ));
            assert!(
                names
                    .iter()
                    .any(|name| crate::core_ir::type_proofs::decode(name).is_some()),
                "{output}: {names:?}"
            );
            assert!(!names.contains("Row::Output::new"));
        }
        let generic = callees(
            "impl<T> Other for Row<T>{type Output=Self;fn create()->Self::Output{Self::Output::new()}}",
        );
        assert!(
            generic
                .iter()
                .any(|name| crate::core_ir::type_proofs::decode(name).is_some()),
            "{generic:?}"
        );
        let names = callees(
            "impl Other for Row{type Output<T>=T;fn create()->Self::Output<i64>{Self::Output::new()}}",
        );
        assert!(
            names
                .iter()
                .any(|name| crate::core_ir::type_proofs::decode(name).is_some()),
            "{names:?}"
        );
    }
}

#[cfg(test)]
mod primitive_constant_tests {
    use super::*;
    #[test]
    fn exact_primitive_paths_and_associated_owners_keep_integer_metadata() {
        for source in [
            "fn main()->u64{u64::MAX}",
            "fn main()->u64{core::primitive::u64::MAX}",
            "fn main()->u64{std::primitive::u64::MAX}",
            "fn main()->u64{::core::primitive::u64::MAX}",
            "fn main()->u64{::std::primitive::u64::MAX}",
            "struct Row{} trait Other{type Output;fn make()->u64;} impl Other for Row{type Output=u64;fn make()->u64{Self::Output::MAX}}",
        ] {
            let mut module = parse_rust_source(source).unwrap();
            let mut found = false;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                found |= matches!(expr, Expr::Ident(name) if name == "#rust-integer-u64::MAX");
            });
            assert!(found, "{source}");
        }
        let path: syn::Path = syn::parse_str("domain::u64::MAX").unwrap();
        assert!(rust_integer_constant(&path, false).is_none());
        let path: syn::Path = syn::parse_str("::u64::MAX").unwrap();
        assert!(
            crate::core_ir::type_proofs::decode(&rust_integer_constant(&path, false).unwrap())
                .is_some()
        );
    }
    #[test]
    fn external_primitive_root_aliases_require_crate_identity() {
        for source in [
            "extern crate alloc as std;fn main()->u8{::std::primitive::u8::MAX}",
            "extern crate alloc as core;fn main()->u8{::core::primitive::u8::MAX}",
        ] {
            assert!(
                parse_rust_source(source)
                    .unwrap_err()
                    .contains("crate identity resolution")
            );
        }
        assert!(
            parse_rust_source("extern crate std;fn main()->u8{::std::primitive::u8::MAX}").is_ok()
        );
    }
    #[test]
    fn absolute_bare_primitive_type_paths_keep_resolution_proofs() {
        for source in [
            "fn main()->::u8{u8::MAX}",
            "fn main()->i64{let value: ::u8=u8::MAX;if value==255u8{42}else{7}}",
        ] {
            let module = parse_rust_source(source).unwrap();
            assert!(crate::core_ir::type_proofs::present(&module.decls));
        }
    }
    #[test]
    fn relative_primitive_roots_require_unshadowed_lexical_identity() {
        for source in [
            "mod std{}fn main()->u8{std::primitive::u8::MAX}",
            "mod core{}fn main()->u8{core::primitive::u8::MAX}",
            "mod other{}use other as std;fn main()->u8{std::primitive::u8::MAX}",
            "fn main()->u8{mod core{}core::primitive::u8::MAX}",
            "fn main()->u8{mod other{}use other as std;std::primitive::u8::MAX}",
            "fn entry<core>()->u8{core::primitive::u8::MAX}",
            "fn main()->u8{let value={mod core{}core::primitive::u8::MAX};value}",
            "fn main()->u8{{mod std{}std::primitive::u8::MAX}}",
            "fn main()->u8{if true{mod core{}core::primitive::u8::MAX}else{0u8}}",
            "fn main()->u8{unsafe{mod std{}std::primitive::u8::MAX}}",
            "fn main()->u8{let value={mod other{}use other as std;std::primitive::u8::MAX};value}",
            "static VALUE:u8={mod core{}core::primitive::u8::MAX};fn main()->u8{VALUE}",
            "mod std{}struct Row;trait Build{type Output;fn make()->i64;}impl Build for Row{type Output=std::primitive::u8;fn make()->i64{Self::Output::MAX as i64}}",
        ] {
            let module = parse_rust_source(source).unwrap();
            let proofs = crate::typecheck::TypeChecker::new().typed_proof_nodes(&module.decls);
            assert!(!proofs.is_empty(), "{source}");
            assert!(
                proofs.values().all(|proof| !proof.is_invalid()),
                "{proofs:?}"
            );
            assert!(
                proofs
                    .values()
                    .all(|proof| proof.message().contains("shadowed primitive root")),
                "{proofs:?}"
            );
        }
    }

    #[test]
    fn absolute_primitive_roots_and_unrelated_scopes_remain_available() {
        for source in [
            "mod std{}fn main()->u8{::std::primitive::u8::MAX}",
            "mod core{}fn main()->u8{::core::primitive::u8::MAX}",
            "fn main()->u8{mod core{}::core::primitive::u8::MAX}",
            "fn main()->u8{let value={mod core{}::core::primitive::u8::MAX};value}",
            "fn main()->u8{if true{mod std{}::std::primitive::u8::MAX}else{0u8}}",
            "mod std{}mod child{pub fn entry()->u8{std::primitive::u8::MAX}}fn main()->u8{child::entry()}",
            "fn main()->u8{{mod std{}let ignored=::std::primitive::u8::MAX;}std::primitive::u8::MAX}",
        ] {
            let module = parse_rust_source(source).unwrap();
            assert!(
                crate::typecheck::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{source}"
            );
            let mut view = module.decls.clone();
            let mut found = false;
            crate::core_opt::map_decl_exprs_mut(&mut view, &mut |expr| {
                found |= matches!(expr,Expr::Ident(name) if name=="#rust-integer-u8::MAX");
            });
            assert!(found, "{source}");
        }
        let module = parse_rust_source("mod std{}fn ambiguous()->u8{std::primitive::u8::MAX}fn main()->u8{::std::primitive::u8::MAX}").unwrap();
        let proofs = crate::typecheck::TypeChecker::new().typed_proof_nodes(&module.decls);
        assert!(proofs.contains_key("ambiguous"));
        assert!(!proofs.contains_key("main"));
    }

    #[test]
    fn primitive_method_calls_keep_exact_owners_and_unknown_calls_refuse() {
        for path in [
            "u64::NOT_A_CONSTANT",
            "u64::wrapping_add",
            "core::primitive::i8::unknown",
            "std::primitive::u32::from",
        ] {
            let expression: syn::Expr = syn::parse_str(path).unwrap();
            let value = lower_expr_with_types(&expression, &mut HashMap::new());
            assert!(
                matches!(value, Expr::Ident(name) if matches!(crate::core_ir::type_proofs::decode(&name), Some(crate::core_ir::type_proofs::ResolutionFailure::Unsupported(_)))),
                "{path}"
            );
            for source in [format!("{path}(1,2)"), format!("(({path}))(1,2)")] {
                let expression: syn::Expr = syn::parse_str(&source).unwrap();
                let call = lower_expr_with_types(&expression, &mut HashMap::new());
                assert!(
                    matches!(call, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if if path=="u64::wrapping_add" { name=="#rust-integer-u64::wrapping_add" } else { matches!(crate::core_ir::type_proofs::decode(name), Some(crate::core_ir::type_proofs::ResolutionFailure::Unsupported(_))) })),
                    "{source}"
                );
            }
        }
        let expression: syn::Expr = syn::parse_str("u64::MAX").unwrap();
        assert_eq!(
            lower_expr_with_types(&expression, &mut HashMap::new()),
            Expr::Ident("#rust-integer-u64::MAX".into())
        );
        for path in ["domain::u64::OTHER", "some::u64::MAX"] {
            let path: syn::Path = syn::parse_str(path).unwrap();
            assert!(rust_integer_constant(&path, false).is_none());
        }
    }

    #[test]
    fn calling_a_primitive_constant_retains_invalid_proof_in_dead_code() {
        for source in [
            "fn main(){if false{u64::MAX();}}",
            "struct Row{} trait Other{type Output;fn make();} impl Other for Row{type Output=u64;fn make(){if false{Self::Output::MAX();}}}",
        ] {
            let mut module = parse_rust_source(source).unwrap();
            let mut invalid = false;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if let Expr::Ident(name) = expr {
                    invalid |= crate::core_ir::type_proofs::decode(name).is_some_and(|proof| {
                        matches!(
                            proof,
                            crate::core_ir::type_proofs::ResolutionFailure::Invalid(_)
                        )
                    });
                }
            });
            assert!(invalid, "{source}");
        }
    }
}

#[cfg(test)]
mod fixed_array_metadata_tests {
    use super::*;
    #[test]
    fn fixed_types_and_values_keep_lengths_without_claiming_vectors() {
        let module = parse_rust_source("fn array(v:[u8;3])->[u8;0]{[]} fn vector()->Vec<u8>{vec![1u8,2u8]} fn repeated()->[u8;2]{[1u8;2]}").unwrap();
        let function = |name: &str| {
            module
                .decls
                .iter()
                .find(|decl| matches!(decl,Decl::Function{name:actual,..} if actual==name))
                .unwrap()
        };
        let Decl::Function {
            params, ret, body, ..
        } = function("array")
        else {
            panic!()
        };
        assert_eq!(
            params[0].1,
            crate::core_ir::rust_arrays::typ(Typ::Named("#rust-integer-u8".into()), 3)
        );
        assert_eq!(
            ret,
            &crate::core_ir::rust_arrays::typ(Typ::Named("#rust-integer-u8".into()), 0)
        );
        assert!(
            matches!(body.last(),Some(Stmt::Return(Some(Expr::Unary{op,expr}))) if crate::core_ir::rust_arrays::value_length(op)==Some(0) && matches!(expr.as_ref(),Expr::ArrayLit(items) if items.is_empty()))
        );
        let Decl::Function { ret, body, .. } = function("vector") else {
            panic!()
        };
        assert!(matches!(ret, Typ::Vector(_)));
        assert!(matches!(
            body.last(),
            Some(Stmt::Return(Some(Expr::ArrayLit(_))))
        ));
        let Decl::Function { body, .. } = function("repeated") else {
            panic!()
        };
        assert!(
            matches!(body.last(),Some(Stmt::Return(Some(Expr::Unary{op,expr}))) if crate::core_ir::rust_arrays::value_length(op)==Some(2) && matches!(expr.as_ref(),Expr::Call{callee,args} if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::ARRAY_REPEAT) && args.len()==2))
        );
    }
    #[test]
    fn unresolved_array_lengths_are_explicit_proofs() {
        let typ = map_type(&syn::parse_quote!([u8; N]));
        assert!(matches!(
            crate::core_ir::type_proofs::failure_in_type(&typ),
            Some(crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                _
            ))
        ));
        let typ = map_type(&syn::parse_quote!([u8; 3u8]));
        assert!(matches!(
            crate::core_ir::type_proofs::failure_in_type(&typ),
            Some(crate::core_ir::type_proofs::ResolutionFailure::Invalid(_))
        ));
    }
    #[test]
    fn fixed_array_counts_use_source_constants_without_runtime_local_capture() {
        let artifact = parse_rust_artifact_source("const COUNT:usize=1+2;fn values()->[i64;COUNT]{[7;COUNT]}fn shadow()->i64{let COUNT=3;let values=[7;COUNT];42}", "counts").unwrap();
        let values = artifact
            .semantic
            .decls
            .iter()
            .find(|decl| matches!(decl,Decl::Function{name,..} if name=="values"))
            .unwrap();
        let Decl::Function { ret, body, .. } = values else {
            unreachable!()
        };
        assert_eq!(
            match ret {
                Typ::Named(name) =>
                    crate::core_ir::rust_arrays::decode(name).map(|array| array.length),
                _ => None,
            },
            Some(3)
        );
        assert!(
            matches!(body.last(),Some(Stmt::Return(Some(Expr::Unary{op,..}))) if crate::core_ir::rust_arrays::value_length(op)==Some(3))
        );
        let shadow = artifact
            .semantic
            .decls
            .iter()
            .find(|decl| matches!(decl,Decl::Function{name,..} if name=="shadow"))
            .unwrap();
        let mut shadow = shadow.clone();
        let mut failure = None;
        if let Decl::Function { body, .. } = &mut shadow {
            for stmt in body {
                crate::core_opt::map_stmt_exprs_mut(stmt, &mut |expr| {
                    if let Expr::Ident(name) = expr {
                        failure = crate::core_ir::type_proofs::prefer_failure(
                            failure.take(),
                            crate::core_ir::type_proofs::decode(name),
                        );
                    }
                });
            }
        }
        assert!(matches!(
            failure,
            Some(crate::core_ir::type_proofs::ResolutionFailure::Invalid(_))
        ));
    }
}

#[cfg(test)]
mod floating_source_metadata_tests {
    use super::*;
    use crate::core_ir::rust_floats::{self, Kind};

    const DECIMAL: &str = "1.0000000596046447753906250000001";
    fn lower(source: &str) -> Expr {
        lower_expr_with_types(&syn::parse_str(source).unwrap(), &mut HashMap::new())
    }
    #[test]
    fn float_literals_keep_original_decimal_digits_and_suffix_identity() {
        for (source, digits, suffix) in [
            (DECIMAL, DECIMAL, None),
            ("1_000.000_000_059f32", "1000.000000059", Some(Kind::F32)),
            ("42f32", "42", Some(Kind::F32)),
            ("42f64", "42", Some(Kind::F64)),
            ("0.1f64", "0.1", Some(Kind::F64)),
            ("1e39f32", "1e39", Some(Kind::F32)),
        ] {
            let value = lower(source);
            let info = rust_floats::literal_info(&value).unwrap();
            assert_eq!((info.digits, info.suffix), (digits, suffix), "{source}");
        }
    }
    #[test]
    fn float_type_annotations_and_source_aliases_preserve_both_widths() {
        let nominal = map_type(&syn::parse_quote!(owned::f32));
        assert!(rust_floats::kind(&nominal).is_none());

        for (source, kind) in [
            ("f32", Kind::F32),
            ("f64", Kind::F64),
            ("core::primitive::f32", Kind::F32),
        ] {
            let typ = map_type(&syn::parse_str(source).unwrap());
            assert_eq!(typ, rust_floats::typ(kind));
            assert_eq!(type_to_string(&typ), kind.name());
        }
        let module=parse_rust_artifact_source("type Identity<T>=T;fn narrow(value:Identity<f32>)->Identity<f32>{value}fn wide(value:Identity<f64>)->Identity<f64>{value}","floattypes").unwrap();
        for (name, kind) in [("narrow", Kind::F32), ("wide", Kind::F64)] {
            let function = module
                .semantic
                .decls
                .iter()
                .find(|decl| matches!(decl,Decl::Function{name:actual,..}if actual==name))
                .unwrap();
            let Decl::Function { params, ret, .. } = function else {
                unreachable!()
            };
            assert_eq!(
                (params[0].1.clone(), ret.clone()),
                (rust_floats::typ(kind), rust_floats::typ(kind))
            );
        }
    }
    #[test]
    fn only_direct_unsuffixed_literal_casts_acquire_the_target_precision() {
        for source in [
            format!("{DECIMAL} as f32"),
            format!("({DECIMAL}) as f32"),
            format!("(-{DECIMAL}) as f32"),
        ] {
            let Expr::Unary { op, mut expr } = lower(&source) else {
                panic!("cast")
            };
            assert_eq!(rust_floats::cast_kind(&op), Some(Kind::F32));
            if let Expr::Unary { op, expr: inner } = expr.as_ref()
                && op == crate::core_ir::rust_references::NEGATE_VALUE
            {
                expr = inner.clone();
            }
            let info = rust_floats::literal_info(&expr).unwrap();
            assert_eq!(
                (info.digits, info.suffix),
                (DECIMAL, Some(Kind::F32)),
                "{source}"
            );
        }
        for (source, suffix) in [
            (format!("{DECIMAL}f64 as f32"), Some(Kind::F64)),
            (format!("({DECIMAL}+0.0) as f32"), None),
        ] {
            let Expr::Unary { op, expr } = lower(&source) else {
                panic!("cast")
            };
            assert_eq!(rust_floats::cast_kind(&op), Some(Kind::F32));
            let literal = match expr.as_ref() {
                Expr::Binary { lhs, .. } => lhs.as_ref(),
                other => other,
            };
            assert_eq!(rust_floats::literal_info(literal).unwrap().suffix, suffix);
        }
        let Expr::Unary { expr, .. } = lower("value as f32") else {
            panic!("cast")
        };
        assert_eq!(*expr, Expr::Ident("value".into()));
    }
}

#[cfg(test)]
mod integer_magnitude_tests {
    #[test]
    fn high_magnitude_literals_retain_bits_without_assigning_unsigned_ownership() {
        for source in [
            "fn main()->i64{let value:u64=18446744073709551615;42}",
            "fn main()->i64{let value=18446744073709551615;42}",
        ] {
            let mut module = super::parse_rust_source(source).unwrap();
            let mut count = 0;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if let crate::core_ir::Expr::Unary { op, expr } = expr
                    && op == "rust-integer-literal-magnitude"
                {
                    assert_eq!(expr.as_ref(), &crate::core_ir::Expr::IntLit(-1));
                    count += 1;
                }
            });
            assert_eq!(count, 1, "{module:?}");
        }
    }
}

#[cfg(test)]
mod tuple_for_binding_tests {
    use super::*;
    #[test]
    fn tuple_for_patterns_use_real_scoped_projection_bindings() {
        let module=parse_rust_source("fn main()->i64{let value:i8=127;for(_tag,(value,))in[(0,(42i64,))]{let x=value;}value as i64}").unwrap();
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!()
        };
        let mut checks = 0;
        let mut fields = 0;
        let mut view = module.decls.clone();
        crate::core_opt::map_decl_exprs_mut(&mut view, &mut |value| {
            if let Expr::Call { callee, .. } = value {
                if matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::tuples::LET_CHECK)
                {
                    checks += 1;
                }
                if matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::tuples::LET_FIELD)
                {
                    fields += 1;
                }
            }
        });
        assert_eq!(checks, 2, "{body:?}");
        assert_eq!(fields, 4, "{body:?}");
        assert!(matches!(body.last(), Some(Stmt::Return(Some(_)))));
    }
}

#[cfg(test)]
mod opaque_macro_dependency_tests {
    use super::*;

    #[test]
    fn opaque_macros_keep_only_free_local_dependencies_and_remain_unsupported() {
        let locals = ["expected", "value", "object", "body"]
            .into_iter()
            .map(|name| (format!("{LENGTH_LOCAL}{name}"), String::new()))
            .collect();
        for (source, expected) in [
            ("opaque!(next(value), 2147483648)", vec!["value"]),
            ("opaque!({let expected=0;expected}, value)", vec!["value"]),
            ("opaque!((|expected| expected)(0), value)", vec!["value"]),
            ("opaque!(|| expected)", vec!["expected"]),
            ("opaque!((|value| value+expected)(0))", vec!["expected"]),
            ("opaque!({let expected=value;expected})", vec!["value"]),
            (
                "opaque!(object.expected, \"value\", Global::value)",
                vec!["object"],
            ),
            ("opaque!(not => expression)", vec![]),
            ("matches!(object, value)", vec!["object"]),
            ("std::matches!(object, Some(value),)", vec!["object"]),
            ("matches!(value, _)", vec!["value"]),
            (
                "matches!(object, Some(value) if value == expected)",
                vec!["expected", "object"],
            ),
            (
                "matches!(expected, Some(expected) if expected > 0)",
                vec!["expected"],
            ),
            (
                "assert!(matches!(&body[0], Stmt::Let(_,None,Expr::IntLit(value)) if *value==expected))",
                vec!["body", "expected"],
            ),
            (
                "matches!(object, Node{value:expected,..} if expected > 0)",
                vec!["object"],
            ),
            (
                "matches!(object, [value,expected@..] if value>0 && expected.len()>0)",
                vec!["object"],
            ),
            (
                "matches!(object, Some(value)|Other(value) if value==expected)",
                vec!["expected", "object"],
            ),
            ("matches!(object, Some(value) if)", vec![]),
        ] {
            let syn::Expr::Macro(mac) = syn::parse_str(source).unwrap() else {
                panic!()
            };
            assert_eq!(
                opaque_macro_dependencies(&mac.mac, &locals),
                Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::UNSUPPORTED_DEFINED_BODY.into())),
                    args: expected
                        .into_iter()
                        .map(|name| Expr::Ident(name.into()))
                        .collect(),
                },
                "{source}"
            );
        }
    }
}
