use super::extract::{
    AstShape, ast_defined_body, collect_kinds, first_named, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, MethodSig, Stmt, Typ, Visibility};
use tree_sitter::Node;

const CSHARP_AST: AstShape = AstShape {
    block_kinds: &["block"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement"],
    local_decl_kinds: &["local_declaration_statement"],
    assignment_kinds: &["assignment_expression"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    call_kinds: &["invocation_expression"],
    arg_container_kinds: &["argument_list"],
    arg_wrapper_kinds: &["argument"],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression", "prefix_unary_expression"],
    int_kinds: &["integer_literal"],
    string_kinds: &["string_literal"],
    type_kinds: &["predefined_type"],
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

pub(super) fn extract_csharp(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut class_nodes = Vec::new();
    collect_kinds(root, &["class_declaration"], &mut class_nodes);
    for c in class_nodes {
        if let Some(d) = csharp_class_decl(src, c) {
            decls.push(d);
        }
    }

    let mut iface_nodes = Vec::new();
    collect_kinds(root, &["interface_declaration"], &mut iface_nodes);
    for i in iface_nodes {
        if let Some(d) = csharp_interface_decl(src, i) {
            decls.push(d);
        }
    }

    let mut method_nodes = Vec::new();
    collect_kinds(root, &["method_declaration"], &mut method_nodes);
    for n in method_nodes {
        if let Some(d) = csharp_method(src, n) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn csharp_method<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let ret = n
        .child_by_field_name("returns")
        .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
        .unwrap_or(Typ::Void);
    let plist = n.child_by_field_name("parameters")?;
    let params = csharp_params(src, plist);
    let body = n
        .child_by_field_name("body")
        .or_else(|| first_named(n, "block"))
        .map(|b| csharp_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn csharp_class_decl<'a>(src: &[u8], class_node: Node<'a>) -> Option<Decl> {
    let name_n = class_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let extends = class_node
        .child_by_field_name("bases")
        .and_then(|b| first_named(b, "identifier"))
        .or_else(|| {
            class_node
                .child_by_field_name("bases")
                .and_then(|b| first_named(b, "type_identifier"))
        })
        .map(|n| node_txt(src, n).trim().to_string());
    let (fields, methods) = csharp_class_body(src, class_node);
    Some(Decl::Class {
        name,
        fields,
        methods,
        visibility: Visibility::Internal,
        extends,
        implements: vec![],
        type_params: vec![],
    })
}

fn csharp_interface_decl<'a>(src: &[u8], iface_node: Node<'a>) -> Option<Decl> {
    let name_n = iface_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let methods = csharp_iface_methods(src, iface_node);
    Some(Decl::Interface {
        name,
        methods,
        visibility: Visibility::Internal,
        type_params: vec![],
    })
}

fn csharp_class_body<'a>(src: &[u8], class_node: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let body = match class_node.child_by_field_name("body") {
        Some(b) => b,
        None => return (Vec::new(), Vec::new()),
    };

    let mut fields = Vec::new();
    let mut field_nodes = Vec::new();
    collect_kinds(body, &["field_declaration"], &mut field_nodes);
    for f in field_nodes {
        let mut var_decls = Vec::new();
        collect_kinds(f, &["variable_declaration"], &mut var_decls);
        for vd in var_decls {
            let field_type = vd
                .child_by_field_name("type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Named("object".into()));
            let mut var_decs = Vec::new();
            collect_kinds(vd, &["variable_declarator"], &mut var_decs);
            for vdec in var_decs {
                if let Some(name_n) = vdec.child_by_field_name("name") {
                    let field_name = node_txt(src, name_n).trim().to_string();
                    fields.push((field_name, field_type.clone()));
                }
            }
        }
    }

    let mut methods = Vec::new();
    let mut method_nodes = Vec::new();
    collect_kinds(body, &["method_declaration"], &mut method_nodes);
    for m in method_nodes {
        if let Some(d) = csharp_method(src, m) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn csharp_iface_methods<'a>(src: &[u8], iface_node: Node<'a>) -> Vec<MethodSig> {
    let body = match iface_node.child_by_field_name("body") {
        Some(b) => b,
        None => return Vec::new(),
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
            .child_by_field_name("returns")
            .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
            .unwrap_or(Typ::Void);
        let params = match m.child_by_field_name("parameters") {
            Some(plist) => csharp_params(src, plist),
            None => vec![],
        };
        sigs.push(MethodSig { name, params, ret });
    }
    sigs
}

fn csharp_params<'a>(src: &[u8], plist: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if ch.kind() == "parameter" || ch.kind() == "optional_parameter" {
            let ty = ch
                .child_by_field_name("type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Named("Any".into()));
            let pname = ch
                .child_by_field_name("name")
                .map(|id| node_txt(src, id).trim().to_string())
                .unwrap_or_else(|| format!("arg{}", out.len()));
            out.push((pname, ty));
        }
    }
    out
}

fn csharp_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_defined_body(src, body, CSHARP_AST)
}
