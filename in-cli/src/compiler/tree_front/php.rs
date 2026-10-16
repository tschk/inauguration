use super::extract::{
    AstShape, ast_body, ast_expr, collect_kinds, first_named, named_descendant, node_txt,
    normalize_entry, simple_bounded_expr,
};
use crate::core_ir::{Decl, MethodSig, Stmt, Typ, Visibility};
use tree_sitter::Node;

const PHPAST: AstShape = AstShape {
    block_kinds: &["compound_statement"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement", "echo_statement"],
    local_decl_kinds: &[],
    assignment_kinds: &["assignment_expression"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    call_kinds: &["function_call_expression"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &["argument", "expression", "primary_expression"],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_op_expression"],
    int_kinds: &["integer"],
    string_kinds: &["string"],
    type_kinds: &["named_type"],
    local_decl_prefixes: &[],
    shell_first_kinds: &["statement"],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
};

pub(super) fn extract_php(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut class_nodes = Vec::new();
    collect_kinds(root, &["class_declaration"], &mut class_nodes);
    for c in class_nodes {
        if let Some(d) = php_class_decl(src, c) {
            decls.push(d);
        }
    }

    let mut iface_nodes = Vec::new();
    collect_kinds(root, &["interface_declaration"], &mut iface_nodes);
    for i in iface_nodes {
        if let Some(d) = php_interface_decl(src, i) {
            decls.push(d);
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_definition"], &mut func_nodes);
    for f in func_nodes {
        let is_class_method = f.parent().is_some_and(|p| {
            let pk = p.kind();
            pk == "declaration_list" || pk == "class_declaration" || pk == "interface_declaration"
        });
        if !is_class_method && let Some(d) = php_function_decl(src, f) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn php_class_decl<'a>(src: &[u8], class_node: Node<'a>) -> Option<Decl> {
    let name_n = class_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let (fields, methods) = php_class_body(src, class_node);
    let extends = class_node
        .child_by_field_name("parent")
        .or_else(|| {
            let mut bases = Vec::new();
            collect_kinds(class_node, &["base_clause"], &mut bases);
            bases.into_iter().next()
        })
        .and_then(|b| first_named(b, "name"))
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

fn php_interface_decl<'a>(src: &[u8], iface_node: Node<'a>) -> Option<Decl> {
    let name_n = iface_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let methods = php_interface_methods(src, iface_node);
    Some(Decl::Interface {
        name,
        methods,
        visibility: Visibility::Pub,
        type_params: vec![],
    })
}

fn php_class_body<'a>(src: &[u8], class_node: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let body = class_node.child_by_field_name("body");
    let Some(body) = body else {
        return (Vec::new(), Vec::new());
    };

    let mut fields = Vec::new();
    let mut field_nodes = Vec::new();
    collect_kinds(body, &["property_declaration"], &mut field_nodes);
    for f in field_nodes {
        let mut var_items = Vec::new();
        collect_kinds(f, &["property_element"], &mut var_items);
        if var_items.is_empty() {
            collect_kinds(f, &["variable_name"], &mut var_items);
        }
        for v in var_items {
            let field_name = v
                .child_by_field_name("name")
                .map(|n| node_txt(src, n).trim().trim_start_matches('$').to_string())
                .unwrap_or_else(|| node_txt(src, v).trim().trim_start_matches('$').to_string());
            let field_type = f
                .child_by_field_name("type")
                .or_else(|| first_named(f, "named_type"))
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Named("Any".into()));
            fields.push((field_name, field_type));
        }
    }

    let mut methods = Vec::new();
    let mut method_nodes = Vec::new();
    collect_kinds(body, &["method_declaration"], &mut method_nodes);
    for m in method_nodes {
        if let Some(d) = php_method_decl(src, m) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn php_interface_methods<'a>(src: &[u8], iface_node: Node<'a>) -> Vec<MethodSig> {
    let body = iface_node.child_by_field_name("body");
    let Some(body) = body else {
        return Vec::new();
    };

    let mut sigs = Vec::new();
    let mut hits = Vec::new();
    collect_kinds(body, &["method_declaration"], &mut hits);
    for m in hits {
        let name_n = match m.child_by_field_name("name") {
            Some(n) => n,
            None => continue,
        };
        let name = node_txt(src, name_n).trim().to_string();
        let ret = m
            .child_by_field_name("return_type")
            .or_else(|| first_named(m, "named_type"))
            .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
            .unwrap_or(Typ::Void);
        let params = match m.child_by_field_name("parameters") {
            Some(plist) => php_params(src, plist),
            None => vec![],
        };
        sigs.push(MethodSig { name, params, ret });
    }
    sigs
}

fn php_method_decl<'a>(src: &[u8], m: Node<'a>) -> Option<Decl> {
    let name_n = m.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let ret = m
        .child_by_field_name("return_type")
        .or_else(|| first_named(m, "named_type"))
        .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
        .unwrap_or(Typ::Void);
    let params = match m.child_by_field_name("parameters") {
        Some(plist) => php_params(src, plist),
        None => vec![],
    };
    let body = m
        .child_by_field_name("body")
        .map(|b| php_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn php_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let ret = n
        .child_by_field_name("return_type")
        .or_else(|| first_named(n, "named_type"))
        .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
        .unwrap_or(Typ::Void);
    let params = match n.child_by_field_name("parameters") {
        Some(plist) => php_params(src, plist),
        None => vec![],
    };
    let body = n
        .child_by_field_name("body")
        .map(|b| php_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn php_params<'a>(src: &[u8], plist: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if ch.kind().contains("parameter") && ch.kind() != "variadic_parameter" {
            let ty = named_descendant(ch, "type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Named("Any".into()));
            let pname = named_descendant(ch, "variable_name")
                .or_else(|| named_descendant(ch, "name"))
                .map(|v| node_txt(src, v).trim().trim_start_matches('$').to_string())
                .unwrap_or_else(|| format!("arg{}", out.len()));
            out.push((pname, ty));
        }
    }
    out
}

fn php_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    let stmts = ast_body(src, body, PHPAST);
    if !stmts.is_empty() {
        return stmts;
    }
    let mut out = Vec::new();
    for raw in node_txt(src, body).lines() {
        let mut line = raw.trim();
        if line.is_empty() || line == "{" || line == "}" || line == "->" {
            continue;
        }
        line = line.trim_end_matches(';').trim();
        if let Some(expr) = line.strip_prefix("return ").and_then(simple_bounded_expr) {
            out.push(Stmt::Return(Some(expr)));
            continue;
        }
        if line == "return" {
            out.push(Stmt::Return(None));
            continue;
        }
        if let Some((lhs, rhs)) = line.split_once(" = ")
            && let Some(expr) = simple_bounded_expr(rhs.trim())
        {
            out.push(Stmt::Assign(
                lhs.trim().trim_start_matches('$').to_string(),
                expr,
            ));
            continue;
        }
        if let Some(expr) = simple_bounded_expr(line.trim_start_matches('$')) {
            out.push(Stmt::Expr(expr));
        }
    }
    if out.is_empty() {
        let mut stack = vec![body];
        while let Some(node) = stack.pop() {
            if node.kind() == "print_intrinsic"
                && let Some(expr) = ast_expr(src, node, PHPAST)
            {
                out.push(Stmt::Expr(expr));
                continue;
            }
            let mut w = node.walk();
            for ch in node.named_children(&mut w) {
                stack.push(ch);
            }
        }
        out.reverse();
    }
    out
}
