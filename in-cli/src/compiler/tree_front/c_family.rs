use super::extract::{
    collect_kinds, decl_fn, first_named, last_named, named_descendant, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Expr, Stmt, Typ, Visibility};
use tree_sitter::Node;

pub(super) fn extract_cpp_with_classes(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut class_nodes = Vec::new();
    collect_kinds(
        root,
        &["class_specifier", "struct_specifier"],
        &mut class_nodes,
    );
    for c in class_nodes {
        if let Some(d) = cpp_class_decl(src, c) {
            decls.push(d);
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_definition"], &mut func_nodes);
    for f in func_nodes {
        let is_class_method = f
            .parent()
            .is_some_and(|p| p.kind() == "field_declaration_list");
        if !is_class_method && let Some(d) = c_like_function_decl(src, f) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn cpp_class_decl<'a>(src: &[u8], class_node: Node<'a>) -> Option<Decl> {
    let name_n = class_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let extends = cpp_base_class(src, class_node);
    let (fields, methods) = cpp_class_body(src, class_node);
    Some(Decl::Class {
        name,
        fields,
        methods,
        visibility: Visibility::Pub,
        extends,
        implements: vec![],
        type_params: vec![],
    })
}

fn cpp_base_class<'a>(src: &[u8], class_node: Node<'a>) -> Option<String> {
    let bases = class_node
        .child_by_field_name("bases")
        .or_else(|| first_named(class_node, "base_class_clause"))?;
    let first =
        first_named(bases, "type_identifier").or_else(|| first_named(bases, "identifier"))?;
    Some(node_txt(src, first).trim().to_string())
}

fn cpp_class_body<'a>(src: &[u8], class_node: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let body = match class_node.child_by_field_name("body") {
        Some(b) => b,
        None => return (Vec::new(), Vec::new()),
    };

    let mut fields = Vec::new();
    let mut field_nodes = Vec::new();
    collect_kinds(body, &["field_declaration"], &mut field_nodes);
    for f in field_nodes {
        let field_type = f
            .child_by_field_name("type")
            .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
            .unwrap_or(Typ::Named("int".into()));
        let decl = f.child_by_field_name("declarator");
        if let Some(decl) = decl {
            let name_node = named_descendant(decl, "field_identifier")
                .or_else(|| named_descendant(decl, "identifier"));
            if let Some(name_n) = name_node {
                let field_name = node_txt(src, name_n).trim().to_string();
                fields.push((field_name, field_type));
            }
        }
    }

    let mut methods = Vec::new();
    let mut method_nodes = Vec::new();
    collect_kinds(body, &["function_definition"], &mut method_nodes);
    for m in method_nodes {
        if let Some(d) = c_like_function_decl(src, m) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn c_like_fn_name<'a>(src: &[u8], func_def: Node<'a>) -> Option<String> {
    let declarator = named_descendant(func_def, "function_declarator")?;
    let id = named_descendant(declarator, "field_identifier")
        .or_else(|| named_descendant(declarator, "identifier"))?;
    Some(normalize_entry(node_txt(src, id).trim()))
}

/// C / C++ / ObjC++ `function_definition`: name, coarse types, parameters, optional trivial
/// `return <integer>;`, `return <param>;`, or `return;` body (single statement, no locals).
pub(super) fn c_like_function_decl<'a>(src: &[u8], func_def: Node<'a>) -> Option<Decl> {
    let name = c_like_fn_name(src, func_def)?;
    let ret = c_coarse_return_typ(src, func_def);
    let params = c_parameter_list(src, func_def);
    let body = func_def
        .child_by_field_name("body")
        .map(|b| c_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn c_strip_decl_storage(s: &str) -> String {
    s.split_whitespace()
        .filter(|w| {
            !matches!(
                *w,
                "static"
                    | "extern"
                    | "inline"
                    | "__inline"
                    | "__inline__"
                    | "const"
                    | "volatile"
                    | "auto"
                    | "register"
                    | "_Noreturn"
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn c_typ_from_decl_specifier_text(raw: &str) -> Typ {
    let s = c_strip_decl_storage(raw.trim());
    if s.is_empty() {
        return Typ::Void;
    }
    let lower = s.to_ascii_lowercase();
    if lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|w| w == "void")
    {
        return Typ::Void;
    }
    if c_decl_specs_look_integral(&lower) {
        return Typ::Int;
    }
    Typ::Named(
        s.split_whitespace()
            .last()
            .unwrap_or(s.as_str())
            .to_string(),
    )
}

fn c_decl_specs_look_integral(lower: &str) -> bool {
    const KW: &[&str] = &[
        "int", "char", "short", "long", "signed", "unsigned", "uint8_t", "uint16_t", "uint32_t",
        "uint64_t", "int8_t", "int16_t", "int32_t", "int64_t", "size_t", "ssize_t", "bool",
        "_bool",
    ];
    KW.iter().any(|k| lower.contains(k))
}

fn c_coarse_return_typ(src: &[u8], func_def: Node<'_>) -> Typ {
    let Some(decl) = func_def.child_by_field_name("declarator") else {
        return Typ::Void;
    };
    let head = src
        .get(func_def.start_byte()..decl.start_byte())
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
        .trim();
    c_typ_from_decl_specifier_text(head)
}

fn c_parameter_list<'a>(src: &[u8], func_def: Node<'a>) -> Vec<(String, Typ)> {
    let Some(decl) = func_def.child_by_field_name("declarator") else {
        return vec![];
    };
    let Some(plist) = named_descendant(decl, "parameter_list") else {
        return vec![];
    };
    let mut out = Vec::new();
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if ch.kind() != "parameter_declaration" {
            continue;
        }
        if let Some(pair) = c_one_parameter(src, ch, out.len()) {
            out.push(pair);
        }
    }
    out
}

fn c_one_parameter(src: &[u8], pd: Node<'_>, idx: usize) -> Option<(String, Typ)> {
    let decl = pd.child_by_field_name("declarator");
    let ty_end = decl.map(|d| d.start_byte()).unwrap_or(pd.end_byte());
    let ty_src = src.get(pd.start_byte()..ty_end)?;
    let ty_text = std::str::from_utf8(ty_src).ok()?.trim();
    if ty_text.is_empty() {
        return None;
    }
    if ty_text == "void" && decl.is_none() {
        return None;
    }
    let ty = c_typ_from_decl_specifier_text(ty_text);
    let name = decl
        .and_then(|d| {
            named_descendant(d, "field_identifier").or_else(|| named_descendant(d, "identifier"))
        })
        .map(|id| node_txt(src, id).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("arg{idx}"));
    Some((name, ty))
}

fn c_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    let block = c_peel_statement_shell(body).unwrap_or(body);
    let mut out = Vec::new();
    let mut w = block.walk();
    for ch in block.named_children(&mut w) {
        out.extend(c_stmts(src, ch));
    }
    out
}

fn c_stmt(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    let stmt = c_peel_statement_shell(stmt)?;
    match stmt.kind() {
        "return_statement" => c_return_expr(src, stmt).map(Stmt::Return),
        "declaration" => c_declaration(src, stmt),
        "expression_statement" => c_expr_statement(src, stmt),
        "if_statement" => c_if_statement(src, stmt),
        "while_statement" => c_while_statement(src, stmt),
        "break_statement" => Some(Stmt::Break),
        "continue_statement" => Some(Stmt::Continue),
        _ => None,
    }
}

fn c_stmts(src: &[u8], stmt: Node<'_>) -> Vec<Stmt> {
    let stmt = c_peel_statement_shell(stmt).unwrap_or(stmt);
    match stmt.kind() {
        "comment" => vec![],
        "expression_statement" if stmt.named_child_count() == 0 => vec![],
        "compound_statement" => vec![Stmt::If {
            cond: Expr::BoolLit(true),
            then_body: c_body(src, stmt),
            else_body: vec![],
        }],
        "for_statement" => {
            c_for_statement(src, stmt).unwrap_or_else(|| c_unsupported_statement("for"))
        }
        _ => c_stmt(src, stmt)
            .map(|stmt| vec![stmt])
            .unwrap_or_else(|| c_unsupported_statement(stmt.kind())),
    }
}

fn c_unsupported_statement(kind: &str) -> Vec<Stmt> {
    vec![Stmt::Expr(Expr::Call {
        callee: Box::new(Expr::Ident(format!("__unsupported_c_{kind}"))),
        args: vec![],
    })]
}

fn c_for_statement(src: &[u8], stmt: Node<'_>) -> Option<Vec<Stmt>> {
    let mut out = Vec::new();
    if let Some(init) = stmt.child_by_field_name("initializer") {
        out.push(match init.kind() {
            "declaration" => {
                let mut cursor = init.walk();
                if init
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "init_declarator")
                    .count()
                    != 1
                {
                    return None;
                }
                c_declaration(src, init)?
            }
            "assignment_expression" => c_assignment(src, init)?,
            _ => Stmt::Expr(c_expr(src, init)?),
        });
    }
    let cond = match stmt.child_by_field_name("condition") {
        Some(cond) => Some(c_expr(src, cond)?),
        None => None,
    };
    let mut body = c_stmt_or_body(src, stmt.child_by_field_name("body")?);
    if let Some(update) = stmt.child_by_field_name("update") {
        let advance = match update.kind() {
            "update_expression" => c_update(src, update)?,
            "assignment_expression" => c_assignment(src, update)?,
            _ => Stmt::Expr(c_expr(src, update)?),
        };
        crate::lower_core::advance_for_continues(&mut body, &advance);
        body.push(advance);
    }
    out.push(Stmt::Loop {
        kind: crate::core_ir::LoopKind::While,
        cond,
        body,
    });
    // The initializer binding belongs to the loop, not the enclosing block.
    Some(vec![Stmt::If {
        cond: Expr::BoolLit(true),
        then_body: out,
        else_body: vec![],
    }])
}

fn c_update(src: &[u8], expr: Node<'_>) -> Option<Stmt> {
    let argument = expr
        .child_by_field_name("argument")
        .or_else(|| expr.named_child(0))?;
    let name = c_assignee_name(src, argument)?;
    let text = node_txt(src, expr).trim();
    let op = if text.starts_with("++") || text.ends_with("++") {
        "+"
    } else if text.starts_with("--") || text.ends_with("--") {
        "-"
    } else {
        return None;
    };
    Some(Stmt::Assign(
        name.clone(),
        Expr::Binary {
            op: op.into(),
            lhs: Box::new(Expr::Ident(name)),
            rhs: Box::new(Expr::IntLit(1)),
        },
    ))
}

fn c_return_expr(src: &[u8], ret: Node<'_>) -> Option<Option<Expr>> {
    let mut w = ret.walk();
    for ch in ret.named_children(&mut w) {
        if let Some(expr) = c_expr(src, ch) {
            return Some(Some(expr));
        }
    }
    Some(None)
}

fn c_declaration(src: &[u8], decl: Node<'_>) -> Option<Stmt> {
    let init = first_named(decl, "init_declarator")?;
    let name_node = named_descendant(init, "identifier")?;
    let name = node_txt(src, name_node).trim().to_string();
    let ty_src = src.get(decl.start_byte()..name_node.start_byte())?;
    let ty = c_typ_from_decl_specifier_text(std::str::from_utf8(ty_src).ok()?);
    let value = init
        .child_by_field_name("value")
        .or_else(|| last_named(init))
        .and_then(|n| c_expr(src, n))?;
    Some(Stmt::Let(name, Some(ty), value))
}

fn c_expr_statement(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    let mut w = stmt.walk();
    let expr = stmt.named_children(&mut w).next()?;
    match expr.kind() {
        "assignment_expression" => c_assignment(src, expr),
        "update_expression" => c_update(src, expr),
        _ => c_expr(src, expr).map(Stmt::Expr),
    }
}

fn c_assignment(src: &[u8], expr: Node<'_>) -> Option<Stmt> {
    let left = expr
        .child_by_field_name("left")
        .or_else(|| expr.named_child(0))?;
    let right = expr
        .child_by_field_name("right")
        .or_else(|| expr.named_child(expr.named_child_count().saturating_sub(1) as u32))?;
    let name = c_assignee_name(src, left)?;
    let operator = std::str::from_utf8(src.get(left.end_byte()..right.start_byte())?)
        .ok()?
        .trim();
    let value = c_expr(src, right)?;
    let value = if operator == "=" {
        value
    } else {
        let op = operator.strip_suffix('=')?;
        if !matches!(
            op,
            "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>"
        ) {
            return None;
        }
        Expr::Binary {
            op: op.into(),
            lhs: Box::new(Expr::Ident(name.clone())),
            rhs: Box::new(value),
        }
    };
    Some(Stmt::Assign(name, value))
}

fn c_assignee_name(src: &[u8], n: Node<'_>) -> Option<String> {
    if n.kind() == "identifier" {
        return Some(node_txt(src, n).trim().to_string());
    }
    None
}

fn c_if_statement(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    let cond = stmt
        .child_by_field_name("condition")
        .and_then(|n| c_expr(src, n))
        .or_else(|| first_named(stmt, "parenthesized_expression").and_then(|n| c_expr(src, n)))?;
    let then_body = stmt
        .child_by_field_name("consequence")
        .map(|n| c_stmt_or_body(src, n))
        .unwrap_or_default();
    let else_body = stmt
        .child_by_field_name("alternative")
        .or_else(|| first_named(stmt, "else_clause"))
        .map(|n| c_else_body(src, n))
        .unwrap_or_default();
    Some(Stmt::If {
        cond,
        then_body,
        else_body,
    })
}

fn c_while_statement(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    let cond = stmt
        .child_by_field_name("condition")
        .and_then(|n| c_expr(src, n))
        .or_else(|| first_named(stmt, "parenthesized_expression").and_then(|n| c_expr(src, n)))?;
    let body = stmt
        .child_by_field_name("body")
        .map(|n| c_stmt_or_body(src, n))
        .unwrap_or_default();
    Some(Stmt::Loop {
        kind: crate::core_ir::LoopKind::While,
        cond: Some(cond),
        body,
    })
}

fn c_stmt_or_body(src: &[u8], n: Node<'_>) -> Vec<Stmt> {
    let n = c_peel_statement_shell(n).unwrap_or(n);
    if n.kind() == "compound_statement" {
        c_body(src, n)
    } else {
        c_stmts(src, n)
    }
}

fn c_else_body(src: &[u8], n: Node<'_>) -> Vec<Stmt> {
    let n = c_peel_statement_shell(n).unwrap_or(n);
    let n = if n.kind() == "else_clause" {
        last_named(n).unwrap_or(n)
    } else {
        n
    };
    if n.kind() == "if_statement" {
        c_stmts(src, n)
    } else {
        c_stmt_or_body(src, n)
    }
}

fn c_expr(src: &[u8], expr: Node<'_>) -> Option<Expr> {
    match expr.kind() {
        "identifier" => Some(Expr::Ident(node_txt(src, expr).trim().to_string())),
        "number_literal" => parse_c_integer_literal(node_txt(src, expr)).map(Expr::IntLit),
        "string_literal" => Some(Expr::StringLit(
            node_txt(src, expr).trim().trim_matches('"').to_string(),
        )),
        "true" => Some(Expr::BoolLit(true)),
        "false" => Some(Expr::BoolLit(false)),
        "parenthesized_expression" | "expression" => {
            expr.named_child(0).and_then(|n| c_expr(src, n))
        }
        "condition_clause" => expr
            .child_by_field_name("value")
            .or_else(|| expr.named_child(0))
            .and_then(|value| c_expr(src, value)),
        "binary_expression" => c_binary_expr(src, expr),
        "unary_expression" => c_unary_expr(src, expr),
        "call_expression" => c_call_expr(src, expr),
        _ => None,
    }
}

fn c_binary_expr(src: &[u8], expr: Node<'_>) -> Option<Expr> {
    let lhs = expr
        .child_by_field_name("left")
        .or_else(|| expr.named_child(0))?;
    let rhs = expr
        .child_by_field_name("right")
        .or_else(|| expr.named_child(expr.named_child_count().saturating_sub(1) as u32))?;
    let op = std::str::from_utf8(src.get(lhs.end_byte()..rhs.start_byte())?)
        .ok()?
        .trim()
        .to_string();
    Some(Expr::Binary {
        op,
        lhs: Box::new(c_expr(src, lhs)?),
        rhs: Box::new(c_expr(src, rhs)?),
    })
}

fn c_unary_expr(src: &[u8], expr: Node<'_>) -> Option<Expr> {
    let inner = last_named(expr)?;
    let op = std::str::from_utf8(src.get(expr.start_byte()..inner.start_byte())?)
        .ok()?
        .trim()
        .to_string();
    Some(Expr::Unary {
        op,
        expr: Box::new(c_expr(src, inner)?),
    })
}

fn c_call_expr(src: &[u8], call: Node<'_>) -> Option<Expr> {
    let func = call
        .child_by_field_name("function")
        .or_else(|| first_named(call, "identifier"))?;
    let args = match call.child_by_field_name("arguments") {
        Some(args) => c_args(src, args)?,
        None => Vec::new(),
    };
    Some(Expr::Call {
        callee: Box::new(Expr::Ident(node_txt(src, func).trim().to_string())),
        args,
    })
}

fn c_args(src: &[u8], args: Node<'_>) -> Option<Vec<Expr>> {
    let mut out = Vec::new();
    let mut w = args.walk();
    for ch in args.named_children(&mut w) {
        out.push(c_expr(src, ch)?);
    }
    Some(out)
}

fn c_peel_statement_shell<'a>(n: Node<'a>) -> Option<Node<'a>> {
    match n.kind() {
        "statement" => {
            let inner = n.named_child(0)?;
            c_peel_statement_shell(inner)
        }
        "attributed_statement" => {
            let idx = n.named_child_count().saturating_sub(1) as u32;
            let inner = n.named_child(idx)?;
            c_peel_statement_shell(inner)
        }
        _ => Some(n),
    }
}

fn parse_c_integer_literal(t: &str) -> Option<i64> {
    let t = t.trim();
    let t = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    if t.is_empty() {
        return None;
    }
    if let Some(rest) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(rest, 16).ok();
    }
    t.parse::<i64>().ok()
}

#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
pub(super) fn objc_like<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    if n.kind() == "function_definition" {
        return c_like_function_decl(src, n);
    }
    if n.kind() == "method_definition" {
        let sel = named_descendant(n, "selector")?;
        let name = node_txt(src, sel).trim().replace(':', "_");
        if name.is_empty() {
            return None;
        }
        return Some(decl_fn(normalize_entry(&name), vec![], Typ::Void));
    }
    None
}
