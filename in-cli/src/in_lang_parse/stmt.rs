use super::expr::parse_expr;
use super::types::parse_in_type;
use super::util::*;
use crate::core_ir::{Expr, LoopKind, Stmt, Typ};

pub(crate) fn parse_let_stmt(s: &str) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("let ")
        .ok_or_else(|| ".in: internal let parse".to_string())?;
    let rest = trim(rest);
    let eq_pos = rest
        .find('=')
        .ok_or_else(|| ".in: `let` needs `=`".to_string())?;
    let lhs = trim(&rest[..eq_pos]);
    let rhs = trim(&rest[eq_pos + 1..]);
    let (name, typ) = if let Some(colon) = lhs.rfind(':') {
        let name_part = trim(&lhs[..colon]);
        let ty_part = trim(&lhs[colon + 1..]);
        if name_part.is_empty() {
            return Err(".in: `let` binding name missing".into());
        }
        (name_part.to_string(), Some(parse_in_type(ty_part)))
    } else {
        if lhs.is_empty() {
            return Err(".in: `let` binding name missing".into());
        }
        (lhs.to_string(), None)
    };
    Ok(Stmt::Let(name, typ, parse_expr(rhs)))
}

pub(crate) fn parse_return_stmt(s: &str) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("return")
        .ok_or_else(|| ".in: internal return parse".to_string())?;
    let rest = trim(rest);
    if rest.is_empty() || rest == ";" {
        return Ok(Stmt::Return(None));
    }
    Ok(Stmt::Return(Some(parse_expr(rest))))
}

pub(crate) fn parse_assign_stmt(s: &str) -> Option<Stmt> {
    let eq_pos = s.find('=')?;
    if s.get(eq_pos + 1..)
        .is_some_and(|tail| tail.starts_with('='))
    {
        return None;
    }
    if eq_pos > 0 && s.get(eq_pos - 1..eq_pos) == Some("!") {
        return None;
    }
    let name = trim(&s[..eq_pos]);
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let value = parse_expr(trim(&s[eq_pos + 1..]));
    match parse_expr(name) {
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
        _ => Some(Stmt::Assign(name.to_string(), value)),
    }
}

pub(crate) fn parse_if_stmt(s: &str) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("if ")
        .ok_or_else(|| ".in: internal if parse".to_string())?;
    let open = rest
        .find('{')
        .ok_or_else(|| ".in: `if` needs `{` body".to_string())?;
    let cond = parse_expr(trim(&rest[..open]));
    let (then_inner, then_close) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `if` body".to_string())?;
    let tail = trim(&rest[then_close + 1..]);
    let else_body = if let Some(else_rest) = tail.strip_prefix("else") {
        let else_rest = trim(else_rest);
        if else_rest.starts_with("if ") {
            vec![parse_if_stmt(else_rest)?]
        } else {
            let open = else_rest
                .find('{')
                .ok_or_else(|| ".in: `else` needs `{` body".to_string())?;
            let (else_inner, _) = brace_content_bounds_after_open(else_rest, open)
                .ok_or_else(|| ".in: unclosed `else` body".to_string())?;
            parse_function_body(else_inner)?
        }
    } else {
        Vec::new()
    };
    Ok(Stmt::If {
        cond,
        then_body: parse_function_body(then_inner)?,
        else_body,
    })
}

pub(crate) fn parse_while_stmt(s: &str) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("while ")
        .ok_or_else(|| ".in: internal while parse".to_string())?;
    let open = rest
        .find('{')
        .ok_or_else(|| ".in: `while` needs `{` body".to_string())?;
    let cond = parse_expr(trim(&rest[..open]));
    let (inner, _) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `while` body".to_string())?;
    Ok(Stmt::Loop {
        kind: LoopKind::While,
        cond: Some(cond),
        body: parse_function_body(inner)?,
    })
}

pub(crate) fn parse_match_stmt(s: &str) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("match ")
        .ok_or_else(|| ".in: internal match parse".to_string())?;
    let open = rest
        .find('{')
        .ok_or_else(|| ".in: `match` needs `{` body".to_string())?;
    let scrutinee = parse_expr(trim(&rest[..open]));
    let (inner, _) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `match` body".to_string())?;
    let arms = parse_match_arms(inner)?;
    Ok(Stmt::Match { scrutinee, arms })
}

pub(crate) fn parse_match_arms(inner: &str) -> Result<Vec<crate::core_ir::MatchArm>, String> {
    let mut arms = Vec::new();
    let mut pos = 0usize;
    while pos < inner.len() {
        let rest = inner[pos..].trim_start();
        if rest.is_empty() {
            break;
        }
        let skipped = inner[pos..].len() - rest.len();
        pos += skipped;
        let rel_open = inner[pos..]
            .find('{')
            .ok_or_else(|| ".in: match arm needs `{` body".to_string())?;
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
        crate::core_ir::MatchPattern::parse(&pattern)
            .map_err(|_| format!(".in: unknown pattern `{pattern}` in match arm"))?;
        let (body_inner, close) = brace_content_bounds_after_open(inner, open)
            .ok_or_else(|| ".in: unclosed match arm body".to_string())?;
        arms.push(crate::core_ir::MatchArm {
            pattern: pattern.to_string(),
            body: parse_function_body(body_inner)?,
        });
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

pub(crate) fn parse_throw_stmt(s: &str) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("throw")
        .ok_or_else(|| ".in: internal throw parse".to_string())?;
    let rest = trim(rest);
    if rest.is_empty() {
        return Err(".in: `throw` needs an expression".into());
    }
    Ok(Stmt::Throw(parse_expr(rest)))
}

pub(crate) fn parse_try_stmt(s: &str) -> Result<Stmt, String> {
    let rest = trim(s)
        .strip_prefix("try ")
        .or_else(|| trim(s).strip_prefix("try{"))
        .or_else(|| if trim(s) == "try" { Some("") } else { None })
        .ok_or_else(|| ".in: internal try parse".to_string())?;
    if trim(s).starts_with("try{") {
        let rest = format!("{{{rest}");
        let rest_str = rest.as_str();
        return parse_try_stmt_inner(rest_str);
    }
    let rest = trim(rest);
    if !rest.starts_with('{') {
        return Err(".in: `try` needs `{` body".into());
    }
    parse_try_stmt_inner(rest)
}

pub(crate) fn parse_try_stmt_inner(rest: &str) -> Result<Stmt, String> {
    let (body_inner, close) = brace_content_bounds_after_open(rest, 0)
        .ok_or_else(|| ".in: unclosed `try` body".to_string())?;
    let mut catches = Vec::new();
    let mut pos = close + 1;
    while pos < rest.len() {
        let tail = trim(&rest[pos..]);
        let Some(catch_rest) = tail.strip_prefix("catch") else {
            break;
        };
        let catch_rest = trim(catch_rest);
        if catch_rest.is_empty() {
            break;
        }
        let open_rel = catch_rest
            .find('{')
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
        let abs_open = pos + rest[pos..].find('{').expect("catch open brace");
        let (catch_inner, catch_close) = brace_content_bounds_after_open(rest, abs_open)
            .ok_or_else(|| ".in: unclosed `catch` body".to_string())?;
        catches.push(crate::core_ir::CatchArm {
            pattern: pattern.to_string(),
            body: parse_function_body(catch_inner)?,
        });
        pos = catch_close + 1;
    }
    Ok(Stmt::Try {
        body: parse_function_body(body_inner)?,
        catches,
    })
}

pub(crate) fn parse_stmt_line(line: &str) -> Result<Stmt, String> {
    let s = trim(line);
    if s.is_empty() {
        return Err(".in: empty statement".into());
    }
    if s.starts_with("let ") {
        return parse_let_stmt(s);
    }
    if s.starts_with("if ") {
        return parse_if_stmt(s);
    }
    if s.starts_with("while ") {
        return parse_while_stmt(s);
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
        return parse_match_stmt(s);
    }
    if s.starts_with("try ") || s.starts_with("try{") || s == "try" {
        return parse_try_stmt(s);
    }
    if s.starts_with("throw")
        && (s.len() == 5 || s.chars().nth(5).is_some_and(|c| c.is_whitespace()))
    {
        return parse_throw_stmt(s);
    }
    if s.starts_with("return")
        && (s.len() == 6 || s.chars().nth(6).is_some_and(|c| c.is_whitespace()))
    {
        return parse_return_stmt(s);
    }
    if let Some(assign) = parse_assign_stmt(s) {
        return Ok(assign);
    }
    Ok(Stmt::Expr(parse_expr(s)))
}

pub(crate) fn parse_function_body(inner: &str) -> Result<Vec<Stmt>, String> {
    let mut stmts = Vec::new();
    for part in split_function_statements(inner) {
        let trimmed = trim(&part);
        if trimmed.starts_with("for ") {
            // Expand for loop into multiple statements (let + while)
            stmts.extend(parse_for_expanded(&part)?);
        } else {
            stmts.push(parse_stmt_line(&part)?);
        }
    }
    Ok(stmts)
}

/// Parse a `for i in start..end { body }` and expand to:
///   let i = start
///   while i < end { body; i = i + 1 }
pub(crate) fn parse_for_expanded(s: &str) -> Result<Vec<Stmt>, String> {
    let rest = trim(s)
        .strip_prefix("for ")
        .ok_or_else(|| ".in: internal for parse".to_string())?;
    let open = rest
        .find('{')
        .ok_or_else(|| ".in: `for` needs `{` body".to_string())?;
    let header = trim(&rest[..open]);
    let parts: Vec<&str> = header.splitn(2, " in ").collect();
    if parts.len() != 2 {
        return Err(".in: `for` needs `<var> in <range>`".into());
    }
    let var_name = trim(parts[0]).to_string();
    if var_name.is_empty() {
        return Err(".in: `for` loop variable name missing".into());
    }
    let range_part = trim(parts[1]);
    let range_segs: Vec<&str> = range_part.splitn(2, "..").collect();
    if range_segs.len() != 2 {
        return Err(".in: `for` range needs `start..end`".into());
    }
    let start_expr = parse_expr(trim(range_segs[0]));
    let end_expr = parse_expr(trim(range_segs[1]));
    let (inner, _) = brace_content_bounds_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `for` body".to_string())?;
    let mut body = parse_function_body(inner)?;
    // Add i = i + 1 at end of body
    body.push(Stmt::Assign(
        var_name.to_string(),
        Expr::Binary {
            op: "+".to_string(),
            lhs: Box::new(Expr::Ident(var_name.to_string())),
            rhs: Box::new(Expr::IntLit(1)),
        },
    ));
    let while_loop = Stmt::Loop {
        kind: LoopKind::While,
        cond: Some(Expr::Binary {
            op: "<".to_string(),
            lhs: Box::new(Expr::Ident(var_name.clone())),
            rhs: Box::new(end_expr),
        }),
        body,
    };
    Ok(vec![
        Stmt::Let(var_name, Some(Typ::Int), start_expr),
        while_loop,
    ])
}
