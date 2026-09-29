use super::extract::{
    AstShape, ast_body, collect_kinds, first_named, named_descendant, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, MethodSig, Stmt, Typ, Visibility};
use tree_sitter::Node;

const JAVA_AST: AstShape = AstShape {
    block_kinds: &["block"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement"],
    local_decl_kinds: &["local_variable_declaration"],
    assignment_kinds: &["assignment_expression"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    call_kinds: &["method_invocation"],
    arg_container_kinds: &["argument_list"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &[
        "decimal_integer_literal",
        "hex_integer_literal",
        "octal_integer_literal",
        "binary_integer_literal",
        "integer_literal",
    ],
    string_kinds: &["string_literal"],
    type_kinds: &[
        "integral_type",
        "floating_point_type",
        "boolean_type",
        "scoped_type_identifier",
        "generic_type",
        "array_type",
        "type_identifier",
    ],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: true,
};

#[cfg_attr(not(feature = "parse-extended"), allow(dead_code))]
pub(super) fn extract_java_style_methods(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut hits = Vec::new();
    collect_kinds(root, &["method_declaration"], &mut hits);
    let mut decls = Vec::new();
    for m in hits {
        if let Some(d) = java_method(src, m) {
            decls.push(d);
        }
    }
    Ok(decls)
}

pub(super) fn extract_java_with_classes(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut class_nodes = Vec::new();
    collect_kinds(root, &["class_declaration"], &mut class_nodes);
    for c in class_nodes {
        if let Some(d) = java_class_decl(src, c) {
            decls.push(d);
        }
    }

    let mut iface_nodes = Vec::new();
    collect_kinds(root, &["interface_declaration"], &mut iface_nodes);
    for i in iface_nodes {
        if let Some(d) = java_interface_decl(src, i) {
            decls.push(d);
        }
    }

    let mut hits = Vec::new();
    collect_kinds(root, &["method_declaration"], &mut hits);
    for m in hits {
        // Class-nested instance methods are captured by their Decl::Class and
        // desugared into `Class_method` functions. Re-emitting them as free
        // functions would duplicate them and leave the copies referencing
        // undeclared fields. Statics (e.g. `main`) stay top-level.
        if nested_in_type_decl(m) && !node_is_static(src, m) {
            continue;
        }
        if let Some(d) = java_method(src, m) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn nested_in_type_decl(mut n: Node<'_>) -> bool {
    while let Some(p) = n.parent() {
        if p.kind() == "class_declaration" || p.kind() == "interface_declaration" {
            return true;
        }
        n = p;
    }
    false
}

fn node_is_static(src: &[u8], node: Node<'_>) -> bool {
    // `modifiers` is a child node, not a named field in tree-sitter-java.
    let mut w = node.walk();
    node.named_children(&mut w)
        .find(|ch| ch.kind() == "modifiers")
        .map(|mods| node_txt(src, mods).contains("static"))
        .unwrap_or(false)
}

fn java_visibility<'a>(src: &[u8], node: Node<'a>) -> Visibility {
    if let Some(mods) = node.child_by_field_name("modifiers") {
        let text = node_txt(src, mods);
        if text.contains("public") {
            return Visibility::Pub;
        }
        if text.contains("private") {
            return Visibility::Private;
        }
        if text.contains("protected") {
            return Visibility::Internal;
        }
    }
    let text = node_txt(src, node);
    if text.contains("public ") || text.starts_with("public") {
        Visibility::Pub
    } else if text.contains("private ") || text.starts_with("private") {
        Visibility::Private
    } else {
        Visibility::Internal
    }
}

fn java_class_decl<'a>(src: &[u8], class_node: Node<'a>) -> Option<Decl> {
    let name_n = class_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let visibility = java_visibility(src, class_node);
    let extends = java_extends(src, class_node);
    let implements = java_implements(src, class_node);
    let (fields, methods) = java_class_body(src, class_node);
    Some(Decl::Class {
        name,
        fields,
        methods,
        visibility,
        extends,
        implements,
        type_params: vec![],
    })
}

fn java_interface_decl<'a>(src: &[u8], iface_node: Node<'a>) -> Option<Decl> {
    let name_n = iface_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let visibility = java_visibility(src, iface_node);
    let methods = java_interface_methods(src, iface_node);
    Some(Decl::Interface {
        name,
        methods,
        visibility,
        type_params: vec![],
    })
}

fn java_extends<'a>(src: &[u8], class_node: Node<'a>) -> Option<String> {
    class_node
        .child_by_field_name("superclass")
        .and_then(|sc| named_descendant(sc, "type_identifier"))
        .map(|n| node_txt(src, n).trim().to_string())
}

fn java_implements<'a>(src: &[u8], class_node: Node<'a>) -> Vec<String> {
    let ifaces = class_node
        .child_by_field_name("super_interfaces")
        .or_else(|| class_node.child_by_field_name("interfaces"));
    let Some(ifaces) = ifaces else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    collect_kinds(ifaces, &["type_identifier"], &mut ids);
    ids.into_iter()
        .map(|n| node_txt(src, n).trim().to_string())
        .collect()
}

fn java_class_body<'a>(src: &[u8], class_node: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let body = class_node
        .child_by_field_name("body")
        .or_else(|| first_named(class_node, "class_body"));
    let Some(body) = body else {
        return (Vec::new(), Vec::new());
    };

    let mut fields = Vec::new();
    let mut field_nodes = Vec::new();
    collect_kinds(body, &["field_declaration"], &mut field_nodes);
    for f in field_nodes {
        let field_type = f
            .child_by_field_name("type")
            .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
            .unwrap_or(Typ::Named("Any".into()));
        let mut declarators = Vec::new();
        collect_kinds(f, &["variable_declarator"], &mut declarators);
        for var in declarators {
            if let Some(name_n) = var
                .child_by_field_name("name")
                .or_else(|| first_named(var, "identifier"))
            {
                let field_name = node_txt(src, name_n).trim().to_string();
                fields.push((field_name, field_type.clone()));
            }
        }
    }

    let mut methods = Vec::new();
    let mut method_nodes = Vec::new();
    collect_kinds(body, &["method_declaration"], &mut method_nodes);
    for m in method_nodes {
        if let Some(d) = java_method(src, m) {
            methods.push(d);
        }
    }

    let mut ctor_nodes = Vec::new();
    collect_kinds(body, &["constructor_declaration"], &mut ctor_nodes);
    for c in ctor_nodes {
        if let Some(d) = java_constructor(src, c) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn java_constructor<'a>(src: &[u8], c: Node<'a>) -> Option<Decl> {
    let fp = named_descendant(c, "formal_parameters")?;
    let parent = fp.parent()?;
    let name_n = parent.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let params = java_formals(src, fp);
    let body = c
        .child_by_field_name("body")
        .map(|b| java_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret: Typ::Void,
        body,
        type_params: vec![],
    })
}

fn java_interface_methods<'a>(src: &[u8], iface_node: Node<'a>) -> Vec<MethodSig> {
    let body = iface_node
        .child_by_field_name("body")
        .or_else(|| first_named(iface_node, "interface_body"));
    let Some(body) = body else {
        return Vec::new();
    };

    let mut sigs = Vec::new();
    let mut hits = Vec::new();
    collect_kinds(
        body,
        &["method_declaration", "abstract_method_declaration"],
        &mut hits,
    );
    for m in hits {
        if let Some(sig) = java_method_sig(src, m) {
            sigs.push(sig);
        }
    }
    sigs
}

fn java_method_sig<'a>(src: &[u8], m: Node<'a>) -> Option<MethodSig> {
    let fp = named_descendant(m, "formal_parameters")?;
    let parent = fp.parent()?;
    let name_n = parent.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let ret = java_ret(src, m);
    let params = java_formals(src, fp);
    Some(MethodSig { name, params, ret })
}

fn java_method<'a>(src: &[u8], m: Node<'a>) -> Option<Decl> {
    let fp = named_descendant(m, "formal_parameters")?;
    let parent = fp.parent()?;
    let name_n = parent.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let ret = java_ret(src, m);
    let params = java_formals(src, fp);
    let body = m
        .child_by_field_name("body")
        .map(|b| java_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn java_ret<'a>(src: &[u8], m: Node<'a>) -> Typ {
    let mut w = m.walk();
    for ch in m.named_children(&mut w) {
        let k = ch.kind();
        if matches!(
            k,
            "void_type"
                | "integral_type"
                | "floating_point_type"
                | "boolean_type"
                | "scoped_type_identifier"
                | "generic_type"
                | "array_type"
                | "type_identifier"
        ) {
            return Typ::Named(node_txt(src, ch).trim().to_string());
        }
    }
    Typ::Named("Unknown".into())
}

fn java_formals<'a>(src: &[u8], fp: Node<'a>) -> Vec<(String, Typ)> {
    let mut params = Vec::new();
    let mut w = fp.walk();
    for ch in fp.named_children(&mut w) {
        if ch.kind() == "formal_parameter" {
            let ty = ch
                .child_by_field_name("type")
                .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                .unwrap_or(Typ::Named("Any".into()));
            let pname = java_param_name(src, ch).unwrap_or_else(|| "arg".into());
            params.push((pname, ty));
        }
    }
    params
}

fn java_param_name<'a>(src: &[u8], fp: Node<'a>) -> Option<String> {
    if let Some(name) = fp.child_by_field_name("name") {
        return Some(node_txt(src, name).trim().to_string());
    }
    let mut ids = Vec::new();
    collect_kinds(fp, &["identifier"], &mut ids);
    let id = ids.into_iter().last()?;
    Some(node_txt(src, id).trim().to_string())
}

fn java_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, JAVA_AST)
}
