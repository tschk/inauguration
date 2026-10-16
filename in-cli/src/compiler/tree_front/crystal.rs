use super::extract::{
    AstShape, ast_body, collect_kinds, declared_return_type, first_named, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Typ};
use tree_sitter::Node;

const CRYSTAL_AST: AstShape = AstShape {
    block_kinds: &["source_file", "class_declaration", "body"],
    return_kinds: &["return_statement"],
    expr_stmt_kinds: &["expression_statement", "print_statement"],
    local_decl_kinds: &[],
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
    local_decl_prefixes: &[],
    shell_first_kinds: &[],
    shell_last_kinds: &[],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: None,
    string_decoder: Some(crystal_string_literal),
    number_decoder: None,
};

pub(super) fn extract_crystal(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut func_nodes = Vec::new();
    collect_kinds(root, &["method_definition"], &mut func_nodes);
    for n in func_nodes {
        if let Some(d) = crystal_method_decl(src, n) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn crystal_method_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let params = crystal_params(src, n);
    let body = ast_body(src, n, CRYSTAL_AST);
    Some(Decl::Function {
        name,
        params,
        ret: declared_return_type(src, n).unwrap_or_else(|| Typ::Named("dynamic".into())),
        body,
        type_params: vec![],
    })
}

fn crystal_params<'a>(src: &[u8], n: Node<'a>) -> Vec<(String, Typ)> {
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

fn crystal_string_literal(source: &str) -> Option<String> {
    let mut chars = source
        .strip_prefix('"')?
        .strip_suffix('"')?
        .chars()
        .peekable();
    let mut output = Vec::new();
    fn push_char(output: &mut Vec<u8>, ch: char) {
        output.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
    }
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            if ch == '"' || (ch == '#' && chars.peek() == Some(&'{')) {
                return None;
            }
            push_char(&mut output, ch);
            continue;
        }
        match chars.next()? {
            'a' => push_char(&mut output, '\u{7}'),
            'b' => push_char(&mut output, '\u{8}'),
            'e' => push_char(&mut output, '\u{1b}'),
            'f' => push_char(&mut output, '\u{c}'),
            'n' => push_char(&mut output, '\n'),
            'r' => push_char(&mut output, '\r'),
            't' => push_char(&mut output, '\t'),
            'v' => push_char(&mut output, '\u{b}'),
            'x' => {
                let digits: String = chars.by_ref().take(2).collect();
                if digits.len() != 2 || !digits.chars().all(|ch| ch.is_ascii_hexdigit()) {
                    return None;
                }
                let byte = u8::from_str_radix(&digits, 16).ok()?;
                output.push(byte);
            }
            'u' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut digits = String::new();
                    loop {
                        let ch = chars.next()?;
                        if ch == '}' {
                            break;
                        }
                        digits.push(ch);
                    }
                    let mut any = false;
                    for point in digits.split_whitespace() {
                        if point.len() > 6 || !point.chars().all(|ch| ch.is_ascii_hexdigit()) {
                            return None;
                        }
                        push_char(
                            &mut output,
                            char::from_u32(u32::from_str_radix(point, 16).ok()?)?,
                        );
                        any = true;
                    }
                    if !any {
                        return None;
                    }
                } else {
                    let digits: String = chars.by_ref().take(4).collect();
                    if digits.len() != 4 || !digits.chars().all(|ch| ch.is_ascii_hexdigit()) {
                        return None;
                    }
                    push_char(
                        &mut output,
                        char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?,
                    );
                }
            }
            first @ '0'..='7' => {
                let mut value = first.to_digit(8)?;
                for _ in 0..2 {
                    let Some(next) = chars.peek().and_then(|ch| ch.to_digit(8)) else {
                        break;
                    };
                    value = value * 8 + next;
                    chars.next();
                }
                output.push(u8::try_from(value).ok()?);
            }
            newline @ ('\r' | '\n') => {
                if newline == '\r' && chars.next()? != '\n' {
                    return None;
                }
                while chars
                    .peek()
                    .is_some_and(|ch| matches!(*ch, '\t'..='\r' | ' '))
                {
                    chars.next();
                }
            }
            other => push_char(&mut output, other),
        }
    }
    String::from_utf8(output).ok()
}

#[test]
fn crystal_regular_strings_decode_without_double_unescaping() {
    for (source, expected) in [
        (r#""hello""#, "hello"),
        ("\"hello \\\r\n  \tworld\"", "hello world"),
        ("\"hello \\\n \t\n\r\n\x0b\x0cworld\"", "hello world"),
        (r#""\xCE\xBB""#, "λ"),
        (r#""\316\273""#, "λ"),
        ("\"hello \\\n  world\"", "hello world"),
        (r#""\n\r\t\a\b\e\f\v""#, "\n\r\t\u{7}\u{8}\u{1b}\u{c}\u{b}"),
        (r#""\\n\"\\""#, "\\n\"\\"),
        (r#""\u0041\u{1f52e 96ea}\x42\101""#, "A🔮雪BA"),
        (r#""\#{value}\q""#, "#{value}q"),
        (r#""\0end""#, "\0end"),
    ] {
        assert_eq!(
            crystal_string_literal(source).as_deref(),
            Some(expected),
            "{source}"
        );
    }
    for source in [
        "\"hello \\\rworld\"",
        r##""#{value}""##,
        r#""\u{d800}""#,
        r#""\xFF""#,
        r#""\u{}""#,
        r#""\u+041""#,
        r#""\x+1""#,
        r#""\u{+41}""#,
        r#"'x'"#,
    ] {
        assert_eq!(crystal_string_literal(source), None, "{source}");
    }
}
