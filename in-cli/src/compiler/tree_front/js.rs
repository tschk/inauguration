use super::extract::{
    AstShape, ast_body, collect_kinds, find_return_expr, first_named, infer_expr_type, node_txt,
    normalize_entry,
};
use crate::core_ir::{Decl, Expr, Stmt, Typ, Visibility};
use std::collections::HashMap;
use tree_sitter::Node;

const JS_AST: AstShape = AstShape {
    block_kinds: &["statement_block"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement"],
    local_decl_kinds: &["lexical_declaration", "variable_declaration"],
    assignment_kinds: &["assignment_expression", "augmented_assignment_expression"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    call_kinds: &["call_expression"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["number"],
    string_kinds: &["string"],
    type_kinds: &[],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &["else_clause"],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
};

pub(super) fn extract_js_with_classes(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut class_nodes = Vec::new();
    collect_kinds(root, &["class_declaration"], &mut class_nodes);
    for c in class_nodes {
        if let Some(d) = js_class_decl(src, c) {
            decls.push(d);
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(
        root,
        &["function_declaration", "generator_function_declaration"],
        &mut func_nodes,
    );
    for f in func_nodes {
        if let Some(d) = js_function_decl(src, f) {
            decls.push(d);
        }
    }

    let mut var_nodes = Vec::new();
    collect_kinds(
        root,
        &["lexical_declaration", "variable_declaration"],
        &mut var_nodes,
    );
    for v in var_nodes {
        let mut vdec_nodes = Vec::new();
        collect_kinds(v, &["variable_declarator"], &mut vdec_nodes);
        for vd in vdec_nodes {
            if let Some(d) = js_var_function(src, vd) {
                decls.push(d);
            }
        }
    }

    rewrite_constructor_calls(&mut decls);
    Ok(decls)
}

fn js_class_decl<'a>(src: &[u8], class_node: Node<'a>) -> Option<Decl> {
    let name_n = class_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let body = class_node.child_by_field_name("body")?;

    let mut fields = Vec::new();
    let mut methods = Vec::new();

    let mut field_nodes = Vec::new();
    collect_kinds(
        body,
        &["public_field_definition", "field_definition"],
        &mut field_nodes,
    );
    for f in field_nodes {
        let field_name_n = f
            .child_by_field_name("name")
            .or_else(|| f.child_by_field_name("property"))
            .or_else(|| first_named(f, "property_identifier"));
        if let Some(field_name_n) = field_name_n {
            let field_name = node_txt(src, field_name_n).trim().to_string();
            fields.push((field_name, Typ::Named("Any".into())));
        }
    }

    let mut method_nodes = Vec::new();
    collect_kinds(body, &["method_definition"], &mut method_nodes);
    for m in method_nodes {
        let is_constructor = m
            .child_by_field_name("name")
            .or_else(|| first_named(m, "property_identifier"))
            .is_some_and(|n| node_txt(src, n).trim() == "constructor");
        if is_constructor && let Some(ctor_fields) = js_ctor_fields(src, m) {
            for (fname, fty) in ctor_fields {
                if !fields.iter().any(|(n, _)| n == &fname) {
                    fields.push((fname, fty));
                }
            }
        }
        if let Some(d) = js_method_decl(src, m) {
            methods.push(d);
        }
    }

    let extends = class_node
        .child_by_field_name("superclass")
        .and_then(|sc| first_named(sc, "identifier"))
        .map(|n| node_txt(src, n).trim().to_string());

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

fn js_method_decl<'a>(src: &[u8], m: Node<'a>) -> Option<Decl> {
    let name_n = m
        .child_by_field_name("name")
        .or_else(|| first_named(m, "property_identifier"))?;
    let name = node_txt(src, name_n).trim().to_string();
    let params = js_formal_params(src, m);
    let mut body = m
        .child_by_field_name("body")
        .map(|b| js_body(src, b))
        .unwrap_or_default();
    rewrite_this_receiver_in_body(&mut body);
    let ret = js_return_type(&body);
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn js_ctor_fields<'a>(src: &[u8], ctor: Node<'a>) -> Option<Vec<(String, Typ)>> {
    let body = ctor.child_by_field_name("body")?;
    let mut fields = Vec::new();
    let mut assigns = Vec::new();
    collect_kinds(body, &["assignment_expression"], &mut assigns);
    for a in assigns {
        let left = a.child(0).or_else(|| a.child_by_field_name("left"));
        if let Some(left_n) = left
            && left_n.kind() == "member_expression"
        {
            let obj = left_n
                .child_by_field_name("object")
                .or_else(|| left_n.child(0));
            if let Some(obj_n) = obj
                && node_txt(src, obj_n).trim() == "this"
                && let Some(prop) = left_n.child_by_field_name("property")
            {
                let field_name = node_txt(src, prop).trim().to_string();
                fields.push((field_name, Typ::Named("Any".into())));
            }
        }
    }
    Some(fields)
}

fn js_var_function<'a>(src: &[u8], vd: Node<'a>) -> Option<Decl> {
    let value = vd.child_by_field_name("value")?;
    if value.kind() != "arrow_function" && value.kind() != "function_expression" {
        return None;
    }
    let name_n = vd.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let params = js_formal_params(src, value);
    let body = value
        .child_by_field_name("body")
        .map(|b| js_body(src, b))
        .unwrap_or_default();
    let ret = js_return_type(&body);
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

pub(super) fn rewrite_this_receiver_in_body(body: &mut [Stmt]) {
    for stmt in body {
        rewrite_this_receiver_in_stmt(stmt);
    }
}

fn rewrite_this_receiver_in_stmt(stmt: &mut Stmt) {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Return(Some(expr))
        | Stmt::Expr(expr)
        | Stmt::Throw(expr) => rewrite_this_receiver_in_expr(expr),
        Stmt::FieldAssign { base, value, .. } => {
            rewrite_this_receiver_in_expr(base);
            rewrite_this_receiver_in_expr(value);
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            rewrite_this_receiver_in_expr(base);
            rewrite_this_receiver_in_expr(index);
            rewrite_this_receiver_in_expr(value);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            rewrite_this_receiver_in_expr(cond);
            rewrite_this_receiver_in_body(then_body);
            rewrite_this_receiver_in_body(else_body);
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(cond) = cond {
                rewrite_this_receiver_in_expr(cond);
            }
            rewrite_this_receiver_in_body(body);
        }
        Stmt::Match { scrutinee, arms } => {
            rewrite_this_receiver_in_expr(scrutinee);
            for arm in arms {
                rewrite_this_receiver_in_body(&mut arm.body);
            }
        }
        Stmt::Try { body, catches } => {
            rewrite_this_receiver_in_body(body);
            for catch in catches {
                rewrite_this_receiver_in_body(&mut catch.body);
            }
        }
        Stmt::Return(None) => {}
        Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
    }
}

fn rewrite_this_receiver_in_expr(expr: &mut Expr) {
    match expr {
        Expr::Ident(name) if name == "this" => {
            *name = "self".to_string();
        }
        Expr::Unary { expr, .. } => rewrite_this_receiver_in_expr(expr),
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_this_receiver_in_expr(lhs);
            rewrite_this_receiver_in_expr(rhs);
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                rewrite_this_receiver_in_expr(expr);
            }
        }
        Expr::Field { base, .. } => rewrite_this_receiver_in_expr(base),
        Expr::ArrayLit(items) => {
            for item in items {
                rewrite_this_receiver_in_expr(item);
            }
        }
        Expr::Index { base, index, .. } => {
            rewrite_this_receiver_in_expr(base);
            rewrite_this_receiver_in_expr(index);
        }
        Expr::Call { callee, args, .. } => {
            rewrite_this_receiver_in_expr(callee);
            for arg in args {
                rewrite_this_receiver_in_expr(arg);
            }
        }
        Expr::Closure { body, .. } => rewrite_this_receiver_in_body(body),
        Expr::Ident(_)
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_) => {}
    }
}

pub(super) fn rewrite_constructor_calls(decls: &mut [Decl]) {
    let class_fields: HashMap<String, Vec<String>> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Class { name, fields, .. } => Some((
                name.clone(),
                fields.iter().map(|(field, _)| field.clone()).collect(),
            )),
            _ => None,
        })
        .collect();
    if class_fields.is_empty() {
        return;
    }
    for decl in decls {
        rewrite_constructor_calls_in_decl(decl, &class_fields);
    }
}

fn rewrite_constructor_calls_in_decl(decl: &mut Decl, class_fields: &HashMap<String, Vec<String>>) {
    match decl {
        Decl::Function { body, .. } => rewrite_constructor_calls_in_body(body, class_fields),
        Decl::Class { methods, .. } => {
            for method in methods {
                rewrite_constructor_calls_in_decl(method, class_fields);
            }
        }
        _ => {}
    }
}

fn rewrite_constructor_calls_in_body(
    body: &mut [Stmt],
    class_fields: &HashMap<String, Vec<String>>,
) {
    for stmt in body {
        rewrite_constructor_calls_in_stmt(stmt, class_fields);
    }
}

fn rewrite_constructor_calls_in_stmt(stmt: &mut Stmt, class_fields: &HashMap<String, Vec<String>>) {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Return(Some(expr))
        | Stmt::Expr(expr)
        | Stmt::Throw(expr) => rewrite_constructor_calls_in_expr(expr, class_fields),
        Stmt::Break | Stmt::Continue => {}
        Stmt::FieldAssign { base, value, .. } => {
            rewrite_constructor_calls_in_expr(base, class_fields);
            rewrite_constructor_calls_in_expr(value, class_fields);
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            rewrite_constructor_calls_in_expr(base, class_fields);
            rewrite_constructor_calls_in_expr(index, class_fields);
            rewrite_constructor_calls_in_expr(value, class_fields);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            rewrite_constructor_calls_in_expr(cond, class_fields);
            rewrite_constructor_calls_in_body(then_body, class_fields);
            rewrite_constructor_calls_in_body(else_body, class_fields);
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(cond) = cond {
                rewrite_constructor_calls_in_expr(cond, class_fields);
            }
            rewrite_constructor_calls_in_body(body, class_fields);
        }
        Stmt::Match { scrutinee, arms } => {
            rewrite_constructor_calls_in_expr(scrutinee, class_fields);
            for arm in arms {
                rewrite_constructor_calls_in_body(&mut arm.body, class_fields);
            }
        }
        Stmt::Try { body, catches } => {
            rewrite_constructor_calls_in_body(body, class_fields);
            for catch in catches {
                rewrite_constructor_calls_in_body(&mut catch.body, class_fields);
            }
        }
        Stmt::Return(None) | Stmt::Propagate => {}
    }
}

fn rewrite_constructor_calls_in_expr(expr: &mut Expr, class_fields: &HashMap<String, Vec<String>>) {
    match expr {
        Expr::Call { callee, args, .. } => {
            rewrite_constructor_calls_in_expr(callee, class_fields);
            for arg in args.iter_mut() {
                rewrite_constructor_calls_in_expr(arg, class_fields);
            }
            if let Expr::Ident(name) = callee.as_ref()
                && let Some(class_name) = name.strip_prefix("__new__")
                && let Some(fields) = class_fields.get(class_name)
            {
                let rendered = fields
                    .iter()
                    .enumerate()
                    .map(|(idx, field)| {
                        (
                            field.clone(),
                            args.get(idx).cloned().unwrap_or(Expr::IntLit(0)),
                        )
                    })
                    .collect();
                *expr = Expr::StructInit {
                    name: class_name.to_string(),
                    fields: rendered,
                };
            }
        }
        Expr::Unary { expr, .. } => rewrite_constructor_calls_in_expr(expr, class_fields),
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_constructor_calls_in_expr(lhs, class_fields);
            rewrite_constructor_calls_in_expr(rhs, class_fields);
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                rewrite_constructor_calls_in_expr(expr, class_fields);
            }
        }
        Expr::Field { base, .. } => rewrite_constructor_calls_in_expr(base, class_fields),
        Expr::ArrayLit(items) => {
            for item in items {
                rewrite_constructor_calls_in_expr(item, class_fields);
            }
        }
        Expr::Index { base, index, .. } => {
            rewrite_constructor_calls_in_expr(base, class_fields);
            rewrite_constructor_calls_in_expr(index, class_fields);
        }
        Expr::Closure { body, .. } => rewrite_constructor_calls_in_body(body, class_fields),
        Expr::Ident(_)
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_) => {}
    }
}

fn js_formal_params<'a>(src: &[u8], fun: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let Some(plist) = fun.child_by_field_name("parameters") else {
        return out;
    };
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if ch.kind() == "required_parameter"
            || ch.kind() == "optional_parameter"
            || ch.kind() == "identifier"
        {
            let id = first_named(ch, "identifier").unwrap_or(ch);
            let name = node_txt(src, id).trim().to_string();
            out.push((name, Typ::Named("Any".into())));
        }
    }
    out
}

fn js_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let params = js_formal_params(src, n);
    let body = n
        .child_by_field_name("body")
        .map(|b| js_body(src, b))
        .unwrap_or_default();
    let ret = js_return_type(&body);
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn js_return_type(body: &[Stmt]) -> Typ {
    if let Some(expr) = find_return_expr(body) {
        return infer_expr_type(expr);
    }
    Typ::Void
}

pub(super) fn js_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, JS_AST)
}
