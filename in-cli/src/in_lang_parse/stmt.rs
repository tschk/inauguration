use super::expr::parse_expr_with_context;
use super::types::parse_in_type;
use super::util::*;
use crate::core_ir::{Expr, LoopKind, Stmt};

pub(crate) struct LoopParseContext<'a> {
    source: &'a str,
    counter: usize,
    pub(crate) error: Option<String>,
}

impl<'a> LoopParseContext<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self {
            source,
            counter: 0,
            error: None,
        }
    }

    fn fresh_counter(&mut self) -> usize {
        while [
            format!("__for_{}", self.counter),
            format!("__for_end_{}", self.counter),
        ]
        .iter()
        .any(|name| self.source.contains(name))
        {
            self.counter += 1;
        }
        let counter = self.counter;
        self.counter += 1;
        counter
    }
}

fn parse_let_stmt_with_context(
    s: &str,
    context: &mut LoopParseContext<'_>,
) -> Result<Stmt, String> {
    let declaration = trim(s);
    let rest = declaration
        .strip_prefix("let ")
        .or_else(|| declaration.strip_prefix("var "))
        .ok_or_else(|| ".in: internal binding parse".to_string())?;
    let rest = trim(rest);
    let eq_pos = rest
        .find('=')
        .ok_or_else(|| ".in: local binding needs `=`".to_string())?;
    let lhs = trim(&rest[..eq_pos]);
    let rhs = trim(&rest[eq_pos + 1..]);
    let (name, typ) = if let Some(colon) = lhs.rfind(':') {
        let name_part = trim(&lhs[..colon]);
        let ty_part = trim(&lhs[colon + 1..]);
        if name_part.is_empty() {
            return Err(".in: local binding name missing".into());
        }
        (name_part.to_string(), Some(parse_in_type(ty_part)))
    } else {
        if lhs.is_empty() {
            return Err(".in: local binding name missing".into());
        }
        (lhs.to_string(), None)
    };
    Ok(Stmt::Let(name, typ, parse_expr_with_context(rhs, context)))
}

fn parse_return_stmt_with_context(
    s: &str,
    context: &mut LoopParseContext<'_>,
) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("return")
        .ok_or_else(|| ".in: internal return parse".to_string())?;
    let rest = trim(rest);
    if rest.is_empty() || rest == ";" {
        return Ok(Stmt::Return(None));
    }
    Ok(Stmt::Return(Some(parse_expr_with_context(rest, context))))
}

/// Compound operators reuse assignment lowering; indexed targets spill their place once.
const COMPOUND_ASSIGN_OPS: [&str; 10] =
    ["<<=", ">>=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^="];

fn find_compound_assign_op(s: &str) -> Option<(usize, &'static str)> {
    find_top_level_ops(s, &COMPOUND_ASSIGN_OPS)
        .into_iter()
        .next()
        .map(|(op, index)| (index, op))
}

fn compound_binary(op: &str, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op: op.strip_suffix('=').unwrap().to_string(),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

#[cfg(test)]
pub(crate) fn parse_assign_stmt(s: &str) -> Option<Stmt> {
    let mut context = LoopParseContext::new(s);
    parse_assign_stmt_with_context(s, &mut context)
}

fn parse_assign_stmt_with_context(s: &str, context: &mut LoopParseContext<'_>) -> Option<Stmt> {
    if let Some((op_idx, op)) = find_compound_assign_op(s) {
        let target = trim(&s[..op_idx]);
        let rhs = parse_expr_with_context(trim(&s[op_idx + op.len()..]), context);
        return match parse_expr_with_context(target, context) {
            Expr::Index { base, index } => Some(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::INDEX_MODIFY_PLACE_FIRST.into())),
                args: vec![
                    *base,
                    *index,
                    rhs,
                    Expr::StringLit(op.strip_suffix('=').unwrap().into()),
                ],
            })),
            Expr::Field { base, name: field } => Some(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::FIELD_MODIFY_PLACE_FIRST.into())),
                args: vec![
                    *base,
                    Expr::StringLit(field),
                    rhs,
                    Expr::StringLit(op.strip_suffix('=').unwrap().into()),
                ],
            })),
            Expr::Ident(name) => {
                let read = Expr::Ident(name.clone());
                Some(Stmt::Assign(name, compound_binary(op, read, rhs)))
            }
            _ => None,
        };
    }
    let eq_pos = find_top_level_ops(s, &["==", "!=", "<=", ">=", "=>", "="])
        .into_iter()
        .find_map(|(op, index)| (op == "=").then_some(index))?;
    let name = trim(&s[..eq_pos]);
    if name.is_empty() {
        return None;
    }
    let value = parse_expr_with_context(trim(&s[eq_pos + 1..]), context);
    match parse_expr_with_context(name, context) {
        Expr::Index { base, index, .. } => Some(Stmt::IndexAssign {
            base: *base,
            index: *index,
            value,
        }),
        Expr::Field { base, name: field } => Some(Stmt::FieldAssign {
            base: *base,
            name: field,
            value,
        }),
        Expr::Ident(name) if !name.contains(char::is_whitespace) => Some(Stmt::Assign(name, value)),
        _ => None,
    }
}

fn parse_if_stmt(s: &str, context: &mut LoopParseContext<'_>) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("if ")
        .ok_or_else(|| ".in: internal if parse".to_string())?;
    let open =
        find_struct_init_open_brace(rest).ok_or_else(|| ".in: `if` needs `{` body".to_string())?;
    let cond = parse_expr_with_context(trim(&rest[..open]), context);
    let (then_inner, then_close) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `if` body".to_string())?;
    let tail = trim(&rest[then_close + 1..]);
    let else_body = if let Some(else_rest) = tail.strip_prefix("else") {
        let else_rest = trim(else_rest);
        if else_rest.starts_with("if ") {
            vec![parse_if_stmt(else_rest, context)?]
        } else {
            let open = else_rest
                .find('{')
                .ok_or_else(|| ".in: `else` needs `{` body".to_string())?;
            let (else_inner, _) = brace_content_bounds_after_open(else_rest, open)
                .ok_or_else(|| ".in: unclosed `else` body".to_string())?;
            parse_function_body_with_context(else_inner, context)?
        }
    } else {
        Vec::new()
    };
    Ok(Stmt::If {
        cond,
        then_body: parse_function_body_with_context(then_inner, context)?,
        else_body,
    })
}

fn parse_while_stmt(s: &str, context: &mut LoopParseContext<'_>) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("while ")
        .ok_or_else(|| ".in: internal while parse".to_string())?;
    let open = find_struct_init_open_brace(rest)
        .ok_or_else(|| ".in: `while` needs `{` body".to_string())?;
    let cond = parse_expr_with_context(trim(&rest[..open]), context);
    let (inner, _) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `while` body".to_string())?;
    Ok(Stmt::Loop {
        kind: LoopKind::While,
        cond: Some(cond),
        body: parse_function_body_with_context(inner, context)?,
    })
}

fn parse_match_stmt(s: &str, context: &mut LoopParseContext<'_>) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("match ")
        .ok_or_else(|| ".in: internal match parse".to_string())?;
    let open = find_struct_init_open_brace(rest)
        .ok_or_else(|| ".in: `match` needs `{` body".to_string())?;
    let scrutinee = parse_expr_with_context(trim(&rest[..open]), context);
    let (inner, _) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `match` body".to_string())?;
    let arms = parse_match_arms(inner, context)?;
    Ok(Stmt::Match { scrutinee, arms })
}

fn parse_match_arms(
    inner: &str,
    context: &mut LoopParseContext<'_>,
) -> Result<Vec<crate::core_ir::MatchArm>, String> {
    parse_match_arm_parts(inner, context, |pattern, guard, body, context| {
        Ok(crate::core_ir::MatchArm {
            pattern,
            guard,
            body: parse_function_body_with_context(body, context)?,
        })
    })
}

pub(crate) fn parse_match_value_arms(
    inner: &str,
    context: &mut LoopParseContext<'_>,
) -> Result<Vec<crate::core_ir::MatchValueArm>, String> {
    parse_match_arm_parts(inner, context, |pattern, guard, body, context| {
        Ok(crate::core_ir::MatchValueArm {
            pattern,
            guard,
            value: parse_expr_with_context(&format!("{{{body}}}"), context),
        })
    })
}

/// Locate the body after an optional struct pattern without consuming an identifier arm's body.
fn find_match_arm_body_open(rest: &str) -> Result<usize, String> {
    let open = find_struct_init_open_brace(rest)
        .ok_or_else(|| ".in: match arm needs `{` body".to_string())?;
    let (_, close) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed match arm pattern or body".to_string())?;
    let tail = rest[close + 1..].trim_start();
    let continuation = tail.starts_with('{')
        || tail.starts_with("->")
        || tail.starts_with(':')
        || tail.starts_with("if ");
    if continuation
        && matches!(
            crate::core_ir::MatchPattern::parse(&rest[..=close]),
            Ok(crate::core_ir::MatchPattern::StructPat { .. })
        )
    {
        let after_pattern = close + 1;
        return find_struct_init_open_brace(&rest[after_pattern..])
            .map(|body| after_pattern + body)
            .ok_or_else(|| ".in: match arm needs `{` body after struct pattern".to_string());
    }
    Ok(open)
}

fn parse_match_arm_parts<T>(
    inner: &str,
    context: &mut LoopParseContext<'_>,
    mut build: impl FnMut(String, Option<Expr>, &str, &mut LoopParseContext<'_>) -> Result<T, String>,
) -> Result<Vec<T>, String> {
    let mut arms = Vec::new();
    let mut pos = 0usize;
    while pos < inner.len() {
        let rest = inner[pos..].trim_start();
        if rest.is_empty() {
            break;
        }
        let skipped = inner[pos..].len() - rest.len();
        pos += skipped;
        let rel_open = find_match_arm_body_open(&inner[pos..])?;
        let open = pos + rel_open;
        let mut pattern = trim(&inner[pos..open])
            .trim_end_matches(':')
            .trim()
            .to_string();
        if pattern.ends_with("->") {
            pattern = pattern[..pattern.len() - 2].trim().to_string();
        }
        if pattern.is_empty() {
            return Err(".in: match arm pattern missing".into());
        }
        // Quoted patterns containing ` if ` are not guard separators.
        let guard = if let Some((_, index)) = find_top_level_binary_op(&pattern, &[" if "]) {
            let guard = parse_expr_with_context(trim(&pattern[index + 4..]), context);
            pattern = trim(&pattern[..index]).to_string();
            Some(guard)
        } else {
            None
        };
        crate::core_ir::MatchPattern::parse(&pattern)
            .map_err(|_| format!(".in: unknown pattern `{pattern}` in match arm"))?;
        let (body_inner, close) = brace_content_bounds_after_open(inner, open)
            .ok_or_else(|| ".in: unclosed match arm body".to_string())?;
        arms.push(build(pattern, guard, body_inner, context)?);
        pos = close + 1;
        while pos < inner.len() {
            let Some(ch) = inner[pos..].chars().next() else {
                break;
            };
            if ch.is_whitespace() || ch == ';' || ch == ',' || ch == '}' {
                pos += ch.len_utf8();
            } else {
                break;
            }
        }
    }
    Ok(arms)
}

fn parse_throw_stmt_with_context(
    s: &str,
    context: &mut LoopParseContext<'_>,
) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("throw")
        .ok_or_else(|| ".in: internal throw parse".to_string())?;
    let rest = trim(rest);
    if rest.is_empty() {
        return Err(".in: `throw` needs an expression".into());
    }
    Ok(Stmt::Throw(parse_expr_with_context(rest, context)))
}

fn parse_try_stmt(s: &str, context: &mut LoopParseContext<'_>) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("try ")
        .or_else(|| trim(s).strip_prefix("try{"))
        .or_else(|| if trim(s) == "try" { Some("") } else { None })
        .ok_or_else(|| ".in: internal try parse".to_string())?;
    if trim(s).starts_with("try{") {
        let rest = format!("{{{rest}");
        let rest_str = rest.as_str();
        return parse_try_stmt_inner(rest_str, context);
    }
    let rest = trim(rest);
    if !rest.starts_with('{') {
        return Err(".in: `try` needs `{` body".into());
    }
    parse_try_stmt_inner(rest, context)
}

fn parse_try_stmt_inner(rest: &str, context: &mut LoopParseContext<'_>) -> Result<Stmt, String> {
    let (body_inner, close) = brace_content_bounds_after_open(rest, 0)
        .ok_or_else(|| ".in: unclosed `try` body".to_string())?;
    let mut catches = Vec::new();
    let mut pos = close + 1;
    while pos < rest.len() {
        let tail = rest[pos..].trim_start();
        let Some(catch_rest) = tail.strip_prefix("catch") else {
            break;
        };
        let catch_rest = catch_rest.trim_start();
        if catch_rest.is_empty() {
            break;
        }
        let open_rel = find_struct_init_open_brace(catch_rest)
            .ok_or_else(|| ".in: `catch` needs `{` body".to_string())?;
        let raw_pattern = trim(&catch_rest[..open_rel]);
        let pattern = raw_pattern
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            .map(str::trim)
            .unwrap_or(raw_pattern);
        if pattern.is_empty() {
            return Err(".in: `catch` pattern missing".into());
        }
        // Keeping the suffix end preserves byte offsets through inter-catch whitespace.
        let catch_start = rest.len() - catch_rest.len();
        let (catch_inner, catch_close) = brace_content_bounds_after_open(catch_rest, open_rel)
            .ok_or_else(|| ".in: unclosed `catch` body".to_string())?;
        catches.push(crate::core_ir::CatchArm {
            pattern: pattern.to_string(),
            body: parse_function_body_with_context(catch_inner, context)?,
        });
        pos = catch_start + catch_close + 1;
    }
    Ok(Stmt::Try {
        body: parse_function_body_with_context(body_inner, context)?,
        catches,
    })
}

pub(crate) fn parse_stmt_line(
    line: &str,
    context: &mut LoopParseContext<'_>,
) -> Result<Stmt, String> {
    let s = trim(line);
    if s.is_empty() {
        return Err(".in: empty statement".into());
    }
    if s == "propagate" {
        return Ok(Stmt::Propagate);
    }
    if s.starts_with("let ") || s.starts_with("var ") {
        return parse_let_stmt_with_context(s, context);
    }
    if s.starts_with("if ") {
        return parse_if_stmt(s, context);
    }
    if s.starts_with("while ") {
        return parse_while_stmt(s, context);
    }
    if s.starts_with("break")
        && (s.len() == 5
            || s.as_bytes()
                .get(5)
                .map_or(true, |&c| c == b' ' || c == b';' || c == b'}'))
    {
        return Ok(Stmt::Break);
    }
    if s.starts_with("continue")
        && (s.len() == 8
            || s.as_bytes()
                .get(8)
                .map_or(true, |&c| c == b' ' || c == b';' || c == b'}'))
    {
        return Ok(Stmt::Continue);
    }
    // `for` is handled by parse_function_body expansion
    if s.starts_with("for ") {
        return Err(
            ".in: unexpected `for` statement (should be expanded by parse_function_body)".into(),
        );
    }
    if s.starts_with("match ") {
        return parse_match_stmt(s, context);
    }
    if s.starts_with("try ") || s.starts_with("try{") || s == "try" {
        return parse_try_stmt(s, context);
    }
    if s.starts_with("throw")
        && (s.len() == 5 || s.chars().nth(5).is_some_and(|c| c.is_whitespace()))
    {
        return parse_throw_stmt_with_context(s, context);
    }
    if s.starts_with("return")
        && (s.len() == 6 || s.chars().nth(6).is_some_and(|c| c.is_whitespace()))
    {
        return parse_return_stmt_with_context(s, context);
    }
    if let Some(assign) = parse_assign_stmt_with_context(s, context) {
        return Ok(assign);
    }
    Ok(Stmt::Expr(parse_expr_with_context(s, context)))
}

pub(crate) fn parse_function_body(inner: &str) -> Result<Vec<Stmt>, String> {
    let mut context = LoopParseContext::new(inner);
    parse_function_body_with_context(inner, &mut context)
}

pub(crate) fn parse_function_body_with_context(
    inner: &str,
    context: &mut LoopParseContext<'_>,
) -> Result<Vec<Stmt>, String> {
    let mut stmts = Vec::new();
    for part in split_function_statements(inner) {
        if trim(&part).starts_with("for ") {
            stmts.extend(parse_for_expanded(&part, context)?);
        } else {
            stmts.push(parse_stmt_line(&part, context)?);
        }
    }
    if let Some(error) = context.error.take() {
        return Err(error);
    }
    Ok(stmts)
}

/// Parse a range or collection loop using the shared Core IR expansion.
fn parse_for_expanded(s: &str, context: &mut LoopParseContext<'_>) -> Result<Vec<Stmt>, String> {
    let rest = trim(s)
        .strip_prefix("for ")
        .ok_or_else(|| ".in: internal for parse".to_string())?;
    let open =
        find_struct_init_open_brace(rest).ok_or_else(|| ".in: `for` needs `{` body".to_string())?;
    let header = trim(&rest[..open]);
    let parts: Vec<&str> = header.splitn(2, " in ").collect();
    if parts.len() != 2 {
        return Err(".in: `for` needs `<var> in <iterator>`".into());
    }
    let var_name = trim(parts[0]).to_string();
    if var_name.is_empty() {
        return Err(".in: `for` loop variable name missing".into());
    }
    let iterator = trim(parts[1]);
    if iterator.is_empty() {
        return Err(".in: `for` iterator missing".into());
    }
    let iter = if let Some((_, pos)) = find_top_level_binary_op(iterator, &[".."]) {
        let start = trim(&iterator[..pos]);
        if start.is_empty() {
            return Err(".in: a range iterator needs a start value".into());
        }
        let end = trim(&iterator[pos + 2..]);
        let (mut range_name, end) = match end.strip_prefix('=') {
            Some(end) => ("range-inclusive", trim(end)),
            None => ("range-exclusive", end),
        };
        let mut args = vec![parse_expr_with_context(start, context)];
        if end.is_empty() {
            if range_name == "range-inclusive" {
                return Err(".in: an inclusive range needs an end value".into());
            }
            range_name = "range-from";
        } else {
            args.push(parse_expr_with_context(end, context));
        }
        Expr::Call {
            callee: Box::new(Expr::Ident(range_name.into())),
            args,
        }
    } else {
        parse_expr_with_context(iterator, context)
    };
    let (inner, _) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `for` body".to_string())?;
    let body = parse_function_body_with_context(inner, context)?;
    let mut counter = context.fresh_counter();
    Ok(crate::lower_core::expand_for_loop(
        var_name,
        iter,
        body,
        &mut counter,
    ))
}

#[cfg(test)]
mod match_arm_boundary_tests {
    use super::find_match_arm_body_open;

    #[test]
    fn unclosed_pattern_reports_boundary_error() {
        assert_eq!(
            find_match_arm_body_open("Entry{text:\"a,b\"").unwrap_err(),
            ".in: unclosed match arm pattern or body"
        );
    }
}

#[cfg(test)]
mod for_header_boundary_tests {
    use super::*;

    #[test]
    fn iterator_headers_skip_quoted_braces_before_the_body() {
        for (source, iterator) in [
            (
                r#"for item in ["a{b","c"] {42;}"#,
                Expr::ArrayLit(vec![
                    Expr::StringLit("a{b".into()),
                    Expr::StringLit("c".into()),
                ]),
            ),
            (
                r#"for item in values("a{b") {42;}"#,
                Expr::Call {
                    callee: Box::new(Expr::Ident("values".into())),
                    args: vec![Expr::StringLit("a{b".into())],
                },
            ),
            (
                r#"for item in values("a\"{b}") {42;}"#,
                Expr::Call {
                    callee: Box::new(Expr::Ident("values".into())),
                    args: vec![Expr::StringLit("a\"{b}".into())],
                },
            ),
        ] {
            let mut context = LoopParseContext::new(source);
            let actual = parse_for_expanded(source, &mut context).unwrap();
            let expected = crate::lower_core::expand_for_loop(
                "item".into(),
                iterator,
                vec![Stmt::Expr(Expr::IntLit(42))],
                &mut 0,
            );
            assert_eq!(actual, expected, "{source}");
            assert!(context.error.is_none(), "{source}");
        }
    }

    #[test]
    fn iterator_quoted_braces_survive_canonical_roundtrip() {
        let original = "fn values(text:String)->[Int]{return [20,22];}\nfn main()->Int{let sum=0;for value in values(\"a{b\") {sum=sum+value;}return sum;}";
        let parsed = crate::in_lang_parse::parse_in_source(original).unwrap();
        let canonical = crate::in_canonical::canonicalize_in_source(original).unwrap();
        assert_eq!(
            crate::in_lang_parse::parse_in_source(&canonical).unwrap(),
            parsed
        );
    }
}

#[cfg(test)]
mod compound_field_marker_tests {
    use super::*;

    #[test]
    fn compound_field_assignment_keeps_one_place_and_rhs() {
        let Some(Stmt::Expr(Expr::Call { callee, args })) =
            parse_assign_stmt("rows[at()].value += next()")
        else {
            panic!("missing compound field marker")
        };
        assert!(
            matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::FIELD_MODIFY_PLACE_FIRST)
        );
        assert!(
            matches!(args.as_slice(),[Expr::Index {..},Expr::StringLit(field),Expr::Call {..},Expr::StringLit(op)] if field=="value" && op=="+")
        );
    }
}

#[cfg(test)]
mod private_index_modify_marker_tests {
    use super::*;
    #[test]
    fn generated_indexed_compounds_have_one_private_place_marker() {
        assert!(
            matches!(parse_assign_stmt("values[at()] += next()"),Some(Stmt::Expr(Expr::Call{callee,args})) if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::INDEX_MODIFY_PLACE_FIRST) && args.len()==4)
        );
    }
}
