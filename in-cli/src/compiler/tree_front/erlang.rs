use super::extract::{
    AstShape, ast_body, collect_kinds, decl_fn, extract_fn_nodes, infer_expr_type,
    named_descendant, node_txt, normalize_entry, simple_bounded_body, strict_simple_bounded_body,
};
use crate::core_ir::{Decl, Expr, Stmt, Typ, Visibility};
use tree_sitter::Node;

const ERLANGAST: AstShape = AstShape {
    block_kinds: &[],
    return_kinds: &[],
    expr_stmt_kinds: &[],
    local_decl_kinds: &[],
    assignment_kinds: &["assignment"],
    if_kinds: &["if_expression"],
    while_kinds: &[],
    call_kinds: &["function_call"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
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

pub(super) fn extract_erlang(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut mod_nodes = Vec::new();
    collect_kinds(root, &["module_"], &mut mod_nodes);
    for m in mod_nodes {
        let name = m
            .child_by_field_name("name")
            .or_else(|| named_descendant(m, "atom"))
            .map(|a| node_txt(src, a).trim().trim_matches('\'').to_string());
        if let Some(name) = name {
            let (fields, methods) = erlang_module_body(src, m);
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
    collect_kinds(root, &["function_clause"], &mut func_nodes);
    for f in func_nodes {
        let is_in_module = f
            .parent()
            .and_then(|p| p.parent())
            .is_some_and(|gp| gp.kind() == "module_");
        if !is_in_module && let Some(d) = erlang_function_decl(src, f) {
            decls.push(d);
        }
    }

    if decls.is_empty() {
        extract_fn_nodes(src, root, &["function_clause"], |src, n| {
            let name_n = n.child_by_field_name("name")?;
            let atom = named_descendant(name_n, "atom")?;
            let raw = node_txt(src, atom).trim().trim_matches('\'');
            Some(decl_fn(normalize_entry(raw), vec![], Typ::Void))
        })
    } else {
        Ok(decls)
    }
}

fn erlang_module_body<'a>(src: &[u8], mod_node: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let fields = Vec::new();
    let mut methods = Vec::new();

    let mut record_nodes = Vec::new();
    collect_kinds(mod_node, &["record_decl"], &mut record_nodes);
    for r in record_nodes {
        let rec_name = r
            .child_by_field_name("name")
            .or_else(|| named_descendant(r, "atom"))
            .map(|a| node_txt(src, a).trim().trim_matches('\'').to_string())
            .unwrap_or_else(|| format!("record{}", fields.len()));
        let mut sfields = Vec::new();
        let mut field_decls = Vec::new();
        collect_kinds(r, &["record_field"], &mut field_decls);
        for f in field_decls {
            let fname = f
                .child_by_field_name("name")
                .or_else(|| named_descendant(f, "atom"))
                .map(|a| node_txt(src, a).trim().trim_matches('\'').to_string())
                .unwrap_or_else(|| format!("field{}", sfields.len()));
            if !fname.is_empty() {
                sfields.push((fname, Typ::Named("Any".into())));
            }
        }
        if !sfields.is_empty() {
            methods.push(Decl::Struct {
                name: rec_name,
                fields: sfields,
                type_params: vec![],
            });
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(mod_node, &["function_clause"], &mut func_nodes);
    for f in func_nodes {
        if let Some(d) = erlang_function_decl(src, f) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn erlang_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = if name_n.kind() == "atom" {
        node_txt(src, name_n).trim().trim_matches('\'').to_string()
    } else {
        let atom = named_descendant(name_n, "atom")?;
        node_txt(src, atom).trim().trim_matches('\'').to_string()
    };
    let name = normalize_entry(&name);

    let params = erlang_params(src, n);

    let body = n
        .child_by_field_name("body")
        .map(|b| erlang_body(src, b))
        .unwrap_or_default();
    let ret = infer_erlang_ret(&body);

    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn infer_erlang_ret(body: &[Stmt]) -> Typ {
    if let Some(Stmt::Expr(expr) | Stmt::Return(Some(expr))) = body.last() {
        let t = infer_expr_type(expr);
        if t.canonical() == Typ::Named("Any".into()) {
            return Typ::Void;
        }
        return t;
    }
    Typ::Void
}

fn erlang_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let plist = n.child_by_field_name("params");
    let Some(plist) = plist else {
        return out;
    };
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        let pk = ch.kind();
        if pk == "variable" || pk == "identifier" || pk == "atom" {
            out.push((
                node_txt(src, ch).trim().trim_start_matches('_').to_string(),
                Typ::Named("Any".into()),
            ));
        } else if pk == "pattern" {
            if let Some(var) = named_descendant(ch, "variable") {
                out.push((
                    node_txt(src, var)
                        .trim()
                        .trim_start_matches('_')
                        .to_string(),
                    Typ::Named("Any".into()),
                ));
            } else {
                out.push((format!("arg{}", out.len()), Typ::Named("Any".into())));
            }
        } else {
            out.push((format!("arg{}", out.len()), Typ::Named("Any".into())));
        }
    }
    out
}

fn erlang_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    if let Some(stmts) = strict_simple_bounded_body(node_txt(src, body), "=") {
        return clean_erlang_stmts(stmts);
    }
    let stmts = clean_erlang_stmts(ast_body(src, body, ERLANGAST));
    if !stmts.is_empty() {
        return stmts;
    }
    clean_erlang_stmts(simple_bounded_body(node_txt(src, body), "=").unwrap_or_default())
}

fn clean_erlang_stmts(mut stmts: Vec<Stmt>) -> Vec<Stmt> {
    stmts.retain(|stmt| !matches!(stmt, Stmt::Expr(Expr::Ident(name)) if name.contains("->")));
    stmts
}
