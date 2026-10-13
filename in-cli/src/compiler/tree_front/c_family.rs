use super::extract::{
    collect_kinds, decl_fn, first_named, last_named, named_descendant, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Expr, Stmt, Typ, Visibility};
use tree_sitter::Node;

pub(super) fn extract_c(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = c_globals(src, root)?;
    let mut functions = Vec::new();
    collect_kinds(root, &["function_definition"], &mut functions);
    decls.extend(
        functions
            .into_iter()
            .filter_map(|node| c_like_function_decl(src, node)),
    );
    Ok(decls)
}

fn c_globals(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for declaration in root.named_children(&mut cursor) {
        if declaration.kind() != "declaration" {
            continue;
        }
        let Some(type_node) = declaration.child_by_field_name("type") else {
            continue;
        };
        let mut modifiers = declaration.walk();
        let modifiers: Vec<_> = declaration
            .named_children(&mut modifiers)
            .map(|node| node_txt(src, node).trim())
            .collect();
        let external = modifiers.contains(&"extern");
        let mutable = !modifiers
            .iter()
            .any(|modifier| matches!(*modifier, "const" | "constexpr"));
        let typ = c_typ_from_decl_specifier_text(node_txt(src, type_node));
        let mut children = declaration.walk();
        for child in declaration.named_children(&mut children) {
            let (declarator, value) = if child.kind() == "init_declarator" {
                (
                    child.child_by_field_name("declarator"),
                    child.child_by_field_name("value"),
                )
            } else if matches!(
                child.kind(),
                "identifier"
                    | "function_declarator"
                    | "pointer_declarator"
                    | "array_declarator"
                    | "reference_declarator"
            ) {
                (Some(child), None)
            } else {
                continue;
            };
            let Some(declarator) = declarator else {
                continue;
            };
            // A declaration without an initializer does not define external storage.
            if external && value.is_none() {
                continue;
            }
            if declarator.kind() == "function_declarator" {
                continue;
            }
            if declarator.kind() != "identifier" {
                return Err(format!(
                    "unsupported C-like global declarator `{}`",
                    node_txt(src, declarator)
                ));
            }
            let name = node_txt(src, declarator).trim().to_string();
            let init = match value {
                Some(value) => {
                    Some(Box::new(c_expr(src, value).ok_or_else(|| {
                        format!("unsupported initializer for global `{name}`")
                    })?))
                }
                None => None,
            };
            out.push(Decl::Global {
                name,
                typ: typ.clone(),
                init,
                mutable,
                constant_value: false,
            });
        }
    }
    Ok(out)
}

pub(super) fn extract_cpp_with_classes(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = c_globals(src, root)?;

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

pub(crate) fn c_typ_from_decl_specifier_text(raw: &str) -> Typ {
    let s = c_strip_decl_storage(raw.trim());
    if s.is_empty() {
        return Typ::Void;
    }
    if s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|w| w == "void")
    {
        return Typ::Void;
    }
    if matches!(s.as_str(), "bool" | "_Bool") {
        return Typ::Bool;
    }
    if matches!(s.as_str(), "float" | "double") {
        return Typ::Float;
    }
    if s.starts_with("struct ")
        || s.starts_with("union ")
        || s.starts_with("reference ")
        || s == "long double"
    {
        return Typ::Named(s);
    }
    if c_decl_specs_look_integral(&s) {
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
    lower
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| KW.contains(&word))
}

fn c_coarse_return_typ(src: &[u8], func_def: Node<'_>) -> Typ {
    let Some(decl) = func_def.child_by_field_name("declarator") else {
        return Typ::Void;
    };
    if decl.kind() == "pointer_declarator" {
        return Typ::Int;
    }
    if decl.kind() == "reference_declarator" {
        return Typ::Named("reference return".into());
    }
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
    let ty = if decl.is_some_and(|decl| decl.kind() == "reference_declarator") {
        Typ::Named(format!("reference {ty_text}"))
    } else if decl
        .is_some_and(|decl| matches!(decl.kind(), "pointer_declarator" | "array_declarator"))
    {
        Typ::Int
    } else {
        c_typ_from_decl_specifier_text(ty_text)
    };
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
        "declaration" => {
            c_declarations(src, stmt).unwrap_or_else(|| c_unsupported_statement("declaration"))
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
        match init.kind() {
            "declaration" => out.extend(c_declarations(src, init)?),
            "assignment_expression" => out.push(c_assignment(src, init)?),
            _ => out.push(Stmt::Expr(c_expr(src, init)?)),
        }
    }
    let condition = stmt.child_by_field_name("condition");
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
    out.push(match condition {
        Some(condition) => c_condition_loop(src, condition, body)?,
        None => Stmt::Loop {
            kind: crate::core_ir::LoopKind::While,
            cond: None,
            body,
        },
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

fn c_declarations(src: &[u8], declaration: Node<'_>) -> Option<Vec<Stmt>> {
    let typ =
        c_typ_from_decl_specifier_text(node_txt(src, declaration.child_by_field_name("type")?));
    let mut out = Vec::new();
    let mut cursor = declaration.walk();
    for (index, child) in declaration.children(&mut cursor).enumerate() {
        if declaration.field_name_for_child(index as u32) != Some("declarator") {
            continue;
        }
        let (declarator, value) = if child.kind() == "init_declarator" {
            (
                child.child_by_field_name("declarator")?,
                child.child_by_field_name("value")?,
            )
        } else {
            (child, declaration.child_by_field_name("value")?)
        };
        if declarator.kind() != "identifier" {
            return None;
        }
        let name = node_txt(src, declarator).trim().to_string();
        let value = c_expr(src, value)?;
        out.push(Stmt::Let(name, Some(typ.clone()), value));
    }
    if out.is_empty() { None } else { Some(out) }
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
        return Some(Stmt::Expr(Expr::Call {
            callee: Box::new(Expr::Ident("modify-value-first".into())),
            args: vec![Expr::Ident(name), value, Expr::StringLit(op.into())],
        }));
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
    let condition = stmt
        .child_by_field_name("condition")
        .or_else(|| first_named(stmt, "parenthesized_expression"))?;
    let value = condition.child_by_field_name("value").unwrap_or(condition);
    let mut prefix = condition
        .child_by_field_name("initializer")
        .map(|initializer| c_stmts(src, initializer))
        .unwrap_or_default();
    let cond = if value.kind() == "declaration" {
        let declarations = c_declarations(src, value)?;
        let [Stmt::Let(name, _, _)] = declarations.as_slice() else {
            return None;
        };
        let cond = Expr::Ident(name.clone());
        prefix.extend(declarations);
        cond
    } else {
        c_expr(src, value)?
    };
    let then_body = stmt
        .child_by_field_name("consequence")
        .map(|n| c_stmt_or_body(src, n))
        .unwrap_or_default();
    let else_body = stmt
        .child_by_field_name("alternative")
        .or_else(|| first_named(stmt, "else_clause"))
        .map(|n| c_else_body(src, n))
        .unwrap_or_default();
    let branch = Stmt::If {
        cond,
        then_body,
        else_body,
    };
    if prefix.is_empty() {
        Some(branch)
    } else {
        prefix.push(branch);
        Some(Stmt::If {
            cond: Expr::BoolLit(true),
            then_body: prefix,
            else_body: vec![],
        })
    }
}

fn c_while_statement(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    let condition = stmt
        .child_by_field_name("condition")
        .or_else(|| first_named(stmt, "parenthesized_expression"))?;
    let body = stmt
        .child_by_field_name("body")
        .map(|node| c_stmt_or_body(src, node))
        .unwrap_or_default();
    c_condition_loop(src, condition, body)
}

fn c_condition_loop(src: &[u8], condition: Node<'_>, body: Vec<Stmt>) -> Option<Stmt> {
    let value = if condition.kind() == "condition_clause" {
        if condition.child_by_field_name("initializer").is_some() {
            return None;
        }
        condition.child_by_field_name("value")?
    } else {
        condition
    };
    if value.kind() == "declaration" {
        let mut header = c_declarations(src, value)?;
        let [Stmt::Let(name, _, _)] = header.as_slice() else {
            return None;
        };
        let cond = Expr::Ident(name.clone());
        header.push(Stmt::If {
            cond,
            then_body: body,
            else_body: vec![Stmt::Break],
        });
        return Some(Stmt::Loop {
            kind: crate::core_ir::LoopKind::While,
            cond: None,
            body: header,
        });
    }
    let cond = c_expr(src, value)?;
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
        "number_literal" => parse_c_number_literal(node_txt(src, expr)),
        "string_literal" => Some(Expr::StringLit(
            node_txt(src, expr).trim().trim_matches('"').to_string(),
        )),
        "true" => Some(Expr::BoolLit(true)),
        "false" => Some(Expr::BoolLit(false)),
        "parenthesized_expression" | "expression" => {
            expr.named_child(0).and_then(|n| c_expr(src, n))
        }
        "condition_clause" if expr.child_by_field_name("initializer").is_none() => expr
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
        "statement" | "init_statement" => {
            let mut cursor = n.walk();
            let inner = n
                .named_children(&mut cursor)
                .find(|child| child.kind() != "comment")?;
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

fn parse_c_number_literal(text: &str) -> Option<Expr> {
    if let Some(value) = parse_c_integer_literal(text) {
        return Some(Expr::IntLit(value));
    }
    let text = text.trim();
    if text.starts_with("0x") || text.starts_with("0X") || !text.contains(['.', 'e', 'E']) {
        return None;
    }
    let (number, single) = match text.strip_suffix(['f', 'F']) {
        Some(number) => (number, true),
        None => (text, false),
    };
    let bytes = number.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\''
            && (index == 0
                || index + 1 == bytes.len()
                || !bytes[index - 1].is_ascii_digit()
                || !bytes[index + 1].is_ascii_digit())
        {
            return None;
        }
    }
    let number = if number.contains('\'') {
        std::borrow::Cow::Owned(number.replace('\'', ""))
    } else {
        std::borrow::Cow::Borrowed(number)
    };
    let value = if single {
        f64::from(number.parse::<f32>().ok()?)
    } else {
        number.parse::<f64>().ok()?
    };
    value
        .is_finite()
        .then_some(Expr::FloatLit(crate::core_ir::FloatVal(value)))
}

fn parse_c_integer_literal(text: &str) -> Option<i64> {
    let text = text.trim();
    let number = text.trim_end_matches(['u', 'U', 'l', 'L']);
    let suffix = &text[number.len()..];
    if suffix.contains("lL")
        || suffix.contains("Ll")
        || !matches!(
            suffix.to_ascii_lowercase().as_str(),
            "" | "u" | "l" | "ul" | "lu" | "ll" | "ull" | "llu"
        )
    {
        return None;
    }
    let (digits, radix) = if let Some(digits) = number
        .strip_prefix("0x")
        .or_else(|| number.strip_prefix("0X"))
    {
        (digits, 16)
    } else if let Some(digits) = number
        .strip_prefix("0b")
        .or_else(|| number.strip_prefix("0B"))
    {
        (digits, 2)
    } else if number.starts_with('0') {
        (number, 8)
    } else {
        (number, 10)
    };
    let bytes = digits.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\''
            && (index == 0
                || index + 1 == bytes.len()
                || !(bytes[index - 1] as char).is_digit(radix)
                || !(bytes[index + 1] as char).is_digit(radix))
        {
            return None;
        }
    }
    if digits.contains('\'') {
        i64::from_str_radix(&digits.replace('\'', ""), radix).ok()
    } else {
        i64::from_str_radix(digits, radix).ok()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_floats_keep_suffix_rounding_and_reject_malformed_forms() {
        for text in ["1.5", "1.5f", "1.5F", "15e-1", ".15e1", "1'5e-1"] {
            assert_eq!(
                parse_c_number_literal(text),
                Some(Expr::FloatLit(crate::core_ir::FloatVal(1.5))),
                "{text}"
            );
        }
        assert_eq!(
            parse_c_number_literal("0.1f"),
            Some(Expr::FloatLit(crate::core_ir::FloatVal(f64::from(0.1f32))))
        );
        assert_eq!(parse_c_number_literal("0x2F"), Some(Expr::IntLit(47)));
        for text in [
            "42f", "1.5L", "1.5foo", "1e", "1e+", "1e400", "1e39f", "1'.5", "1.''5", "0x1.8p1",
        ] {
            assert_eq!(parse_c_number_literal(text), None, "{text}");
        }
    }

    #[test]
    fn integer_literal_radices_suffixes_and_separators_preserve_values() {
        for text in [
            "42",
            "0x2A",
            "0X2a",
            "052",
            "0b101010",
            "0B101010",
            "42u",
            "42UL",
            "42lu",
            "42LL",
            "42uLL",
            "42LLU",
            "0x2Aull",
            "0'52",
            "4'2",
            "0x2'A",
            "0b10'1010",
        ] {
            assert_eq!(parse_c_integer_literal(text), Some(42), "{text}");
        }
        assert_eq!(parse_c_integer_literal("0"), Some(0));
        assert_eq!(parse_c_integer_literal("0xFF"), Some(255));
        assert_eq!(
            parse_c_integer_literal("0x7FFFFFFFFFFFFFFFULL"),
            Some(i64::MAX)
        );
    }

    #[test]
    fn integer_literal_invalid_and_out_of_range_forms_are_refused() {
        for text in [
            "",
            "0x",
            "0b",
            "08",
            "0b102",
            "42foo",
            "42uu",
            "42lL",
            "42lll",
            "42z",
            "'42",
            "42'",
            "4''2",
            "0x'2A",
            "9223372036854775808",
            "0xffffffffffffffffULL",
            "1.5",
            "1e2",
        ] {
            assert_eq!(parse_c_integer_literal(text), None, "{text}");
        }
    }
}
