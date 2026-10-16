use super::extract::{
    AstShape, ast_body, collect_kinds, first_named, named_descendant, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Stmt, Typ};
use tree_sitter::Node;

const LUAAST: AstShape = AstShape {
    block_kinds: &["block", "chunk"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &[],
    local_decl_kinds: &["variable_declaration"],
    assignment_kinds: &["assignment_statement"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement", "repeat_statement"],
    call_kinds: &["function_call"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &["expression_list", "expression", "variable", "variable_list"],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["number"],
    string_kinds: &["string"],
    type_kinds: &[],
    local_decl_prefixes: &["local "],
    shell_first_kinds: &["statement"],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    string_decoder: None,
};

pub(super) fn extract_lua(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_declaration"], &mut func_nodes);
    for n in func_nodes {
        if let Some(d) = lua_function_decl(src, n) {
            decls.push(d);
        }
    }

    let mut local_hits = Vec::new();
    collect_kinds(root, &["variable_declaration"], &mut local_hits);
    for v in local_hits {
        if let Some(d) = lua_var_function(src, v) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn lua_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = match n.child_by_field_name("name") {
        Some(nm) => nm,
        None => {
            let mut ids = Vec::new();
            collect_kinds(n, &["identifier", "dot_index_expression"], &mut ids);
            ids.into_iter().next()?
        }
    };
    let raw = node_txt(src, name_n).trim();
    let compact = raw.replace(['.', ':'], "_");
    let name = normalize_entry(&compact);
    let params = lua_params(src, n);
    let body = n
        .child_by_field_name("body")
        .or_else(|| first_named(n, "block"))
        .map(|b| lua_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret: Typ::Void,
        body,
        type_params: vec![],
    })
}

fn lua_var_function<'a>(src: &[u8], v: Node<'a>) -> Option<Decl> {
    let name_n = first_named(v, "identifier")
        .or_else(|| first_named(v, "variable_list").and_then(|vl| first_named(vl, "identifier")))?;
    let name = normalize_entry(node_txt(src, name_n).trim());

    let mut func_defs = Vec::new();
    collect_kinds(v, &["function_definition"], &mut func_defs);
    if func_defs.is_empty() {
        return None;
    }
    let func = func_defs.into_iter().next()?;

    let params = lua_params(src, func);
    let body = func
        .child_by_field_name("body")
        .map(|b| lua_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret: Typ::Void,
        body,
        type_params: vec![],
    })
}

fn lua_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let plist = n
        .child_by_field_name("parameters")
        .or_else(|| named_descendant(n, "parameters"));
    let Some(plist) = plist else {
        return out;
    };
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if ch.kind() == "identifier" {
            out.push((
                node_txt(src, ch).trim().to_string(),
                Typ::Named("Any".into()),
            ));
        } else if ch.kind() == "variadic_argument" {
            let txt = node_txt(src, ch)
                .trim()
                .trim_start_matches("...")
                .to_string();
            out.push((txt, Typ::Named("Any".into())));
        }
    }
    out
}

fn lua_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, LUAAST)
}
