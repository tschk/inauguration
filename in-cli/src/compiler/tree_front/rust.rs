use super::extract::{
    extract_fn_nodes, first_named, named_descendant, node_txt, normalize_entry, simple_bounded_expr,
};
use crate::core_ir::{Decl, Expr, LoopKind, MatchArm, Stmt, Typ};
use tree_sitter::Node;

pub(super) fn extract_rust(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    extract_fn_nodes(
        src,
        root,
        &["function_item", "function_signature_item"],
        |src, n| {
            let name_n = n.child_by_field_name("name")?;
            let name = normalize_entry(node_txt(src, name_n).trim());
            let plist = n.child_by_field_name("parameters")?;
            let params = rust_params(src, plist);
            let ret = n
                .child_by_field_name("return_type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Void);
            let body = n
                .child_by_field_name("body")
                .map(|b| rust_body(src, b))
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

fn rust_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    let mut stmts = Vec::new();
    let mut w = body.walk();
    for child in body.named_children(&mut w) {
        if let Some(s) = rust_stmt_from_node(src, child) {
            stmts.push(s);
        }
    }
    stmts
}

/// Convert a Tree-sitter Rust statement/expression node to a Stmt.
fn rust_stmt_from_node(src: &[u8], n: Node<'_>) -> Option<Stmt> {
    match n.kind() {
        "let_declaration" => {
            let pat = n.child_by_field_name("pattern")?;
            let name = rust_pattern_name(src, pat)?;
            let ty = n
                .child_by_field_name("type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()));
            let val = n.child_by_field_name("value")?;
            let expr = rust_expr_from_node(src, val)?;
            Some(Stmt::Let(name, ty, expr))
        }
        "expression_statement" => {
            // Get the first named child (the expression inside the statement)
            let mut w = n.walk();
            let expr_node = n.named_children(&mut w).next()?;
            // Statement-like expressions: if, match, while, loop, for
            match expr_node.kind() {
                "if_expression" => rust_if_stmt(src, expr_node),
                "match_expression" => rust_match_stmt(src, expr_node),
                "while_expression" => rust_loop_stmt(src, expr_node, LoopKind::While),
                "loop_expression" => rust_loop_stmt(src, expr_node, LoopKind::Infinite),
                "return_expression" => {
                    // tree-sitter-rust: return value is a child with field name "value",
                    // but the expression children may not be direct named children.
                    // Try field access first, then text-based fallback.
                    let val = expr_node
                        .child_by_field_name("value")
                        .and_then(|v| rust_expr_from_node_any(src, v));
                    let expr = val.or_else(|| {
                        // Fallback: extract expression from text after "return"
                        let text = node_txt(src, expr_node);
                        text.strip_prefix("return").and_then(|rest| {
                            let rest = rest.trim().trim_end_matches(';');
                            simple_bounded_expr(rest)
                        })
                    });
                    Some(Stmt::Return(expr))
                }
                _ => {
                    let expr = rust_expr_from_node(src, expr_node)?;
                    Some(Stmt::Expr(expr))
                }
            }
        }
        "if_expression" => rust_if_stmt(src, n),
        "return_expression" => {
            let val = n
                .child_by_field_name("value")
                .and_then(|v| rust_expr_from_node_any(src, v));
            let expr = val.or_else(|| {
                let text = node_txt(src, n);
                text.strip_prefix("return").and_then(|rest| {
                    let rest = rest.trim().trim_end_matches(';');
                    simple_bounded_expr(rest)
                })
            });
            Some(Stmt::Return(expr))
        }
        "match_expression" => rust_match_stmt(src, n),
        "while_expression" => rust_loop_stmt(src, n, LoopKind::While),
        "loop_expression" => rust_loop_stmt(src, n, LoopKind::Infinite),
        _ => {
            // Try as expression statement
            rust_expr_from_node(src, n).map(Stmt::Expr)
        }
    }
}

fn rust_if_stmt(src: &[u8], n: Node<'_>) -> Option<Stmt> {
    // tree-sitter-rust: if_expression → condition (named child), consequence (block), alternative (else_clause | if_expression | None)
    let mut cond = None;
    let mut consequence = None;
    let mut alternative = None;
    let mut w = n.walk();
    for child in n.named_children(&mut w) {
        match child.kind() {
            "block" if consequence.is_none() => consequence = Some(child),
            "block" => {} // second block → inside else_clause, handled below
            "else_clause" => {
                // else_clause contains a block or if_expression
                let mut w2 = child.walk();
                for inner in child.named_children(&mut w2) {
                    if inner.kind() == "block" || inner.kind() == "if_expression" {
                        alternative = Some(inner);
                    }
                }
            }
            "if_expression" => alternative = Some(child), // else if
            _ if cond.is_none() => cond = Some(child),
            _ => {}
        }
    }
    let cond_expr = cond.and_then(|c| rust_expr_from_node(src, c))?;
    let then_body = consequence
        .map(|b| ast_block_to_stmts(src, b))
        .unwrap_or_default();
    let else_body = alternative
        .map(|alt| {
            if alt.kind() == "if_expression" {
                rust_stmt_from_node(src, alt)
                    .map(|s| vec![s])
                    .unwrap_or_default()
            } else {
                ast_block_to_stmts(src, alt)
            }
        })
        .unwrap_or_default();
    Some(Stmt::If {
        cond: cond_expr,
        then_body,
        else_body,
    })
}

fn rust_match_stmt(src: &[u8], n: Node<'_>) -> Option<Stmt> {
    let scrutinee = n.child_by_field_name("value")?;
    let scrut_expr = rust_expr_from_node(src, scrutinee)?;
    let mut arms = Vec::new();
    let block = n.child_by_field_name("body")?;
    let mut w = block.walk();
    for child in block.named_children(&mut w) {
        if child.kind() == "match_arm" {
            let pattern = child.child_by_field_name("pattern")?;
            let pat = pattern
                .named_child(0)
                .map(|p| node_txt(src, p).trim().to_string())
                .unwrap_or_default();
            let guard = match pattern.child_by_field_name("condition") {
                Some(condition) => Some(rust_expr_from_node(src, condition)?),
                None => None,
            };
            let body_node = child.child_by_field_name("value")?;
            let body = if body_node.kind() == "block" {
                ast_block_to_stmts(src, body_node)
            } else {
                // Expression body like `_ => expr`
                rust_expr_from_node(src, body_node)
                    .map(|e| vec![Stmt::Expr(e)])
                    .unwrap_or_default()
            };
            arms.push(MatchArm {
                guard,
                pattern: pat,
                body,
            });
        }
    }
    Some(Stmt::Match {
        scrutinee: scrut_expr,
        arms,
    })
}

fn rust_loop_stmt(src: &[u8], n: Node<'_>, kind: LoopKind) -> Option<Stmt> {
    let cond = n
        .child_by_field_name("condition")
        .and_then(|c| rust_expr_from_node(src, c));
    let body_node = n.child_by_field_name("body")?;
    let body = ast_block_to_stmts(src, body_node);
    Some(Stmt::Loop { kind, cond, body })
}

/// Convert a Tree-sitter block node `{ stmts }` to a Vec<Stmt>.
fn ast_block_to_stmts(src: &[u8], block: Node<'_>) -> Vec<Stmt> {
    if block.kind() != "block" {
        return rust_stmt_from_node(src, block).into_iter().collect();
    }
    let mut stmts = Vec::new();
    let mut w = block.walk();
    for child in block.named_children(&mut w) {
        if let Some(s) = rust_stmt_from_node(src, child) {
            stmts.push(s);
        }
    }
    stmts
}

fn rust_expr_from_node_any(src: &[u8], n: Node<'_>) -> Option<Expr> {
    if n.kind() == "integer_literal" {
        let txt = node_txt(src, n).trim();
        let txt = txt.trim_end_matches(&['_', 'i', 'u', '8', '6', '3', '2', '4'] as &[_]);
        txt.parse::<i64>().ok().map(Expr::IntLit)
    } else if n.kind() == "string_literal" {
        let txt = node_txt(src, n).trim();
        if txt.len() >= 2 {
            Some(Expr::StringLit(txt[1..txt.len() - 1].to_string()))
        } else {
            Some(Expr::StringLit(String::new()))
        }
    } else if n.kind() == "boolean_literal" {
        let txt = node_txt(src, n).trim();
        Some(Expr::BoolLit(txt == "true"))
    } else if n.kind() == "identifier" {
        let txt = node_txt(src, n).trim().to_string();
        if txt == "true" {
            return Some(Expr::BoolLit(true));
        }
        if txt == "false" {
            return Some(Expr::BoolLit(false));
        }
        Some(Expr::Ident(txt))
    } else if n.is_named() {
        rust_expr_from_node(src, n)
    } else {
        // Unnamed node (like `return` keyword) — try text-based parsing of siblings
        None
    }
}
fn rust_expr_from_node(src: &[u8], n: Node<'_>) -> Option<Expr> {
    match n.kind() {
        "identifier" => {
            let txt = node_txt(src, n).trim().to_string();
            if txt == "true" {
                return Some(Expr::BoolLit(true));
            }
            if txt == "false" {
                return Some(Expr::BoolLit(false));
            }
            Some(Expr::Ident(txt))
        }
        "integer_literal" => {
            let txt = node_txt(src, n).trim();
            let txt = txt.trim_end_matches(&['_', 'i', 'u'] as &[_]);
            txt.parse::<i64>().ok().map(Expr::IntLit)
        }
        "string_literal" => {
            let txt = node_txt(src, n).trim();
            let inner = &txt[1..txt.len() - 1];
            Some(Expr::StringLit(inner.to_string()))
        }
        "boolean_literal" => {
            let txt = node_txt(src, n).trim();
            Some(Expr::BoolLit(txt == "true"))
        }
        "call_expression" => {
            let func_n = n.child_by_field_name("function")?;
            let callee = rust_expr_from_node(src, func_n)?;
            let mut args = Vec::new();
            if let Some(args_n) = n.child_by_field_name("arguments") {
                let mut w = args_n.walk();
                for ch in args_n.named_children(&mut w) {
                    if let Some(e) = rust_expr_from_node(src, ch) {
                        args.push(e);
                    }
                }
            }
            Some(Expr::Call {
                callee: Box::new(callee),
                args,
            })
        }
        "binary_expression" => {
            let left_n = n.child_by_field_name("left")?;
            let right_n = n.child_by_field_name("right")?;
            let op_n = n.child_by_field_name("operator")?;
            let lhs = rust_expr_from_node(src, left_n)?;
            let rhs = rust_expr_from_node(src, right_n)?;
            let op = node_txt(src, op_n).trim().to_string();
            Some(Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }
        "unary_expression" => {
            let op_n = n.child_by_field_name("operator")?;
            let inner_n = n.child_by_field_name("argument")?;
            let expr = rust_expr_from_node(src, inner_n)?;
            let op = node_txt(src, op_n).trim().to_string();
            Some(Expr::Unary {
                op,
                expr: Box::new(expr),
            })
        }
        "field_expression" => {
            let base_n = n.child_by_field_name("value")?;
            let field_n = n.child_by_field_name("field")?;
            let base = rust_expr_from_node(src, base_n)?;
            let name = node_txt(src, field_n).trim().to_string();
            Some(Expr::Field {
                base: Box::new(base),
                name,
            })
        }
        "parenthesized_expression" => {
            first_named(n, "_").and_then(|inner| rust_expr_from_node(src, inner))
        }
        "struct_expression" => {
            let name_n = n.child_by_field_name("name")?;
            let name = node_txt(src, name_n).trim().to_string();
            // Struct literal: `Name { field: val, ... }`
            if let Some(body_n) = n.child_by_field_name("body") {
                let mut fields = Vec::new();
                let mut w = body_n.walk();
                for ch in body_n.named_children(&mut w) {
                    if ch.kind() == "field_initializer" {
                        let fn_n = ch.child_by_field_name("field")?;
                        let fname = node_txt(src, fn_n).trim().to_string();
                        let val_n = ch.child_by_field_name("value")?;
                        let val = rust_expr_from_node(src, val_n)?;
                        fields.push((fname, val));
                    }
                }
                return Some(Expr::StructInit { name, fields });
            }
            // Enum variant: `Name(args)` → treat as call with Name (enum variant) as callee
            let mut args = Vec::new();
            if let Some(args_n) = n.child_by_field_name("arguments") {
                let mut w = args_n.walk();
                for ch in args_n.named_children(&mut w) {
                    if let Some(e) = rust_expr_from_node(src, ch) {
                        args.push(e);
                    }
                }
            }
            Some(Expr::Call {
                callee: Box::new(Expr::Ident(name)),
                args,
            })
        }
        "array_expression" => {
            let mut elems = Vec::new();
            let mut w = n.walk();
            for ch in n.named_children(&mut w) {
                if let Some(e) = rust_expr_from_node(src, ch) {
                    elems.push(e);
                }
            }
            Some(Expr::ArrayLit(elems))
        }
        "index_expression" => {
            let base_n = n.child_by_field_name("value")?;
            let idx_n = n.child_by_field_name("index")?;
            let base = rust_expr_from_node(src, base_n)?;
            let index = rust_expr_from_node(src, idx_n)?;
            Some(Expr::Index {
                base: Box::new(base),
                index: Box::new(index),
            })
        }
        "block" => {
            // Expression block: `{ stmts; expr }` — extract last expression
            let stmts = ast_block_to_stmts(src, n);
            stmts.last().and_then(|s| match s {
                Stmt::Expr(e) => Some(e.clone()),
                _ => None,
            })
        }
        _ => {
            // Fallback: try text-based expression parsing
            let text = node_txt(src, n);
            simple_bounded_expr(text.trim())
        }
    }
}

fn rust_params<'a>(src: &[u8], plist: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if ch.kind() == "parameter" {
            let Some(pattern) = ch.child_by_field_name("pattern") else {
                continue;
            };
            let pname =
                rust_pattern_name(src, pattern).unwrap_or_else(|| format!("arg{}", out.len()));
            let ty = ch
                .child_by_field_name("type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Named("_".into()));
            out.push((pname, ty));
        }
    }
    out
}

fn rust_pattern_name<'a>(src: &[u8], pat: Node<'a>) -> Option<String> {
    if pat.kind() == "identifier" {
        return Some(node_txt(src, pat).trim().to_string());
    }
    let id = named_descendant(pat, "identifier")?;
    Some(node_txt(src, id).trim().to_string())
}
