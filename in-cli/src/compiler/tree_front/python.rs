use super::extract::{
    AstShape, ast_body, ast_expr, collect_kinds, first_named, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Stmt, Typ, Visibility};
use tree_sitter::Node;

const PYTHON_AST: AstShape = AstShape {
    block_kinds: &["block"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement"],
    local_decl_kinds: &[],
    assignment_kinds: &["assignment"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    call_kinds: &["call"],
    arg_container_kinds: &["argument_list"],
    arg_wrapper_kinds: &[],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_operator", "comparison_operator"],
    unary_kinds: &["unary_operator"],
    int_kinds: &["integer"],
    string_kinds: &["string"],
    type_kinds: &[],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &["try_statement"],
    catch_kinds: &["except_clause"],
    match_kinds: &[],
    first_assignment_is_let: true,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
    number_decoder: None,
};

pub(super) fn extract_python_with_classes(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut class_nodes = Vec::new();
    collect_kinds(root, &["class_definition"], &mut class_nodes);
    for c in class_nodes {
        if let Some(d) = python_class_decl(src, c) {
            decls.push(d);
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_definition"], &mut func_nodes);
    for f in func_nodes {
        let is_class_method = f.parent().is_some_and(|p| p.kind() == "block")
            && f.parent()
                .and_then(|p| p.parent())
                .is_some_and(|gp| gp.kind() == "class_definition");
        if !is_class_method && let Some(d) = python_function_decl(src, f) {
            decls.push(d);
        }
    }

    let mut lambda_nodes = Vec::new();
    collect_kinds(root, &["lambda"], &mut lambda_nodes);
    for l in lambda_nodes {
        if let Some(parent) = l.parent()
            && parent.kind() == "assignment"
        {
            let left = parent
                .child_by_field_name("left")
                .or_else(|| parent.named_child(0));
            if let Some(left_n) = left
                && left_n.kind() == "identifier"
            {
                let name = normalize_entry(node_txt(src, left_n).trim());
                let params = python_lambda_params(src, l);
                let ret = Typ::Void;
                let body_expr = l.named_child(l.named_child_count().saturating_sub(1) as u32);
                let body = body_expr
                    .and_then(|b| ast_expr(src, b, PYTHON_AST))
                    .map(|e| vec![Stmt::Return(Some(e))])
                    .unwrap_or_default();
                decls.push(Decl::Function {
                    name,
                    params,
                    ret,
                    body,
                    type_params: vec![],
                });
            }
        }
    }

    Ok(decls)
}

fn python_class_decl<'a>(src: &[u8], class_node: Node<'a>) -> Option<Decl> {
    let name_n = class_node.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let body = class_node.child_by_field_name("body")?;

    let mut fields = Vec::new();
    let mut methods = Vec::new();
    let mut init_body: Option<Node> = None;

    let mut body_w = body.walk();
    for ch in body.named_children(&mut body_w) {
        if ch.kind() == "function_definition"
            && let Some(d) = python_function_decl(src, ch)
        {
            if let Decl::Function { name: fn_name, .. } = &d
                && fn_name == "__init__"
            {
                init_body = ch
                    .child_by_field_name("body")
                    .or_else(|| first_named(ch, "block"));
            }
            methods.push(d);
        }
    }

    if let Some(init) = init_body {
        let mut assigns = Vec::new();
        collect_kinds(init, &["expression_statement"], &mut assigns);
        for es in assigns {
            let mut ew = es.walk();
            if let Some(assign) = es.named_children(&mut ew).next()
                && assign.kind() == "assignment"
            {
                let left = assign
                    .child_by_field_name("left")
                    .or_else(|| assign.named_child(0));
                if let Some(left_n) = left
                    && left_n.kind() == "attribute"
                    && let Some(obj) = left_n.child_by_field_name("object")
                    && node_txt(src, obj).trim() == "self"
                    && let Some(attr) = left_n.child_by_field_name("attribute")
                {
                    let field_name = node_txt(src, attr).trim().to_string();
                    fields.push((field_name, Typ::Named("Any".into())));
                }
            }
        }
    }

    Some(Decl::Class {
        name,
        fields,
        methods,
        visibility: Visibility::Pub,
        extends: None,
        implements: vec![],
        type_params: vec![],
    })
}

fn python_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let ret = n
        .child_by_field_name("return_type")
        .or_else(|| {
            let params = n.child_by_field_name("parameters")?;
            let mut seen_params = false;
            let mut w = n.walk();
            for ch in n.named_children(&mut w) {
                if ch == params {
                    seen_params = true;
                    continue;
                }
                if seen_params && ch.kind() == "type" {
                    return Some(ch);
                }
                if ch.kind() == "block" {
                    break;
                }
            }
            None
        })
        .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
        .unwrap_or(Typ::Void);
    let plist = n.child_by_field_name("parameters")?;
    let mut params = simple_param_names(src, plist);
    if params.first().is_some_and(|(name, _)| name == "self") {
        params.remove(0);
    }
    let body = n
        .child_by_field_name("body")
        .or_else(|| first_named(n, "block"))
        .map(|b| python_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret,
        body,
        type_params: vec![],
    })
}

fn python_lambda_params<'a>(src: &[u8], lambda_node: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    if let Some(params) = first_named(lambda_node, "lambda_parameters") {
        let mut w = params.walk();
        for ch in params.named_children(&mut w) {
            if ch.kind() == "identifier" {
                out.push((
                    node_txt(src, ch).trim().to_string(),
                    Typ::Named("Any".into()),
                ));
            }
        }
    }
    out
}

fn simple_param_names<'a>(src: &[u8], plist: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if matches!(
            ch.kind(),
            "identifier" | "typed_parameter" | "typed_default_parameter"
        ) {
            let (name, ty) = if ch.kind() == "identifier" {
                (
                    node_txt(src, ch).trim().to_string(),
                    Typ::Named("Any".into()),
                )
            } else {
                let id = first_named(ch, "identifier").unwrap_or(ch);
                let nm = node_txt(src, id).trim().to_string();
                let ty = ch
                    .child_by_field_name("type")
                    .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
                    .unwrap_or(Typ::Named("Any".into()));
                (nm, ty)
            };
            out.push((name, ty));
        }
    }
    out
}

fn python_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, PYTHON_AST)
}
