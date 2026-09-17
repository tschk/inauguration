use super::extract::{collect_kinds, extract_fn_nodes, node_txt, normalize_entry};
use crate::core_ir::{Decl, Expr, Stmt, Typ};
use tree_sitter::Node;

fn holyc_coarse_typ(type_text: &str) -> Typ {
    match type_text.split_whitespace().next().unwrap_or("").trim() {
        "U0" | "void" => Typ::Void,
        "Bool" => Typ::Bool,
        "F32" | "F64" => Typ::Float,
        "I8" | "I16" | "I32" | "I64" | "U8" | "U16" | "U32" | "U64" | "auto" => Typ::Int,
        other if other.ends_with('*') => Typ::Named(other.to_string()),
        other if !other.is_empty() => Typ::Named(other.to_string()),
        _ => Typ::Void,
    }
}

fn holyc_function_decl<'a>(src: &[u8], func_def: Node<'a>) -> Option<Decl> {
    let name_node = func_def.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_node).trim());
    let ret = func_def
        .child_by_field_name("type")
        .map(|t| holyc_coarse_typ(node_txt(src, t).trim()))
        .unwrap_or(Typ::Void);
    let params = func_def
        .child_by_field_name("parameters")
        .map(|p| holyc_parameter_list(src, p))
        .unwrap_or_default();
    let body = func_def
        .child_by_field_name("body")
        .map(|b| holyc_block_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn holyc_parameter_list(src: &[u8], params: Node<'_>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let mut w = params.walk();
    for ch in params.named_children(&mut w) {
        if ch.kind() != "parameter_declaration" {
            continue;
        }
        let Some(name_n) = ch.child_by_field_name("name") else {
            continue;
        };
        let name = node_txt(src, name_n).trim().to_string();
        let ty = ch
            .child_by_field_name("type")
            .map(|t| holyc_coarse_typ(node_txt(src, t).trim()))
            .unwrap_or(Typ::Int);
        out.push((name, ty));
    }
    out
}

fn holyc_block_body(src: &[u8], block: Node<'_>) -> Vec<Stmt> {
    let mut out = Vec::new();
    let mut w = block.walk();
    for ch in block.named_children(&mut w) {
        if let Some(s) = holyc_stmt(src, ch) {
            out.push(s);
        }
    }
    out
}

fn holyc_stmt(src: &[u8], node: Node<'_>) -> Option<Stmt> {
    match node.kind() {
        "return_statement" => Some(Stmt::Return(holyc_return_expr(src, node))),
        "declaration" => holyc_local_decl(src, node),
        "expression_statement" => holyc_expression_statement(src, node),
        "if_statement" => holyc_if(src, node),
        "while_statement" => holyc_while(src, node),
        _ => None,
    }
}

fn holyc_return_expr(src: &[u8], ret: Node<'_>) -> Option<Expr> {
    let mut w = ret.walk();
    for ch in ret.named_children(&mut w) {
        if let Some(expr) = holyc_expr(src, ch) {
            return Some(expr);
        }
    }
    None
}

fn holyc_local_decl(src: &[u8], decl: Node<'_>) -> Option<Stmt> {
    let name = decl
        .child_by_field_name("name")
        .map(|n| node_txt(src, n).trim().to_string())
        .or_else(|| {
            let mut w = decl.walk();
            decl.named_children(&mut w)
                .find(|c| c.kind() == "identifier")
                .map(|c| node_txt(src, c).trim().to_string())
        })?;
    let ty = decl
        .child_by_field_name("type")
        .map(|t| holyc_coarse_typ(node_txt(src, t).trim()));
    let value = decl
        .child_by_field_name("value")
        .and_then(|v| holyc_expr(src, v))
        .unwrap_or(Expr::IntLit(0));
    Some(Stmt::Let(name, ty, value))
}

fn holyc_expression_statement(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    if let Some(print) = stmt.child_by_field_name("print") {
        return holyc_print_stmt(src, print);
    }
    let mut w = stmt.walk();
    let expr = stmt
        .named_children(&mut w)
        .next()
        .and_then(|c| holyc_expr(src, c))?;
    Some(Stmt::Expr(expr))
}

fn holyc_print_stmt(src: &[u8], print: Node<'_>) -> Option<Stmt> {
    let fmt = print
        .child_by_field_name("format")
        .and_then(|f| holyc_expr(src, f))?;
    let mut args = vec![fmt];
    let mut w = print.walk();
    let mut saw_format = false;
    for ch in print.named_children(&mut w) {
        if ch.kind() == "string_literal" && !saw_format {
            saw_format = true;
            continue;
        }
        if let Some(a) = holyc_expr(src, ch) {
            args.push(a);
        }
    }
    Some(Stmt::Expr(Expr::Call {
        callee: Box::new(Expr::Ident("print".into())),
        args,
    }))
}

fn holyc_if(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    let cond = stmt
        .child_by_field_name("condition")
        .and_then(|c| holyc_expr(src, c))?;
    let then_body = stmt
        .child_by_field_name("consequence")
        .map(|c| holyc_stmt_list(src, c))
        .unwrap_or_default();
    let else_body = stmt
        .child_by_field_name("alternative")
        .map(|c| holyc_stmt_list(src, c))
        .unwrap_or_default();
    Some(Stmt::If {
        cond,
        then_body,
        else_body,
    })
}

fn holyc_while(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    let cond = stmt
        .child_by_field_name("condition")
        .and_then(|c| holyc_expr(src, c))?;
    let body = stmt
        .child_by_field_name("body")
        .map(|c| holyc_stmt_list(src, c))
        .unwrap_or_default();
    Some(Stmt::Loop {
        kind: crate::core_ir::LoopKind::While,
        cond: Some(cond),
        body,
    })
}

fn holyc_stmt_list(src: &[u8], node: Node<'_>) -> Vec<Stmt> {
    if node.kind() == "compound_statement" {
        return holyc_block_body(src, node);
    }
    holyc_stmt(src, node).into_iter().collect()
}

fn holyc_string_lit(raw: &str) -> String {
    let text = raw.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        let mut out = String::new();
        let mut chars = text[1..text.len() - 1].chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        }
        out
    } else {
        text.to_string()
    }
}

fn holyc_expr(src: &[u8], node: Node<'_>) -> Option<Expr> {
    match node.kind() {
        "identifier" => Some(Expr::Ident(node_txt(src, node).trim().to_string())),
        "number_literal" => node_txt(src, node)
            .trim()
            .parse::<i64>()
            .ok()
            .map(Expr::IntLit)
            .or(Some(Expr::Ident(node_txt(src, node).trim().to_string()))),
        "string_literal" => Some(Expr::StringLit(holyc_string_lit(node_txt(src, node)))),
        "call_expression" => {
            let callee = node
                .child_by_field_name("function")
                .and_then(|f| holyc_expr(src, f))?;
            let mut args = Vec::new();
            if let Some(alist) = node.child_by_field_name("arguments") {
                let mut w = alist.walk();
                for ch in alist.named_children(&mut w) {
                    if let Some(a) = holyc_expr(src, ch) {
                        args.push(a);
                    }
                }
            }
            Some(Expr::Call {
                callee: Box::new(callee),
                args,
            })
        }
        "assignment_expression" => {
            let name = node
                .child_by_field_name("left")
                .map(|n| node_txt(src, n).trim().to_string())?;
            let rhs = node
                .child_by_field_name("right")
                .and_then(|r| holyc_expr(src, r))?;
            Some(Expr::Binary {
                op: "=".to_string(),
                lhs: Box::new(Expr::Ident(name)),
                rhs: Box::new(rhs),
            })
        }
        "binary_expression" => {
            let lhs = node
                .child_by_field_name("left")
                .or_else(|| node.named_child(0))?;
            let rhs = node
                .child_by_field_name("right")
                .or_else(|| node.named_child(node.named_child_count().saturating_sub(1) as u32))?;
            let op = std::str::from_utf8(src.get(lhs.end_byte()..rhs.start_byte())?)
                .ok()?
                .trim()
                .to_string();
            Some(Expr::Binary {
                op,
                lhs: Box::new(holyc_expr(src, lhs)?),
                rhs: Box::new(holyc_expr(src, rhs)?),
            })
        }
        "parenthesized_expression" => {
            let mut w = node.walk();
            node.named_children(&mut w)
                .next()
                .and_then(|c| holyc_expr(src, c))
        }
        _ => None,
    }
}

pub(super) fn extract_holyc(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = extract_fn_nodes(src, root, &["function_definition"], holyc_function_decl)?;
    let mut tops = Vec::new();
    collect_kinds(root, &["bare_call_statement"], &mut tops);
    for n in tops {
        let Some(name_n) = n.child_by_field_name("name") else {
            continue;
        };
        let name = normalize_entry(node_txt(src, name_n).trim());
        if decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name: n, .. } if n == &name))
        {
            continue;
        }
        decls.push(Decl::Function {
            name: name.clone(),
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(name)),
                args: vec![],
            })],
            type_params: vec![],
        });
    }
    Ok(decls)
}
