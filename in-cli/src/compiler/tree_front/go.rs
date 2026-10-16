use super::extract::{AstShape, ast_defined_body, first_named, node_txt};
use crate::core_ir::{Stmt, Typ};
use tree_sitter::Node;

const GO_AST: AstShape = AstShape {
    block_kinds: &["block", "statement_list"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement"],
    local_decl_kinds: &["short_var_declaration", "var_declaration"],
    assignment_kinds: &["assignment_statement"],
    if_kinds: &["if_statement"],
    while_kinds: &["for_statement"],
    call_kinds: &["call_expression"],
    arg_container_kinds: &["argument_list"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["int_literal"],
    string_kinds: &["interpreted_string_literal", "raw_string_literal"],
    type_kinds: &["type", "type_identifier"],
    local_decl_prefixes: &[],
    shell_first_kinds: &["block"],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &["expression_switch_statement", "type_switch_statement"],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
    number_decoder: None,
};

fn norm_go_type(raw: &str) -> Typ {
    match raw.trim() {
        "int" | "int8" | "int16" | "int32" | "int64" | "uint" | "uint8" | "uint16" | "uint32"
        | "uint64" | "byte" | "rune" => Typ::Int,
        "string" => Typ::String,
        "bool" => Typ::Bool,
        "float32" | "float64" => Typ::Float,
        other if other.starts_with("[]") => Typ::Named(other.to_string()),
        other if other.starts_with('*') => Typ::Named(other.to_string()),
        other => Typ::Named(other.to_string()),
    }
}

pub(super) fn go_params(src: &[u8], func: Node<'_>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    if let Some(plist) = func.child_by_field_name("parameters") {
        let mut w = plist.walk();
        for ch in plist.named_children(&mut w) {
            if ch.kind() != "parameter_declaration" {
                continue;
            }
            let name = first_named(ch, "identifier")
                .map(|n| node_txt(src, n).trim().to_string())
                .unwrap_or_else(|| "_".to_string());
            let ty = first_named(ch, "type_identifier")
                .or_else(|| first_named(ch, "pointer_type"))
                .or_else(|| first_named(ch, "type"))
                .map(|t| norm_go_type(node_txt(src, t).trim()))
                .unwrap_or(Typ::Named("Any".into()));
            out.push((name, ty));
        }
    }
    out
}

pub(super) fn go_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_defined_body(src, body, GO_AST)
}

pub(super) fn go_return_type(src: &[u8], func: Node<'_>) -> Option<Typ> {
    // ponytail: prefer field name; fallback scans only direct children between params and body
    if let Some(node) = func
        .child_by_field_name("result")
        .or_else(|| func.child_by_field_name("return_type"))
    {
        return Some(norm_go_type(node_txt(src, node).trim()));
    }
    let params = func.child_by_field_name("parameters")?;
    let mut saw_params = false;
    let mut w = func.walk();
    for node in func.named_children(&mut w) {
        if node == params {
            saw_params = true;
            continue;
        }
        if saw_params
            && matches!(
                node.kind(),
                "type_identifier" | "simple_type" | "qualified_type"
            )
        {
            return Some(norm_go_type(node_txt(src, node).trim()));
        }
        if node.kind() == "block" {
            break;
        }
    }
    None
}

// ─── OCaml ────────────────────────────────────────────────────────────
