use super::extract::{
    AstShape, ast_expr, ast_return_expr, collect_kinds, decl_fn, extract_fn_nodes, first_named,
    node_txt, normalize_entry, simple_bounded_body,
};
use crate::core_ir::{Decl, Stmt, Typ, Visibility};
use tree_sitter::Node;

const PERLAST: AstShape = AstShape {
    block_kinds: &["block"],
    return_kinds: &["return_expression"],
    expr_stmt_kinds: &[],
    local_decl_kinds: &[],
    assignment_kinds: &[],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement"],
    // ponytail: tree-sitter-perl 1.1.2 merged call/expr into binary_expression;
    // call extraction handled in perl-specific code
    call_kinds: &[
        "call_expression_with_spaced_args",
        "call_expression_with_bareword",
        "call_expression_with_args_with_brackets",
    ],
    arg_container_kinds: &["arguments", "array"],
    arg_wrapper_kinds: &[],
    paren_kinds: &[],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["integer"],
    string_kinds: &["string_double_quoted"],
    type_kinds: &[],
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
};

pub(super) fn extract_perl(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut pkg_nodes = Vec::new();
    collect_kinds(root, &["package_statement"], &mut pkg_nodes);
    for pkg in pkg_nodes {
        if let Some(d) = perl_package_decl(src, pkg) {
            decls.push(d);
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_definition"], &mut func_nodes);
    for f in func_nodes {
        let is_class_method = f.parent().is_some_and(|p| p.kind() == "package_statement");
        if !is_class_method && let Some(d) = perl_function_decl(src, f) {
            decls.push(d);
        }
    }

    if decls.is_empty() {
        extract_fn_nodes(src, root, &["function_definition"], |src, n| {
            let name_n = n.child_by_field_name("name")?;
            let name = normalize_entry(node_txt(src, name_n).trim());
            Some(decl_fn(name, vec![], Typ::Void))
        })
    } else {
        Ok(decls)
    }
}

fn perl_package_decl<'a>(src: &[u8], pkg: Node<'a>) -> Option<Decl> {
    let name_n = pkg.child_by_field_name("name")?;
    let name = node_txt(src, name_n).trim().to_string();
    let (fields, methods) = perl_package_body(src, pkg);
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

fn perl_package_body<'a>(src: &[u8], pkg: Node<'a>) -> (Vec<(String, Typ)>, Vec<Decl>) {
    let mut fields = Vec::new();
    let mut methods = Vec::new();

    let mut my_nodes = Vec::new();
    collect_kinds(pkg, &["variable_declaration"], &mut my_nodes);
    for my_stmt in my_nodes {
        let mut vars = Vec::new();
        collect_kinds(
            my_stmt,
            &["scalar_variable", "array", "hash", "identifier"],
            &mut vars,
        );
        for v in vars {
            let field_name = node_txt(src, v)
                .trim()
                .trim_start_matches(['$', '@', '%'])
                .to_string();
            if !field_name.is_empty() {
                fields.push((field_name, Typ::Named("Any".into())));
            }
        }
    }

    let mut func_nodes = Vec::new();
    collect_kinds(pkg, &["function_definition"], &mut func_nodes);
    for f in func_nodes {
        if let Some(d) = perl_function_decl(src, f) {
            methods.push(d);
        }
    }

    (fields, methods)
}

fn perl_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let params = perl_params(src, n);
    let body = n
        .child_by_field_name("body")
        .or_else(|| first_named(n, "block"))
        .map(|b| perl_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret: Typ::Void,
        body,
        type_params: vec![],
    })
}

fn perl_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let sig = n.child_by_field_name("signature");
    if let Some(sig) = sig {
        let mut w = sig.walk();
        for ch in sig.named_children(&mut w) {
            let raw = node_txt(src, ch).trim();
            let clean = raw.trim_start_matches(['$', '@', '%']);
            if !clean.is_empty() && ch.kind() != "{" {
                out.push((clean.to_string(), Typ::Named("Any".into())));
            }
        }
    }
    if out.is_empty() {
        let mut body_node = n.child_by_field_name("body");
        if body_node.is_none() {
            body_node = first_named(n, "block");
        }
        if let Some(b) = body_node {
            let mut w = b.walk();
            for ch in b.named_children(&mut w) {
                if ch.kind() == "variable_declaration" {
                    let mut vars = Vec::new();
                    collect_kinds(
                        ch,
                        &["scalar_variable", "array", "hash", "identifier"],
                        &mut vars,
                    );
                    for v in vars {
                        let clean = node_txt(src, v)
                            .trim()
                            .trim_start_matches(['$', '@', '%'])
                            .to_string();
                        if !clean.is_empty() {
                            out.push((clean, Typ::Named("Any".into())));
                        }
                    }
                }
            }
        }
    }
    out
}

fn perl_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    // ponytail: tree-sitter-perl 1.1.2 uses binary_expression for both
    // assignments and binary ops, and variable_declaration for my/our.
    // The generic ast_body can't distinguish them, so we handle manually.
    let mut out = Vec::new();
    let mut w = body.walk();
    for ch in body.named_children(&mut w) {
        match ch.kind() {
            "return_expression" => {
                out.push(Stmt::Return(
                    ast_return_expr(src, ch, PERLAST).unwrap_or(None),
                ));
            }
            "call_expression_with_spaced_args" | "call_expression_with_bareword" => {
                if let Some(expr) = ast_expr(src, ch, PERLAST) {
                    out.push(Stmt::Expr(expr));
                }
            }
            "if_statement" | "while_statement" => {
                let kind = ch.kind();
                // condition is wrapped in array child
                let mut cw = ch.walk();
                let cond = ch
                    .named_children(&mut cw)
                    .find(|c| c.kind() == "array")
                    .and_then(|a| a.named_child(0))
                    .and_then(|ex| ast_expr(src, ex, PERLAST));
                let mut cw2 = ch.walk();
                let blocks: Vec<Node<'_>> = ch
                    .named_children(&mut cw2)
                    .filter(|c| c.kind() == "block")
                    .collect();
                if kind == "if_statement" {
                    if let Some(cond) = cond {
                        let then_body = blocks
                            .first()
                            .map(|b| perl_body(src, *b))
                            .unwrap_or_default();
                        let else_body = if blocks.len() > 1 {
                            perl_body(src, blocks[1])
                        } else if let Some(ec) = ch
                            .named_children(&mut ch.walk())
                            .find(|c| c.kind() == "else_clause")
                        {
                            let mut ew = ec.walk();
                            ec.named_children(&mut ew)
                                .find(|b| b.kind() == "block")
                                .map(|b| perl_body(src, b))
                                .unwrap_or_default()
                        } else {
                            vec![]
                        };
                        out.push(Stmt::If {
                            cond,
                            then_body,
                            else_body,
                        });
                    }
                } else {
                    if let Some(cond) = cond {
                        let body = blocks
                            .first()
                            .map(|b| perl_body(src, *b))
                            .unwrap_or_default();
                        out.push(Stmt::Loop {
                            kind: crate::core_ir::LoopKind::While,
                            cond: Some(cond),
                            body,
                        });
                    }
                }
            }
            "binary_expression" => {
                // ponytail: Perl binary_expression has no field names;
                // use positional children: left=named_child(0), right=named_child(1)
                let left = ch.named_child(0);
                let right = ch.named_child(1);
                if let Some(l) = left {
                    match l.kind() {
                        "variable_declaration" => {
                            // my $x = 42
                            let name = l
                                .named_children(&mut l.walk())
                                .find(|c| c.kind() == "scalar_variable")
                                .map(|v| {
                                    node_txt(src, v).trim().trim_start_matches('$').to_string()
                                });
                            if let Some(n) = name {
                                if let Some(r) = right {
                                    if let Some(e) = ast_expr(src, r, PERLAST) {
                                        out.push(Stmt::Let(n, None, e));
                                    }
                                }
                            }
                        }
                        "scalar_variable" => {
                            // $x = 42  (assignment)
                            let name = node_txt(src, l).trim().trim_start_matches('$').to_string();
                            if let Some(r) = right {
                                if let Some(e) = ast_expr(src, r, PERLAST) {
                                    out.push(Stmt::Assign(name, e));
                                }
                            }
                        }
                        _ => {
                            // regular binary expression: 2 + 3 * 4
                            if let Some(e) = ast_expr(src, ch, PERLAST) {
                                out.push(Stmt::Expr(e));
                            }
                        }
                    }
                } else if let Some(e) = ast_expr(src, ch, PERLAST) {
                    out.push(Stmt::Expr(e));
                }
            }
            _ => {
                if let Some(e) = ast_expr(src, ch, PERLAST) {
                    out.push(Stmt::Expr(e));
                }
            }
        }
    }
    if !out.is_empty() {
        return out;
    }
    simple_bounded_body(node_txt(src, body), "=").unwrap_or_default()
}
