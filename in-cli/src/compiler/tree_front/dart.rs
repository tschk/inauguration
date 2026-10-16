use super::extract::{
    AstShape, ast_body, extract_fn_nodes, first_named, named_descendant, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Stmt, Typ};
use tree_sitter::Node;

const DART_AST: AstShape = AstShape {
    block_kinds: &["function_body", "block"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement"],
    local_decl_kinds: &["local_variable_declaration"],
    assignment_kinds: &["assignment_expression"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    call_kinds: &["call_expression"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &["assignable_expression"],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &[
        "additive_expression",
        "multiplicative_expression",
        "relational_expression",
    ],
    unary_kinds: &["unary_expression"],
    int_kinds: &[
        "decimal_integer_literal",
        "integer_literal",
        "number_literal",
    ],
    string_kinds: &["string_literal"],
    type_kinds: &["type"],
    local_decl_prefixes: &[],
    shell_first_kinds: &["function_body"],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    string_decoder: None,
};

pub(super) fn extract_dart(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    extract_fn_nodes(
        src,
        root,
        &[
            "function_declaration",
            "external_function_declaration",
            "method_declaration",
        ],
        |src, n| {
            let sig = n.child_by_field_name("signature")?;
            let fp = named_descendant(sig, "formal_parameter_list")?;
            let parent = fp.parent()?;
            let mut prev: Option<Node<'_>> = None;
            let mut w = parent.walk();
            for ch in parent.named_children(&mut w) {
                if ch == fp {
                    break;
                }
                prev = Some(ch);
            }
            let name_n = prev?;
            let raw = if name_n.kind() == "identifier" {
                node_txt(src, name_n).trim()
            } else {
                let id = named_descendant(name_n, "identifier")?;
                node_txt(src, id).trim()
            };
            let name = normalize_entry(raw);
            let params = dart_params(src, fp);
            let ret = sig
                .child_by_field_name("return_type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Void);
            let body = n
                .child_by_field_name("body")
                .or_else(|| first_named(n, "function_body"))
                .map(|b| dart_body(src, b))
                .unwrap_or_default();
            Some(Decl::Function {
                name,
                params,
                ret,
                body,
                type_params: vec![],
            })
        },
    )
}

fn dart_params<'a>(src: &[u8], plist: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if ch.kind() != "formal_parameter" {
            continue;
        }
        let Some(name) = ch
            .child_by_field_name("name")
            .or_else(|| first_named(ch, "identifier"))
        else {
            continue;
        };
        let ty = ch
            .child_by_field_name("type")
            .or_else(|| first_named(ch, "type"))
            .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
            .unwrap_or(Typ::Named("Any".into()));
        out.push((node_txt(src, name).trim().to_string(), ty));
    }
    out
}

fn dart_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, DART_AST)
}

// ─── Swift ────────────────────────────────────────────────────────────
