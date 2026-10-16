use super::extract::{
    AstShape, ast_body, collect_kinds, decl_fn, first_named, infer_expr_type, named_descendant,
    node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Stmt, Typ, Visibility};
use tree_sitter::Node;

const ELIXIRAST: AstShape = AstShape {
    block_kinds: &["block", "do_block", "stab_clause"],
    return_kinds: &[],
    expr_stmt_kinds: &[],
    local_decl_kinds: &[],
    assignment_kinds: &["assignment"],
    if_kinds: &["if"],
    while_kinds: &[],
    call_kinds: &["call"],
    arg_container_kinds: &["arguments", "keyword_list"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_call"],
    binary_kinds: &["binary_operator"],
    unary_kinds: &["unary_operator"],
    int_kinds: &["integer"],
    string_kinds: &["string"],
    type_kinds: &[],
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

pub(super) fn extract_elixir(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();
    let mut module_methods = Vec::new();

    let mut mod_nodes = Vec::new();
    collect_kinds(root, &["call"], &mut mod_nodes);
    for c in mod_nodes {
        let mut w = c.walk();
        let kids: Vec<Node<'_>> = c.named_children(&mut w).collect();
        let Some(head) = kids.first().copied() else {
            continue;
        };
        let hk = head.kind();
        let ht = node_txt(src, head).trim();
        if !matches!(hk, "identifier" | "operator_identifier")
            || !matches!(ht, "defmodule" | "defprotocol" | "defexception")
        {
            continue;
        }
        if let Some(second) = kids.get(1).copied() {
            let mod_name = node_txt(src, second).trim().to_string();
            let (fields, methods) = elixir_module_body(src, c);
            module_methods.extend(methods.iter().cloned());
            decls.push(Decl::Class {
                name: mod_name,
                fields,
                methods,
                visibility: Visibility::Pub,
                extends: None,
                implements: vec![],
                type_params: vec![],
            });
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["call"], &mut func_nodes);
    for c in func_nodes {
        let mut w = c.walk();
        let kids: Vec<Node<'_>> = c.named_children(&mut w).collect();
        let Some(head) = kids.first().copied() else {
            continue;
        };
        let hk = head.kind();
        let ht = node_txt(src, head).trim();
        if !matches!(hk, "identifier" | "operator_identifier")
            || !matches!(ht, "def" | "defp" | "defmacro")
        {
            continue;
        }
        let is_in_module = c.parent().is_some_and(|p| {
            if p.kind() == "do_block" {
                if let Some(gp) = p.parent() {
                    gp.kind() == "call"
                        && gp.named_child(0).is_some_and(|nc| {
                            matches!(node_txt(src, nc).trim(), "defmodule" | "defprotocol")
                        })
                } else {
                    false
                }
            } else {
                false
            }
        });
        if is_in_module {
            continue;
        }
        if let Some(d) = elixir_function_decl(src, c) {
            decls.push(d);
        }
    }

    for method in module_methods {
        if let Decl::Function { name, .. } = &method
            && !decls.iter().any(
                |decl| matches!(decl, Decl::Function { name: existing, .. } if existing == name),
            )
        {
            decls.push(method);
        }
    }

    if decls.is_empty() {
        let mut out = Vec::new();
        let mut calls = Vec::new();
        collect_kinds(root, &["call"], &mut calls);
        for c in calls {
            let mut w = c.walk();
            let kids: Vec<Node<'_>> = c.named_children(&mut w).collect();
            let Some(head) = kids.first().copied() else {
                continue;
            };
            let hk = head.kind();
            let ht = node_txt(src, head).trim();
            if !matches!(hk, "identifier" | "operator_identifier")
                || !matches!(ht, "def" | "defp" | "defmacro")
            {
                continue;
            }
            if let Some(second) = kids.get(1).copied()
                && (second.kind() == "identifier" || second.kind() == "keyword")
            {
                let nm = normalize_entry(node_txt(src, second).trim());
                out.push(decl_fn(nm, vec![], Typ::Void));
            }
        }
        Ok(out)
    } else {
        Ok(decls)
    }
}

fn elixir_module_body<'a>(src: &[u8], call_node: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let fields = Vec::new();
    let mut methods = Vec::new();

    let body = call_node
        .parent()
        .and_then(|p| first_named(p, "do_block"))
        .or_else(|| named_descendant(call_node, "do_block"));
    let Some(body) = body else {
        return (fields, methods);
    };

    let mut call_nodes = Vec::new();
    collect_kinds(body, &["call"], &mut call_nodes);
    for c in call_nodes {
        let mut w = c.walk();
        let kids: Vec<Node<'_>> = c.named_children(&mut w).collect();
        let Some(head) = kids.first().copied() else {
            continue;
        };
        let hk = head.kind();
        let ht = node_txt(src, head).trim();

        if matches!(ht, "defstruct")
            && let Some(name_node) = kids.get(1).copied()
        {
            let sname = node_txt(src, name_node).trim().to_string();
            let mut sfields = Vec::new();
            let sbody = named_descendant(c, "keyword_list");
            if let Some(sbody) = sbody {
                let mut w2 = sbody.walk();
                for ch in sbody.named_children(&mut w2) {
                    if ch.kind() == "pair" {
                        let key = first_named(ch, "keyword")
                            .or_else(|| first_named(ch, "atom"))
                            .or_else(|| first_named(ch, "identifier"))
                            .map(|k| node_txt(src, k).trim().trim_matches(':').to_string());
                        if let Some(k) = key {
                            sfields.push((k, Typ::Named("Any".into())));
                        }
                    }
                }
            }
            methods.push(Decl::Struct {
                name: sname,
                fields: sfields,
                type_params: vec![],
            });
            continue;
        }

        if !matches!(hk, "identifier" | "operator_identifier") {
            continue;
        }
        if matches!(ht, "def" | "defp" | "defmacro")
            && let Some(d) = elixir_function_decl(src, c)
        {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn elixir_function_decl<'a>(src: &[u8], c: Node<'a>) -> Option<Decl> {
    let mut w = c.walk();
    let kids: Vec<Node<'_>> = c.named_children(&mut w).collect();
    let head = kids.first().copied()?;
    if !matches!(node_txt(src, head).trim(), "def" | "defp" | "defmacro") {
        return None;
    }
    let name_n = kids.get(1).copied()?;
    let name_text = node_txt(src, name_n).trim();
    let name = normalize_entry(
        name_text
            .trim_start_matches(':')
            .split('(')
            .next()
            .unwrap_or(name_text)
            .trim(),
    );

    let mut params = Vec::new();
    if matches!(name_n.kind(), "arguments" | "parenthesized_call") {
        let args_node = name_n;
        let mut aw = args_node.walk();
        let mut children = args_node.named_children(&mut aw).peekable();
        // First child is the function name; remaining identifiers are parameters.
        let _ = children.next();
        for ch in children {
            if ch.kind() == "identifier" {
                params.push((
                    node_txt(src, ch).trim().to_string(),
                    Typ::Named("Any".into()),
                ));
            } else if ch.kind() == "binary_operator"
                && let Some(lhs) = ch.child_by_field_name("left")
            {
                params.push((
                    node_txt(src, lhs).trim().to_string(),
                    Typ::Named("Any".into()),
                ));
            }
        }
    }

    let body = named_descendant(c, "do_block")
        .map(|b| elixir_body(src, b))
        .unwrap_or_default();
    let ret = infer_elixir_ret(&body);

    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn infer_elixir_ret(body: &[Stmt]) -> Typ {
    if let Some(Stmt::Expr(expr) | Stmt::Return(Some(expr))) = body.last() {
        let t = infer_expr_type(expr);
        if t.canonical() == Typ::Named("Any".into()) {
            return Typ::Void;
        }
        return t;
    }
    Typ::Void
}

fn elixir_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, ELIXIRAST)
}
