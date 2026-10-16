use super::extract::{
    AstShape, ast_body, collect_kinds, first_named, named_descendant, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Stmt, Typ};
use tree_sitter::Node;

const LUAAST: AstShape = AstShape {
    block_kinds: &["block", "chunk", "do_statement", "else_statement"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &[],
    local_decl_kinds: &["variable_declaration"],
    assignment_kinds: &["assignment_statement"],
    if_kinds: &["if_statement"],
    while_kinds: &["while_statement", "repeat_statement"],
    call_kinds: &["function_call"],
    arg_container_kinds: &["arguments"],
    arg_wrapper_kinds: &["expression_list", "expression", "variable", "variable_list"],
    paren_kinds: &["parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["number"],
    string_kinds: &["string"],
    type_kinds: &[],
    local_decl_prefixes: &["local "],
    shell_first_kinds: &["statement", "do_statement", "else_statement"],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: None,
    string_decoder: Some(lua_string_literal),
    number_decoder: Some(lua_number_literal),
};

// Only closed integer initializer trees are admitted here: Lua arithmetic wraps
// at 64 bits even when the eventual native target has a narrower word.
pub(super) fn lua_closed_integer_initializer(src: &[u8], node: Node<'_>) -> Option<i64> {
    fn fold(src: &[u8], node: Node<'_>, depth: usize) -> Option<i64> {
        if depth > 64 || node.has_error() || node.is_missing() {
            return None;
        }
        if node.kind() == "number" {
            return match lua_number_literal(node_txt(src, node))? {
                crate::core_ir::Expr::IntLit(value) => Some(value),
                _ => None,
            };
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|child| !child.is_extra())
            .collect();
        if matches!(node.kind(), "parenthesized_expression" | "expression") {
            return (children.len() == 1)
                .then(|| fold(src, children[0], depth + 1))
                .flatten();
        }
        if node.kind() != "binary_expression" || children.len() != 2 {
            return None;
        }
        let left = node.child_by_field_name("left")?;
        let right = node.child_by_field_name("right")?;
        let operator = node.child_by_field_name("operator")?;
        if children != [left, right] {
            return None;
        }
        let left = fold(src, left, depth + 1)?;
        let right = fold(src, right, depth + 1)?;
        match node_txt(src, operator) {
            "+" => Some(left.wrapping_add(right)),
            "-" => Some(left.wrapping_sub(right)),
            "*" => Some(left.wrapping_mul(right)),
            _ => None,
        }
    }
    fold(src, node, 0)
}

pub(super) fn extract_lua(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["function_declaration"], &mut func_nodes);
    for n in func_nodes {
        if let Some(d) = lua_function_decl(src, n) {
            decls.push(d);
        }
    }

    let mut local_hits = Vec::new();
    collect_kinds(root, &["variable_declaration"], &mut local_hits);
    for v in local_hits {
        if let Some(d) = lua_var_function(src, v) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn lua_function_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = match n.child_by_field_name("name") {
        Some(nm) => nm,
        None => {
            let mut ids = Vec::new();
            collect_kinds(n, &["identifier", "dot_index_expression"], &mut ids);
            ids.into_iter().next()?
        }
    };
    let raw = node_txt(src, name_n).trim();
    let compact = raw.replace(['.', ':'], "_");
    let name = normalize_entry(&compact);
    let params = lua_params(src, n);
    let body = n
        .child_by_field_name("body")
        .or_else(|| first_named(n, "block"))
        .map(|b| lua_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret: Typ::Void,
        body,
        type_params: vec![],
    })
}

fn lua_var_function<'a>(src: &[u8], v: Node<'a>) -> Option<Decl> {
    let name_n = first_named(v, "identifier")
        .or_else(|| first_named(v, "variable_list").and_then(|vl| first_named(vl, "identifier")))?;
    let name = normalize_entry(node_txt(src, name_n).trim());

    let mut func_defs = Vec::new();
    collect_kinds(v, &["function_definition"], &mut func_defs);
    if func_defs.is_empty() {
        return None;
    }
    let func = func_defs.into_iter().next()?;

    let params = lua_params(src, func);
    let body = func
        .child_by_field_name("body")
        .map(|b| lua_body(src, b))
        .unwrap_or_default();
    Some(Decl::Function {
        name,
        params,
        ret: Typ::Void,
        body,
        type_params: vec![],
    })
}

fn lua_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let plist = n
        .child_by_field_name("parameters")
        .or_else(|| named_descendant(n, "parameters"));
    let Some(plist) = plist else {
        return out;
    };
    let mut w = plist.walk();
    for ch in plist.named_children(&mut w) {
        if ch.kind() == "identifier" {
            out.push((
                node_txt(src, ch).trim().to_string(),
                Typ::Named("Any".into()),
            ));
        } else if ch.kind() == "variadic_argument" {
            let txt = node_txt(src, ch)
                .trim()
                .trim_start_matches("...")
                .to_string();
            out.push((txt, Typ::Named("Any".into())));
        }
    }
    out
}

fn lua_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    ast_body(src, body, LUAAST)
}

// shortcut: Core strings require UTF-8; byte strings need a byte-valued Core type.
fn lua_string_literal(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let first = *bytes.first()?;
    if first == b'[' {
        let mut opening = 1;
        while bytes.get(opening) == Some(&b'=') {
            opening += 1;
        }
        if bytes.get(opening) != Some(&b'[') {
            return None;
        }
        let delimiter = opening + 1;
        let closing = bytes
            .get(..delimiter)?
            .iter()
            .map(|byte| match byte {
                b'[' => b']',
                byte => *byte,
            })
            .collect::<Vec<_>>();
        // Reject trailing data after the first matching delimiter.
        let content_end = delimiter
            + bytes
                .get(delimiter..)?
                .windows(delimiter)
                .position(|window| window == closing.as_slice())?;
        if content_end.checked_add(delimiter)? != bytes.len() {
            return None;
        }
        let mut at = delimiter;
        if matches!(bytes.get(at), Some(b'\r' | b'\n')) {
            at = lua_newline_end(bytes, at);
        }
        let mut output = Vec::new();
        while at < content_end {
            if matches!(bytes[at], b'\r' | b'\n') {
                output.push(b'\n');
                at = lua_newline_end(bytes, at);
            } else {
                output.push(bytes[at]);
                at += 1;
            }
        }
        return String::from_utf8(output).ok();
    }
    if !matches!(first, b'\'' | b'"') || bytes.len() < 2 || bytes.last() != Some(&first) {
        return None;
    }
    let end = bytes.len() - 1;
    let mut at = 1;
    let mut output = Vec::new();
    while at < end {
        let byte = bytes[at];
        at += 1;
        if byte != b'\\' {
            if byte == first || matches!(byte, b'\r' | b'\n') {
                return None;
            }
            output.push(byte);
            continue;
        }
        let escape = *bytes.get(at).filter(|_| at < end)?;
        at += 1;
        match escape {
            b'a' => output.push(7),
            b'b' => output.push(8),
            b'f' => output.push(12),
            b'n' => output.push(b'\n'),
            b'r' => output.push(b'\r'),
            b't' => output.push(b'\t'),
            b'v' => output.push(11),
            b'\\' | b'\'' | b'"' => output.push(escape),
            b'\r' | b'\n' => {
                output.push(b'\n');
                at = lua_newline_end(bytes, at - 1);
            }
            b'z' => {
                while at < end && matches!(bytes[at], b' ' | b'\t' | b'\r' | b'\n' | 11 | 12) {
                    at += 1;
                }
            }
            b'0'..=b'9' => {
                let mut value = u16::from(escape - b'0');
                let mut digits = 1;
                while digits < 3 && at < end && bytes[at].is_ascii_digit() {
                    value = value * 10 + u16::from(bytes[at] - b'0');
                    at += 1;
                    digits += 1;
                }
                output.push(u8::try_from(value).ok()?);
            }
            b'x' => {
                if at.checked_add(2)? > end {
                    return None;
                }
                let high = (bytes[at] as char).to_digit(16)?;
                let low = (bytes[at + 1] as char).to_digit(16)?;
                output.push((high * 16 + low) as u8);
                at += 2;
            }
            b'u' => {
                if bytes.get(at) != Some(&b'{') {
                    return None;
                }
                at += 1;
                let start = at;
                let mut value = 0u32;
                while at < end && bytes[at] != b'}' {
                    value = value
                        .checked_mul(16)?
                        .checked_add((bytes[at] as char).to_digit(16)?)?;
                    at += 1;
                }
                if start == at || at >= end {
                    return None;
                }
                let scalar = char::from_u32(value)?;
                let mut encoded = [0; 4];
                output.extend_from_slice(scalar.encode_utf8(&mut encoded).as_bytes());
                at += 1;
            }
            _ => return None,
        }
    }
    String::from_utf8(output).ok()
}

// Lua treats CR, LF, CRLF and LFCR as one line ending.
fn lua_newline_end(bytes: &[u8], at: usize) -> usize {
    let next = at + 1;
    if next < bytes.len() && matches!(bytes[next], b'\r' | b'\n') && bytes[next] != bytes[at] {
        next + 1
    } else {
        next
    }
}

#[cfg(test)]
mod string_tests {
    use super::lua_string_literal;
    #[test]
    fn short_strings_decode_bytes_without_losing_quotes_or_escapes() {
        for (raw, expected) in [
            ("'\"inner\"'", "\"inner\""),
            ("\"'inner'\"", "'inner'"),
            (
                "\"\\a\\b\\f\\n\\r\\t\\v\\\\\\\"\\'\"",
                "\u{7}\u{8}\u{c}\n\r\t\u{b}\\\"'",
            ),
            ("\"\\\\n\"", "\\n"),
            ("\"\\0\\12\\0654\"", "\u{0}\u{c}A4"),
            ("\"\\x41\\xc3\\xa9\"", "Aé"),
            ("\"\\u{41}\\u{1f600}\"", "A😀"),
            ("\"\\u{00000000000000000000000000000041}\"", "A"),
            ("\"a\\z \t\u{b}\u{c}\r\nb\"", "ab"),
            ("\"a\\\nb\"", "a\nb"),
            ("\"a\\\r\nb\"", "a\nb"),
            ("\"a\\\n\rb\"", "a\nb"),
        ] {
            assert_eq!(
                lua_string_literal(raw).as_deref(),
                Some(expected),
                "{raw:?}"
            );
        }
    }
    #[test]
    fn long_strings_match_delimiters_and_normalize_only_line_endings() {
        for (raw, expected) in [
            ("[[]]", ""),
            ("[[\"quotes\"\\n]]", "\"quotes\"\\n"),
            ("[=[\r\nhello\rworld\n\ragain]=]", "hello\nworld\nagain"),
            ("[[\n\nnext]]", "\nnext"),
            ("[==[a]=]b]==]", "a]=]b"),
        ] {
            assert_eq!(
                lua_string_literal(raw).as_deref(),
                Some(expected),
                "{raw:?}"
            );
        }
    }
    #[test]
    fn invalid_or_non_utf8_strings_are_explicitly_unsupported() {
        for raw in [
            "",
            "a",
            "\"",
            "'unterminated",
            "\"a'b'\"junk",
            "\"\\q\"",
            "\"\\256\"",
            "\"\\xg0\"",
            "\"\\x1\"",
            "\"\\u{}\"",
            "\"\\u{d800}\"",
            "\"\\u{110000}\"",
            "\"\\u{ffffffffffffffff}\"",
            "\"\\255\"",
            "\"\\xff\"",
            "\"a\nb\"",
            "[=[mismatch]]",
            "[[first]]trailing]]",
        ] {
            assert_eq!(lua_string_literal(raw), None, "{raw:?}");
        }
    }
}

#[cfg(test)]
mod decoder_ast_tests {
    use super::*;
    use crate::core_ir::Expr;

    fn parsed_body(source: &str) -> Vec<Stmt> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_lua::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{source}: {}",
            tree.root_node().to_sexp()
        );
        extract_lua(source.as_bytes(), tree.root_node())
            .unwrap()
            .into_iter()
            .find_map(|decl| match decl {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn closed_integer_initializers_wrap_at_lua_width_before_target_lowering() {
        for (expression, expected) in [
            ("20 + 22", 42),
            ("(2147483647 + 1) - 2147483606", 42),
            ("(0x7fffffffffffffff + 1) - 0x7fffffffffffffd6", 42),
            ("0x100000000 * 0x100000000 + 42", 42),
            ("2147483647 + 1", 2147483648),
        ] {
            let body = parsed_body(&format!(
                "function main() local value = {expression} return value end"
            ));
            assert!(
                matches!(&body[0], Stmt::Let(_, None, Expr::IntLit(value)) if *value == expected),
                "{expression}: {body:?}"
            );
        }
        for expression in [
            "other + 1",
            "effect() + 1",
            "1.0 + 41",
            "21 << 1",
            "84 / 2",
            "-1 + 43",
        ] {
            let body = parsed_body(&format!(
                "function main() local value = {expression} return value end"
            ));
            assert!(!matches!(&body[0], Stmt::Let(..)), "{expression}: {body:?}");
        }
    }

    #[test]
    fn lua_decoder_reaches_local_initializer_and_every_call_argument() {
        for (raw, expected) in [
            ("'\"inner\"'", "\"inner\""),
            ("\"'inner'\"", "'inner'"),
            (
                "\"\\a\\b\\f\\n\\r\\t\\v\\\\\\\"\\'\"",
                "\u{7}\u{8}\u{c}\n\r\t\u{b}\\\"'",
            ),
            ("\"\\\\n\"", "\\n"),
            ("\"\\0\\12\\0654\"", "\u{0}\u{c}A4"),
            ("\"\\x41\\xc3\\xa9\"", "Aé"),
            ("\"\\u{41}\\u{1f600}\"", "A😀"),
            ("[=[\r\nhello\rworld\n\ragain]=]", "hello\nworld\nagain"),
        ] {
            let source = format!("function main() local value = {raw}; print({raw}); return end");
            let body = parsed_body(&source);
            assert_eq!(body.len(), 3, "{source}: {body:?}");
            assert_eq!(
                body[0],
                Stmt::Let("value".into(), None, Expr::StringLit(expected.into())),
                "{source}"
            );
            assert_eq!(
                body[1],
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("print".into())),
                    args: vec![Expr::StringLit(expected.into())]
                }),
                "{source}"
            );
            assert_eq!(body[2], Stmt::Return(None));
        }
    }

    #[test]
    fn unsupported_lua_byte_strings_survive_instead_of_dropping_statements_or_args() {
        for raw in ["\"\\xff\"", "\"\\u{d800}\""] {
            let source = format!("function main() local value = {raw}; print({raw}); return end");
            let body = parsed_body(&source);
            let refused = Expr::Ident("#unsupported-string-literal".into());
            assert_eq!(body.len(), 3, "{source}: {body:?}");
            assert_eq!(
                body[0],
                Stmt::Let("value".into(), None, refused.clone()),
                "{source}"
            );
            assert_eq!(
                body[1],
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("print".into())),
                    args: vec![refused]
                }),
                "{source}"
            );
            assert_eq!(body[2], Stmt::Return(None));
        }
    }
    #[test]
    fn lua_number_decoder_preserves_kinds_and_bits_in_initializers_and_args() {
        for (raw, expected) in [
            ("42", Expr::IntLit(42)),
            ("0xffffffffffffffff", Expr::IntLit(-1)),
            ("0x10000000000000000", Expr::IntLit(0)),
            (
                "9223372036854775808",
                Expr::FloatLit(crate::core_ir::FloatVal(f64::from_bits(0x43e0000000000000))),
            ),
            ("0x1.8p1", Expr::FloatLit(crate::core_ir::FloatVal(3.0))),
            (
                "0x1p-1074",
                Expr::FloatLit(crate::core_ir::FloatVal(f64::from_bits(1))),
            ),
            ("0x1p-1075", Expr::FloatLit(crate::core_ir::FloatVal(0.0))),
            (
                "0x1.fffffffffffffp1023",
                Expr::FloatLit(crate::core_ir::FloatVal(f64::MAX)),
            ),
        ] {
            let source = format!("function main() local value = {raw}; print({raw}); return end");
            let body = parsed_body(&source);
            assert_eq!(body.len(), 3, "{source}: {body:?}");
            assert_eq!(
                body[0],
                Stmt::Let("value".into(), None, expected.clone()),
                "{source}"
            );
            assert_eq!(
                body[1],
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("print".into())),
                    args: vec![expected]
                }),
                "{source}"
            );
            assert_eq!(body[2], Stmt::Return(None));
        }
    }

    #[test]
    fn lua_number_decoder_refusal_preserves_following_statements_and_args() {
        for raw in ["1LL", "0b10", "0x2LL", "1e309", "0x1p1024"] {
            let source = format!("function main() local value = {raw}; print({raw}); return end");
            let body = parsed_body(&source);
            let refused = Expr::Ident("#unsupported-number-literal".into());
            assert_eq!(body.len(), 3, "{source}: {body:?}");
            assert_eq!(
                body[0],
                Stmt::Let("value".into(), None, refused.clone()),
                "{source}"
            );
            assert_eq!(
                body[1],
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("print".into())),
                    args: vec![refused]
                }),
                "{source}"
            );
            assert_eq!(body[2], Stmt::Return(None));
        }
    }
}

// Lua integers are signed 64-bit values; hexadecimal integer tokens wrap at
// that width. Floating literals use binary64. Core currently represents only
// finite literal values, so overflow survives as an explicit frontend refusal.
fn lua_number_literal(raw: &str) -> Option<crate::core_ir::Expr> {
    use crate::core_ir::{Expr, FloatVal};
    let bytes = raw.as_bytes();
    if bytes.starts_with(b"0x") || bytes.starts_with(b"0X") {
        let mut at = 2;
        let mut digits = Vec::new();
        let mut fraction_digits = 0usize;
        let mut point = false;
        while let Some(&byte) = bytes.get(at) {
            if let Some(digit) = lua_hex_digit(byte) {
                digits.push(digit);
                fraction_digits += usize::from(point);
            } else if byte == b'.' && !point {
                point = true;
            } else {
                break;
            }
            at += 1;
        }
        if digits.is_empty() {
            return None;
        }
        let mut exponent = 0i128;
        let floating = point || matches!(bytes.get(at), Some(b'p' | b'P'));
        if matches!(bytes.get(at), Some(b'p' | b'P')) {
            at += 1;
            exponent = lua_number_exponent(bytes, &mut at)?;
        }
        if at != bytes.len() {
            return None;
        }
        if !floating {
            let value = digits.into_iter().fold(0u64, |value, digit| {
                value.wrapping_mul(16).wrapping_add(u64::from(digit))
            });
            return Some(Expr::IntLit(value as i64));
        }
        return lua_hex_float(&digits, fraction_digits, exponent)
            .map(|value| Expr::FloatLit(FloatVal(value)));
    }

    let mut at = 0;
    let mut digits = 0;
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
        digits += 1;
    }
    let point = bytes.get(at) == Some(&b'.');
    if point {
        at += 1;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    let exponent = matches!(bytes.get(at), Some(b'e' | b'E'));
    if exponent {
        at += 1;
        lua_number_exponent(bytes, &mut at)?;
    }
    if at != bytes.len() {
        return None;
    }
    if !point
        && !exponent
        && let Ok(value) = raw.parse::<i64>()
    {
        return Some(Expr::IntLit(value));
    }
    raw.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .map(|value| Expr::FloatLit(FloatVal(value)))
}

fn lua_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// Saturation is safe here: only the sign of an exponent beyond i128's range
// can affect a finite-sized significand (overflow, underflow, or exact zero).
fn lua_number_exponent(bytes: &[u8], at: &mut usize) -> Option<i128> {
    let negative = bytes.get(*at) == Some(&b'-');
    if matches!(bytes.get(*at), Some(b'+' | b'-')) {
        *at += 1;
    }
    let start = *at;
    let mut exponent = 0i128;
    while let Some(&digit) = bytes.get(*at).filter(|digit| digit.is_ascii_digit()) {
        exponent = exponent
            .saturating_mul(10)
            .saturating_add(i128::from(digit - b'0'));
        *at += 1;
    }
    (*at > start).then_some(if negative { -exponent } else { exponent })
}

// Round the exact hexadecimal significand once, with guard/sticky bits and
// round-to-nearest, ties-to-even. No intermediate floating accumulation or
// scaling can introduce double rounding, underflow, or locale dependence.
fn lua_hex_float(digits: &[u8], fraction_digits: usize, exponent: i128) -> Option<f64> {
    let Some(first) = digits.iter().position(|digit| *digit != 0) else {
        return Some(0.0);
    };
    let significant_bits =
        (digits.len() - first - 1) as i128 * 4 + i128::from(8 - digits[first].leading_zeros());
    let scale = exponent.saturating_sub(fraction_digits as i128 * 4);
    let mut binary_exponent = scale.saturating_add(significant_bits - 1);
    if binary_exponent > 1023 {
        return None;
    }
    if binary_exponent < -1075 {
        return Some(0.0);
    }
    // Normal numbers retain 53 significant bits. Subnormal numbers instead
    // round at the fixed 2^-1074 quantum, including its halfway-to-zero case.
    let quantum = (binary_exponent - 52).max(-1074);
    let keep = (binary_exponent + 1 - quantum) as usize;
    let mut significand = 0u64;
    let mut seen = 0usize;
    let mut started = false;
    let mut guard = false;
    let mut sticky = false;
    for &digit in &digits[first..] {
        for shift in (0..4).rev() {
            let bit = (digit >> shift) & 1;
            if !started && bit == 0 {
                continue;
            }
            started = true;
            if seen < keep {
                significand = (significand << 1) | u64::from(bit);
            } else if seen == keep {
                guard = bit != 0;
            } else {
                sticky |= bit != 0;
            }
            if seen <= keep {
                seen += 1;
            }
        }
    }
    if seen < keep {
        significand <<= keep - seen;
    }
    if guard && (sticky || significand & 1 != 0) {
        significand += 1;
    }
    if quantum == -1074 {
        // The contiguous bit encoding includes a carry from subnormal into
        // the smallest normal range, so it needs no second rounding step.
        return Some(f64::from_bits(significand));
    }
    if significand == 1u64 << 53 {
        significand >>= 1;
        binary_exponent += 1;
    }
    if binary_exponent > 1023 {
        return None;
    }
    let bits = ((binary_exponent + 1023) as u64) << 52 | (significand - (1u64 << 52));
    Some(f64::from_bits(bits))
}
#[cfg(test)]
mod number_tests {
    use super::lua_number_literal;
    use crate::core_ir::{Expr, FloatVal};
    #[test]
    fn lua_number_literals_match_reference_kinds_and_exact_bits() {
        for (raw, expected) in [
            ("0", Some(Expr::IntLit(0))),
            ("42", Some(Expr::IntLit(42))),
            ("00042", Some(Expr::IntLit(42))),
            (
                "9223372036854775807",
                Some(Expr::IntLit(9223372036854775807)),
            ),
            (
                "9223372036854775808",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x43e0000000000000)))),
            ),
            (
                "18446744073709551615",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x43f0000000000000)))),
            ),
            (
                "999999999999999999999999999999999999999",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x48078287f49c4a1d)))),
            ),
            (
                "1.0",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3ff0000000000000)))),
            ),
            (
                ".5",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3fe0000000000000)))),
            ),
            (
                "1.",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3ff0000000000000)))),
            ),
            (
                "1e+2",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x4059000000000000)))),
            ),
            (
                "1E-2",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3f847ae147ae147b)))),
            ),
            (
                "9007199254740993.0",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x4340000000000000)))),
            ),
            (
                "1e-99999999999999999999999999999999999",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000000)))),
            ),
            ("1e99999999999999999999999999999999999", None),
            ("0x0", Some(Expr::IntLit(0))),
            ("0X2a", Some(Expr::IntLit(42))),
            ("0xffffffffffffffff", Some(Expr::IntLit(-1))),
            (
                "0x8000000000000000",
                Some(Expr::IntLit(-9223372036854775808)),
            ),
            ("0x10000000000000000", Some(Expr::IntLit(0))),
            (
                "0xffffffffffffffffffffffffffffffffffffffffffffffff",
                Some(Expr::IntLit(-1)),
            ),
            (
                "0x00000000000000000000000000000000001",
                Some(Expr::IntLit(1)),
            ),
            (
                "0x1.8p1",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x4008000000000000)))),
            ),
            (
                "0x.8p1",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3ff0000000000000)))),
            ),
            (
                "0x1.f",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3fff000000000000)))),
            ),
            (
                "0XAp+1",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x4034000000000000)))),
            ),
            (
                "0x1p0",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3ff0000000000000)))),
            ),
            (
                "0x1.00000000000008p0",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3ff0000000000000)))),
            ),
            (
                "0x1.00000000000018p0",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3ff0000000000002)))),
            ),
            (
                "0x1.fffffffffffff8p0",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x4000000000000000)))),
            ),
            (
                "0x1.fffffffffffffp1023",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x7fefffffffffffff)))),
            ),
            (
                "0x1.fffffffffffff7ffffffffp1023",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x7fefffffffffffff)))),
            ),
            ("0x1.fffffffffffff8p1023", None),
            ("0x1p1024", None),
            (
                "0x1p-1074",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000001)))),
            ),
            (
                "0x1p-1075",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000000)))),
            ),
            (
                "0x1.0000000000000001p-1075",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000001)))),
            ),
            (
                "0x1.8p-1074",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000002)))),
            ),
            (
                "0x0.fffffffffffffp-1022",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x000fffffffffffff)))),
            ),
            (
                "0x0.fffffffffffff8p-1022",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0010000000000000)))),
            ),
            (
                "0x0.fffffffffffff7ffffffffp-1022",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x000fffffffffffff)))),
            ),
            (
                "0x1p-1022",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0010000000000000)))),
            ),
            (
                "0x1.fffffffffffff8p-1022",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0020000000000000)))),
            ),
            (
                "0x1.0000000000000800000000000000000000000001p0",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3ff0000000000001)))),
            ),
            (
                "0x1.00000000000007ffffffffffffffffffffffffp0",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3ff0000000000000)))),
            ),
            (
                "0x0p9999999999999999999999999999999999999",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000000)))),
            ),
            ("0x1p9999999999999999999999999999999999999", None),
            (
                "0x1p-9999999999999999999999999999999999999",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000000)))),
            ),
            (
                "0x0000000000000000.000000000000000000001p80",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x3fb0000000000000)))),
            ),
            (
                "0x1.0000000000001p-1075",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000001)))),
            ),
            (
                "0x1.fffffffffffffp-1023",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0010000000000000)))),
            ),
            (
                "0x0p9999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000000)))),
            ),
            (
                "0x1p9999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999",
                None,
            ),
            (
                "0x1p-9999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999",
                Some(Expr::FloatLit(FloatVal(f64::from_bits(0x0000000000000000)))),
            ),
        ] {
            match (lua_number_literal(raw), expected) {
                (Some(Expr::FloatLit(actual)), Some(Expr::FloatLit(expected))) => {
                    assert_eq!(actual.0.to_bits(), expected.0.to_bits(), "{raw}")
                }
                (actual, expected) => assert_eq!(actual, expected, "{raw}"),
            }
        }
    }
    #[test]
    fn lua_number_literals_reject_foreign_suffixes_and_malformed_tokens() {
        for raw in [
            "1LL", "1L", "1ULL", "0x2LL", "0b10", "1_000", "0x1_0", "0x", "0xp1", "0x.p1", "1e",
            "1e+", "0x1p", "0x1p-", "1.2.3", "0x1.2.3", "nan", "inf", "", "+1", "-1", " 1", "1 ",
        ] {
            assert_eq!(lua_number_literal(raw), None, "{raw}");
        }
    }
}
