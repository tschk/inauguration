use super::extract::{
    AstShape, ast_body, collect_kinds, declared_return_type, first_named, node_txt, normalize_entry,
};
use crate::core_ir::{Decl, Typ};
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
    string_decoder: Some(nim_string_literal),
};

pub(super) fn extract_nim(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    let mut decls = Vec::new();

    let mut func_nodes = Vec::new();
    collect_kinds(
        root,
        &["proc_declaration", "func_declaration"],
        &mut func_nodes,
    );
    for n in func_nodes {
        if let Some(d) = nim_proc_decl(src, n) {
            decls.push(d);
        }
    }

    Ok(decls)
}

fn nim_proc_decl<'a>(src: &[u8], n: Node<'a>) -> Option<Decl> {
    let name_n = n.child_by_field_name("name")?;
    let name = normalize_entry(node_txt(src, name_n).trim());
    let params = nim_params(src, n);
    let body = ast_body(src, n, NIM_AST);
    Some(Decl::Function {
        name,
        params,
        ret: declared_return_type(src, n).unwrap_or_else(|| Typ::Named("dynamic".into())),
        body,
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
            let pname = normalize_entry(node_txt(src, name_n).trim());
            params.push((pname, Typ::Named("dynamic".into())));
        }
    }
    params
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
