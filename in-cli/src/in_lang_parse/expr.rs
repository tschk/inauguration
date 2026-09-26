use super::stmt::parse_function_body;
use super::types::{parse_in_type, parse_param};
use super::util::*;
use crate::core_ir::{Expr, FloatVal, Typ};

pub(crate) fn parse_expr(s: &str) -> Expr {
    let s = trim(s);
    if let Some(inner) = strip_enclosing_parens(s) {
        return parse_expr(inner);
    }
    if let Some(closure) = try_parse_closure_expr(s) {
        return closure;
    }
    for ops in [
        &["||"][..],
        &["&&"][..],
        &["|"][..],
        &["^"][..],
        &["&"][..],
        &["<<", ">>", "==", "!=", ">=", "<=", ">", "<"][..],
        &["+", "-"][..],
        &["*", "/", "%"][..],
    ] {
        if let Some((op, idx)) = find_top_level_binary_op(s, ops) {
            let lhs = trim(&s[..idx]);
            let rhs = trim(&s[idx + op.len()..]);
            if !lhs.is_empty() && !rhs.is_empty() {
                return Expr::Binary {
                    op: op.to_string(),
                    lhs: Box::new(parse_expr(lhs)),
                    rhs: Box::new(parse_expr(rhs)),
                };
            }
        }
    }
    if let Some(rest) = s.strip_prefix('!')
        && !trim(rest).is_empty()
    {
        return Expr::Unary {
            op: "!".into(),
            expr: Box::new(parse_expr(rest)),
        };
    }
    if let Some(rest) = s.strip_prefix('-')
        && !trim(rest).is_empty()
        && rest.parse::<i64>().is_err()
    {
        // -9223372036854775808 is i64::MIN: its positive magnitude overflows
        // i64, so fold it into a single literal instead of a Unary over a
        // numeric-looking identifier.
        if trim(rest) == "9223372036854775808" {
            return Expr::IntLit(i64::MIN);
        }
        return Expr::Unary {
            op: "-".into(),
            expr: Box::new(parse_expr(rest)),
        };
    }
    if s == "true" {
        return Expr::BoolLit(true);
    }
    if s == "false" {
        return Expr::BoolLit(false);
    }
    if let Ok(n) = s.parse::<i64>() {
        return Expr::IntLit(n);
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        if let Ok(n) = i64::from_str_radix(hex, 16) {
            return Expr::IntLit(n);
        }
    }
    if s.contains('.')
        && let Ok(f) = s.parse::<f64>()
    {
        return Expr::FloatLit(FloatVal(f));
    }
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        let raw = &s[1..s.len() - 1];
        // Process escape sequences: \n, \t, \r, \\, \"
        let mut processed = String::with_capacity(raw.len());
        let mut esc = false;
        for c in raw.chars() {
            if esc {
                match c {
                    'n' => processed.push('\n'),
                    't' => processed.push('\t'),
                    'r' => processed.push('\r'),
                    '\\' => processed.push('\\'),
                    '"' => processed.push('"'),
                    _ => {
                        processed.push('\\');
                        processed.push(c);
                    }
                }
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else {
                processed.push(c);
            }
        }
        return Expr::StringLit(processed);
    }
    if let Some(closure) = try_parse_closure_expr(s) {
        return closure;
    }
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        return Expr::ArrayLit(
            split_call_args(inner)
                .into_iter()
                .map(|arg| parse_expr(&arg))
                .collect(),
        );
    }
    if let Some(open) = find_top_level_index_open(s)
        && s.ends_with(']')
    {
        let base = trim(&s[..open]);
        let index = trim(&s[open + 1..s.len() - 1]);
        if !base.is_empty() && !index.is_empty() {
            return Expr::Index {
                base: Box::new(parse_expr(base)),
                index: Box::new(parse_expr(index)),
            };
        }
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
                    Some((trim(name).to_string(), parse_expr(trim(expr))))
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
                .map(|arg| parse_expr(&arg))
                .collect::<Vec<_>>();
            if let Some(dot) = find_top_level_field_dot(callee) {
                let base = trim(&callee[..dot]);
                let name = trim(&callee[dot + 1..]);
                if !base.is_empty() && !name.is_empty() {
                    args.insert(0, parse_expr(base));
                    return Expr::Call {
                        callee: Box::new(Expr::Ident(format!("__method__{name}"))),
                        args,
                    };
                }
            }
            return Expr::Call {
                callee: Box::new(Expr::Ident(callee.to_string())),
                args,
            };
        }
    }
    if let Some(dot) = find_top_level_field_dot(s) {
        let base = trim(&s[..dot]);
        let name = trim(&s[dot + 1..]);
        if !base.is_empty() && !name.is_empty() {
            return Expr::Field {
                base: Box::new(parse_expr(base)),
                name: name.to_string(),
            };
        }
    }
    Expr::Ident(s.to_string())
}

pub(crate) fn try_parse_closure_expr(s: &str) -> Option<Expr> {
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
    let body_inner = brace_content_after_open(body_rest, 0)?;
    let body = parse_function_body(body_inner).ok()?;
    Some(Expr::Closure {
        params,
        ret,
        body,
        captures: vec![],
    })
}
