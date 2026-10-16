use super::extract::{
    AstShape, ast_body, collect_kinds, decl_fn, first_named, infer_expr_type, named_descendant,
    node_txt, normalize_entry, simple_bounded_body,
};
use crate::core_ir::{Decl, Stmt, Typ};
use tree_sitter::Node;

const RAST: AstShape = AstShape {
    block_kinds: &["brace_list", "braced_expression"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &[],
    local_decl_kinds: &[],
    assignment_kinds: &["assignment"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    call_kinds: &["call"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &["argument", "named_argument"],
    paren_kinds: &["parenthesized_expression"],
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
};

pub(super) fn extract_r_lang(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut out = Vec::new();
    let mut hits = Vec::new();
    collect_kinds(root, &["binary_operator", "assignment"], &mut hits);
    for n in hits {
        let Some(op_node) = n.child_by_field_name("operator") else {
            continue;
        };
        let op = node_txt(src, op_node);
        if !matches!(op.trim(), "<-" | "<<-" | ":=" | "=" | "->") {
            continue;
        }
        let Some(lhs) = n.child_by_field_name("lhs") else {
            continue;
        };
        let Some(rhs) = n.child_by_field_name("rhs") else {
            continue;
        };
        if lhs.kind() != "identifier" {
            continue;
        }
        if let Some(func_node) = named_descendant(rhs, "function_definition") {
            let name = normalize_entry(node_txt(src, lhs).trim());
            if let Some(d) = r_function_decl(src, name, func_node) {
                out.push(d);
            }
        }
    }

    if out.is_empty() {
        let mut fallback = Vec::new();
        collect_kinds(root, &["binary_operator"], &mut fallback);
        for n in fallback {
            let Some(op_node) = n.child_by_field_name("operator") else {
                continue;
            };
            let op = node_txt(src, op_node);
            if !matches!(op.trim(), "<-" | "<<-" | ":=" | "=" | "->") {
                continue;
            }
            let Some(lhs) = n.child_by_field_name("lhs") else {
                continue;
            };
            let Some(rhs) = n.child_by_field_name("rhs") else {
                continue;
            };
            if lhs.kind() != "identifier" {
                continue;
            }
            if named_descendant(rhs, "function_definition").is_none() {
                continue;
            }
            let name = normalize_entry(node_txt(src, lhs).trim());
            out.push(decl_fn(name, vec![], Typ::Void));
        }
    }

    Ok(out)
}

fn r_function_decl<'a>(src: &[u8], name: String, func_node: Node<'a>) -> Option<Decl> {
    let params = r_params(src, func_node);
    let body = func_node
        .child_by_field_name("body")
        .map(|b| r_body(src, b))
        .unwrap_or_default();
    let ret = infer_r_ret(&body);

    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn infer_r_ret(body: &[Stmt]) -> Typ {
    if let Some(Stmt::Expr(expr) | Stmt::Return(Some(expr))) = body.last() {
        let t = infer_expr_type(expr);
        if t.canonical() == Typ::Named("Any".into()) {
            return Typ::Void;
        }
        return t;
    }
    Typ::Void
}

fn r_params<'a>(src: &[u8], func_node: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let plist = func_node
        .child_by_field_name("parameters")
        .or_else(|| named_descendant(func_node, "parameters"));
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
        } else if pk == "formal_parameter" || pk == "parameter" {
            let id = first_named(ch, "identifier").or_else(|| named_descendant(ch, "identifier"));
            if let Some(id) = id {
                out.push((
                    node_txt(src, id).trim().to_string(),
                    Typ::Named("Any".into()),
                ));
            }
        }
    }
    out
}

fn r_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    let stmts = ast_body(src, body, RAST);
    if stmts.is_empty() {
        simple_bounded_body(node_txt(src, body), "<-").unwrap_or_default()
    } else {
        stmts
    }
}
