use super::extract::{
    AstShape, ast_expr, ast_stmt, collect_kinds, declared_return_type, first_named, node_txt,
};
use crate::core_ir::{Decl, Expr, Stmt, Typ};
use tree_sitter::Node;

const NIM_AST: AstShape = AstShape {
    block_kinds: &["source_file", "body"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement", "echo_statement"],
    local_decl_kinds: &["local_decl"],
    assignment_kinds: &["assignment"],
    if_kinds: &[],
    while_kinds: &[],
    call_kinds: &["call"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &[],
    paren_kinds: &[],
    binary_kinds: &["binary_expression"],
    unary_kinds: &[],
    int_kinds: &["int"],
    string_kinds: &["string"],
    type_kinds: &["type"],
    local_decl_prefixes: &["let ", "var ", "const "],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: Some(nim_binary_precedence),
    string_decoder: Some(nim_string_literal),
    number_decoder: None,
};

/// Nim identifiers ignore underscores and ASCII case after the first
/// character. Non-ASCII spelling is byte-exact; private compiler names are not
/// source identifiers and must retain their reserved identity.
fn nim_identifier(raw: &str) -> String {
    if raw.starts_with('#') {
        return raw.into();
    }
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut result = String::with_capacity(raw.len());
    result.push(first);
    result.extend(
        chars
            .filter(|ch| *ch != '_')
            .map(|ch| ch.to_ascii_lowercase()),
    );
    result
}
fn nim_type(typ: Typ) -> Typ {
    match typ {
        Typ::Named(name) => Typ::Named(nim_identifier(&name)),
        other => other,
    }
}
fn nim_expression_identifiers(value: &mut Expr) {
    if let Expr::Ident(name) = value {
        *name = nim_identifier(name);
    }
    crate::core_ir::for_each_expr_child_mut(value, &mut nim_expression_identifiers);
}
fn nim_expr(src: &[u8], node: Node<'_>) -> Option<Expr> {
    let mut value = ast_expr(src, node, NIM_AST)?;
    nim_expression_identifiers(&mut value);
    Some(value)
}

pub(super) fn extract_nim(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut func_nodes = Vec::new();
    collect_kinds(
        root,
        &["proc_declaration", "func_declaration"],
        &mut func_nodes,
    );
    for n in &func_nodes {
        if let Some(d) = nim_proc_decl(src, *n) {
            decls.push(d);
        }
    }

    if decls.len() != func_nodes.len() {
        return Err("Nim function declaration is missing a name".into());
    }
    let mut functions = std::collections::HashSet::new();
    for declaration in &decls {
        if let Decl::Function { name, params, .. } = declaration {
            if !functions.insert(name) {
                return Err(format!("Nim function style collision `{name}`"));
            }
            let mut names = std::collections::HashSet::new();
            for (name, _) in params {
                if !names.insert(name) {
                    return Err(format!("Nim parameter style collision `{name}`"));
                }
            }
        }
    }
    // The installed grammar ignores indentation and has no unary expression
    // nodes. Never reinterpret a next-line unary statement as a binary RHS.
    for (declaration, node) in decls.iter_mut().zip(&func_nodes) {
        if node
            .child_by_field_name("body")
            .is_some_and(nim_binary_starts_new_statement)
        {
            if let Decl::Function { body, .. } = declaration {
                *body = vec![Stmt::Expr(Expr::Ident(
                    "#unsupported-nim-binary-statement-boundary".into(),
                ))];
            }
        }
    }
    let checker = crate::typecheck::TypeChecker::new();
    for (index, node) in func_nodes.iter().copied().enumerate() {
        if node
            .child_by_field_name("body")
            .is_some_and(nim_binary_starts_new_statement)
        {
            continue;
        }
        let facts = checker.collect_decl_facts(&decls[..=index]);
        if let Decl::Function { params, ret, .. } = &mut decls[index]
            && matches!(ret,Typ::Named(name) if name=="auto")
            && let Some(inferred) = nim_auto_return(src, node, params, &facts)
        {
            *ret = inferred;
        }
    }
    for (index, node) in func_nodes.into_iter().enumerate() {
        if node
            .child_by_field_name("body")
            .is_some_and(nim_binary_starts_new_statement)
        {
            continue;
        }
        let facts = checker.collect_decl_facts(&decls[..=index]);
        if let Decl::Function {
            params, ret, body, ..
        } = &mut decls[index]
        {
            *body = nim_proc_body(src, node, params, ret, &facts);
        }
    }
    if root.has_error() {
        // Keep declaration identity for inspection, but never execute a partial
        // grammar tree (including unsupported explicit grouping syntax).
        for declaration in &mut decls {
            if let Decl::Function { body, .. } = declaration {
                *body = vec![Stmt::Expr(Expr::Ident(
                    "#unsupported-nim-grammar-syntax".into(),
                ))];
            }
        }
    }
    Ok(decls)
}

/// Operators already on the preceding line may continue onto a later RHS.
/// A binary operator beginning a later line is an unsupported separate unary
/// statement in the installed Nim grammar, rather than a safe binary tree.
fn nim_binary_starts_new_statement(node: Node<'_>) -> bool {
    if node.kind() == "binary_expression" {
        if let Some(lhs) = node.child_by_field_name("left") {
            let mut cursor = node.walk();
            if node.children(&mut cursor).any(|child| {
                !child.is_named() && child.start_position().row > lhs.end_position().row
            }) {
                return true;
            }
        }
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(nim_binary_starts_new_statement)
}

fn nim_proc_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = nim_identifier(node_txt(src, name_n).trim());
    let params = nim_params(src, n);
    let ret = declared_return_type(src, n)
        .map(nim_type)
        .unwrap_or(Typ::Void);
    Some(Decl::Function {
        name,
        params,
        ret,
        body: vec![],
        type_params: vec![],
    })
}

fn nim_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
    let mut params = Vec::new();
    let mut param_nodes = Vec::new();
    collect_kinds(n, &["parameter"], &mut param_nodes);
    for p in param_nodes {
        let name_n = p
            .child_by_field_name("name")
            .or_else(|| first_named(p, "identifier"));
        if let Some(name_n) = name_n {
            let pname = nim_identifier(node_txt(src, name_n).trim());
            let mut walk = p.walk();
            let typ = p
                .named_children(&mut walk)
                .find(|node| *node != name_n)
                .map(|node| Typ::Named(nim_identifier(node_txt(src, node).trim())))
                .unwrap_or_else(|| Typ::Named("dynamic".into()));
            params.push((pname, typ));
        }
    }
    params
}

fn nim_scalar_type(typ: &Typ) -> Option<Typ> {
    match typ {
        Typ::Int | Typ::Float | Typ::Bool | Typ::String | Typ::Void => Some(typ.clone()),
        Typ::Named(name) => match name.as_str() {
            "int" => Some(Typ::Int),
            "float" | "float64" => Some(Typ::Float),
            "bool" => Some(Typ::Bool),
            "string" => Some(Typ::String),
            "void" => Some(Typ::Void),
            _ => None,
        },
        _ => None,
    }
}

fn nim_result_expr(expr: &mut Expr, shadowed: bool) {
    if !shadowed && matches!(expr, Expr::Ident(name) if name == "result") {
        *expr = Expr::Ident("#nim-result".into());
    } else {
        crate::core_ir::for_each_expr_child_mut(expr, &mut |child| {
            nim_result_expr(child, shadowed)
        });
    }
}

// The bundled grammar treats newlines as whitespace and can attach the next
// statement to a bare return. Nim's return operand must begin on the same
// source line; any swallowed later expression is unreachable after this exit.
fn nim_stmt(
    src: &[u8],
    node: Node<'_>,
    locals: &mut std::collections::HashSet<String>,
) -> Option<Stmt> {
    if node.kind() == "return_statement" {
        let mut cursor = node.walk();
        let value = node
            .named_children(&mut cursor)
            .find(|child| child.kind() != "comment");
        return match value {
            Some(value) if value.start_position().row == node.start_position().row => {
                nim_expr(src, value).map(|value| Stmt::Return(Some(value)))
            }
            _ => Some(Stmt::Return(None)),
        };
    }
    let mut statement = ast_stmt(src, node, NIM_AST, locals)?;
    if let Stmt::Let(name, annotation, _) = &mut statement {
        *name = nim_identifier(name);
        if let Some(typ) = annotation {
            *typ = nim_type(typ.clone());
        }
    } else if let Stmt::Assign(name, _) = &mut statement {
        *name = nim_identifier(name);
    }
    crate::core_opt::map_stmt_exprs_mut(&mut statement, &mut |expr| {
        if let Expr::Ident(name) = expr {
            *name = nim_identifier(name);
        }
    });
    Some(statement)
}

fn nim_proc_body(
    src: &[u8],
    function: Node<'_>,
    params: &[(String, Typ)],
    ret: &Typ,
    facts: &crate::typecheck::Facts,
) -> Vec<Stmt> {
    let unsupported = |reason: &str| {
        vec![Stmt::Expr(Expr::Ident(format!(
            "#unsupported-nim-{reason}"
        )))]
    };
    let Some(result_type) = nim_scalar_type(ret) else {
        return unsupported("result-layout");
    };
    let value_result = result_type != Typ::Void;
    if value_result && params.iter().any(|(name, _)| name == "result") {
        return unsupported("result-shadow");
    }
    let mut env: std::collections::HashMap<String, Typ> = params.iter().cloned().collect();
    let mut declared_locals = std::collections::HashSet::new();
    let mut result_shadowed = false;
    let mut implicit_result_assigned = false;
    let mut locals = params.iter().map(|(name, _)| name.clone()).collect();
    let mut immutable: std::collections::HashSet<String> =
        params.iter().map(|(name, _)| name.clone()).collect();
    let mut body = Vec::new();
    if value_result {
        let default = match result_type {
            Typ::Int => Expr::IntLit(0),
            Typ::Float => Expr::FloatLit(crate::core_ir::FloatVal(0.0)),
            Typ::Bool => Expr::BoolLit(false),
            Typ::String => Expr::StringLit(String::new()),
            _ => unreachable!(),
        };
        body.push(Stmt::Let(
            "#nim-result".into(),
            Some(result_type.clone()),
            default,
        ));
        env.insert("#nim-result".into(), result_type.clone());
    }
    let Some(block) = function.child_by_field_name("body") else {
        return unsupported("missing-body");
    };
    if block.has_error() {
        return unsupported("body-syntax");
    }
    let mut walk = block.walk();
    let children: Vec<_> = block
        .named_children(&mut walk)
        .filter(|node| node.kind() != "comment")
        .collect();
    for (index, child) in children.iter().copied().enumerate() {
        if child.kind() == "echo_statement" {
            let Some(value) = child
                .child_by_field_name("value")
                .and_then(|node| nim_expr(src, node))
            else {
                return unsupported("echo-value");
            };
            let mut value = value;
            if value_result {
                nim_result_expr(&mut value, result_shadowed);
            }
            let callee = match nim_prepare_expr(&mut value, facts, &env) {
                Some(Typ::String) => crate::core_ir::COMPILER_PRINT,
                Some(Typ::Int) => crate::core_ir::COMPILER_PRINT_INT,
                _ => return unsupported("echo-type"),
            };
            body.push(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(callee.into())),
                args: vec![value],
            }));
            body.push(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::COMPILER_PRINT.into())),
                args: vec![Expr::StringLit("\n".into())],
            }));
            continue;
        }
        if !matches!(
            child.kind(),
            "return_statement" | "expression_statement" | "local_decl" | "assignment"
        ) {
            return unsupported("statement");
        }
        let Some(mut stmt) = nim_stmt(src, child, &mut locals) else {
            return unsupported("statement");
        };
        match &mut stmt {
            Stmt::Let(name, annotation, value) => {
                if let Some(declared) = nim_local_annotation(src, child) {
                    *annotation = Some(declared);
                }
                if !declared_locals.insert(name.clone()) {
                    return unsupported("local-redeclaration");
                }
                if value_result {
                    nim_result_expr(value, result_shadowed);
                }
                let Some(actual) = nim_prepare_expr(value, facts, &env) else {
                    return unsupported("value-type");
                };
                let typ = if let Some(expected) = annotation {
                    if nim_expected_expr(value, actual, expected).is_none() {
                        return unsupported("value-type");
                    }
                    expected.clone()
                } else {
                    actual
                };
                if child.child(0).is_some_and(|token| token.kind() == "const")
                    && !nim_supported_const(value)
                {
                    return unsupported("const-initializer");
                }
                // The initializer used the preceding scope; only now does
                // this local hide a parameter or the implicit result spelling.
                env.insert(name.clone(), typ);
                if child
                    .child(0)
                    .is_some_and(|token| matches!(token.kind(), "let" | "const"))
                {
                    immutable.insert(name.clone());
                } else {
                    immutable.remove(name);
                }
                if name == "result" {
                    result_shadowed = true;
                }
            }
            Stmt::Assign(name, value) => {
                if immutable.contains(name) {
                    return unsupported("immutable-assignment");
                }
                if value_result && !result_shadowed && name == "result" {
                    implicit_result_assigned = true;
                    *name = "#nim-result".into();
                }
                if value_result {
                    nim_result_expr(value, result_shadowed);
                }
                let Some(actual) = nim_prepare_expr(value, facts, &env) else {
                    return unsupported("value-type");
                };
                let Some(expected) = env.get(name) else {
                    return unsupported("assignment-target");
                };
                if nim_expected_expr(value, actual, expected).is_none() {
                    return unsupported("value-type");
                }
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    if value_result {
                        nim_result_expr(value, result_shadowed);
                    }
                    if !matches!(value,Expr::Ident(name) if name=="#unsupported-string-literal") {
                        let Some(actual) = nim_prepare_expr(value, facts, &env) else {
                            return unsupported("return-type");
                        };
                        if nim_expected_expr(value, actual, &result_type).is_none() {
                            return unsupported("return-type");
                        }
                    }
                } else if value_result {
                    *value = Some(Expr::Ident("#nim-result".into()));
                }
            }
            Stmt::Expr(value) => {
                if value_result {
                    nim_result_expr(value, result_shadowed);
                }
                let typ = nim_prepare_expr(value, facts, &env);
                if index + 1 == children.len() && value_result && typ != Some(Typ::Void) {
                    if implicit_result_assigned {
                        return unsupported("discarded-value");
                    }
                    let Some(actual) = typ else {
                        return unsupported("tail-type");
                    };
                    if nim_expected_expr(value, actual, &result_type).is_none() {
                        return unsupported("tail-type");
                    }
                    stmt = Stmt::Return(Some(value.clone()));
                } else if typ != Some(Typ::Void) {
                    return unsupported("discarded-value");
                }
            }
            _ => return unsupported("statement"),
        }
        body.push(stmt);
    }
    body.push(Stmt::Return(
        value_result.then(|| Expr::Ident("#nim-result".into())),
    ));
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(source: &str) -> Vec<Decl> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_nim::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        extract_nim(source.as_bytes(), tree.root_node()).unwrap()
    }

    #[test]
    fn expected_float_contexts_convert_int_once_without_reverse_coercions() {
        for source in [
            "proc main(): float =\n  1\n",
            "proc main(): float =\n  return 1\n",
            "proc main(): auto =\n  let value: float = 1\n  value\n",
            "proc main(): float =\n  var value: float = 0\n  value = 1\n  value\n",
            "proc main(): float =\n  result = 1\n",
            "proc half(value: float): float =\n  value / 2\nproc main(): auto =\n  half(1)\n",
        ] {
            let declarations = parse(source);
            let Decl::Function { body, .. } = declarations.last().unwrap() else {
                panic!()
            };
            let body = format!("{body:?}");
            assert!(!body.contains("#unsupported"), "{source}: {body}");
            assert!(body.contains("int-to-float"), "{source}: {body}");
        }
        for source in [
            "proc main(): auto =\n  let value: string = 1\n  value\n",
            "proc main(): int =\n  1 / 2\n",
            "proc main(): bool =\n  1\n",
            "proc value(): int =\n  1\nproc main(): float =\n  value()\n",
            "proc fractional(): float =\n  1 / 2\nproc main(): float =\n  let value = 1\n  value + fractional()\n",
            "proc main(): float =\n  9223372036854775807 + 1\n",
        ] {
            let declarations = parse(source);
            assert!(
                format!("{declarations:?}").contains("#unsupported-nim"),
                "{source}"
            );
        }
        let declarations = parse(
            "proc value(): int =\n  echo \"once\"\n  1\nproc main(): float =\n  value() / 2\n",
        );
        let Decl::Function { body, .. } = declarations.last().unwrap() else {
            panic!()
        };
        assert!(!format!("{body:?}").contains("#unsupported"));
        assert!(
            matches!(&body[1], Stmt::Return(Some(Expr::Binary { op, lhs, rhs }))
            if op == "/" && matches!(lhs.as_ref(), Expr::Unary { op, expr }
                if op == "int-to-float" && matches!(expr.as_ref(), Expr::Call { callee, args }
                    if matches!(callee.as_ref(), Expr::Ident(name) if name == "value") && args.is_empty()))
            && matches!(rhs.as_ref(), Expr::Unary { op, expr } if op == "int-to-float" && matches!(expr.as_ref(), Expr::IntLit(2))))
        );
        let call = Expr::Call {
            callee: Box::new(Expr::Ident("effect".into())),
            args: vec![],
        };
        let mut converted = call.clone();
        assert_eq!(
            nim_expected_expr(&mut converted, Typ::Int, &Typ::Float),
            None
        );
        assert_eq!(converted, call);
        let mut constant = Expr::Binary {
            op: "+".into(),
            lhs: Box::new(Expr::IntLit(20)),
            rhs: Box::new(Expr::IntLit(22)),
        };
        assert_eq!(
            nim_expected_expr(&mut constant, Typ::Int, &Typ::Float),
            Some(Typ::Float)
        );
        assert!(matches!(constant, Expr::Unary { op, .. } if op == "int-to-float"));
        for (op, lhs, rhs) in [("+", i64::MAX, 1), ("-", i64::MIN, 1), ("*", i64::MAX, 2)] {
            let mut overflow = Expr::Binary {
                op: op.into(),
                lhs: Box::new(Expr::IntLit(lhs)),
                rhs: Box::new(Expr::IntLit(rhs)),
            };
            assert_eq!(
                nim_expected_expr(&mut overflow, Typ::Int, &Typ::Float),
                None
            );
        }
        // A prepared String concatenation has Core '+' identity; expected typing
        // must not run the Nim source-operator preparation a second time.
        let mut concatenation = Expr::Binary {
            op: "+".into(),
            lhs: Box::new(Expr::StringLit("a".into())),
            rhs: Box::new(Expr::StringLit("b".into())),
        };
        assert_eq!(
            nim_expected_expr(&mut concatenation, Typ::String, &Typ::String),
            Some(Typ::String)
        );
    }

    #[test]
    fn nim_value_procedures_own_result_storage_and_bare_return() {
        for source in [
            "proc main(): int =\n  1 + 2\n",
            "proc main(): int =\n  let value = 3\n  value\n",
            "proc main(): int =\n  result = 3\n  return\n",
            "proc main(): int =\n  return\n",
        ] {
            let declarations = parse(source);
            let Decl::Function { ret, body, .. } = &declarations[0] else {
                panic!()
            };
            assert_eq!(ret, &Typ::Named("int".into()));
            assert!(
                matches!(&body[0], Stmt::Let(name, Some(Typ::Int), Expr::IntLit(0)) if name == "#nim-result")
            );
            assert!(
                matches!(body.last(), Some(Stmt::Return(Some(Expr::Ident(name)))) if name == "#nim-result")
            );
            assert!(!format!("{body:?}").contains("#unsupported-nim"));
        }
    }
    #[test]
    fn nim_echo_preserves_effect_newline_and_result_before_it() {
        let declarations = parse("proc main(): int =\n  result = 3\n  echo \"side\"\n");
        let Decl::Function { body, .. } = &declarations[0] else {
            panic!()
        };
        assert!(matches!(&body[1], Stmt::Assign(name, Expr::IntLit(3)) if name == "#nim-result"));
        assert!(
            matches!(&body[2], Stmt::Expr(Expr::Call{callee,args}) if matches!(callee.as_ref(),Expr::Ident(name) if name == crate::core_ir::COMPILER_PRINT) && args == &vec![Expr::StringLit("side".into())])
        );
        assert!(
            matches!(&body[3], Stmt::Expr(Expr::Call{args,..}) if args == &vec![Expr::StringLit("\n".into())])
        );
    }
    #[test]
    fn nim_void_and_unsupported_layouts_are_not_guessed_values() {
        let declarations = parse("proc main() =\n  echo \"side\"\n");
        assert!(
            matches!(&declarations[0],Decl::Function{ret:Typ::Void,body,..} if matches!(body.last(),Some(Stmt::Return(None))))
        );
        for source in [
            "proc main() =\n  3\n",
            "proc main(): Custom =\n  3\n",
            "proc main(result:int): int =\n  result\n",
        ] {
            let declarations = parse(source);
            assert!(
                matches!(&declarations[0],Decl::Function{body,..} if matches!(&body[0],Stmt::Expr(Expr::Ident(name)) if name.starts_with("#unsupported-nim-")))
            );
        }
    }
}

/// Decode Nim regular strings into the UTF-8 Core String representation.
/// Platform-dependent escapes and byte strings not representable as UTF-8 are
/// refused rather than preserved literally or silently recoded.
fn nim_string_literal(quoted: &str) -> Option<String> {
    let raw = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = raw.chars().peekable();
    let mut bytes = Vec::new();
    while let Some(ch) = chars.next() {
        if ch == '"' || ch == '\n' || ch == '\r' {
            return None;
        }
        if ch != '\\' {
            bytes.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
            continue;
        }
        let escape = chars.next()?;
        let byte = match escape {
            'n' | 'l' => b'\n',
            'r' | 'c' => b'\r',
            't' => b'\t',
            'f' => 12,
            'v' => 11,
            'a' => 7,
            'b' => 8,
            'e' => 27,
            '\\' => b'\\',
            '"' => b'"',
            '\'' => b'\'',
            '0'..='9' => {
                let mut value = escape.to_digit(10)?;
                while chars.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                    value = value
                        .checked_mul(10)?
                        .checked_add(chars.next()?.to_digit(10)?)?;
                }
                u8::try_from(value).ok()?
            }
            'x' => {
                let high = chars.next()?.to_digit(16)?;
                let low = chars.next()?.to_digit(16)?;
                (high * 16 + low) as u8
            }
            'u' => {
                let mut value = 0u32;
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut digits = 0;
                    loop {
                        let ch = chars.next()?;
                        if ch == '}' {
                            if digits == 0 {
                                return None;
                            }
                            break;
                        }
                        value = value.checked_mul(16)?.checked_add(ch.to_digit(16)?)?;
                        digits += 1;
                    }
                } else {
                    for _ in 0..4 {
                        value = value
                            .checked_mul(16)?
                            .checked_add(chars.next()?.to_digit(16)?)?;
                    }
                }
                bytes.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut [0; 4]).as_bytes());
                continue;
            }
            _ => return None,
        };
        bytes.push(byte);
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod string_tests {
    use super::nim_string_literal;
    #[test]
    fn nim_string_callback_retains_valid_values_and_invalid_literal_refusals() {
        for (literal, expected) in [
            (r#""a\nb\t\"q\"\\""#, Some("a\nb\t\"q\"\\")),
            (r#""\\n""#, Some("\\n")),
            (r#""\q""#, None),
        ] {
            let source = format!("proc main(): string =\n  return {literal}\n");
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_nim::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(&source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "{}",
                tree.root_node().to_sexp()
            );
            let decls = super::extract_nim(source.as_bytes(), tree.root_node()).unwrap();
            let crate::core_ir::Decl::Function { body, .. } = &decls[0] else {
                panic!()
            };
            assert!(!body.is_empty(), "invalid literal must not disappear");
            if let Some(expected) = expected {
                assert!(
                    body.iter().any(|stmt| matches!(stmt,crate::core_ir::Stmt::Return(Some(crate::core_ir::Expr::StringLit(value))) if value == expected)),
                    "{body:?}"
                );
            } else {
                assert!(
                    body.iter().any(|stmt| matches!(stmt,crate::core_ir::Stmt::Return(Some(crate::core_ir::Expr::Ident(name))) if name == "#unsupported-string-literal")),
                    "{body:?}"
                );
            }
        }
    }

    #[test]
    fn nim_regular_string_escapes_are_decoded_without_unknown_preservation() {
        for (raw, expected) in [
            (r#""a\nb\tc\r""#, "a\nb\tc\r"),
            (r#""say \"hi\" \\ end""#, "say \"hi\" \\ end"),
            (r#""\\n""#, "\\n"),
            (r#""\n""#, "\n"),
            (r#""\065\x42\0""#, "AB\0"),
            (r#""\xCE\xBB""#, "λ"),
            (r#""\u03BB\u{1F642}""#, "λ🙂"),
        ] {
            assert_eq!(nim_string_literal(raw).as_deref(), Some(expected), "{raw}");
        }
        for raw in [
            r#""\q""#,
            r#""\x4""#,
            r#""\256""#,
            r#""\xFF""#,
            r#""\uD800""#,
            r#""\u{}""#,
            r#""\p""#,
        ] {
            assert_eq!(nim_string_literal(raw), None, "{raw}");
        }
    }
}

fn nim_local_annotation(src: &[u8], node: Node<'_>) -> Option<Typ> {
    if node.kind() != "local_decl" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    let value = node.child_by_field_name("value")?;
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| *child != name && *child != value)
        .map(|typ| Typ::Named(nim_identifier(node_txt(src, typ).trim())))
}

/// Lower Nim numeric/string operations while operand types and source operator
/// identity remain available. Each operand tree is transformed exactly once.
fn nim_prepare_expr(
    expr: &mut Expr,
    facts: &crate::typecheck::Facts,
    env: &std::collections::HashMap<String, Typ>,
) -> Option<Typ> {
    match expr {
        Expr::UnitLit => Some(Typ::Void),
        Expr::IntLit(_) => Some(Typ::Int),
        Expr::FloatLit(_) => Some(Typ::Float),
        Expr::StringLit(_) => Some(Typ::String),
        Expr::BoolLit(_) => Some(Typ::Bool),
        Expr::Ident(name) => {
            if let Some(typ) = env.get(name) {
                return nim_scalar_type(typ);
            }
            // Nim spells these predefined constants as identifier AST nodes.
            // Lexical bindings and declared functions keep their identity.
            if facts.function_signature(name).is_some() {
                return None;
            }
            let value = match name.as_str() {
                "true" => true,
                "false" => false,
                _ => return None,
            };
            *expr = Expr::BoolLit(value);
            Some(Typ::Bool)
        }
        Expr::Unary { op, expr } if op == "int-to-float" => {
            (nim_prepare_expr(expr, facts, env)? == Typ::Int).then_some(Typ::Float)
        }
        Expr::Binary { op, lhs, rhs } => {
            let left = nim_prepare_expr(lhs, facts, env)?;
            let right = nim_prepare_expr(rhs, facts, env)?;
            if op == "&" && left == Typ::String && right == Typ::String {
                *op = "+".into();
                return Some(Typ::String);
            }
            let numeric =
                matches!(left, Typ::Int | Typ::Float) && matches!(right, Typ::Int | Typ::Float);
            let comparison = matches!(op.as_str(), "==" | "!=" | "<" | "<=" | ">" | ">=");
            if !numeric {
                return (comparison
                    && left == right
                    && (left == Typ::Bool
                        || (left == Typ::String && matches!(op.as_str(), "==" | "!="))))
                .then_some(Typ::Bool);
            }
            if !comparison && !matches!(op.as_str(), "+" | "-" | "*" | "/") {
                return None;
            }
            let floating = op == "/" || left == Typ::Float || right == Typ::Float;
            if floating {
                let integer_division = op == "/" && left == Typ::Int && right == Typ::Int;
                for (operand, kind) in [(lhs, left), (rhs, right)] {
                    if kind == Typ::Int {
                        if integer_division {
                            // Nim's Int/Int '/' overload returns Float even for
                            // dynamic operands; this is not implicit conversion.
                            nim_int_to_float(operand);
                        } else {
                            nim_expected_expr(operand, kind, &Typ::Float)?;
                        }
                    }
                }
            }
            Some(if comparison {
                Typ::Bool
            } else if floating {
                Typ::Float
            } else {
                Typ::Int
            })
        }
        Expr::Call { callee, args } => {
            let Expr::Ident(name) = callee.as_ref() else {
                return None;
            };
            let (params, ret) = facts.function_signature(name)?;
            if params.len() != args.len() {
                return None;
            }
            for ((_, expected), value) in params.iter().zip(args) {
                let actual = nim_prepare_expr(value, facts, env)?;
                nim_expected_expr(value, actual, expected)?;
            }
            nim_scalar_type(ret)
        }
        _ => None,
    }
}

/// Prove a source integer constant without accepting generic wrapping folds.
/// Calls/locals (including erased const aliases) are deliberately not constants.
fn nim_int_constant(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::IntLit(value) => Some(*value),
        Expr::Binary { op, lhs, rhs } => {
            let left = nim_int_constant(lhs)?;
            let right = nim_int_constant(rhs)?;
            match op.as_str() {
                "+" => left.checked_add(right),
                "-" => left.checked_sub(right),
                "*" => left.checked_mul(right),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Keep admitted const initializers independent of runtime state/effects.
/// Broader Nim compile-time execution needs an actual compile-time evaluator.
fn nim_supported_const(expr: &Expr) -> bool {
    match expr {
        Expr::IntLit(_) | Expr::BoolLit(_) | Expr::StringLit(_) => true,
        Expr::FloatLit(value) => value.0.is_finite(),
        Expr::Unary { op, expr } if op == "int-to-float" => nim_int_constant(expr).is_some(),
        Expr::Binary { .. } => nim_int_constant(expr).is_some(),
        _ => false,
    }
}

fn nim_int_to_float(expr: &mut Expr) {
    *expr = Expr::Unary {
        op: "int-to-float".into(),
        expr: Box::new(std::mem::replace(expr, Expr::UnitLit)),
    };
}

/// Apply only Nim's expected-context Int-to-Float conversion. `actual` was
/// already computed while preparing this expression; never prepare it twice.
fn nim_expected_expr(expr: &mut Expr, actual: Typ, expected: &Typ) -> Option<Typ> {
    let expected = nim_scalar_type(expected)?;
    if actual == expected {
        return Some(expected);
    }
    if actual == Typ::Int && expected == Typ::Float && nim_int_constant(expr).is_some() {
        nim_int_to_float(expr);
        return Some(Typ::Float);
    }
    None
}

fn nim_known_type(
    expr: &Expr,
    facts: &crate::typecheck::Facts,
    env: &std::collections::HashMap<String, Typ>,
) -> Option<Typ> {
    nim_prepare_expr(&mut expr.clone(), facts, env)
}

fn nim_binary_precedence(op: &str) -> Option<u8> {
    match op {
        "==" | "!=" | "<" | "<=" | ">" | ">=" => Some(5),
        "&" => Some(7),
        "+" | "-" => Some(8),
        "*" | "/" => Some(9),
        _ => None,
    }
}

#[cfg(test)]
mod precedence_tests {
    use super::*;
    fn expression(source: &str, javascript: bool) -> crate::core_ir::Expr {
        let mut parser = tree_sitter::Parser::new();
        let language = if javascript {
            tree_sitter_javascript::LANGUAGE.into()
        } else {
            tree_sitter_nim::LANGUAGE.into()
        };
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let mut binaries = Vec::new();
        collect_kinds(tree.root_node(), &["binary_expression"], &mut binaries);
        let shape = if javascript {
            AstShape {
                paren_kinds: &["parenthesized_expression"],
                int_kinds: &["number"],
                ..NIM_AST
            }
        } else {
            NIM_AST
        };
        super::super::extract::ast_expr(source.as_bytes(), binaries[0], shape).unwrap()
    }
    #[test]
    fn nim_owned_binary_precedence_is_rebuilt_from_ast_operands() {
        use crate::core_ir::Expr;
        let value = expression("proc main(): int =\n  1 + 2 * 3\n", false);
        assert!(
            matches!(&value,Expr::Binary{op,lhs,rhs} if op=="+"&&matches!(lhs.as_ref(),Expr::IntLit(1))&&matches!(rhs.as_ref(),Expr::Binary{op,..}if op=="*")),
            "{value:?}"
        );
        let value = expression("proc main(): int =\n  8 - 3 - 1\n", false);
        assert!(
            matches!(&value,Expr::Binary{op,lhs,..} if op=="-"&&matches!(lhs.as_ref(),Expr::Binary{op,..}if op=="-")),
            "{value:?}"
        );
    }
    #[test]
    fn shared_precedence_helper_keeps_explicit_grouping_opaque() {
        use crate::core_ir::Expr;
        let value = expression("function main(){return (1+2)*3;}", true);
        assert!(
            matches!(&value,Expr::Binary{op,lhs,..} if op=="*"&&matches!(lhs.as_ref(),Expr::Binary{op,..}if op=="+")),
            "{value:?}"
        );
    }
}

fn nim_auto_return(
    src: &[u8],
    function: Node<'_>,
    params: &[(String, Typ)],
    facts: &crate::typecheck::Facts,
) -> Option<Typ> {
    fn merge(result: &mut Option<Typ>, kind: Typ) -> Option<()> {
        if result.as_ref().is_some_and(|previous| *previous != kind) {
            return None;
        }
        *result = Some(kind);
        Some(())
    }
    let block = function.child_by_field_name("body")?;
    if block.has_error() {
        return None;
    }
    let mut env: std::collections::HashMap<_, _> = params.iter().cloned().collect();
    let mut declared_locals = std::collections::HashSet::new();
    let mut result_shadowed = false;
    let mut locals = params.iter().map(|(name, _)| name.clone()).collect();
    let mut result = None;
    let mut cursor = block.walk();
    let children: Vec<_> = block
        .named_children(&mut cursor)
        .filter(|node| node.kind() != "comment")
        .collect();
    for (index, node) in children.iter().copied().enumerate() {
        if node.kind() == "echo_statement" {
            let value = nim_expr(src, node.child_by_field_name("value")?)?;
            if !matches!(nim_known_type(&value, facts, &env)?, Typ::Int | Typ::String) {
                return None;
            }
            continue;
        }
        if !matches!(
            node.kind(),
            "local_decl" | "assignment" | "return_statement" | "expression_statement"
        ) {
            return None;
        }
        match nim_stmt(src, node, &mut locals)? {
            Stmt::Let(name, _, mut value) => {
                if !declared_locals.insert(name.clone()) {
                    return None;
                }
                let actual = nim_prepare_expr(&mut value, facts, &env)?;
                let typ = if let Some(annotation) = nim_local_annotation(src, node) {
                    nim_expected_expr(&mut value, actual, &annotation)?
                } else {
                    actual
                };
                if node.child(0).is_some_and(|token| token.kind() == "const")
                    && !nim_supported_const(&value)
                {
                    return None;
                }
                if name == "result" {
                    result_shadowed = true;
                }
                env.insert(name, typ);
            }
            Stmt::Assign(name, mut value) => {
                let actual = nim_prepare_expr(&mut value, facts, &env)?;
                if !result_shadowed && name == "result" {
                    merge(&mut result, actual.clone())?;
                    env.insert(name, actual);
                } else {
                    nim_expected_expr(&mut value, actual, env.get(&name)?)?;
                }
            }
            Stmt::Return(value) => {
                let actual = match value {
                    Some(value) => nim_known_type(&value, facts, &env)?,
                    None => result.clone().unwrap_or(Typ::Void),
                };
                merge(&mut result, actual)?;
                return result;
            }
            Stmt::Expr(value) => {
                let actual = nim_known_type(&value, facts, &env)?;
                if index + 1 == children.len() && actual != Typ::Void {
                    // Explicit implicit-result assignment disables expression
                    // body inference, even after a local shadows its spelling.
                    if result.is_some() {
                        return None;
                    }
                    merge(&mut result, actual)?;
                } else if actual != Typ::Void {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(result.unwrap_or(Typ::Void))
}

#[cfg(test)]
mod auto_tests {
    use super::*;
    fn parse(source: &str) -> Vec<Decl> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_nim::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        extract_nim(source.as_bytes(), tree.root_node()).unwrap()
    }
    #[test]
    fn auto_return_inference_uses_sequential_values_and_previous_helpers() {
        for (source, expected) in [
            ("proc main(): auto =\n  1 + 2\n", Typ::Int),
            ("proc main(): auto =\n  1 / 2\n", Typ::Float),
            ("proc main(): auto =\n  1 / 2 + 3\n", Typ::Float),
            ("proc main(): auto =\n  \"a\" & \"b\"\n", Typ::String),
            ("proc main(): auto =\n  1 < 2\n", Typ::Bool),
            (
                "proc main(): auto =\n  let value = \"hi\"\n  value\n",
                Typ::String,
            ),
            ("proc main(): auto =\n  result = 3\n", Typ::Int),
            ("proc main(): auto =\n  return\n", Typ::Void),
            (
                "proc value(): auto =\n  \"hi\"\nproc main(): auto =\n  value()\n",
                Typ::String,
            ),
        ] {
            let decls = parse(source);
            assert!(
                matches!(decls.last(),Some(Decl::Function{ret,body,..}) if *ret==expected && !format!("{body:?}").contains("#unsupported")),
                "{decls:?}"
            );
        }
    }
    #[test]
    fn unresolved_auto_result_and_invalid_values_never_become_int() {
        for source in [
            "proc main(): auto =\n  result + 3\n",
            "proc main(): auto =\n  unknown + 3\n",
            "proc main(): auto =\n  let value: string = 3\n  value\n",
            "proc main(): auto =\n  result = 3\n  \"hi\"\n",
            "proc main(): auto =\n  later()\nproc later(): auto =\n  3\n",
        ] {
            let decls = parse(source);
            assert!(
                matches!(&decls[0],Decl::Function{ret:Typ::Named(name),body,..} if name=="auto" && format!("{body:?}").contains("#unsupported-nim")),
                "{decls:?}"
            );
        }
    }
}

#[cfg(test)]
mod boolean_const_tests {
    use super::*;
    fn parse(source: &str) -> Vec<Decl> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_nim::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        extract_nim(source.as_bytes(), tree.root_node()).unwrap()
    }
    #[test]
    fn predefined_booleans_are_values_but_lexical_bindings_take_precedence() {
        for literal in ["true", "false"] {
            let declarations = parse(&format!("proc main(): auto =\n  {literal}\n"));
            assert!(
                matches!(&declarations[0],Decl::Function{ret,body,..} if *ret==Typ::Bool && body.iter().any(|stmt|matches!(stmt,Stmt::Return(Some(Expr::BoolLit(_)))))),
                "{declarations:?}"
            );
        }
        for source in [
            "proc main(): auto =\n  let true = 42\n  true\n",
            "proc main(true: int): int =\n  true\n",
        ] {
            let declarations = parse(source);
            assert!(
                matches!(&declarations[0],Decl::Function{body,..} if body.iter().any(|stmt|matches!(stmt,Stmt::Return(Some(Expr::Ident(name))) if name=="true")) && !format!("{body:?}").contains("#unsupported")),
                "{declarations:?}"
            );
        }
    }
    #[test]
    fn const_initializers_never_become_runtime_calls_or_reads() {
        for annotation in ["int", "auto"] {
            for initializer in ["helper()", "local", "9223372036854775807 + 1"] {
                let source = format!(
                    "proc helper(): int =\n  echo \"effect\"\n  42\nproc main(): {annotation} =\n  let local = 1\n  const value = {initializer}\n  value\n"
                );
                let declarations = parse(&source);
                assert!(
                    matches!(declarations.last(),Some(Decl::Function{body,..}) if format!("{body:?}").contains("#unsupported")),
                    "{declarations:?}"
                );
            }
        }
        for source in [
            "proc main(): int =\n  const value = 1 + 2 * 3\n  value\n",
            "proc main(): auto =\n  const value = true\n  value\n",
            "proc main(): string =\n  const value = \"safe\"\n  value\n",
        ] {
            let declarations = parse(source);
            assert!(
                matches!(&declarations[0],Decl::Function{body,..} if !format!("{body:?}").contains("#unsupported")),
                "{declarations:?}"
            );
        }
    }
}

#[cfg(test)]
mod return_line_tests {
    use super::*;
    fn parse(source: &str) -> Vec<Decl> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_nim::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        extract_nim(source.as_bytes(), tree.root_node()).unwrap()
    }
    #[test]
    fn next_line_call_after_bare_return_is_not_an_operand() {
        for ret in ["", ": auto"] {
            let source = format!(
                "proc side() =\n  echo \"unexpected\"\nproc helper(){ret} =\n  return\n  side()\n"
            );
            let decls = parse(&source);
            let Decl::Function { ret, body, .. } = &decls[1] else {
                panic!()
            };
            assert_eq!(ret, &Typ::Void, "{decls:?}");
            assert!(matches!(body.first(), Some(Stmt::Return(None))), "{body:?}");
            assert!(!format!("{body:?}").contains("side"), "{body:?}");
        }
    }
    #[test]
    fn bare_value_return_keeps_result_while_same_line_operand_is_preserved() {
        let decls = parse("proc main(): int =\n  result = 42\n  return\n  7\n");
        let Decl::Function { body, .. } = &decls[0] else {
            panic!()
        };
        assert!(
            matches!(&body[2],Stmt::Return(Some(Expr::Ident(name))) if name=="#nim-result"),
            "{body:?}"
        );
        assert!(
            !body
                .iter()
                .any(|stmt| matches!(stmt, Stmt::Return(Some(Expr::IntLit(7)))))
        );
        for ret in ["int", "auto"] {
            let decls = parse(&format!("proc main(): {ret} =\n  return 42\n"));
            let Decl::Function { body, .. } = &decls[0] else {
                panic!()
            };
            assert!(
                body.iter()
                    .any(|stmt| matches!(stmt, Stmt::Return(Some(Expr::IntLit(42))))),
                "{body:?}"
            );
        }
    }
    #[test]
    fn same_line_void_return_call_keeps_its_effect() {
        let decls = parse("proc side() =\n  echo \"effect\"\nproc helper() =\n  return side()\n");
        let Decl::Function { body, .. } = &decls[1] else {
            panic!()
        };
        assert!(
            matches!(body.first(),Some(Stmt::Return(Some(Expr::Call{callee,..}))) if matches!(callee.as_ref(),Expr::Ident(name)if name=="side")),
            "{body:?}"
        );
    }
}

#[cfg(test)]
mod binary_boundary_tests {
    use super::*;
    fn parse(source: &str) -> Vec<Decl> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_nim::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        extract_nim(source.as_bytes(), tree.root_node()).unwrap()
    }
    #[test]
    fn next_line_unary_is_not_swallowed_into_previous_initializer() {
        for result in ["int", "auto"] {
            for operator in ["-", "+"] {
                let source =
                    format!("proc main(): {result} =\n  let a = 10\n  let b = 42\n  {operator}a\n");
                let declarations = parse(&source);
                assert!(
                    matches!(&declarations[0],Decl::Function{body,..} if matches!(&body[..],[Stmt::Expr(Expr::Ident(name))] if name=="#unsupported-nim-binary-statement-boundary")),
                    "{declarations:?}"
                );
            }
        }
    }
    #[test]
    fn prior_line_operator_keeps_legitimate_multiline_rhs() {
        for indentation in ["  ", "    "] {
            let source =
                format!("proc main(): int =\n  let a = 10\n  let b = 42 -\n{indentation}a\n  b\n");
            let declarations = parse(&source);
            assert!(
                matches!(&declarations[0],Decl::Function{body,..} if !format!("{body:?}").contains("#unsupported")),
                "{declarations:?}"
            );
        }
    }
}

#[cfg(test)]
mod identifier_style_tests {
    use super::*;
    fn parse(source: &str) -> Result<Vec<Decl>, String> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_nim::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        extract_nim(source.as_bytes(), tree.root_node())
    }
    #[test]
    fn identifier_style_is_ascii_only_after_first_character_and_preserves_private_names() {
        for (source, expected) in [
            ("foo_Bar", "foobar"),
            ("Foo_bar", "Foobar"),
            ("rEs_Ult", "result"),
            ("i_Nt", "int"),
            ("é_Foo", "éfoo"),
            ("é_foo", "éfoo"),
            ("É_foo", "Éfoo"),
            ("a_É", "aÉ"),
            ("#print_int", "#print_int"),
            ("#nim-result", "#nim-result"),
        ] {
            assert_eq!(nim_identifier(source), expected);
        }
    }
    #[test]
    fn functions_parameters_locals_assignments_types_and_result_share_style_identity() {
        for source in [
            "proc fooBar(valueName: i_Nt): i_Nt =\n  var localCount = value_name\n  local_count = local_count + 22\n  localCount\nproc main(): int =\n  foo_bar(20)\n",
            "proc addValue(inputValue: int): int =\n  rEs_Ult = input_value + 22\nproc main(): int =\n  add_value(20)\n",
            "proc fooBar(): int =\n  20\nproc FooBar(): int =\n  22\nproc main(): int =\n  foo_bar() + Foo_bar()\n",
            "proc fooBar(): int =\n  7\nproc main(): int =\n  let foo_bar = 42\n  fooBar\n",
        ] {
            let declarations = parse(source).unwrap();
            assert!(
                !format!("{declarations:?}").contains("#unsupported"),
                "{source}: {declarations:?}"
            );
            assert!(
                declarations
                    .iter()
                    .any(|decl| matches!(decl,Decl::Function{name,..}if name=="main"))
            );
        }
        let declarations =
            parse("proc main(): string =\n  let fooBar = \"foo_bar FooBar rEs_Ult\"\n  foo_bar\n")
                .unwrap();
        assert!(
            matches!(&declarations[0],Decl::Function{body,..}if body.iter().any(|stmt|matches!(stmt,Stmt::Let(_,_,Expr::StringLit(value))if value=="foo_bar FooBar rEs_Ult")))
        );
    }
    #[test]
    fn style_collisions_refuse_without_folding_first_letter_case() {
        for source in [
            "proc fooBar(): int =\n  20\nproc foo_bar(): int =\n  22\n",
            "proc take(fooBar:int,foo_bar:int):int =\n  fooBar\n",
        ] {
            assert!(parse(source).unwrap_err().contains("style collision"));
        }
        let declarations =
            parse("proc main():int =\n  let fooBar=20\n  let foo_bar=22\n  fooBar+foo_bar\n")
                .unwrap();
        assert!(format!("{declarations:?}").contains("#unsupported-nim-local-redeclaration"));
        let declarations = parse("proc Main():int =\n  20\nproc main():int =\n  22\n").unwrap();
        assert!(matches!(&declarations[0],Decl::Function{name,..}if name=="Main"));
        assert!(matches!(&declarations[1],Decl::Function{name,..}if name=="main"));
    }
}

#[cfg(test)]
mod lexical_shadow_tests {
    use super::*;
    fn parse(source: &str) -> Vec<Decl> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_nim::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        extract_nim(source.as_bytes(), tree.root_node()).unwrap()
    }
    #[test]
    fn local_shadow_uses_previous_initializer_scope_and_new_binding_mutability() {
        for ret in ["int", "auto"] {
            for body in [
                "let foo_bar=fooBar+22\n  fooBar",
                "var foo_bar=fooBar+20\n  fooBar=foo_bar+2\n  fooBar",
                "const foo_bar=42\n  fooBar",
            ] {
                let source = format!(
                    "proc work(fooBar:int):{ret} =\n  {body}\nproc main():int =\n  work(20)\n"
                );
                let parsed = crate::core_ir::UnifiedModule::new(parse(&source));
                assert!(
                    !format!("{parsed:?}").contains("#unsupported"),
                    "{source}: {parsed:?}"
                );
                let normalized = crate::typecheck::normalize_module(
                    crate::parser_registry::ParserId::Nim,
                    &parsed,
                );
                for profile in [
                    crate::emit_profile::EmitProfile::Default,
                    crate::emit_profile::EmitProfile::Lean,
                    crate::emit_profile::EmitProfile::Harden,
                ] {
                    let mut module = normalized.clone();
                    crate::core_opt::optimize_with_profile(
                        &mut module.decls,
                        Some("main"),
                        profile,
                    );
                    assert_eq!(
                        crate::native_emit::inisa::eval_module(&module, "main").unwrap(),
                        42,
                        "{source} {profile:?}"
                    );
                }
            }
        }
    }
    #[test]
    fn explicit_local_result_uses_shadow_but_bare_and_fallback_return_hidden_result() {
        for (body, expected) in [
            ("let rEs_Ult=42\n  result", 42),
            ("var result=20\n  rEs_Ult=result+22\n  result", 42),
            ("result=20\n  let rEs_Ult=result+22\n  return result", 42),
            ("let result=42", 0),
            ("let result=42\n  return", 0),
            ("result=20\n  let result=result+22\n  return", 20),
        ] {
            let source = format!("proc main():int =\n  {body}\n");
            let parsed = crate::core_ir::UnifiedModule::new(parse(&source));
            assert!(
                !format!("{parsed:?}").contains("#unsupported"),
                "{source}: {parsed:?}"
            );
            let normalized =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Nim, &parsed);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut module = normalized.clone();
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    crate::native_emit::inisa::eval_module(&module, "main").unwrap(),
                    expected,
                    "{source} {profile:?}"
                );
            }
        }
        for body in [
            "let result=42\n  result",
            "result=20\n  let result=result+22\n  return result",
        ] {
            let declarations = parse(&format!("proc main():auto =\n  {body}\n"));
            assert!(
                matches!(&declarations[0], Decl::Function { ret: Typ::Int, .. }),
                "{declarations:?}"
            );
            assert!(!format!("{declarations:?}").contains("#unsupported"));
        }
    }
    #[test]
    fn shadowing_does_not_enable_duplicate_locals_or_immutable_assignment() {
        for body in [
            "let foo_bar=42\n  fooBar=0\n  foo_bar",
            "const foo_bar=42\n  fooBar=0\n  foo_bar",
            "let foo_bar=20\n  let fooBar=22\n  foo_bar",
        ] {
            let declarations = parse(&format!("proc work(fooBar:int):int =\n  {body}\n"));
            assert!(
                format!("{declarations:?}").contains("#unsupported-nim"),
                "{declarations:?}"
            );
        }
        for ret in ["int", "auto"] {
            for body in [
                "result=20\n  let result=result+22\n  result",
                "result=20\n  42",
            ] {
                let declarations = parse(&format!("proc work():{ret} =\n  {body}\n"));
                assert!(
                    format!("{declarations:?}").contains("#unsupported-nim"),
                    "{declarations:?}"
                );
            }
        }
        let declarations = parse("proc work(rEs_Ult:int):int =\n  42\n");
        assert!(format!("{declarations:?}").contains("#unsupported-nim-result-shadow"));
    }
}
