use super::stmt::{LoopParseContext, parse_function_body, parse_function_body_with_context};
use super::types::{parse_in_type, parse_param};
use super::util::*;
use crate::core_ir::{Expr, FloatVal, Stmt, Typ};

fn value_block_ends_with_semicolon(inner: &str) -> bool {
    let mut chars = inner.chars().peekable();
    let (mut in_string, mut escape, mut comment) = (false, false, false);
    let mut last = None;
    while let Some(c) = chars.next() {
        if comment {
            if c == '\n' {
                comment = false;
            }
            continue;
        }
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
                last = Some('"');
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'/') {
            chars.next();
            comment = true;
        } else if c == '"' {
            in_string = true;
        } else if !c.is_whitespace() {
            last = Some(c);
        }
    }
    last == Some(';')
}

fn try_parse_control_value(s: &str, context: &mut LoopParseContext<'_>) -> Option<Expr> {
    if let Some(rest) = s.strip_prefix("match ") {
        let rest = trim(rest);
        let open = find_struct_init_open_brace(rest)?;
        let (inner, close) = brace_content_bounds_after_open(rest, open)?;
        if !trim(&rest[close + 1..]).is_empty() {
            return None;
        }
        let arms = match super::stmt::parse_match_value_arms(inner, context) {
            Ok(arms) => arms,
            Err(error) => {
                context.error = Some(error);
                return Some(Expr::Call {
                    callee: Box::new(Expr::Ident("__unsupported_in_match_value".into())),
                    args: vec![],
                });
            }
        };
        return Some(Expr::MatchValue {
            scrutinee: Box::new(parse_expr_with_context(&rest[..open], context)),
            arms,
        });
    }
    if let Some(rest) = s
        .strip_prefix("if ")
        .or_else(|| s.strip_prefix("if(").map(|_| &s[2..]))
    {
        let rest = trim(rest);
        let open = find_struct_init_open_brace(rest)?;
        let condition = trim(&rest[..open]);
        let (_, close) = brace_content_bounds_after_open(rest, open)?;
        let tail = trim(&rest[close + 1..]).strip_prefix("else")?;
        let tail = trim(tail);
        let then_expr = try_parse_control_value(&rest[open..=close], context)?;
        let else_expr = try_parse_control_value(tail, context)?;
        if condition.is_empty() {
            return None;
        }
        return Some(Expr::Conditional {
            cond: Box::new(parse_expr_with_context(condition, context)),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
        });
    }
    if !s.starts_with('{') {
        return None;
    }
    let (inner, close) = brace_content_bounds_after_open(s, 0)?;
    if !trim(&s[close + 1..]).is_empty() {
        return None;
    }
    let mut parts = split_function_statements(inner);
    let mut result = None;
    if !value_block_ends_with_semicolon(inner)
        && let Some(last) = parts.last()
    {
        let value = if trim(last).starts_with("if ")
            || trim(last).starts_with("if(")
            || trim(last).starts_with("match ")
        {
            try_parse_control_value(last, context)
        } else {
            match super::stmt::parse_stmt_line(last, context) {
                Ok(Stmt::Expr(value)) => Some(value),
                Ok(_) => None,
                Err(error) => {
                    context.error = Some(error);
                    None
                }
            }
        };
        if let Some(value) = value {
            result = Some(Box::new(value));
            parts.pop();
        }
    }
    let body = match parse_function_body_with_context(&parts.join(";\n"), context) {
        Ok(body) => body,
        Err(error) => {
            context.error = Some(error);
            return Some(Expr::Call {
                callee: Box::new(Expr::Ident("__unsupported_in_block_value".into())),
                args: vec![],
            });
        }
    };
    Some(Expr::Block { body, result })
}

pub(crate) fn parse_expr(s: &str) -> Expr {
    parse_expr_with_context(s, &mut LoopParseContext::new(s))
}

pub(crate) fn parse_expr_with_context(s: &str, context: &mut LoopParseContext<'_>) -> Expr {
    let s = trim(s);
    if s == "()" {
        return Expr::UnitLit;
    }
    if let Some(inner) = strip_enclosing_parens(s) {
        if trim(inner).is_empty() {
            return Expr::UnitLit;
        }
        return parse_expr_with_context(inner, context);
    }
    if let Some(value) = try_parse_control_value(s, context) {
        return value;
    }
    if let Some((closure, tail)) = parse_closure_prefix(s) {
        if tail.is_empty() {
            return closure;
        }
        if tail.starts_with('(') {
            // Group the complete closure before ordinary postfix parsing, so
            // its return-type arrow cannot be mistaken for a binary operator.
            let prefix = &s[..s.len() - tail.len()];
            return parse_expr_with_context(&format!("({prefix}){tail}"), context);
        }
    }
    if s.contains(['.', 'e', 'E'])
        && let Ok(value) = s.parse::<f64>()
    {
        return Expr::FloatLit(FloatVal(value));
    }
    let operators = find_top_level_ops(
        s,
        &[
            "||", "&&", "|", "^", "&", "==", "!=", ">=", "<=", ">", "<", "<<", ">>", "+", "-", "*",
            "/", "%",
        ],
    );
    for ops in [
        &["||"][..],
        &["&&"][..],
        &["|"][..],
        &["^"][..],
        &["&"][..],
        &["==", "!="][..],
        &[">=", "<=", ">", "<"][..],
        &["<<", ">>"][..],
        &["+", "-"][..],
        &["*", "/", "%"][..],
    ] {
        if let Some(&(op, idx)) = operators.iter().rev().find(|(op, _)| ops.contains(op)) {
            let lhs = trim(&s[..idx]);
            let rhs = trim(&s[idx + op.len()..]);
            if !lhs.is_empty() && !rhs.is_empty() {
                return Expr::Binary {
                    op: op.to_string(),
                    lhs: Box::new(parse_expr_with_context(lhs, context)),
                    rhs: Box::new(parse_expr_with_context(rhs, context)),
                };
            }
        }
    }
    if let Some(rest) = s.strip_prefix('!')
        && !trim(rest).is_empty()
    {
        return Expr::Unary {
            op: "!".into(),
            expr: Box::new(parse_expr_with_context(rest, context)),
        };
    }
    if let Some(rest) = s.strip_prefix('~')
        && !trim(rest).is_empty()
    {
        return Expr::Unary {
            op: "~".into(),
            expr: Box::new(parse_expr_with_context(rest, context)),
        };
    }
    if let Some(rest) = s.strip_prefix('-')
        && !trim(rest).is_empty()
        && rest.parse::<i64>().is_err()
    {
        // The positive magnitude of i64::MIN cannot be represented as an
        // IntLit. Recognize that exact magnitude before parsing unary minus.
        if crate::core_ir::parse_int_literal(&format!("-{}", trim(rest))) == Some(i64::MIN) {
            return Expr::IntLit(i64::MIN);
        }
        return Expr::Unary {
            op: "-".into(),
            expr: Box::new(parse_expr_with_context(rest, context)),
        };
    }
    if s == "true" {
        return Expr::BoolLit(true);
    }
    if s == "false" {
        return Expr::BoolLit(false);
    }
    if let Some(n) = crate::core_ir::parse_int_literal(s) {
        return Expr::IntLit(n);
    }
    if let Some(value) = crate::core_ir::decode_string_literal(s) {
        return Expr::StringLit(value);
    }
    if let Some(closure) = try_parse_closure_expr(s) {
        return closure;
    }
    if let Some(open) = find_top_level_index_open(s)
        && s.ends_with(']')
    {
        let base = trim(&s[..open]);
        let index = trim(&s[open + 1..s.len() - 1]);
        if !base.is_empty() && !index.is_empty() {
            return Expr::Index {
                base: Box::new(parse_expr_with_context(base, context)),
                index: Box::new(parse_expr_with_context(index, context)),
            };
        }
    }
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        return Expr::ArrayLit(
            split_call_args(inner)
                .into_iter()
                .map(|arg| parse_expr_with_context(&arg, context))
                .collect(),
        );
    }
    if let Some(open) = find_struct_init_open_brace(s)
        && s.ends_with('}')
    {
        let name = trim(&s[..open]);
        if !name.is_empty() {
            let inner = &s[open + 1..s.len() - 1];
            let fields = split_struct_init_fields(inner)
                .into_iter()
                .filter_map(|field| {
                    let (name, expr) = field.split_once(':')?;
                    Some((
                        trim(name).to_string(),
                        parse_expr_with_context(trim(expr), context),
                    ))
                })
                .collect();
            return Expr::StructInit {
                name: name.to_string(),
                fields,
            };
        }
    }
    if let Some(open) = find_call_open_paren(s)
        && s.ends_with(')')
    {
        let callee = trim(&s[..open]);
        if !callee.is_empty() {
            let inner = &s[open + 1..s.len() - 1];
            let mut args = split_call_args(inner)
                .into_iter()
                .map(|arg| parse_expr_with_context(&arg, context))
                .collect::<Vec<_>>();
            if let Some(dot) = find_top_level_field_dot(callee) {
                let base = trim(&callee[..dot]);
                let name = trim(&callee[dot + 1..]);
                if !base.is_empty() && !name.is_empty() {
                    args.insert(0, parse_expr_with_context(base, context));
                    return Expr::Call {
                        callee: Box::new(Expr::Ident(format!("__method__{name}"))),
                        args,
                    };
                }
            }
            return Expr::Call {
                callee: Box::new(parse_expr_with_context(callee, context)),
                args,
            };
        }
    }
    if let Some(dot) = find_top_level_field_dot(s) {
        let base = trim(&s[..dot]);
        let name = trim(&s[dot + 1..]);
        if !base.is_empty() && !name.is_empty() {
            return Expr::Field {
                base: Box::new(parse_expr_with_context(base, context)),
                name: name.to_string(),
            };
        }
    }
    Expr::Ident(s.to_string())
}

pub(crate) fn try_parse_closure_expr(s: &str) -> Option<Expr> {
    let (closure, tail) = parse_closure_prefix(s)?;
    tail.is_empty().then_some(closure)
}

fn parse_closure_prefix(s: &str) -> Option<(Expr, &str)> {
    let s = trim(s);
    let rest = s.strip_prefix("fn")?;
    if !rest.starts_with('(') && !rest.starts_with(" (") {
        return None;
    }
    let rest = trim(rest);
    let rest = &rest[1..];
    let mut paren = 1i32;
    let mut in_string = false;
    let mut escape = false;
    let mut close_idx = None;
    for (i, c) in rest.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => paren += 1,
            ')' => {
                paren -= 1;
                if paren == 0 {
                    close_idx = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close_idx = close_idx?;
    let param_blob = trim(&rest[..close_idx]);
    let params = if param_blob.is_empty() {
        Vec::new()
    } else {
        let mut params = Vec::new();
        for t in split_and_trim(',', param_blob) {
            params.push(parse_param(&t).ok()?);
        }
        params
    };
    let tail = trim(&rest[close_idx + 1..]);
    let body_start = tail.find('{')?;
    let type_text = trim(&tail[..body_start]);
    let ret = if let Some(rest) = type_text
        .strip_prefix("->")
        .or_else(|| type_text.strip_prefix("- >"))
    {
        parse_in_type(trim(rest))
    } else {
        Typ::Void
    };
    let body_rest = &tail[body_start..];
    let (body_inner, close) = brace_content_bounds_after_open(body_rest, 0)?;
    let tail = trim(&body_rest[close + 1..]);
    let body = parse_function_body(body_inner).ok()?;
    Some((
        Expr::Closure {
            params,
            ret,
            body,
            captures: vec![],
        },
        tail,
    ))
}
