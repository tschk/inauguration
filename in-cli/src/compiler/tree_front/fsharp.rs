use super::extract::{
    AstShape, ast_body, ast_expr, ast_stmt, collect_kinds, decl_fn, extract_fn_nodes,
    find_return_expr, first_named, infer_expr_type, node_txt, normalize_entry, simple_bounded_body,
};
use crate::core_ir::{Decl, Stmt, Typ, Visibility};
use std::collections::HashSet;
use tree_sitter::Node;

const FSHAST: AstShape = AstShape {
    block_kinds: &[],
    return_kinds: &[],
    expr_stmt_kinds: &[],
    local_decl_kinds: &["let_statement"],
    assignment_kinds: &[],
    if_kinds: &["if_expression"],
    while_kinds: &[],
    call_kinds: &[
        "function_call_expression",
        "member_call_expression",
        "call_expression",
    ],
    arg_container_kinds: &[],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["integer"],
    string_kinds: &["string"],
    type_kinds: &["type_"],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
    number_decoder: None,
};

pub(super) fn extract_fsharp(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut mod_nodes = Vec::new();
    collect_kinds(root, &["module"], &mut mod_nodes);
    for m in mod_nodes {
        let name = m
            .child_by_field_name("name")
            .or_else(|| first_named(m, "identifier").or_else(|| first_named(m, "long_identifier")))
            .map(|n| node_txt(src, n).trim().to_string());
        if let Some(name) = name {
            let (fields, methods) = fsharp_module_body(src, m);
            decls.push(Decl::Class {
                name,
                fields,
                methods,
                visibility: Visibility::Pub,
                extends: None,
                implements: vec![],
                type_params: vec![],
            });
        }
    }

    let mut type_nodes = Vec::new();
    collect_kinds(root, &["type_definition"], &mut type_nodes);
    for t in type_nodes {
        if let Some(d) = fsharp_type_decl(src, t) {
            decls.push(d);
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_or_value_defn"], &mut func_nodes);
    for f in func_nodes {
        let is_in_type = f
            .parent()
            .is_some_and(|p| p.kind() == "type_definition" || p.kind() == "module");
        if !is_in_type && let Some(d) = fsharp_function_decl(src, f) {
            decls.push(d);
        }
    }

    if decls.is_empty() {
        extract_fn_nodes(src, root, &["function_or_value_defn"], |src, n| {
            let left = first_named(n, "function_declaration_left")?;
            let mut w = left.walk();
            let name_node = left
                .named_children(&mut w)
                .find(|c| matches!(c.kind(), "identifier" | "op_identifier"))?;
            let name = normalize_entry(node_txt(src, name_node).trim());
            Some(decl_fn(name, vec![], Typ::Void))
        })
    } else {
        Ok(decls)
    }
}

fn fsharp_type_decl<'a>(src: &[u8], t: Node<'a>) -> Option<Decl> {
    let name_n = t
        .child_by_field_name("name")
        .or_else(|| first_named(t, "identifier"))?;
    let name = node_txt(src, name_n).trim().to_string();

    let first_kid = t.named_child(0);
    let is_class = first_kid.is_some_and(|c| {
        let raw = node_txt(src, c).trim().to_lowercase();
        raw == "class"
    });
    let is_struct = first_kid.is_some_and(|c| {
        let raw = node_txt(src, c).trim().to_lowercase();
        raw == "struct"
    });

    let fields = Vec::new();
    let mut methods = Vec::new();

    if is_class || is_struct {
        let mut mdefs = Vec::new();
        collect_kinds(t, &["member_definition"], &mut mdefs);
        for md in mdefs {
            if let Some(d) = fsharp_member_decl(src, md) {
                methods.push(d);
            }
        }
    }

    if is_struct {
        Some(Decl::Struct {
            name,
            fields,
            type_params: vec![],
        })
    } else {
        Some(Decl::Class {
            name,
            fields,
            methods,
            visibility: Visibility::Pub,
            extends: None,
            implements: vec![],
            type_params: vec![],
        })
    }
}

fn fsharp_module_body<'a>(src: &[u8], m: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let fields = Vec::new();
    let mut methods = Vec::new();

    let mut func_nodes = Vec::new();
    collect_kinds(m, &["function_or_value_defn"], &mut func_nodes);
    for f in func_nodes {
        if let Some(d) = fsharp_function_decl(src, f) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn fsharp_member_decl<'a>(src: &[u8], md: Node<'a>) -> Option<Decl> {
    let name_n = md
        .child_by_field_name("name")
        .or_else(|| first_named(md, "method_or_prop_name"))
        .or_else(|| {
            let mut ids = Vec::new();
            collect_kinds(md, &["identifier"], &mut ids);
            ids.into_iter().next()
        })?;
    let name = node_txt(src, name_n).trim().to_string();
    let params = fsharp_params(src, md);
    let body = md
        .child_by_field_name("body")
        .map(|b| fsharp_body(src, b))
        .unwrap_or_default();

    Some(Decl::Function {
        name,
        params,
        ret: Typ::Named("unit".into()),
        body,
        type_params: vec![],
    })
}

fn fsharp_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let left = first_named(n, "function_declaration_left")?;
    let mut w = left.walk();
    let name_node = left
        .named_children(&mut w)
        .find(|c| matches!(c.kind(), "identifier" | "op_identifier"))?;
    let name = normalize_entry(node_txt(src, name_node).trim());

    let params = fsharp_params(src, n);
    let body = n
        .child_by_field_name("body")
        .map(|b| fsharp_body(src, b))
        .unwrap_or_default();
    let ret_type = n
        .child_by_field_name("return_type")
        .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
        .unwrap_or_else(|| infer_fsharp_ret(&body));

    Some(Decl::Function {
        name,
        params,
        ret: ret_type,
        body,
        type_params: vec![],
    })
}

fn fsharp_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let left = first_named(n, "function_declaration_left");
    let right = first_named(n, "function_declaration_right");
    let Some(target) = left.or(right) else {
        return out;
    };

    let mut w = target.walk();
    for ch in target.named_children(&mut w) {
        if ch.kind() == "tuple_pattern" {
            let mut tw = ch.walk();
            for tp in ch.named_children(&mut tw) {
                if tp.kind() == "identifier" {
                    out.push((
                        node_txt(src, tp).trim().to_string(),
                        Typ::Named("obj".into()),
                    ));
                }
            }
        }
    }
    out
}

fn infer_fsharp_ret(body: &[Stmt]) -> Typ {
    let inferred = if let Some(Stmt::Expr(expr) | Stmt::Return(Some(expr))) = body.last() {
        infer_expr_type(expr)
    } else if let Some(expr) = find_return_expr(body) {
        infer_expr_type(expr)
    } else {
        Typ::Named("unit".into())
    };
    if inferred.canonical() == Typ::Named("Any".into()) {
        Typ::Named("unit".into())
    } else {
        inferred
    }
}

fn fsharp_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    // ponytail: unit literal () = empty body
    if body.kind() == "unit" {
        return vec![];
    }
    let stmts = ast_body(src, body, FSHAST);
    if !stmts.is_empty() {
        return stmts;
    }
    let mut locals = HashSet::new();
    let mut out = Vec::new();
    let mut w = body.walk();
    for ch in body.named_children(&mut w) {
        if let Some(stmt) = ast_stmt(src, ch, FSHAST, &mut locals) {
            out.push(stmt);
        }
    }
    if !out.is_empty() {
        out
    } else {
        simple_bounded_body(node_txt(src, body), "=")
            .or_else(|| ast_expr(src, body, FSHAST).map(|expr| vec![Stmt::Expr(expr)]))
            .unwrap_or_default()
    }
}
