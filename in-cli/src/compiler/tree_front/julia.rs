use super::extract::{
    AstShape, ast_body, ast_expr, collect_kinds, decl_fn, extract_fn_nodes, first_named,
    infer_expr_type, last_named, named_descendant, node_txt, normalize_entry, simple_bounded_body,
};
use crate::core_ir::{Decl, Stmt, Typ, Visibility};
use tree_sitter::Node;

const JULIAAST: AstShape = AstShape {
    block_kinds: &["block", "compound_statement"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &[],
    local_decl_kinds: &[],
    assignment_kinds: &["assignment"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement", "for_statement"],
    call_kinds: &["function_call", "call_expression"],
    arg_container_kinds: &["argument_list", "arguments"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["integer"],
    string_kinds: &["string", "string_literal"],
    type_kinds: &[],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &["try_statement"],
    catch_kinds: &["catch_clause"],
    match_kinds: &[],
    first_assignment_is_let: true,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
    number_decoder: None,
};

pub(super) fn extract_julia(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut struct_nodes = Vec::new();
    collect_kinds(root, &["struct_definition"], &mut struct_nodes);
    for s in struct_nodes {
        if let Some(d) = julia_struct_decl(src, s) {
            decls.push(d);
        }
    }

    let mut mod_nodes = Vec::new();
    collect_kinds(root, &["module_definition"], &mut mod_nodes);
    for m in mod_nodes {
        let name = m
            .child_by_field_name("name")
            .or_else(|| named_descendant(m, "identifier"))
            .map(|id| node_txt(src, id).trim().to_string());
        if let Some(name) = name {
            let (fields, methods) = julia_module_body(src, m);
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

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_definition"], &mut func_nodes);
    for f in func_nodes {
        let is_in_module = f.parent().is_some_and(|p| p.kind() == "module_definition");
        if !is_in_module && let Some(d) = julia_function_decl(src, f) {
            decls.push(d);
        }
    }

    if decls.is_empty() {
        extract_fn_nodes(src, root, &["function_definition"], |src, n| {
            let sig = first_named(n, "signature").or_else(|| named_descendant(n, "signature"))?;
            let id = named_descendant(sig, "identifier").or_else(|| {
                let mut ids = Vec::new();
                collect_kinds(sig, &["identifier"], &mut ids);
                ids.into_iter().next()
            })?;
            let name = normalize_entry(node_txt(src, id).trim());
            Some(decl_fn(name, vec![], Typ::Void))
        })
    } else {
        Ok(decls)
    }
}

fn julia_struct_decl<'a>(src: &[u8], s: Node<'a>) -> Option<Decl> {
    let name = s
        .child_by_field_name("name")
        .or_else(|| named_descendant(s, "identifier"))
        .map(|id| node_txt(src, id).trim().to_string())?;
    let mut sfields = Vec::new();
    let body = s
        .child_by_field_name("body")
        .or_else(|| first_named(s, "block"));
    if let Some(body) = body {
        let mut w = body.walk();
        for ch in body.named_children(&mut w) {
            if ch.kind() == "identifier" || ch.kind() == "field" {
                sfields.push((
                    node_txt(src, ch).trim().to_string(),
                    Typ::Named("Any".into()),
                ));
            } else if ch.kind() == "parametric_type"
                && let Some(id) = named_descendant(ch, "identifier")
            {
                sfields.push((
                    node_txt(src, id).trim().to_string(),
                    Typ::Named("Any".into()),
                ));
            }
        }
    }
    Some(Decl::Struct {
        name,
        fields: sfields,
        type_params: vec![],
    })
}

fn julia_module_body<'a>(src: &[u8], m: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let fields = Vec::new();
    let mut methods = Vec::new();

    let body = m
        .child_by_field_name("body")
        .or_else(|| first_named(m, "block"));
    let Some(body) = body else {
        return (fields, methods);
    };

    let mut func_nodes = Vec::new();
    collect_kinds(body, &["function_definition"], &mut func_nodes);
    for f in func_nodes {
        if let Some(d) = julia_function_decl(src, f) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn julia_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let sig = first_named(n, "signature").or_else(|| named_descendant(n, "signature"))?;
    let id = named_descendant(sig, "identifier").or_else(|| {
        let mut ids = Vec::new();
        collect_kinds(sig, &["identifier"], &mut ids);
        ids.into_iter().next()
    })?;
    let name = normalize_entry(node_txt(src, id).trim());

    let params = julia_params(src, &sig);

    let body = n
        .child_by_field_name("body")
        .or_else(|| first_named(n, "block"))
        .or_else(|| last_named(n).filter(|child| child.kind() != "signature"))
        .map(|b| julia_body(src, b))
        .unwrap_or_default();
    let ret = infer_julia_ret(&body);

    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn infer_julia_ret(body: &[Stmt]) -> Typ {
    if let Some(Stmt::Expr(expr) | Stmt::Return(Some(expr))) = body.last() {
        let t = infer_expr_type(expr);
        if t.canonical() == Typ::Named("Any".into()) {
            return Typ::Void;
        }
        return t;
    }
    Typ::Void
}

fn julia_params<'a>(src: &[u8], sig: &Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let plist = sig
        .child_by_field_name("parameters")
        .or_else(|| named_descendant(*sig, "parameter_list"))
        .or_else(|| {
            let mut lists = Vec::new();
            collect_kinds(*sig, &["parameter_list"], &mut lists);
            lists.into_iter().next()
        });
    let Some(plist) = plist else {
        return out;
    };
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        let pk = ch.kind();
        if pk == "identifier" {
            out.push((
                node_txt(src, ch).trim().to_string(),
                Typ::Named("Any".into()),
            ));
        } else if pk == "optional_parameter" || pk == "keyword_parameter" {
            if let Some(id) = named_descendant(ch, "identifier") {
                out.push((
                    node_txt(src, id).trim().to_string(),
                    Typ::Named("Any".into()),
                ));
            }
        } else if (pk == "typed_parameter" || pk == "parameter")
            && let Some(id) = named_descendant(ch, "identifier")
        {
            out.push((
                node_txt(src, id).trim().to_string(),
                Typ::Named("Any".into()),
            ));
        }
    }
    out
}

fn julia_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    // ponytail: try expression first (covers call_expression, juxtaposition_expression, etc.)
    if let Some(expr) = ast_expr(src, body, JULIAAST) {
        return vec![Stmt::Expr(expr)];
    }
    let stmts = ast_body(src, body, JULIAAST);
    if !stmts.is_empty() {
        return stmts;
    }
    simple_bounded_body(node_txt(src, body), "=")
        .or_else(|| ast_expr(src, body, JULIAAST).map(|expr| vec![Stmt::Expr(expr)]))
        .unwrap_or_default()
}
