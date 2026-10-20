use super::extract::{
    collect_kinds, decl_fn, first_named, last_named, named_descendant, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Expr, Stmt, Typ, Visibility};
use tree_sitter::Node;

pub(super) fn extract_c(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = c_globals(src, root)?;
    let mut functions = Vec::new();
    collect_kinds(root, &["function_definition"], &mut functions);
    for node in functions {
        validate_c_definition_parameters(src, node)?;
        if let Some(function) = c_like_function_decl(src, node) {
            decls.push(function);
        }
    }
    Ok(decls)
}

// Follow only the declared name, never a parameter's declarator subtree.
fn c_declarator_spine(mut node: Node<'_>) -> Vec<Node<'_>> {
    let mut spine = vec![node];
    loop {
        let next = node.child_by_field_name("declarator").or_else(|| {
            if matches!(
                node.kind(),
                "parenthesized_declarator" | "reference_declarator"
            ) {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|child| child.kind() != "comment")
            } else {
                None
            }
        });
        let Some(next) = next else { break };
        spine.push(next);
        node = next;
    }
    spine
}

fn c_actual_function_declarator(definition: Node<'_>) -> Option<Node<'_>> {
    c_declarator_spine(definition.child_by_field_name("declarator")?)
        .into_iter()
        .rev()
        .find(|node| node.kind() == "function_declarator")
}

// Parameter extraction models fixed arity only. Validate before extraction so
// unsupported source signatures cannot silently lose parameters.
fn validate_c_definition_parameters(src: &[u8], definition: Node<'_>) -> Result<(), String> {
    let name = c_like_fn_name(src, definition).unwrap_or_else(|| "<anonymous>".into());
    let function = c_actual_function_declarator(definition)
        .ok_or_else(|| format!("unsupported C-like definition declarator `{name}`"))?;
    let parameters = function
        .child_by_field_name("parameters")
        .or_else(|| first_named(function, "parameter_list"))
        .ok_or_else(|| format!("missing C-like definition parameters `{name}`"))?;
    let mut cursor = parameters.walk();
    // C++ represents `...` as an anonymous token; inspect all children so
    // named-child extraction cannot silently discard variadic parameters.
    let nodes: Vec<_> = parameters
        .children(&mut cursor)
        .filter(|node| !matches!(node.kind(), "(" | ")" | "," | "comment"))
        .collect();
    if nodes.len() == 1 && node_txt(src, nodes[0]).trim() == "void" {
        return Ok(());
    }
    for (index, parameter) in nodes.into_iter().enumerate() {
        if parameter.kind() != "parameter_declaration" {
            return Err(format!(
                "unsupported variadic or K&R C-like definition `{name}`"
            ));
        }
        let (_, typ) = c_one_parameter(src, parameter, index)
            .ok_or_else(|| format!("unsupported void parameter in C-like definition `{name}`"))?;
        if typ.canonical() == Typ::Void {
            return Err(format!(
                "unsupported named or mixed void parameter in C-like definition `{name}`"
            ));
        }
    }
    Ok(())
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
        let typ = c_destination_typ(src, type_node);
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
        if !is_class_method {
            validate_c_definition_parameters(src, f)?;
            if let Some(d) = c_like_function_decl(src, f) {
                decls.push(d);
            }
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
        let decl = f.child_by_field_name("declarator");
        let field_type = f
            .child_by_field_name("type")
            .map(|specifier| {
                if decl.is_some_and(|declarator| declarator.kind() == "field_identifier") {
                    c_destination_typ(src, specifier)
                } else {
                    // Keep existing indirect/array field representation; its
                    // element spelling is not a scalar destination width.
                    Typ::Named(node_txt(src, specifier).trim().to_string())
                }
            })
            .unwrap_or(Typ::Named("int".into()));
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
    let declarator = func_def.child_by_field_name("declarator")?;
    let id = c_declarator_spine(declarator)
        .into_iter()
        .rev()
        .find_map(|node| match node.kind() {
            "identifier" | "field_identifier" => Some(node),
            "qualified_identifier" => named_descendant(node, "field_identifier")
                .or_else(|| named_descendant(node, "identifier")),
            _ => None,
        })?;
    Some(normalize_entry(node_txt(src, id).trim()))
}

/// Only a source global `int main` has C/C++'s implicit success return.
/// Entry-name normalization also maps Main/_main, which are ordinary functions;
/// class/namespace/nested declarations must not inherit the global-main rule.
fn c_global_main(src: &[u8], definition: Node<'_>, ret: &Typ) -> bool {
    if !matches!(ret, Typ::Named(name)
        if name == "int" || crate::core_ir::integer_type_name(name) == Some("i32"))
    {
        return false;
    }
    let Some(declarator) = definition.child_by_field_name("declarator") else {
        return false;
    };
    let spine = c_declarator_spine(declarator);
    if spine.iter().any(|node| {
        matches!(
            node.kind(),
            "qualified_identifier" | "pointer_declarator" | "reference_declarator"
        )
    }) || !spine.iter().rev().any(|node| {
        matches!(node.kind(), "identifier" | "field_identifier")
            && node_txt(src, *node).trim() == "main"
    }) {
        return false;
    }
    let mut parent = definition.parent();
    while let Some(node) = parent {
        match node.kind() {
            "translation_unit" => return true,
            "class_specifier"
            | "struct_specifier"
            | "union_specifier"
            | "namespace_definition"
            | "function_definition"
            | "lambda_expression" => return false,
            _ => parent = node.parent(),
        }
    }
    false
}

/// Lower a C / C++ / ObjC++ function definition with its source body.
pub(super) fn c_like_function_decl<'a>(src: &[u8], func_def: Node<'a>) -> Option<Decl> {
    let name = c_like_fn_name(src, func_def)?;
    let ret = c_coarse_return_typ(src, func_def);
    let params = c_parameter_list(src, func_def);
    let body_node = func_def.child_by_field_name("body");
    let mut body = body_node.map(|b| c_body(src, b)).unwrap_or_default();
    if body_node.is_some() {
        if c_global_main(src, func_def, &ret) {
            // Append the language's fallback, including after partial branches.
            // A terminal direct return already makes this fallback unreachable.
            if !matches!(body.last(), Some(Stmt::Return(_))) {
                body.push(Stmt::Return(Some(Expr::IntLit(0))));
            }
        } else if body.is_empty() {
            // [] denotes an external declaration. A real empty definition must
            // remain a definition; non-void Return(None) is rejected downstream.
            body.push(Stmt::Return(None));
        }
    }
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
    let spine = c_declarator_spine(decl);
    if spine
        .iter()
        .any(|node| node.kind() == "reference_declarator")
    {
        return Typ::Named("reference return".into());
    }
    if spine.iter().any(|node| node.kind() == "pointer_declarator") {
        return Typ::Int;
    }
    func_def
        .child_by_field_name("type")
        .map(|specifier| c_destination_typ(src, specifier))
        .unwrap_or(Typ::Void)
}

fn c_parameter_list<'a>(src: &[u8], func_def: Node<'a>) -> Vec<(String, Typ)> {
    let Some(function) = c_actual_function_declarator(func_def) else {
        return vec![];
    };
    let Some(plist) = function
        .child_by_field_name("parameters")
        .or_else(|| first_named(function, "parameter_list"))
    else {
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
    let specifier = pd.child_by_field_name("type")?;
    let ty_text = c_type_specifier_text(src, specifier);
    let ty_text = ty_text.trim();
    if ty_text.is_empty() {
        return None;
    }
    if ty_text == "void" && decl.is_none() {
        return None;
    }
    let spine = decl.map(c_declarator_spine).unwrap_or_default();
    let ty = if spine
        .iter()
        .any(|node| node.kind() == "reference_declarator")
    {
        Typ::Named(format!("reference {}", node_txt(src, specifier).trim()))
    } else if spine.iter().any(|node| {
        matches!(
            node.kind(),
            "pointer_declarator" | "array_declarator" | "function_declarator"
        )
    }) {
        Typ::Int
    } else {
        c_destination_typ(src, specifier)
    };
    let name = decl
        .and_then(|d| {
            c_declarator_spine(d)
                .into_iter()
                .rev()
                .find(|node| matches!(node.kind(), "field_identifier" | "identifier"))
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
        "do_statement" => c_do_statement(src, stmt),
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
            rhs: Box::new(c_number_literal("1")?),
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
    let typ = c_destination_typ(src, declaration.child_by_field_name("type")?);
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
            callee: Box::new(Expr::Ident(crate::core_ir::MODIFY_VALUE_FIRST.into())),
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

fn c_do_statement(src: &[u8], stmt: Node<'_>) -> Option<Stmt> {
    let cond = c_expr(src, stmt.child_by_field_name("condition")?)?;
    let body = c_stmt_or_body(src, stmt.child_by_field_name("body")?);
    let mut first = format!("__c_do_first_{}", stmt.start_byte());
    let source = std::str::from_utf8(src).ok()?;
    while source.contains(&first) {
        first.push('_');
    }
    Some(Stmt::If {
        cond: Expr::BoolLit(true),
        then_body: vec![
            Stmt::Let(first.clone(), Some(Typ::Bool), Expr::BoolLit(true)),
            Stmt::Loop {
                kind: crate::core_ir::LoopKind::While,
                cond: Some(Expr::Binary {
                    op: "||".into(),
                    lhs: Box::new(Expr::Ident(first.clone())),
                    rhs: Box::new(cond),
                }),
                body: vec![
                    Stmt::Assign(first, Expr::BoolLit(false)),
                    Stmt::If {
                        cond: Expr::BoolLit(true),
                        then_body: body,
                        else_body: vec![],
                    },
                ],
            },
        ],
        else_body: vec![],
    })
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
        "number_literal" => c_number_literal(node_txt(src, expr)),
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
        "cast_expression" => c_cast_expr(src, expr),
        "binary_expression" => c_binary_expr(src, expr),
        "unary_expression" => c_unary_expr(src, expr),
        "call_expression" => c_call_expr(src, expr),
        _ => None,
    }
}

fn c_type_specifier_text(src: &[u8], node: Node<'_>) -> String {
    if node.kind() == "comment" {
        return String::new();
    }
    if node.child_count() == 0 {
        return node_txt(src, node).into();
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .map(|child| c_type_specifier_text(src, child))
        .collect::<Vec<_>>()
        .join(" ")
}

// Preserve only widths shared by the supported native targets. Plain char
// signedness and long width still require target-specific metadata.
fn c_fixed_integer_name(raw: &str) -> Option<&'static str> {
    let spelling = c_strip_decl_storage(raw);
    let mut words: Vec<_> = spelling.split_whitespace().collect();
    words.sort_unstable();
    match words.as_slice() {
        ["char", "signed"] => Some("i8"),
        ["char", "unsigned"] => Some("u8"),
        ["short"] | ["int", "short"] | ["short", "signed"] | ["int", "short", "signed"] => {
            Some("i16")
        }
        ["short", "unsigned"] | ["int", "short", "unsigned"] => Some("u16"),
        ["int"] | ["signed"] | ["int", "signed"] => Some("i32"),
        ["unsigned"] | ["int", "unsigned"] => Some("u32"),
        _ => None,
    }
}

fn c_destination_typ(src: &[u8], specifier: Node<'_>) -> Typ {
    let text = c_type_specifier_text(src, specifier);
    c_fixed_integer_name(&text)
        .map(|name| Typ::Named(format!("#integer-{name}")))
        .unwrap_or_else(|| c_typ_from_decl_specifier_text(node_txt(src, specifier)))
}

fn c_cast_expr(src: &[u8], expr: Node<'_>) -> Option<Expr> {
    let descriptor = expr.child_by_field_name("type")?;
    if descriptor.child_by_field_name("declarator").is_some() {
        return None;
    }
    let specifier = descriptor.child_by_field_name("type")?;
    // The C grammar exposes the reserved _Bool keyword as a type_identifier.
  if !(matches!(specifier.kind(), "primitive_type" | "sized_type_specifier")
    || (specifier.kind() == "type_identifier" && node_txt(src, specifier) == "_Bool"))
    {
        return None;
    }
    let narrow = c_fixed_integer_name(&c_type_specifier_text(src, specifier));
    let target = if let Some(narrow) = narrow {
        narrow
    } else {
        match c_typ_from_decl_specifier_text(node_txt(src, specifier)) {
            Typ::Int => "Int",
            Typ::Float => "Float",
            Typ::Bool => "Bool",
            _ => return None,
        }
    };
    Some(Expr::Call {
        callee: Box::new(Expr::Ident("c-numeric-cast".into())),
        args: vec![
            c_expr(src, expr.child_by_field_name("value")?)?,
            Expr::StringLit(target.into()),
        ],
    })
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

// Source integer kinds survive until C promotion resolution. Keep the value
// parser independent: a valid literal need not have a representable C32 kind.
fn c_number_literal(text: &str) -> Option<Expr> {
    let text = text.trim();
    if let Some(positive) = text.strip_prefix('+') {
        return c_number_literal(positive);
    }
    if let Some(positive) = text.strip_prefix('-') {
        return Some(Expr::Unary {
            op: "-".into(),
            expr: Box::new(c_number_literal(positive)?),
        });
    }
    let value = parse_c_number_literal(text)?;
    let Expr::IntLit(integer) = &value else {
        return Some(value);
    };
    let text = text.trim();
    let number = text.trim_end_matches(['u', 'U', 'l', 'L']);
    let suffix = &text[number.len()..];
    let kind = if suffix.contains(['l', 'L']) {
        None
    } else if suffix.eq_ignore_ascii_case("u") {
        (0..=i64::from(u32::MAX)).contains(integer).then_some("u32")
    } else if (0..=i64::from(i32::MAX)).contains(integer) {
        Some("i32")
    } else if number.starts_with('0') && (0..=i64::from(u32::MAX)).contains(integer) {
        Some("u32")
    } else {
        None
    };
    Some(match kind {
        Some(kind) => Expr::Unary {
            op: format!("c-integer-literal-{kind}"),
            expr: Box::new(value),
        },
        None => value,
    })
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

#[cfg(test)]
mod empty_definition_tests {
    use super::*;
    use crate::core_ir::UnifiedModule;

    fn extract(source: &str, cpp: bool) -> UnifiedModule {
        let mut parser = tree_sitter::Parser::new();
        let language = if cpp {
            tree_sitter_cpp::LANGUAGE.into()
        } else {
            tree_sitter_c::LANGUAGE.into()
        };
        parser.set_language(&language).expect("C-family grammar");
        let tree = parser.parse(source, None).expect("parse C-family source");
        assert!(!tree.root_node().has_error(), "{source}");
        let decls = if cpp {
            extract_cpp_with_classes(source.as_bytes(), tree.root_node())
        } else {
            extract_c(source.as_bytes(), tree.root_node())
        };
        UnifiedModule::new(decls.expect("extract C-family source"))
    }

    #[test]
    fn empty_void_definitions_remain_defined_on_native_backends() {
        for cpp in [false, true] {
            for contents in ["", "/* intentionally empty */", ";"] {
                let source = format!(
                    "void noop(void) {{{contents}}} long long main(void) {{ noop(); return 42; }}"
                );
                let module = extract(&source, cpp);
                let body = module
                    .decls
                    .iter()
                    .find_map(|decl| match decl {
                        Decl::Function { name, body, .. } if name == "noop" => Some(body),
                        _ => None,
                    })
                    .expect("noop definition");
                assert!(matches!(body.as_slice(), [Stmt::Return(None)]));
                // Source-only numeric metadata is consumed by the shared C
                // normalization stage before either native backend runs.
                let parser = if cpp {
                    crate::parser_registry::ParserId::Cpp
                } else {
                    crate::parser_registry::ParserId::C
                };
                let module = crate::typecheck::normalize_module(parser, &module);
                let thumb = crate::native_emit::thumb_lower::lower_module(&module, "main")
                    .expect("lower empty void definition to Thumb");
                assert!(!thumb.code.is_empty());
                assert!(thumb.exports.iter().any(|(name, _)| name == "noop"));
                assert!(!thumb.externs.iter().any(|name| name == "noop"));
                let x86 = crate::native_emit::x86_64_lower::lower_module(&module, "main")
                    .expect("lower empty void definition to x86");
                assert!(!x86.code.is_empty());
                assert!(x86.exports.iter().any(|(name, _)| name == "noop"));
                assert!(!x86.externs.iter().any(|name| name == "noop"));
            }
        }
    }

    #[test]
    fn actual_global_main_has_implicit_zero_after_empty_and_partial_bodies() {
        for cpp in [false, true] {
            for source in [
                "int main(void) {}",
                "int main(void) { /* comment */ ; }",
                "int main(void) { int value = 7; }",
                "int main(int argc, char **argv) { if (argc > 1) { return 42; } }",
            ] {
                let module = extract(source, cpp);
                assert!(
                    matches!(&module.decls[0], Decl::Function { body, .. }
                    if matches!(body.last(), Some(Stmt::Return(Some(Expr::IntLit(0)))))),
                    "{source}: {module:?}"
                );
            }
            let module = extract("int main(void) { return 42; }", cpp);
            assert!(
                matches!(&module.decls[0], Decl::Function { body, .. }
                if matches!(body.as_slice(), [Stmt::Return(Some(value))]
                    if Some(value.clone()) == c_number_literal("42"))),
                "{module:?}"
            );
            let module = extract(
                "int external(void); int main(void) { return external(); }",
                cpp,
            );
            // Prototypes are currently omitted, never materialized as definitions.
            assert!(!module.decls.iter().any(|decl| matches!(decl,
                Decl::Function { name, .. } if name == "external")));
            for name in ["Main", "_main"] {
                let module = extract(&format!("int {name}(void) {{}}"), cpp);
                assert!(matches!(&module.decls[0], Decl::Function { body, .. }
                    if matches!(body.as_slice(), [Stmt::Return(None)])));
            }
        }
    }

    #[test]
    fn cpp_member_and_namespace_main_are_not_program_entry_fallthrough() {
        let module = extract(
            "class Worker { public: int main() {} }; int main() { return 42; }",
            true,
        );
        let methods = module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Class { methods, .. } => Some(methods),
                _ => None,
            })
            .unwrap();
        assert!(methods.iter().any(|method| matches!(method,
            Decl::Function { name, body, .. } if name == "main" && matches!(body.as_slice(), [Stmt::Return(None)]))));
        let module = extract("namespace Worker { int main() {} }", true);
        assert!(module.decls.iter().any(|decl| matches!(decl,
            Decl::Function { body, .. } if matches!(body.as_slice(), [Stmt::Return(None)]))));
    }

    #[test]
    fn nonvoid_empty_definitions_remain_visible_without_hiding_unsupported_bodies() {
        for cpp in [false, true] {
            let module = extract(
                "long long empty(void) {} void unsupported(void) { goto done; done: ; }",
                cpp,
            );
            let thumb_error = crate::native_emit::thumb_lower::lower_module(&module, "unsupported")
                .map(|_| ())
                .expect_err("unsupported statement must refuse Thumb lowering");
            assert!(thumb_error.contains("__unsupported_c_"), "{thumb_error}");
            let x86_error = crate::native_emit::x86_64_lower::lower_module(&module, "unsupported")
                .map(|_| ())
                .expect_err("unsupported statement must refuse x86 lowering");
            assert!(x86_error.contains("__unsupported_c_"), "{x86_error}");
            for decl in &module.decls {
                if let Decl::Function { name, body, .. } = decl {
                    match name.as_str() {
                        "empty" => {
                            assert!(matches!(body.as_slice(), [Stmt::Return(None)]))
                        }
                        "unsupported" => {
                            assert!(!body.is_empty());
                            assert!(body.iter().any(|stmt| matches!(stmt,
                                Stmt::Expr(Expr::Call { callee, .. })
                                    if matches!(callee.as_ref(), Expr::Ident(name) if name.starts_with("__unsupported_c_"))
                            )));
                        }
                        _ => panic!("unexpected definition {name}"),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod definition_parameter_tests {
    use super::*;

    fn parse(source: &str, cpp: bool) -> Result<Vec<Decl>, String> {
        let mut parser = tree_sitter::Parser::new();
        let language = if cpp {
            tree_sitter_cpp::LANGUAGE.into()
        } else {
            tree_sitter_c::LANGUAGE.into()
        };
        parser.set_language(&language).expect("C-family grammar");
        let tree = parser.parse(source, None).expect("C-family tree");
        if tree.root_node().has_error() {
            return Err("invalid C-family syntax".into());
        }
        if cpp {
            extract_cpp_with_classes(source.as_bytes(), tree.root_node())
        } else {
            extract_c(source.as_bytes(), tree.root_node())
        }
    }

    #[test]
    fn destination_width_parsing_preserves_qualified_nominal_names() {
        let decls = parse("class Holder { public: Ns::Widget value; }; Ns::Widget answer(Ns::Widget value) { Ns::Widget local = value; return local; }", true).unwrap();
        let expected = Typ::Named("Ns::Widget".into());
        let Decl::Class { fields, .. } = &decls[0] else {
            panic!("Holder")
        };
        assert_eq!(fields[0].1, expected);
        let Decl::Function {
            params, ret, body, ..
        } = &decls[1]
        else {
            panic!("answer")
        };
        assert_eq!(params[0].1, expected);
        assert_eq!(*ret, expected);
        assert!(matches!(&body[0], Stmt::Let(_, Some(typ), _) if typ == &expected));
        let qualified = parse("int Ns::answer(int value) { return value; }", true).unwrap();
        assert!(
            matches!(&qualified[0], Decl::Function { name, params, .. } if name == "answer" && params.len() == 1)
        );
        let reference = parse(
            "unsigned char *&answer(unsigned char *value) { return value; }",
            true,
        )
        .unwrap();
        assert!(
            matches!(&reference[0], Decl::Function { ret: Typ::Named(name), .. } if name == "reference return")
        );
    }

    #[test]
    fn function_pointer_declarators_keep_address_carriers_and_actual_parameters() {
        for cpp in [false, true] {
            for source in [
                "long long answer(unsigned char cb(int value)) {return 42;}",
                "long long answer(unsigned char (*cb)(int value)) {return 42;}",
            ] {
                let decls = parse(source, cpp).unwrap();
                assert!(
                    matches!(&decls[0], Decl::Function {params,..} if params == &vec![("cb".into(),Typ::Int)]),
                    "{source}: {decls:?}"
                );
            }
            let decls =
                parse("unsigned char (*factory(void))(int value) {return 0;}", cpp).unwrap();
            assert!(
                matches!(&decls[0], Decl::Function {name,params,ret,..} if name == "factory" && params.is_empty() && *ret == Typ::Int),
                "{decls:?}"
            );
            let decls = parse("unsigned char (scalar)(int *pointer) {return 42;}", cpp).unwrap();
            assert!(
                matches!(&decls[0], Decl::Function {name,params,ret,..} if name == "scalar" && params == &vec![("pointer".into(),Typ::Int)] && *ret == Typ::Named("#integer-u8".into())),
                "{decls:?}"
            );
            assert!(
                parse(
                    "unsigned char (*factory(int count, ...))(int value) {return 0;}",
                    cpp
                )
                .is_err()
            );
            assert!(
                parse(
                    "unsigned char (*factory(void))(int count, ...) {return 0;}",
                    cpp
                )
                .is_ok()
            );
        }
        let decls = parse("long long answer(Ns::Widget &value) {return 42;}", true).unwrap();
        assert!(
            matches!(&decls[0], Decl::Function {params,..} if params[0].1 == Typ::Named("reference Ns::Widget".into())),
            "{decls:?}"
        );
    }

    #[test]
    fn declared_fixed_widths_survive_all_scalar_destination_boundaries() {
        for cpp in [false, true] {
            for (spelling, expected) in [
                ("unsigned char", "u8"),
                ("signed char", "i8"),
                ("short unsigned int", "u16"),
                ("signed short", "i16"),
                ("unsigned int", "u32"),
                ("signed int", "i32"),
            ] {
                let source = format!(
                    "const {spelling} global=0; {spelling} answer({spelling} value) {{{spelling} local=0; return value;}}"
                );
                let decls = parse(&source, cpp).unwrap();
                let alias = Typ::Named(format!("#integer-{expected}"));
                assert!(
                    matches!(&decls[0], Decl::Global {typ,..} if typ == &alias),
                    "{source}: {decls:?}"
                );
                let Decl::Function {
                    params, ret, body, ..
                } = &decls[1]
                else {
                    panic!("answer")
                };
                assert_eq!(ret, &alias, "{source}");
                assert_eq!(params[0].1, alias, "{source}");
                assert!(
                    matches!(&body[0], Stmt::Let(_,Some(typ),_) if typ == &alias),
                    "{source}: {body:?}"
                );
            }
            let decls = parse("unsigned char *answer(unsigned char *pointer, unsigned short values[]) {return pointer;}", cpp).unwrap();
            let Decl::Function { params, ret, .. } = &decls[0] else {
                panic!("answer")
            };
            assert_eq!(ret, &Typ::Int);
            assert!(params.iter().all(|(_, typ)| *typ == Typ::Int));
        }
        // Public nominal normalization remains a carrier operation.
        assert_eq!(c_typ_from_decl_specifier_text("unsigned char"), Typ::Int);
        assert_eq!(
            c_typ_from_decl_specifier_text("struct Example"),
            Typ::Named("struct Example".into())
        );
    }

    #[test]
    fn class_scalar_fields_preserve_width_without_narrowing_indirect_fields() {
        let decls = parse("class Example { public: unsigned char byte; signed short word; unsigned char *pointer; unsigned short array[2]; };", true).unwrap();
        let Decl::Class { fields, .. } = &decls[0] else {
            panic!("Example")
        };
        assert_eq!(
            fields,
            &vec![
                ("byte".into(), Typ::Named("#integer-u8".into())),
                ("word".into(), Typ::Named("#integer-i16".into())),
                ("pointer".into(), Typ::Named("unsigned char".into())),
                ("array".into(), Typ::Named("unsigned short".into())),
            ]
        );
    }

    #[test]
    fn user_nominal_integer_alias_name_stays_distinct_from_primitive_metadata() {
        let decls = parse("class u8 {}; u8 identity(u8 value) {return value;} unsigned char primitive(unsigned char value) {return value;}", true).unwrap();
        let nominal = decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function {name,..} if name == "identity"))
            .unwrap();
        assert!(
            matches!(nominal, Decl::Function {params,ret,..} if *ret == Typ::Named("u8".into()) && params[0].1 == Typ::Named("u8".into()))
        );
        let primitive = decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function {name,..} if name == "primitive"))
            .unwrap();
        assert!(
            matches!(primitive, Decl::Function {params,ret,..} if *ret == Typ::Named("#integer-u8".into()) && params[0].1 == Typ::Named("#integer-u8".into()))
        );
    }

    #[test]
    fn generated_increment_operands_keep_c_int_kind() {
        for cpp in [false, true] {
            for update in ["value++", "++value", "value--", "--value"] {
                let decls = parse(
                    &format!("long long main(void){{unsigned int value=1;{update};return 42;}}"),
                    cpp,
                )
                .unwrap();
                let Decl::Function { body, .. } = &decls[0] else {
                    panic!("main")
                };
                assert!(
                    matches!(&body[1], Stmt::Assign(_,Expr::Binary {rhs,..})
                    if matches!(rhs.as_ref(), Expr::Unary {op,expr}
                        if op == "c-integer-literal-i32" && matches!(expr.as_ref(), Expr::IntLit(1)))),
                    "{update}/{cpp}: {body:?}"
                );
            }
        }
    }

    #[test]
    fn signed_source_literals_keep_the_positive_operand_rank() {
        for cpp in [false, true] {
            for literal in ["-1", "-1U", "-2147483648", "-2147483648U"] {
                let positive = literal.strip_prefix('-').unwrap();
                let expected = Expr::Unary {
                    op: "-".into(),
                    expr: Box::new(c_number_literal(positive).unwrap()),
                };
                let decls =
                    parse(&format!("long long main(void){{return {literal};}}"), cpp).unwrap();
                assert!(
                    matches!(&decls[0], Decl::Function {body,..} if body == &vec![Stmt::Return(Some(expected))]),
                    "{literal}/{cpp}: {decls:?}"
                );
            }
        }
        assert_eq!(c_number_literal("+1U"), c_number_literal("1U"));
        assert!(
            matches!(c_number_literal("-2147483648"), Some(Expr::Unary {expr,..}) if matches!(expr.as_ref(), Expr::IntLit(2147483648)))
        );
        assert!(
            matches!(c_number_literal("-1U"), Some(Expr::Unary {expr,..}) if matches!(expr.as_ref(), Expr::Unary {op,..} if op == "c-integer-literal-u32"))
        );
    }

    #[test]
    fn source_integer_literals_preserve_only_proven_c32_kinds() {
        for cpp in [false, true] {
            for (literal, kind, expected) in [
                ("0", Some("i32"), 0),
                ("2147483647", Some("i32"), 2147483647),
                ("2147483648", None, 2147483648),
                ("42U", Some("u32"), 42),
                ("4294967295u", Some("u32"), 4294967295),
                ("4294967296U", None, 4294967296),
                ("0x7fffffff", Some("i32"), 2147483647),
                ("0X80000000", Some("u32"), 2147483648),
                ("0xffffffff", Some("u32"), 4294967295),
                ("037777777777", Some("u32"), 4294967295),
                (
                    "0b10000000000000000000000000000000",
                    Some("u32"),
                    2147483648,
                ),
                ("42L", None, 42),
                ("42LL", None, 42),
                ("42UL", None, 42),
                ("42llu", None, 42),
                ("0x100000000", None, 4294967296),
            ] {
                let decls =
                    parse(&format!("long long main(void){{return {literal};}}"), cpp).unwrap();
                let expected = match kind {
                    Some(kind) => Expr::Unary {
                        op: format!("c-integer-literal-{kind}"),
                        expr: Box::new(Expr::IntLit(expected)),
                    },
                    None => Expr::IntLit(expected),
                };
                assert!(
                    matches!(&decls[0], Decl::Function {body,..} if body == &vec![Stmt::Return(Some(expected))]),
                    "{literal}/{cpp}: {decls:?}"
                );
            }
        }
        assert!(matches!(c_number_literal("1.5"), Some(Expr::FloatLit(_))));
        for invalid in ["42uu", "42lL", "0x", "08", "0b102"] {
            assert_eq!(c_number_literal(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn explicit_narrow_casts_keep_width_and_signedness_metadata() {
        for cpp in [false, true] {
            for (spelling, expected) in [
                ("signed char", "i8"),
                ("unsigned char", "u8"),
                ("short", "i16"),
                ("signed short int", "i16"),
                ("const unsigned short", "u16"),
                ("short unsigned int", "u16"),
                ("int", "i32"),
                ("signed", "i32"),
                ("signed int", "i32"),
                ("unsigned", "u32"),
                ("unsigned int", "u32"),
                ("unsigned /* width */ char", "u8"),
                ("signed /* width */ short int", "i16"),
            ] {
                let source = format!("long long main(void) {{return ({spelling})256;}}");
                let mut module = crate::core_ir::UnifiedModule::new(parse(&source, cpp).unwrap());
                let mut targets = Vec::new();
                crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                    if let Expr::Call { callee, args } = expr
                        && matches!(callee.as_ref(), Expr::Ident(name) if name == "c-numeric-cast")
                        && let [_, Expr::StringLit(target)] = args.as_slice()
                    {
                        targets.push(target.clone());
                    }
                });
                assert_eq!(targets, [expected], "{source}");
            }
        }
    }

    #[test]
    fn fixed_definition_signatures_and_empty_void_bodies_are_preserved() {
        for cpp in [false, true] {
            for (source, count) in [
                ("void answer() {}", 0),
                ("void answer(void) {}", 0),
                ("void answer(long long value, void *pointer) {}", 2),
            ] {
                let decls = parse(source, cpp).expect("supported fixed definition");
                assert!(matches!(&decls[0], Decl::Function { params, body, .. }
                    if params.len() == count && matches!(body.as_slice(), [Stmt::Return(None)])));
            }
        }
    }

    #[test]
    fn lossy_definition_parameter_forms_are_refused() {
        for cpp in [false, true] {
            for source in [
                "void answer(long long value, ...) {}",
                "void answer(void named) {}",
                "void answer(void, long long value) {}",
                "void answer(long long value, void) {}",
            ] {
                let error = match parse(source, cpp) {
                    Err(error) => error,
                    Ok(decls) => {
                        panic!("lossy signature accepted (cpp={cpp}): {source}; {decls:?}")
                    }
                };
                assert!(
                    error.contains("definition") || error.contains("syntax"),
                    "{error}"
                );
            }
        }
        let error = parse("void answer(value) long long value; {}", false)
            .expect_err("K&R definition must refuse");
        assert!(error.contains("K&R"), "{error}");
    }

    #[test]
    fn class_methods_keep_the_existing_extraction_path() {
        let decls = parse(
            "class Example { public: void method(void) {} }; void answer(void) {}",
            true,
        )
        .expect("class and free function");
        assert!(
            decls
                .iter()
                .any(|decl| matches!(decl, Decl::Class { methods, .. } if !methods.is_empty()))
        );
        assert!(
            decls
                .iter()
                .any(|decl| matches!(decl, Decl::Function { name, .. } if name == "answer"))
        );
    }
    #[test]
    fn generated_c_compound_uses_private_value_first_operation() {
        for cpp in [false, true] {
            let mut decls =
                parse("int main(void){int value=20;value+=22;return value;}", cpp).unwrap();
            let mut found = false;
            crate::core_opt::map_decl_exprs_mut(&mut decls, &mut |expr| {
                found |= matches!(expr,Expr::Call{callee,..} if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::MODIFY_VALUE_FIRST));
            });
            assert!(found);
        }
    }
}
