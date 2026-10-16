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
    assignment_kinds: &["assignment", "augmented_assignment"],
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
    number_decoder: Some(python_integer_literal),
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

// Python rejects nonzero decimals with leading zeros and L suffixes.
// Radix prefixes allow one following underscore; values outside the signed
// 64-bit carrier remain explicitly unsupported.
fn python_integer_literal(raw: &str) -> Option<crate::core_ir::Expr> {
    let (digits, radix) =
        if let Some(digits) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
            (digits.strip_prefix('_').unwrap_or(digits), 16)
        } else if let Some(digits) = raw.strip_prefix("0b").or_else(|| raw.strip_prefix("0B")) {
            (digits.strip_prefix('_').unwrap_or(digits), 2)
        } else if let Some(digits) = raw.strip_prefix("0o").or_else(|| raw.strip_prefix("0O")) {
            (digits.strip_prefix('_').unwrap_or(digits), 8)
        } else {
            (raw, 10)
        };
    let mut needs_digit = true;
    for byte in digits.bytes() {
        if byte == b'_' {
            if needs_digit {
                return None;
            }
            needs_digit = true;
        } else {
            let value = match byte {
                b'0'..=b'9' => u32::from(byte - b'0'),
                b'a'..=b'f' => u32::from(byte - b'a') + 10,
                b'A'..=b'F' => u32::from(byte - b'A') + 10,
                _ => return None,
            };
            if value >= radix {
                return None;
            }
            needs_digit = false;
        }
    }
    if needs_digit {
        return None;
    }
    let compact = digits.replace('_', "");
    if radix == 10 && compact.starts_with('0') && compact.bytes().any(|byte| byte != b'0') {
        return None;
    }
    i64::from_str_radix(&compact, radix)
        .ok()
        .map(crate::core_ir::Expr::IntLit)
}

// Python evaluates a scalar target's old value before the RHS. Computed
// targets need a retained place and Python-owned in-place object semantics;
// they must not fall through to the generic first-expression fallback.
pub(super) fn augmented_assignment(src: &[u8], node: Node<'_>) -> Stmt {
    let unsupported = || {
        Stmt::Expr(crate::core_ir::Expr::Call {
            callee: Box::new(crate::core_ir::Expr::Ident(
                crate::core_ir::UNSUPPORTED_DEFINED_BODY.into(),
            )),
            args: vec![],
        })
    };
    if node.has_error() {
        return unsupported();
    }
    let (Some(left), Some(right), Some(operator)) = (
        node.child_by_field_name("left"),
        node.child_by_field_name("right"),
        node.child_by_field_name("operator"),
    ) else {
        return unsupported();
    };
    if left.kind() != "identifier" {
        return unsupported();
    }
    // Division, floor division, modulo and power require Python numeric
    // semantics. Shifts also need Python's negative/large-count checks.
    let op = match node_txt(src, operator).trim() {
        "+=" => "+",
        "-=" => "-",
        "*=" => "*",
        "&=" => "&",
        "|=" => "|",
        "^=" => "^",
        _ => return unsupported(),
    };
    let Some(rhs) = augmented_rhs(src, right) else {
        return unsupported();
    };
    let name = node_txt(src, left).trim().to_string();
    Stmt::Assign(
        name.clone(),
        crate::core_ir::Expr::Binary {
            op: op.into(),
            lhs: Box::new(crate::core_ir::Expr::Ident(name)),
            rhs: Box::new(rhs),
        },
    )
}

// Deliberately recursive admission: the generic expression fallback projects
// unknown syntax onto its first child, which cannot preserve RHS effects.
fn augmented_rhs(src: &[u8], node: Node<'_>) -> Option<crate::core_ir::Expr> {
    use crate::core_ir::{Expr, FloatVal, Typ};
    if node.has_error() || node.is_missing() {
        return None;
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect();
    match node.kind() {
        "identifier" if children.is_empty() => Some(Expr::Ident(node_txt(src, node).into())),
        "integer" if children.is_empty() => ast_expr(src, node, PYTHON_AST),
        "float" if children.is_empty() => node_txt(src, node)
            .replace('_', "")
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| Expr::FloatLit(FloatVal(value))),
        "true" => Some(Expr::BoolLit(true)),
        "false" => Some(Expr::BoolLit(false)),
        "string" => {
            // Escapes, interpolation and triple strings need Python's own
            // decoder; never silently reuse generic quote trimming for them.
            let raw = node_txt(src, node);
            let quote = *raw.as_bytes().first()?;
            if !matches!(quote, b'\'' | b'"')
                || raw.len() < 2
                || raw.as_bytes().last() != Some(&quote)
                || raw.contains('\\')
                || raw.starts_with("\"\"\"")
                || raw.starts_with("'''")
            {
                return None;
            }
            Some(Expr::StringLit(raw[1..raw.len() - 1].into()))
        }
        "parenthesized_expression" if children.len() == 1 => augmented_rhs(src, children[0]),
        "binary_operator" => {
            let left = node.child_by_field_name("left")?;
            let right = node.child_by_field_name("right")?;
            let op = node_txt(src, node.child_by_field_name("operator")?);
            if children.len() != 2 || !matches!(op, "+" | "-" | "*" | "&" | "|" | "^") {
                return None;
            }
            Some(Expr::Binary {
                op: op.into(),
                lhs: Box::new(augmented_rhs(src, left)?),
                rhs: Box::new(augmented_rhs(src, right)?),
            })
        }
        "comparison_operator" if children.len() == 2 => {
            let op = node_txt(src, node.child_by_field_name("operators")?);
            if !matches!(op, "==" | "!=" | "<" | "<=" | ">" | ">=") {
                return None;
            }
            let lhs = augmented_rhs(src, children[0])?;
            let rhs = augmented_rhs(src, children[1])?;
            let left = augmented_known_kind(&lhs)?;
            let right = augmented_known_kind(&rhs)?;
            let numeric =
                matches!(left, Typ::Int | Typ::Float) && matches!(right, Typ::Int | Typ::Float);
            let equality = matches!(op, "==" | "!=")
                && left == right
                && matches!(left, Typ::Bool | Typ::String);
            if !numeric && !equality {
                return None;
            }
            Some(Expr::Binary {
                op: op.into(),
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }
        "unary_operator" | "not_operator" if children.len() == 1 => {
            let argument = node.child_by_field_name("argument")?;
            let op = node_txt(src, node)
                .get(..argument.start_byte() - node.start_byte())?
                .trim();
            let mut expr = augmented_rhs(src, argument)?;
            if op == "not" {
                return augmented_truth_proven(&expr).then(|| Expr::Unary {
                    op: "!".into(),
                    expr: Box::new(expr),
                });
            }
            // Python's unary numeric operators promote Bool to Int. Known
            // literal Bool values can do so without duplicating evaluation.
            if let Expr::BoolLit(value) = expr {
                expr = Expr::IntLit(i64::from(value));
            }
            let kind = augmented_known_kind(&expr)?;
            if !matches!(kind, Typ::Int | Typ::Float) {
                return None;
            }
            match op {
                "+" => Some(expr),
                "-" => Some(Expr::Unary {
                    op: op.into(),
                    expr: Box::new(expr),
                }),
                "~" if kind == Typ::Int => Some(Expr::Unary {
                    op: op.into(),
                    expr: Box::new(expr),
                }),
                _ => None,
            }
        }
        "conditional_expression" if children.len() == 3 => {
            // !! keeps numeric truth normalization and evaluates the condition
            // once; branches retain lazy evaluation in the existing IR.
            let condition = augmented_rhs(src, children[1])?;
            if !augmented_truth_proven(&condition) {
                return None;
            }
            let condition = Expr::Unary {
                op: "!".into(),
                expr: Box::new(Expr::Unary {
                    op: "!".into(),
                    expr: Box::new(condition),
                }),
            };
            Some(Expr::Conditional {
                cond: Box::new(condition),
                then_expr: Box::new(augmented_rhs(src, children[0])?),
                else_expr: Box::new(augmented_rhs(src, children[2])?),
            })
        }
        "named_expression" if children.len() == 2 => {
            let name = node.child_by_field_name("name")?;
            if name.kind() != "identifier" {
                return None;
            }
            let name = node_txt(src, name).to_string();
            let value = augmented_rhs(src, node.child_by_field_name("value")?)?;
            // Assign updates an existing binding. A newly introduced walrus
            // binding fails existing scope validation rather than disappearing.
            Some(Expr::Block {
                body: vec![Stmt::Assign(name.clone(), value)],
                result: Some(Box::new(Expr::Ident(name))),
            })
        }
        "call" => {
            let callee = node.child_by_field_name("function")?;
            let args = node.child_by_field_name("arguments")?;
            if children.len() != 2
                || callee.kind() != "identifier"
                || args.kind() != "argument_list"
            {
                return None;
            }
            let mut cursor = args.walk();
            let args = args
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra())
                .map(|arg| augmented_rhs(src, arg))
                .collect::<Option<Vec<_>>>()?;
            Some(Expr::Call {
                callee: Box::new(Expr::Ident(node_txt(src, callee).into())),
                args,
            })
        }
        _ => None,
    }
}

// Without lexical type facts, never guess an identifier/call is numeric.
// Literals have exact scalar types, and admitted comparisons return Bool.
fn augmented_truth_proven(expr: &crate::core_ir::Expr) -> bool {
    use crate::core_ir::Expr;
    matches!(expr, Expr::IntLit(_) | Expr::FloatLit(_) | Expr::BoolLit(_))
        || matches!(expr, Expr::Binary { op, .. } if matches!(op.as_str(), "==" | "!=" | "<" | "<=" | ">" | ">="))
        || matches!(expr, Expr::Unary { op, expr } if op=="!" && augmented_truth_proven(expr))
}

// A proof for the locally represented expression, not a guess about unknown
// source identifiers, callback results, or nominal objects.
fn augmented_known_kind(expr: &crate::core_ir::Expr) -> Option<Typ> {
    use crate::core_ir::Expr;
    match expr {
        Expr::IntLit(_) => Some(Typ::Int),
        Expr::FloatLit(_) => Some(Typ::Float),
        Expr::BoolLit(_) => Some(Typ::Bool),
        Expr::StringLit(_) => Some(Typ::String),
        Expr::Unary { op, expr } if op == "-" => {
            augmented_known_kind(expr).filter(|kind| matches!(kind, Typ::Int | Typ::Float))
        }
        Expr::Unary { op, expr } if op == "~" => {
            augmented_known_kind(expr).filter(|kind| *kind == Typ::Int)
        }
        Expr::Unary { op, .. } if op == "!" => Some(Typ::Bool),
        Expr::Binary { op, lhs, rhs } => {
            let left = augmented_known_kind(lhs)?;
            let right = augmented_known_kind(rhs)?;
            if matches!(op.as_str(), "==" | "!=" | "<" | "<=" | ">" | ">=") {
                return Some(Typ::Bool);
            }
            if op == "+" && left == Typ::String && right == Typ::String {
                return Some(Typ::String);
            }
            if matches!(op.as_str(), "+" | "-" | "*")
                && matches!(left, Typ::Int | Typ::Float)
                && matches!(right, Typ::Int | Typ::Float)
            {
                return Some(if left == Typ::Float || right == Typ::Float {
                    Typ::Float
                } else {
                    Typ::Int
                });
            }
            if matches!(op.as_str(), "&" | "|" | "^") {
                return crate::core_ir::bitwise_result_type(Some(left), Some(right));
            }
            None
        }
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => {
            let left = augmented_known_kind(then_expr)?;
            (Some(left.clone()) == augmented_known_kind(else_expr)).then_some(left)
        }
        Expr::Block {
            result: Some(result),
            ..
        } => augmented_known_kind(result),
        _ => None,
    }
}

#[cfg(test)]
mod augmented_assignment_tests {
    use super::*;
    use crate::core_ir::{Expr, UnifiedModule};
    fn parse(source: &str) -> UnifiedModule {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        UnifiedModule::new(
            extract_python_with_classes(source.as_bytes(), tree.root_node()).unwrap(),
        )
    }
    fn body(module: &UnifiedModule) -> &[Stmt] {
        module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function { name, body, .. } if name == "main" => Some(body.as_slice()),
                _ => None,
            })
            .unwrap()
    }
    #[test]
    fn scalar_compounds_retain_old_value_rhs_and_assignment() {
        for (op, binary) in [
            ("+=", "+"),
            ("-=", "-"),
            ("*=", "*"),
            ("&=", "&"),
            ("|=", "|"),
            ("^=", "^"),
        ] {
            let source = format!(
                "def main(value:int)->int:
    value {op} next_value()
    return value
"
            );
            let module = parse(&source);
            assert!(
                matches!(&body(&module)[0],Stmt::Assign(name,Expr::Binary{op,lhs,rhs})
                if name=="value" && op==binary && matches!(lhs.as_ref(),Expr::Ident(name) if name=="value")
                && matches!(rhs.as_ref(),Expr::Call{callee,args} if args.is_empty() && matches!(callee.as_ref(),Expr::Ident(name) if name=="next_value"))),
                "{source}: {module:?}"
            );
        }
    }
    #[test]
    fn unsupported_compound_targets_and_python_specific_ops_refuse_explicitly() {
        for statement in [
            "item.value += effect()",
            "items[at()] += effect()",
            "value /= 2",
            "value //= 2",
            "value %= 2",
            "value **= 2",
            "value <<= 1",
            "value >>= 1",
            "value @= other",
        ] {
            let module = parse(&format!(
                "def main()->int:
    {statement}
    return 42
"
            ));
            assert!(
                matches!(&body(&module)[0],Stmt::Expr(Expr::Call{callee,..}) if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::UNSUPPORTED_DEFINED_BODY)),
                "{statement}: {module:?}"
            );
        }
    }
    #[test]
    fn scalar_operator_sources_preserve_value_through_normalization_and_profiles() {
        for (operator, initial, rhs) in [
            ("+=", 20, 22),
            ("-=", 64, 22),
            ("*=", 21, 2),
            ("&=", 127, 42),
            ("|=", 40, 2),
            ("^=", 40, 2),
        ] {
            let source = format!(
                "def adjust(value:int)->int:
    value {operator} {rhs}
    return value
def main()->int:
    return adjust({initial})
"
            );
            let module = crate::typecheck::normalize_module(
                crate::parser_registry::ParserId::Python,
                &parse(&source),
            );
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut candidate = module.clone();
                crate::core_opt::optimize_with_profile(&mut candidate.decls, Some("main"), profile);
                assert_eq!(
                    crate::native_emit::inisa::eval_module(&candidate, "main").unwrap(),
                    42,
                    "{source} {profile:?}"
                );
            }
        }
    }
    #[test]
    fn unary_numeric_literals_preserve_python_bool_promotion() {
        for (initial, rhs) in [
            (41, "+True"),
            (43, "-True"),
            (44, "~True"),
            (20, "+(10+12)"),
        ] {
            let source = format!(
                "def main()->int:
    value={initial}
    value += {rhs}
    return value
"
            );
            let module = crate::typecheck::normalize_module(
                crate::parser_registry::ParserId::Python,
                &parse(&source),
            );
            assert_eq!(
                crate::native_emit::inisa::eval_module(&module, "main").unwrap(),
                42,
                "{source}"
            );
        }
        let module = parse(
            "def main()->str:
    value='a'
    value += +'b'
    return value
",
        );
        assert!(
            matches!(&body(&module)[1],Stmt::Expr(Expr::Call{callee,..}) if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::UNSUPPORTED_DEFINED_BODY))
        );
    }
    #[test]
    fn unknown_rhs_nodes_never_project_into_supported_values() {
        for rhs in [
            "(lambda: 42)",
            "next_value(lambda: 42)",
            "(22 if False else (lambda: 42))",
            "[22]",
            "items[0]",
            "item.value",
            "next_value(value=22)",
            "next_value(*items)",
            "(10 // 2)",
            "(1 < 2 < 3)",
            "f'answer{value}'",
            "(22 if '' else 0)",
            "(22 if value else 0)",
            "(22 if next_value() else 0)",
            "(not '')",
            "+'b'",
            "-'b'",
            "+next_value()",
            "~1.5",
            "(22 if 'z' < 'a' else 2)",
            "(22 if value < 2 else 2)",
            r#""\x2a""#,
        ] {
            let module = parse(&format!(
                "def main()->int:\n    value=20\n    value += {rhs}\n    return value\n"
            ));
            assert!(
                matches!(&body(&module)[1],Stmt::Expr(Expr::Call{callee,..}) if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::UNSUPPORTED_DEFINED_BODY)),
                "{rhs}: {module:?}"
            );
        }
    }
    #[test]
    fn missing_compound_target_is_a_read_and_never_an_implicit_declaration() {
        let module = parse(
            "def main()->int:
    missing += 42
    return missing
",
        );
        assert!(matches!(&body(&module)[0],Stmt::Assign(name,_) if name=="missing"));
        assert!(!crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
    }
}

#[cfg(test)]
mod python_integer_literal_tests {
    use super::*;
    use crate::core_ir::Expr;
    #[test]
    fn python_integer_lexemes_preserve_radix_and_underscore_rules() {
        for (raw, expected) in [
            ("0", 0),
            ("00", 0),
            ("00_0", 0),
            ("0_0_0", 0),
            ("42", 42),
            ("4_2", 42),
            ("0x_2a", 42),
            ("0X_2A", 42),
            ("0b_101010", 42),
            ("0o_52", 42),
            ("0O52", 42),
            ("9223372036854775807", i64::MAX),
        ] {
            assert!(
                matches!(python_integer_literal(raw),Some(Expr::IntLit(value)) if value==expected),
                "{raw}"
            );
        }
        for raw in [
            "01",
            "0001",
            "00_1",
            "0_1",
            "1__0",
            "1_",
            "_1",
            "0x",
            "0x_",
            "0x__2a",
            "0x2_a_",
            "0b2",
            "0o8",
            "42L",
            "42l",
            "0x2AL",
            "0b10L",
            "9223372036854775808",
            "0xffffffffffffffff",
            "1j",
            "1.0",
            "1e3",
        ] {
            assert!(python_integer_literal(raw).is_none(), "{raw}");
        }
    }
    #[test]
    fn integer_decoder_reaches_plain_augmented_return_and_call_positions() {
        fn parse(source: &str) -> Vec<Decl> {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_python::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(source, None).unwrap();
            assert!(
                !tree.root_node().has_error(),
                "{}",
                tree.root_node().to_sexp()
            );
            extract_python_with_classes(source.as_bytes(), tree.root_node()).unwrap()
        }
        for source in [
            "def main():\n    value=01\n    return value\n",
            "def main():\n    value=0\n    value+=01\n    return value\n",
            "def main():\n    return 42L\n",
            "def main():\n    return take(00_1)\n",
        ] {
            let declarations = parse(source);
            assert!(
                format!("{declarations:?}").contains("#unsupported-number-literal"),
                "{source}: {declarations:?}"
            );
        }
        let declarations =
            parse("def main():\n    value=0_0\n    value+=0o_52\n    return value\n");
        let text = format!("{declarations:?}");
        assert!(!text.contains("#unsupported"), "{text}");
        assert!(text.contains("IntLit(42)"), "{text}");
    }
}
