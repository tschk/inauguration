use crate::core_atomic::Kind;
use crate::core_ir::{Decl, Expr, Stmt, Typ};
use std::collections::{HashMap, HashSet};
fn collect(tree: &syn::UseTree, prefix: &str, out: &mut HashMap<String, String>) {
    match tree {
        syn::UseTree::Path(path) => collect(&path.tree, &format!("{prefix}{}::", path.ident), out),
        syn::UseTree::Name(name) => {
            if name.ident == "self" {
                if let Some(path) = prefix.strip_suffix("::") {
                    if let Some(alias) = path.rsplit("::").next() {
                        out.insert(alias.into(), path.into());
                    }
                }
            } else {
                out.insert(name.ident.to_string(), format!("{prefix}{}", name.ident));
            }
        }
        syn::UseTree::Rename(name) => {
            out.insert(name.rename.to_string(), format!("{prefix}{}", name.ident));
        }
        syn::UseTree::Group(group) => {
            for tree in &group.items {
                collect(tree, prefix, out);
            }
        }
        syn::UseTree::Glob(_) => {
            if matches!(prefix, "std::sync::atomic::" | "core::sync::atomic::") {
                for name in ["AtomicU32", "AtomicU64", "AtomicUsize", "Ordering"] {
                    out.insert(name.into(), format!("{prefix}{name}"));
                }
            } else if matches!(
                prefix,
                "std::sync::atomic::Ordering::" | "core::sync::atomic::Ordering::"
            ) {
                for name in ["Relaxed", "Acquire", "Release", "AcqRel", "SeqCst"] {
                    out.insert(name.into(), format!("{prefix}{name}"));
                }
            }
        }
    }
}
fn resolve(name: &str, aliases: &HashMap<String, String>) -> String {
    let (head, rest) = name.split_once("::").unwrap_or((name, ""));
    let resolved = aliases
        .get(head)
        .map(|head| {
            if rest.is_empty() {
                head.clone()
            } else {
                format!("{head}::{rest}")
            }
        })
        .unwrap_or_else(|| name.into());
    resolved
        .strip_prefix("core::sync::atomic::")
        .map(|tail| format!("std::sync::atomic::{tail}"))
        .unwrap_or(resolved)
}
fn typ(ty: &mut Typ, aliases: &HashMap<String, String>) {
    match ty {
        Typ::Named(name) => {
            let candidate = resolve(name, aliases);
            if Kind::from_type(&Typ::Named(candidate.clone())).is_some()
                || candidate == "std::sync::atomic::Ordering"
            {
                *name = candidate;
            }
        }
        Typ::Array(elem) | Typ::Vector(elem) => typ(elem, aliases),
        _ => {}
    }
}
fn expr(value: &mut Expr, aliases: &HashMap<String, String>, own: &HashSet<String>) {
    if let Expr::Block { body, result } = value {
        stmts(body, aliases, own);
        if let Some(result) = result {
            expr(result, aliases, own);
        }
    } else if let Expr::Closure {
        params, ret, body, ..
    } = value
    {
        for (_, ty) in params {
            typ(ty, aliases);
        }
        typ(ret, aliases);
        stmts(body, aliases, own);
    } else {
        crate::core_ir::for_each_expr_child_mut(value, &mut |value| expr(value, aliases, own));
    }
    if let Expr::Ident(name) = value {
        if let Some(order) = name.strip_prefix("$atomic_order$") {
            *value = Expr::Call {
                callee: Box::new(Expr::Ident(format!("__inrt_atomic_order_{order}"))),
                args: vec![],
            };
        }
    }
    if let Expr::Call { callee, .. } = value {
        if let Expr::Ident(name) = callee.as_ref() {
            let candidate = resolve(name, aliases);
            if let Some((prefix, op)) = candidate
                .rsplit_once("::")
                .filter(|(_, op)| matches!(*op, "new" | "load" | "store" | "fetch_add"))
            {
                if !own.contains(prefix) {
                    if let Some(kind) = Kind::from_type(&Typ::Named(prefix.into())) {
                        *callee = Box::new(Expr::Ident(kind.intrinsic(op)));
                    }
                }
            }
        }
    }
}
fn stmts(body: &mut [Stmt], aliases: &HashMap<String, String>, own: &HashSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::Let(_, ty, value) => {
                if let Some(ty) = ty {
                    typ(ty, aliases);
                }
                expr(value, aliases, own);
            }
            Stmt::Assign(_, value)
            | Stmt::Expr(value)
            | Stmt::Throw(value)
            | Stmt::Return(Some(value)) => expr(value, aliases, own),
            Stmt::IndexAssign { base, index, value } => {
                expr(base, aliases, own);
                expr(index, aliases, own);
                expr(value, aliases, own);
            }
            Stmt::FieldAssign { base, value, .. } => {
                expr(base, aliases, own);
                expr(value, aliases, own);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                expr(cond, aliases, own);
                stmts(then_body, aliases, own);
                stmts(else_body, aliases, own);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    expr(cond, aliases, own);
                }
                stmts(body, aliases, own);
            }
            Stmt::Match { scrutinee, arms } => {
                expr(scrutinee, aliases, own);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        expr(guard, aliases, own);
                    }
                    stmts(&mut arm.body, aliases, own);
                }
            }
            Stmt::Try { body, catches } => {
                stmts(body, aliases, own);
                for catch in catches {
                    stmts(&mut catch.body, aliases, own);
                }
            }
            _ => {}
        }
    }
}
pub(super) fn normalize(decls: &mut [Decl], file: &syn::File) {
    let mut aliases = HashMap::new();
    for item in &file.items {
        if let syn::Item::Use(item) = item {
            collect(&item.tree, "", &mut aliases);
        }
    }
    aliases.retain(|_, path| {
        path.starts_with("std::sync::atomic")
            || path.starts_with("core::sync::atomic")
            || matches!(path.as_str(), "std" | "std::sync" | "core" | "core::sync")
    });
    let own: HashSet<_> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Struct { name, .. } | Decl::Class { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    aliases.retain(|alias, _| {
        !own.contains(alias)
            && !file.items.iter().any(|item| match item {
                syn::Item::Enum(item) => item.ident == alias.as_str(),
                syn::Item::Type(item) => item.ident == alias.as_str(),
                _ => false,
            })
    });
    let variants = ["Relaxed", "Acquire", "Release", "AcqRel", "SeqCst"];
    let mut references = HashMap::new();
    for (index, variant) in variants.iter().enumerate() {
        let path = format!("std::sync::atomic::Ordering::{variant}");
        for candidate in [path.clone(), path.replacen("std::", "core::", 1)] {
            references.insert(candidate, format!("$atomic_order${index}"));
        }
        for (alias, target) in &aliases {
            let target = resolve(target, &HashMap::new());
            if path == target {
                references.insert(alias.clone(), format!("$atomic_order${index}"));
            } else if let Some(tail) = path.strip_prefix(&format!("{target}::")) {
                references.insert(format!("{alias}::{tail}"), format!("$atomic_order${index}"));
            }
        }
    }
    let mut names = crate::core_bindings::module_names(decls);
    names.extend(references.keys().cloned());
    let children: HashSet<_> = file
        .items
        .iter()
        .filter_map(|item| {
            if let syn::Item::Mod(module) = item {
                Some(module.ident.to_string())
            } else {
                None
            }
        })
        .collect();
    for decl in decls {
        let nested = match decl {
            Decl::Function { name, .. } | Decl::Global { name, .. } => name
                .split("::")
                .next()
                .is_some_and(|name| children.contains(name)),
            _ => false,
        };
        if nested {
            continue;
        }
        match decl {
            Decl::Global { typ: ty, init, .. } => {
                typ(ty, &aliases);
                if let Some(init) = init {
                    crate::core_bindings::normalize_initializer_with_references(
                        init,
                        &names,
                        &references,
                    );
                    expr(init, &aliases, &own);
                }
            }
            Decl::Function {
                params, ret, body, ..
            } => {
                for (_, ty) in params.iter_mut() {
                    typ(ty, &aliases);
                }
                typ(ret, &aliases);
                crate::core_bindings::normalize_function_with_references(
                    params,
                    body,
                    &names,
                    &references,
                );
                stmts(body, &aliases, &own);
            }
            _ => {}
        }
    }
}
