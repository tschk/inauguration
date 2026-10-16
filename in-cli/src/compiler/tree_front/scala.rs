use super::extract::{
    AstShape, ast_body, collect_kinds, first_named, named_descendant, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, MethodSig, Stmt, Typ, Visibility};
use tree_sitter::Node;

const SCALAAST: AstShape = AstShape {
    block_kinds: &["block", "indented_block"],
    return_kinds: &["return_expression"],
    expr_stmt_kinds: &[],
    local_decl_kinds: &[
        "val_definition",
        "var_definition",
        "val_declaration",
        "var_declaration",
    ],
    assignment_kinds: &["assignment_expression"],
    if_kinds: &["if_expression"],
    while_kinds: &["while_expression", "for_expression"],
    call_kinds: &["call_expression"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["infix_expression", "binary_expression"],
    unary_kinds: &["prefix_expression", "unary_expression"],
    int_kinds: &["integer_literal"],
    string_kinds: &["string"],
    type_kinds: &[
        "generic_type",
        "projected_type",
        "type_definition",
        "compound_type",
        "identifier",
    ],
    local_decl_prefixes: &[],
    shell_first_kinds: &["block_expression", "_definition", "expression"],
    shell_last_kinds: &[],
    try_kinds: &["try_expression"],
    catch_kinds: &["catch_clause"],
    match_kinds: &["match_expression"],
    first_assignment_is_let: false,
    strict_args: false,
    string_decoder: None,
};

pub(super) fn extract_scala(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut class_nodes = Vec::new();
    collect_kinds(root, &["class_definition"], &mut class_nodes);
    for c in class_nodes {
        if let Some(d) = scala_class_decl(src, c) {
            decls.push(d);
        }
    }

    let mut object_nodes = Vec::new();
    collect_kinds(root, &["object_definition"], &mut object_nodes);
    for o in object_nodes {
        if let Some(d) = scala_class_decl(src, o) {
            decls.push(d);
        }
    }

    let mut trait_nodes = Vec::new();
    collect_kinds(root, &["trait_definition"], &mut trait_nodes);
    for t in trait_nodes {
        if let Some(d) = scala_trait_decl(src, t) {
            decls.push(d);
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_definition"], &mut func_nodes);
    for f in func_nodes {
        let is_class_method = f.parent().is_some_and(|p| p.kind() == "template_body");
        if !is_class_method && let Some(d) = scala_function_decl(src, f) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn scala_class_decl<'a>(src: &[u8], class_node: Node<'a>) -> Option<Decl> {
    let name_n = class_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let extends = scala_extends(src, class_node);
    let (fields, methods) = scala_class_body(src, class_node);
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

fn scala_trait_decl<'a>(src: &[u8], trait_node: Node<'a>) -> Option<Decl> {
    let name_n = trait_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let methods = scala_trait_methods(src, trait_node);
    Some(Decl::Interface {
        name,
        methods,
        visibility: Visibility::Pub,
        type_params: vec![],
    })
}

fn scala_extends<'a>(src: &[u8], class_node: Node<'a>) -> Option<String> {
    class_node
        .child_by_field_name("extend")
        .map(|n| node_txt(src, n).trim().to_string())
}

fn scala_class_body<'a>(src: &[u8], class_node: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let body = class_node
        .child_by_field_name("body")
        .or_else(|| first_named(class_node, "template_body"));
    let Some(body) = body else {
        return (Vec::new(), Vec::new());
    };

    let mut fields = Vec::new();

    let mut ctor_params = Vec::new();
    collect_kinds(class_node, &["class_parameters"], &mut ctor_params);
    for cp in ctor_params {
        let mut params = Vec::new();
        collect_kinds(cp, &["class_parameter"], &mut params);
        for p in params {
            let pname = p
                .child_by_field_name("name")
                .or_else(|| first_named(p, "identifier"))
                .map(|id| node_txt(src, id).trim().to_string());
            if let Some(pname) = pname {
                let ptype = scala_field_type(src, p);
                fields.push((pname, ptype));
            }
        }
    }

    let mut val_nodes = Vec::new();
    collect_kinds(body, &["val_definition", "var_definition"], &mut val_nodes);
    for v in val_nodes {
        let field_name = v
            .child_by_field_name("pattern")
            .and_then(|p| first_named(p, "identifier"))
            .map(|id| node_txt(src, id).trim().to_string());
        if let Some(field_name) = field_name {
            let field_type = scala_field_type(src, v);
            fields.push((field_name, field_type));
        }
    }

    let mut method_nodes = Vec::new();
    collect_kinds(body, &["function_definition"], &mut method_nodes);
    let mut methods = Vec::new();
    for m in method_nodes {
        if let Some(d) = scala_function_decl(src, m) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn scala_field_type<'a>(src: &[u8], node: Node<'a>) -> Typ {
    node.child_by_field_name("type")
        .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
        .unwrap_or(Typ::Named("Any".into()))
}

fn scala_trait_methods<'a>(src: &[u8], trait_node: Node<'a>) -> Vec<MethodSig> {
    let body = trait_node
        .child_by_field_name("body")
        .or_else(|| first_named(trait_node, "template_body"));
    let Some(body) = body else {
        return Vec::new();
    };

    let mut sigs = Vec::new();
    let mut hits = Vec::new();
    collect_kinds(body, &["function_declaration"], &mut hits);
    for m in hits {
        if let Some(sig) = scala_method_sig(src, m) {
            sigs.push(sig);
        }
    }
    sigs
}

fn scala_method_sig<'a>(src: &[u8], m: Node<'a>) -> Option<MethodSig> {
    let name_n = m.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let ret = m
        .child_by_field_name("return_type")
        .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
        .unwrap_or(Typ::Named("Unit".into()));
    let params = scala_params(src, m);
    Some(MethodSig { name, params, ret })
}

fn scala_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = find_field_deep(n, "name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let params = scala_params(src, n);
    let ret = n
        .child_by_field_name("return_type")
        .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
        .unwrap_or(Typ::Named("Unit".into()));
    let body = n
        .child_by_field_name("body")
        .map(|b| scala_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn scala_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let plist = n
        .child_by_field_name("parameters")
        .or_else(|| named_descendant(n, "parameters"));
    let Some(plist) = plist else {
        return out;
    };
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        let pk = ch.kind();
        if pk == "parameter" || pk.contains("parameter") {
            let pname = ch
                .child_by_field_name("name")
                .or_else(|| first_named(ch, "identifier"))
                .map(|id| node_txt(src, id).trim().to_string())
                .unwrap_or_else(|| format!("arg{}", out.len()));
            let ptype = ch
                .child_by_field_name("type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Named("Any".into()));
            out.push((pname, ptype));
        }
    }
    out
}

fn scala_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, SCALAAST)
}

fn find_field_deep<'a>(n: Node<'a>, field: &str) -> Option<Node<'a>> {
    if let Some(c) = n.child_by_field_name(field) {
        return Some(c);
    }
    let mut w = n.walk();
    for ch in n.named_children(&mut w) {
        if let Some(r) = find_field_deep(ch, field) {
            return Some(r);
        }
    }
    None
}
