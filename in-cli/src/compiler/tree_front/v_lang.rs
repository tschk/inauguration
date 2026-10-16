use super::extract::{AstShape, ast_body, first_named, node_txt};
use crate::core_ir::{Stmt, Typ};
use tree_sitter::Node;

const V_AST: AstShape = AstShape {
    block_kinds: &["block"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &[],
    local_decl_kinds: &["short_var_declaration"],
    assignment_kinds: &["assignment_statement"],
    if_kinds: &["if_expression"],
    while_kinds: &["for_statement"],
    call_kinds: &["call_expression"],
    arg_container_kinds: &["argument_list"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["int_literal", "float_literal"],
    string_kinds: &["interpreted_string_literal", "raw_string_literal"],
    type_kinds: &["type", "type_identifier"],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &["match_expression"],
    first_assignment_is_let: false,
    strict_args: false,
    string_decoder: None,
};

pub(super) fn v_params(src: &[u8], func: Node<'_>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    if let Some(plist) = func.child_by_field_name("parameters") {
        let mut w = plist.walk();
        for ch in plist.named_children(&mut w) {
            if ch.kind() != "parameter_declaration" {
                continue;
            }
            let name = ch
                .child_by_field_name("name")
                .or_else(|| first_named(ch, "identifier"))
                .map(|n| node_txt(src, n).trim().to_string())
                .unwrap_or_else(|| "_".to_string());
            let ty = ch
                .child_by_field_name("type")
                .or_else(|| first_named(ch, "type_identifier"))
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Named("Any".into()));
            out.push((name, ty));
        }
    }
    out
}

pub(super) fn v_return_type(src: &[u8], func: Node<'_>) -> Option<Typ> {
    let params = func.child_by_field_name("parameters")?;
    let mut saw_params = false;
    let mut w = func.walk();
    for node in func.named_children(&mut w) {
        if node == params {
            saw_params = true;
            continue;
        }
        if saw_params && matches!(node.kind(), "type_identifier" | "type") {
            return Some(Typ::Named(node_txt(src, node).trim().to_string()));
        }
        if node.kind() == "block" {
            break;
        }
    }
    None
}

pub(super) fn v_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, V_AST)
}

// ─── OCaml dispatch ───────────────────────────────────────────────────
