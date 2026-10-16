use super::extract::{
    AstShape, ast_body, extract_fn_nodes, first_named, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Stmt, Typ};
use tree_sitter::Node;

const SWIFT_AST: AstShape = AstShape {
    block_kinds: &["statements"],
    return_kinds: &["control_transfer_statement"],
    expr_stmt_kinds: &["expression_statement"],
    local_decl_kinds: &["value_binding_pattern"],
    assignment_kinds: &["assignment"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    call_kinds: &["call_expression"],
    arg_container_kinds: &["value_arguments", "call_suffix"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["tuple_expression"],
    binary_kinds: &[
        "infix_expression",
        "comparison_expression",
        "equality_expression",
        "conjunction_expression",
        "disjunction_expression",
        "additive_expression",
        "multiplicative_expression",
        "bitwise_operation",
    ],
    unary_kinds: &["prefix_expression", "postfix_expression"],
    int_kinds: &[
        "integer_literal",
        "hex_literal",
        "oct_literal",
        "bin_literal",
    ],
    string_kinds: &[
        "line_string_literal",
        "multi_line_string_literal",
        "raw_string_literal",
    ],
    type_kinds: &["type", "user_type", "array_type", "optional_type"],
    local_decl_prefixes: &["let ", "var "],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &["do_statement"],
    catch_kinds: &["catch_keyword"],
    match_kinds: &["switch_statement"],
    first_assignment_is_let: true,
    strict_args: false,
    string_decoder: None,
};

pub(super) fn extract_swift(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    extract_fn_nodes(src, root, &["function_declaration"], |src, n| {
        let name_n = n.child_by_field_name("name")?;
        let name = normalize_entry(node_txt(src, name_n).trim());
        let params = swift_params(src, n);
        let ret = n
            .child_by_field_name("return_type")
            .and_then(|t| first_named(t, "type_annotation").or(Some(t)))
            .map(|t| norm_swift_type(node_txt(src, t).trim()))
            .unwrap_or(Typ::Void);
        let body = n
            .child_by_field_name("body")
            .map(|b| swift_body(src, b))
            .unwrap_or_default();
        Some(Decl::Function {
            name,
            params,
            ret,
            body,
            type_params: vec![],
        })
    })
}

fn norm_swift_type(raw: &str) -> Typ {
    match raw.trim() {
        "Int" | "Int8" | "Int16" | "Int32" | "Int64" | "UInt" | "UInt8" | "UInt16" | "UInt32"
        | "UInt64" => Typ::Int,
        "String" => Typ::String,
        "Bool" => Typ::Bool,
        "Float" | "Double" | "Float32" | "Float64" | "CGFloat" => Typ::Float,
        "Void" | "()" => Typ::Void,
        other => Typ::Named(other.to_string()),
    }
}

fn swift_params(src: &[u8], func: Node<'_>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    // ponytail: tree-sitter-swift 0.7.x puts parameter nodes as direct children
    // of function_declaration, not under a named "parameters" field
    for i in 0..func.child_count() {
        let Some(ch) = func.child(i as u32) else {
            continue;
        };
        if ch.kind() != "parameter" {
            continue;
        }
        let name = first_named(ch, "simple_identifier")
            .map(|n| normalize_entry(node_txt(src, n).trim()))
            .unwrap_or_else(|| "_".to_string());
        if name == "_" {
            continue;
        }
        let ty = first_named(ch, "user_type")
            .map(|t| norm_swift_type(node_txt(src, t).trim()))
            .unwrap_or(Typ::Named("Any".into()));
        out.push((name, ty));
    }
    out
}

fn swift_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, SWIFT_AST)
}

// ─── Go ───────────────────────────────────────────────────────────────
