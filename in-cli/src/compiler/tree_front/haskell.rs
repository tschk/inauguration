use super::extract::{
    AstShape, ast_expr, extract_fn_nodes, infer_expr_type, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Expr, Stmt, Typ};
use tree_sitter::Node;

const HASKELL_AST: AstShape = AstShape {
    block_kinds: &["expressions", "declarations"],
    return_kinds: &[],
    expr_stmt_kinds: &[],
    local_decl_kinds: &["local_binds"],
    assignment_kinds: &[],
    if_kinds: &["conditional"],
    while_kinds: &[],
    call_kinds: &["apply"],
    arg_container_kinds: &[],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parens"],
    binary_kinds: &[],
    unary_kinds: &["negation"],
    int_kinds: &["integer"],
    string_kinds: &["string"],
    type_kinds: &["type"],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &["case", "alternatives"],
    first_assignment_is_let: false,
    strict_args: false,
    string_decoder: None,
};

pub(super) fn extract_haskell(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let decls = extract_fn_nodes(src, root, &["function"], |src, n| {
        let name_n = n.child_by_field_name("name")?;
        let name = normalize_entry(node_txt(src, name_n).trim());
        let params = haskell_params(src, n);
        let body = haskell_body(src, n);
        let ret = n
            .child_by_field_name("result")
            .map(|r| Typ::Named(node_txt(src, r).trim().to_string()))
            .unwrap_or_else(|| infer_haskell_ret(&body));
        Some(Decl::Function {
            name,
            params,
            ret,
            body,
            type_params: vec![],
        })
    })?;
    if !decls.is_empty() {
        return Ok(decls);
    }
    let mut fallback = Vec::new();
    for raw in std::str::from_utf8(src).ok().unwrap_or_default().lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        let left = left.trim();
        let right = right.trim();
        if left.is_empty() || right.is_empty() {
            continue;
        }
        let mut parts = left.split_whitespace();
        let Some(name_part) = parts.next() else {
            continue;
        };
        let name = normalize_entry(name_part);
        let params = parts
            .map(|param| (param.to_string(), Typ::Named("a".into())))
            .collect();
        let Some(body) = simple_haskell_body(right) else {
            continue;
        };
        let ret = infer_haskell_ret(&body);
        fallback.push(Decl::Function {
            name,
            params,
            ret,
            body,
            type_params: vec![],
        });
    }
    Ok(fallback)
}

fn haskell_params(src: &[u8], func: Node<'_>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    if let Some(patterns) = func.child_by_field_name("patterns") {
        let mut w = patterns.walk();
        for ch in patterns.named_children(&mut w) {
            let name = match ch.kind() {
                "variable" => node_txt(src, ch).trim().to_string(),
                "wildcard" => "_".to_string(),
                _ => continue,
            };
            out.push((name, Typ::Named("a".into())));
        }
    }
    out
}

fn infer_haskell_ret(body: &[Stmt]) -> Typ {
    if let Some(Stmt::Expr(expr) | Stmt::Return(Some(expr))) = body.last() {
        let t = infer_expr_type(expr);
        return if t.canonical() == Typ::Named("Any".into()) {
            Typ::Void
        } else {
            t
        };
    }
    Typ::Void
}

fn haskell_body(src: &[u8], func: Node<'_>) -> Vec<Stmt> {
    let mut w = func.walk();
    for ch in func.named_children(&mut w) {
        if ch.kind() == "match" {
            if let Some(expr) = ch.child_by_field_name("expression") {
                if let Some(e) = ast_expr(src, expr, HASKELL_AST) {
                    return vec![Stmt::Expr(e)];
                }
            }
        }
    }
    vec![]
}

fn simple_haskell_body(text: &str) -> Option<Vec<Stmt>> {
    Some(vec![Stmt::Expr(simple_haskell_expr(text.trim())?)])
}

fn simple_haskell_expr(text: &str) -> Option<Expr> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix("print ") {
        return Some(Expr::Call {
            callee: Box::new(Expr::Ident("print".into())),
            args: vec![simple_haskell_expr(rest.trim())?],
        });
    }
    if let Some((lhs, rhs)) = text.split_once(" + ") {
        return Some(Expr::Binary {
            op: "+".into(),
            lhs: Box::new(simple_haskell_expr(lhs)?),
            rhs: Box::new(simple_haskell_expr(rhs)?),
        });
    }
    if let Ok(value) = text.parse::<i64>() {
        return Some(Expr::IntLit(value));
    }
    if (text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\'') && text.ends_with('\''))
    {
        return Some(Expr::StringLit(text[1..text.len() - 1].to_string()));
    }
    Some(Expr::Ident(text.to_string()))
}

// ─── V ───────────────────────────────────────────────────────────────
