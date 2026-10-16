use super::extract::{
    AstShape, ast_body, extract_fn_nodes, first_named, infer_expr_type, node_txt, normalize_entry,
    simple_bounded_body, strict_simple_bounded_body,
};
use crate::core_ir::{Decl, Stmt, Typ};
use tree_sitter::Node;

const OCAML_AST: AstShape = AstShape {
    block_kinds: &["sequence_expression"],
    return_kinds: &[],
    expr_stmt_kinds: &[],
    local_decl_kinds: &["let_binding"],
    assignment_kinds: &[],
    if_kinds: &["if_expression"],
    while_kinds: &["while_expression"],
    call_kinds: &["application_expression"],
    arg_container_kinds: &[],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["infix_expression"],
    unary_kinds: &["prefix_expression"],
    int_kinds: &["number"],
    string_kinds: &["string"],
    type_kinds: &["constructed_type", "type_variable"],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &["try_expression"],
    catch_kinds: &[],
    match_kinds: &["match_expression"],
    first_assignment_is_let: true,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
    number_decoder: None,
};

pub(super) fn extract_ocaml(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    extract_fn_nodes(src, root, &["let_binding"], |src, n| {
        let pattern = n.child_by_field_name("pattern")?;
        let name = ocaml_name_from_pattern(src, pattern);
        let params = ocaml_params(src, n);
        let body = n
            .child_by_field_name("body")
            .map(|b| ocaml_body(src, b))
            .unwrap_or_default();
        let ret = infer_ocaml_ret(&body);
        Some(Decl::Function {
            name,
            params,
            ret,
            body,
            type_params: vec![],
        })
    })
}

fn infer_ocaml_ret(body: &[Stmt]) -> Typ {
    if let Some(Stmt::Expr(expr) | Stmt::Return(Some(expr))) = body.last() {
        let t = infer_expr_type(expr);
        if t.canonical() == Typ::Named("Any".into()) {
            return Typ::Void;
        }
        return t;
    }
    Typ::Void
}

fn ocaml_name_from_pattern(src: &[u8], pattern: Node<'_>) -> String {
    let mut w = pattern.walk();
    for ch in pattern.named_children(&mut w) {
        if ch.kind() == "value_name" {
            return normalize_entry(node_txt(src, ch).trim());
        }
    }
    normalize_entry(node_txt(src, pattern).trim())
}

fn ocaml_params(src: &[u8], binding: Node<'_>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let mut w = binding.walk();
    for ch in binding.named_children(&mut w) {
        if ch.kind() == "parameter" {
            let name = first_named(ch, "value_name")
                .or_else(|| first_named(ch, "value_pattern"))
                .map(|n| node_txt(src, n).trim().to_string())
                .unwrap_or_else(|| "_".to_string());
            out.push((name, Typ::Void));
        }
    }
    out
}

fn ocaml_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    // ponytail: unit literal () = empty body (void return)
    if body.kind() == "unit" {
        return vec![];
    }
    if let Some(stmts) = strict_simple_bounded_body(node_txt(src, body), "=") {
        return stmts;
    }
    let stmts = ast_body(src, body, OCAML_AST);
    if !stmts.is_empty() {
        return stmts;
    }
    simple_bounded_body(node_txt(src, body), "=").unwrap_or_default()
}

// ─── Haskell ───────────────────────────────────────────────────────────
