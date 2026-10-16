use super::extract::{AstShape, ast_body, collect_kinds, first_named, node_txt, normalize_entry};
use crate::core_ir::{Decl, Typ};
use tree_sitter::Node;

const LOLCATAST: AstShape = AstShape {
    block_kinds: &["source_file", "function_declaration", "if_statement"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement", "print_statement"],
    local_decl_kinds: &["variable_declaration"],
    assignment_kinds: &["assignment"],
    if_kinds: &["if_statement"],
    while_kinds: &["loop_statement"],
    call_kinds: &["function_call"],
    arg_container_kinds: &["argument"],
    arg_wrapper_kinds: &["_expression"],
    paren_kinds: &[],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["number"],
    string_kinds: &["string"],
    type_kinds: &[],
    local_decl_prefixes: &["I HAS A "],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
    number_decoder: None,
};

pub(super) fn extract_lolcat(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_declaration"], &mut func_nodes);
    for n in func_nodes {
        if let Some(d) = lolcat_function_decl(src, n) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn lolcat_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let params = lolcat_params(src, n);
    let body = ast_body(src, n, LOLCATAST);
    Some(Decl::Function {
        name,
        params,
        ret: Typ::Named("dynamic".into()),
        body,
        type_params: vec![],
    })
}

fn lolcat_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
    let mut params = Vec::new();
    let mut param_nodes = Vec::new();
    collect_kinds(n, &["parameter"], &mut param_nodes);
    for p in param_nodes {
        let name_n = p
            .child_by_field_name("name")
            .or_else(|| first_named(p, "identifier"));
        if let Some(name_n) = name_n {
            let pname = normalize_entry(node_txt(src, name_n).trim());
            params.push((pname, Typ::Named("dynamic".into())));
        }
    }
    params
}
