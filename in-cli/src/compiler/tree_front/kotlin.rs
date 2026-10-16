use super::extract::{
    AstShape, ast_defined_body, extract_fn_nodes, first_named, named_descendant, node_txt,
    normalize_entry,
};
use crate::core_ir::{Decl, Stmt, Typ};
use tree_sitter::Node;

const KOTLIN_AST: AstShape = AstShape {
    block_kinds: &["block", "control_structure_body"],
    return_kinds: &["return_expression"],
    expr_stmt_kinds: &[],
    local_decl_kinds: &["property_declaration"],
    assignment_kinds: &["assignment"],
    if_kinds: &["if_expression"],
    while_kinds: &["while_statement", "while_expression"],
    call_kinds: &["call_expression"],
    arg_container_kinds: &["value_arguments"],
    arg_wrapper_kinds: &["value_argument"],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["number_literal"],
    string_kinds: &["string_literal"],
    type_kinds: &["user_type", "type"],
    local_decl_prefixes: &[],
    shell_first_kinds: &["control_structure_body"],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
};

pub(super) fn extract_kotlin(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    extract_fn_nodes(src, root, &["function_declaration"], |src, n| {
        let name_n = n.child_by_field_name("name")?;
        let name = normalize_entry(node_txt(src, name_n).trim());
        let params = kotlin_params(src, n);
        let ret = n
            .child_by_field_name("type")
            .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
            .or_else(|| kotlin_return_type(src, n))
            .unwrap_or(Typ::Void);
        let body = n
            .child_by_field_name("body")
            .or_else(|| first_named(n, "function_body"))
            .map(|b| kotlin_body(src, b))
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

fn kotlin_return_type(src: &[u8], fun: Node<'_>) -> Option<Typ> {
    let params = named_descendant(fun, "function_value_parameters")?;
    let mut after_params = false;
    let mut w = fun.walk();
    for ch in fun.named_children(&mut w) {
        if ch == params {
            after_params = true;
            continue;
        }
        if after_params && matches!(ch.kind(), "user_type" | "type") {
            return Some(Typ::Named(node_txt(src, ch).trim().to_string()));
        }
        if ch.kind() == "function_body" {
            break;
        }
    }
    None
}

fn kotlin_params<'a>(src: &[u8], fun: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let Some(params) = named_descendant(fun, "function_value_parameters") else {
        return out;
    };
    let mut w = params.walk();
    for ch in params.named_children(&mut w) {
        if ch.kind() != "parameter" {
            continue;
        }
        let Some(id) = first_named(ch, "identifier") else {
            continue;
        };
        let pname = node_txt(src, id).trim().to_string();
        let mut ty = Typ::Named("Any".into());
        let mut cw = ch.walk();
        for sub in ch.named_children(&mut cw) {
            if sub.kind() == "user_type" || sub.kind() == "type" {
                ty = Typ::Named(node_txt(src, sub).trim().to_string());
                break;
            }
        }
        out.push((pname, ty));
    }
    out
}

fn kotlin_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_defined_body(src, body, KOTLIN_AST)
}
