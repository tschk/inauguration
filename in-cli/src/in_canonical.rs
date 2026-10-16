use crate::core_ir::{Decl, Expr, LoopKind, MatchArm, Stmt, Typ};

pub fn canonicalize_in_source(source: &str) -> Result<String, String> {
    let crate::in_lang_parse::module::CanonicalSource {
        module,
        items,
        generated_count,
        counts,
    } = crate::in_lang_parse::module::parse_in_source_for_canonical(source)?;
    let mut declarations = module.decls.into_iter().skip(generated_count);
    let mut counts = counts.into_iter();
    let mut out = String::new();
    for (_, text) in items {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        if crate::in_lang_parse::lexer::is_core_decl_item(&text) {
            let count = counts.next().ok_or(".in: missing source item provenance")?;
            if count == 0 {
                return Err(".in: empty source declaration provenance".into());
            }
            let decl = declarations
                .next()
                .ok_or(".in: missing source declaration provenance")?;
            for _ in 1..count {
                declarations
                    .next()
                    .ok_or(".in: missing expanded declaration provenance")?;
            }
            if count > 1 || text.trim_start().starts_with("extern ") {
                // Extern metadata and struct method grouping require original source syntax.
                out.push_str(text.trim());
            } else {
                if text.trim_start().starts_with("interrupt fn ") {
                    out.push_str("interrupt ");
                }
                format_decl(&decl, &mut out);
            }
        } else {
            // Keep annotation association, task arguments and product-owned blocks.
            out.push_str(text.trim());
        }
    }
    if declarations.next().is_some() {
        return Err(".in: extra source declaration provenance".into());
    }
    out.push('\n');
    Ok(out)
}

fn format_decl(decl: &Decl, out: &mut String) {
    match decl {
        Decl::Struct { name, fields, .. } => {
            out.push_str("struct ");
            out.push_str(name);
            out.push_str(" {\n");
            for (field, typ) in fields {
                out.push_str("  ");
                out.push_str(&format_type(typ));
                out.push(' ');
                out.push_str(field);
                out.push('\n');
            }
            out.push('}');
        }
        Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } => {
            out.push_str("fn ");
            out.push_str(name);
            out.push('(');
            for (idx, (param, typ)) in params.iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                out.push_str(param);
                out.push_str(": ");
                out.push_str(&format_type(typ));
            }
            out.push_str(") -> ");
            out.push_str(&format_type(ret));
            out.push_str(" {\n");
            format_body(body, 1, out);
            out.push('}');
        }
        Decl::Class {
            name,
            fields,
            methods,
            extends,
            implements,
            ..
        } => {
            out.push_str("class ");
            out.push_str(name);
            if let Some(parent) = extends {
                out.push_str(" extends ");
                out.push_str(parent);
            }
            if !implements.is_empty() {
                out.push_str(" implements ");
                out.push_str(&implements.join(", "));
            }
            out.push_str(" {\n");
            for (field, typ) in fields {
                out.push_str(&format!("  {field}: {}\n", format_type(typ)));
            }
            for method in methods {
                let mut rendered = String::new();
                format_decl(method, &mut rendered);
                for line in rendered.lines() {
                    out.push_str("  ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            out.push('}');
        }
        Decl::Interface { name, methods, .. } => {
            out.push_str(&format!("interface {name} {{\n"));
            for method in methods {
                let params = method
                    .params
                    .iter()
                    .map(|(name, typ)| format!("{name}: {}", format_type(typ)))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!(
                    "  fn {}({params}) -> {}\n",
                    method.name,
                    format_type(&method.ret)
                ));
            }
            out.push('}');
        }
        Decl::Component {
            name,
            target,
            deterministic,
            checkpoint,
            imports,
            exports,
            capabilities,
        } => {
            out.push_str(&format!("component {name} {{\n"));
            if !target.is_empty() {
                out.push_str(&format!(
                    "  target {}\n",
                    format_expr(&Expr::StringLit(target.clone()))
                ));
            }
            out.push_str(&format!("  deterministic {deterministic}\n"));
            if !checkpoint.is_empty() {
                out.push_str(&format!("  checkpoint {checkpoint}\n"));
            }
            for import in imports {
                out.push_str(&format!("  import {}: {}\n", import.name, import.interface));
            }
            for export in exports {
                out.push_str(&format!("  export {}: {}\n", export.name, export.interface));
            }
            for capability in capabilities {
                out.push_str(&format!(
                    "  capability {}: {}({})\n",
                    capability.name,
                    capability.capability_type,
                    capability.args.join(", ")
                ));
            }
            out.push('}');
        }
        Decl::Global {
            name,
            typ,
            mutable,
            init,
            ..
        } => {
            out.push_str(if *mutable { "var " } else { "const " });
            out.push_str(name);
            out.push_str(": ");
            out.push_str(&format_type(typ));
            if let Some(value) = init {
                out.push_str(" = ");
                out.push_str(&format_expr(value));
            }
            out.push(';');
        }
    }
}

fn format_type(typ: &Typ) -> String {
    match typ {
        Typ::Int => "Int".into(),
        Typ::Float => "Float".into(),
        Typ::String => "String".into(),
        Typ::Bool => "Bool".into(),
        Typ::Void => "void".into(),
        Typ::Array(item) => format!("[{}]", format_type(item)),
        Typ::Vector(item) => format!("Vec<{}>", format_type(item)),
        Typ::Named(name) => name.clone(),
        Typ::Generic(name) => name.clone(),
    }
}

fn format_body(body: &[Stmt], depth: usize, out: &mut String) {
    for stmt in body {
        format_stmt(stmt, depth, out);
    }
}

fn format_stmt(stmt: &Stmt, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match stmt {
        Stmt::Let(name, typ, expr) => {
            out.push_str(&indent);
            out.push_str("let ");
            out.push_str(name);
            if let Some(typ) = typ {
                out.push_str(": ");
                out.push_str(&format_type(typ));
            }
            out.push_str(" = ");
            out.push_str(&format_expr(expr));
            out.push('\n');
        }
        Stmt::Assign(name, expr) => {
            out.push_str(&indent);
            out.push_str(name);
            out.push_str(" = ");
            out.push_str(&format_expr(expr));
            out.push('\n');
        }
        Stmt::FieldAssign {
            base, name, value, ..
        } => {
            out.push_str(&indent);
            out.push_str(&format_expr(base));
            out.push('.');
            out.push_str(name);
            out.push_str(" = ");
            out.push_str(&format_expr(value));
            out.push('\n');
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            out.push_str(&indent);
            out.push_str(&format_expr(base));
            out.push('[');
            out.push_str(&format_expr(index));
            out.push_str("] = ");
            out.push_str(&format_expr(value));
            out.push('\n');
        }
        Stmt::Return(None) => {
            out.push_str(&indent);
            out.push_str("return\n");
        }
        Stmt::Break => out.push_str(&format!("{indent}break\n")),
        Stmt::Continue => out.push_str(&format!("{indent}continue\n")),
        Stmt::Return(Some(expr)) => {
            out.push_str(&indent);
            out.push_str("return ");
            out.push_str(&format_expr(expr));
            out.push('\n');
        }
        Stmt::Expr(Expr::Call { callee, args })
            if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::INDEX_MODIFY_PLACE_FIRST)
                && matches!(args.as_slice(),[_,_,_,Expr::StringLit(op)] if matches!(op.as_str(),"+"|"-"|"*"|"/"|"%"|"&"|"|"|"^"|"<<"|">>")) =>
        {
            let [base, index, rhs, Expr::StringLit(op)] = args.as_slice() else {
                unreachable!()
            };
            out.push_str(&indent);
            out.push_str(&format_expr(base));
            out.push('[');
            out.push_str(&format_expr(index));
            out.push_str("] ");
            out.push_str(op);
            out.push_str("= ");
            out.push_str(&format_expr(rhs));
            out.push('\n');
        }
        Stmt::Expr(Expr::Call { callee, args })
            if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::FIELD_MODIFY_PLACE_FIRST)
                && matches!(args.as_slice(), [_, Expr::StringLit(field), _, Expr::StringLit(op)]
                    if !field.is_empty() && field.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || (!ch.is_ascii() && !ch.is_whitespace() && !ch.is_control()))
                        && matches!(op.as_str(), "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>")) =>
        {
            let [base, Expr::StringLit(field), rhs, Expr::StringLit(op)] = args.as_slice() else {
                unreachable!()
            };
            out.push_str(&indent);
            out.push_str(&format_expr(base));
            out.push('.');
            out.push_str(field);
            out.push(' ');
            out.push_str(op);
            out.push_str("= ");
            out.push_str(&format_expr(rhs));
            out.push('\n');
        }
        Stmt::Expr(expr) => {
            out.push_str(&indent);
            out.push_str(&format_expr(expr));
            out.push('\n');
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            format_if(cond, then_body, else_body, depth, &indent, out);
        }
        Stmt::Loop {
            kind: LoopKind::While,
            cond: Some(cond),
            body,
        } => {
            format_while_loop(cond, body, depth, &indent, out);
        }
        Stmt::Loop { body, .. } => {
            format_loop(body, depth, &indent, out);
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            format_match(scrutinee, arms, depth, &indent, out);
        }
        Stmt::Throw(value) => out.push_str(&format!("{indent}throw {}\n", format_expr(value))),
        Stmt::Propagate => out.push_str(&format!("{indent}propagate\n")),
        Stmt::Try { body, catches } => {
            out.push_str(&format!("{indent}try {{\n"));
            format_body(body, depth + 1, out);
            out.push_str(&format!("{indent}}}"));
            for catch in catches {
                out.push_str(&format!(" catch {} {{\n", catch.pattern));
                format_body(&catch.body, depth + 1, out);
                out.push_str(&format!("{indent}}}"));
            }
            out.push('\n');
        }
    }
}

fn format_expr(expr: &Expr) -> String {
    match expr {
        Expr::UnitLit => "()".into(),
        Expr::IntLit(n) => n.to_string(),
        // Overflow notation preserves infinities accepted by the Float parser.
        Expr::FloatLit(f) if f.0.is_infinite() => if f.0.is_sign_negative() {
            "-1e999"
        } else {
            "1e999"
        }
        .into(),
        Expr::FloatLit(f) => format!("{:?}", f.0),
        Expr::StringLit(value) => format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t")
        ),
        Expr::BoolLit(value) => value.to_string(),
        Expr::Ident(name) => name.clone(),
        // Preserve expression boundaries independently of the surrounding
        // operator or postfix call/index/field position.
        Expr::Unary { op, expr, .. } => format!("({op}{})", format_expr(expr)),
        Expr::Binary { op, lhs, rhs, .. } => {
            format!("({} {} {})", format_expr(lhs), op, format_expr(rhs))
        }
        Expr::StructInit { name, fields, .. } => {
            let rendered = fields
                .iter()
                .map(|(field, expr)| format!("{field}: {}", format_expr(expr)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name} {{ {rendered} }}")
        }
        Expr::Field { base, name, .. } => {
            format!("{}.{}", format_expr(base), name)
        }
        Expr::ArrayLit(items) => {
            let rendered = items.iter().map(format_expr).collect::<Vec<_>>().join(", ");
            format!("[{rendered}]")
        }
        Expr::Index { base, index, .. } => {
            format!("{}[{}]", format_expr(base), format_expr(index))
        }
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(name) = callee.as_ref()
                && let Some(method) = name.strip_prefix("__method__")
                && let Some((base, rest)) = args.split_first()
            {
                let rendered_args = rest.iter().map(format_expr).collect::<Vec<_>>().join(", ");
                return format!("{}.{}({rendered_args})", format_expr(base), method);
            }
            let mut out = format_expr(callee);
            out.push('(');
            for (idx, arg) in args.iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format_expr(arg));
            }
            out.push(')');
            out
        }
        Expr::MatchValue { scrutinee, arms } => {
            let mut out = format!("(match ({}) {{\n", format_expr(scrutinee));
            for arm in arms {
                out.push_str("  ");
                out.push_str(&arm.pattern);
                if let Some(guard) = &arm.guard {
                    out.push_str(" if ");
                    out.push_str(&format_expr(guard));
                }
                out.push_str(" -> ");
                out.push_str(&format_value_branch(&arm.value));
                out.push('\n');
            }
            out.push_str("})");
            out
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => format!(
            "(if ({}) {} else {})",
            format_expr(cond),
            format_value_branch(then_expr),
            if matches!(else_expr.as_ref(), Expr::Conditional { .. }) {
                let nested = format_expr(else_expr);
                nested[1..nested.len() - 1].to_string()
            } else {
                format_value_branch(else_expr)
            }
        ),
        Expr::Block { body, result } => {
            let mut out = String::from("({\n");
            for stmt in body {
                let mut rendered = String::new();
                format_stmt(stmt, 1, &mut rendered);
                out.push_str(rendered.trim_end());
                out.push_str(";\n");
            }
            if let Some(result) = result {
                out.push_str("  ");
                out.push_str(&format_expr(result));
                out.push('\n');
            }
            out.push_str("})");
            out
        }
        Expr::Closure {
            params, ret, body, ..
        } => {
            let params = params
                .iter()
                .map(|(name, typ)| format!("{name}: {}", format_type(typ)))
                .collect::<Vec<_>>()
                .join(", ");
            // Parentheses preserve closure values in call/argument positions.
            let mut out = format!("(fn({params}) -> {} {{\n", format_type(ret));
            format_body(body, 1, &mut out);
            out.push_str("})");
            out
        }
    }
}

fn format_value_branch(expr: &Expr) -> String {
    let rendered = format_expr(expr);
    if matches!(expr, Expr::Block { .. }) {
        rendered[1..rendered.len() - 1].to_string()
    } else {
        format!("{{ {rendered} }}")
    }
}

fn format_if(
    cond: &Expr,
    then_body: &[Stmt],
    else_body: &[Stmt],
    depth: usize,
    indent: &str,
    out: &mut String,
) {
    out.push_str(indent);
    out.push_str("if (");
    out.push_str(&format_expr(cond));
    out.push_str(") {\n");
    format_body(then_body, depth + 1, out);
    out.push_str(indent);
    out.push('}');
    if else_body.is_empty() {
        out.push('\n');
    } else {
        out.push_str(" else {\n");
        format_body(else_body, depth + 1, out);
        out.push_str(indent);
        out.push_str("}\n");
    }
}

fn format_while_loop(cond: &Expr, body: &[Stmt], depth: usize, indent: &str, out: &mut String) {
    out.push_str(indent);
    out.push_str("while (");
    out.push_str(&format_expr(cond));
    out.push_str(") {\n");
    format_body(body, depth + 1, out);
    out.push_str(indent);
    out.push_str("}\n");
}

fn format_loop(body: &[Stmt], depth: usize, indent: &str, out: &mut String) {
    out.push_str(indent);
    out.push_str("while true {\n");
    format_body(body, depth + 1, out);
    out.push_str(indent);
    out.push_str("}\n");
}

fn format_match(scrutinee: &Expr, arms: &[MatchArm], depth: usize, indent: &str, out: &mut String) {
    out.push_str(indent);
    out.push_str("match (");
    out.push_str(&format_expr(scrutinee));
    out.push_str(") {\n");
    for arm in arms {
        out.push_str(indent);
        out.push_str("  ");
        out.push_str(&arm.pattern);
        if let Some(guard) = &arm.guard {
            out.push_str(" if ");
            out.push_str(&format_expr(guard));
        }
        out.push_str(" {\n");
        format_body(&arm.body, depth + 2, out);
        out.push_str(indent);
        out.push_str("  }\n");
    }
    out.push_str(indent);
    out.push_str("}\n");
}

#[cfg(test)]
mod tests {
    #[test]
    fn canonical_string_literals_roundtrip_control_bytes_and_unicode() {
        for value in ["\0", "\u{1}\u{7}\u{1b}", "\\0\\u{1}", "λ雪\n\r\t\"\\"] {
            let literal = super::format_expr(&crate::core_ir::Expr::StringLit(value.into()));
            assert_eq!(
                crate::core_ir::decode_string_literal(&literal).as_deref(),
                Some(value)
            );
            let source = format!("const word:String={literal};\nfn main()->String{{return word;}}");
            let canonical = super::canonicalize_in_source(&source).unwrap();
            assert_eq!(
                crate::in_lang_parse::parse_in_source(&canonical).unwrap(),
                crate::in_lang_parse::parse_in_source(&source).unwrap(),
                "{value:?}: {canonical:?}"
            );
        }
    }

    use super::*;
    use crate::in_lang_parse::parse_in_source;

    #[test]
    fn canonical_components_preserve_defaults_and_target_strings() {
        for source in [
            "component Empty {\n}\n",
            r#"component Bare {
 target a"//"b
 capability noop: Console()
}"#,
            r#"component Escaped {
 target "path\\tail\\\"//{snow}雪";
 deterministic false
 checkpoint incremental;
}"#,
            r#"component Quoted {
 target "'quoted'";
}"#,
        ] {
            let source = format!("{source}\nfn main()->Int{{return 42;}}");
            let before = parse_in_source(&source).unwrap();
            let canonical = canonicalize_in_source(&source).unwrap();
            assert_eq!(parse_in_source(&canonical).unwrap(), before, "{canonical}");
            assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        }
        let module = parse_in_source(
            r#"component Escaped {
 target "a\\b\"c";
}
fn main()->Int{return 42;}
"#,
        )
        .unwrap();
        assert!(matches!(&module.decls[0], Decl::Component { target, .. } if target == "a\\b\"c"));
    }

    #[test]
    fn canonical_closures_preserve_params_return_body_and_nested_scopes() {
        for source in [
            "fn main()->Int{let f=fn(value:Float)->Float{let copy=value;return copy;};if f(1.5)==1.5{return 42;}return 0;}",
            "fn main()->Int{let f=fn()->Int{let inner=fn(value:Int)->Int{return value+1;};return inner(41);};return f();}",
            "fn main()->Int{let value:Int=999;let f=fn(value:Int)->Int{return value;};return f(42);}",
            "fn main()->Int{let f=fn(flag:Bool,text:String)->Int{if flag && text==\"λ雪\"{return 42;}return 0;};return f(true,\"λ雪\");}",
            // Source closure syntax has implicit lexical names, not an explicit
            // Core IR captures list. Parsing preserves the body references.
            "fn main()->Int{let captured:Int=42;let f=fn()->Int{return captured;};return 42;}",
        ] {
            let original = parse_in_source(source).unwrap();
            let canonical = canonicalize_in_source(source).unwrap();
            assert!(!canonical.contains("= closure"), "{canonical}");
            assert_eq!(
                parse_in_source(&canonical).unwrap(),
                original,
                "{canonical}"
            );
            assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        }
    }

    #[test]
    fn canonical_operator_grouping_preserves_nested_and_postfix_expressions() {
        for source in [
            "!(read_once() != 0)",
            "!!(read_once() != 0)",
            "~(left() | right())",
            "-(left() + right())",
            "(left() + right()) * other()",
            "left() - (right() - other())",
            "left() / (right() * other())",
            "(left() || right()) && other()",
            "left() << (right() + other())",
            "accept(!(read_once() != 0), left() - (right() - other()))",
            "values[(left() + right()) * other()]",
            "(left() + right())[index()]",
            "(left() + right()).field",
            "(!flag).field",
            "(!flag)[index()]",
            "(!flag)(argument())",
            "(left() + right())(argument())",
        ] {
            let original = crate::in_lang_parse::parse_expr(source);
            let canonical = format_expr(&original);
            assert_eq!(
                crate::in_lang_parse::parse_expr(&canonical),
                original,
                "{source}: {canonical}"
            );
            assert_eq!(
                format_expr(&crate::in_lang_parse::parse_expr(&canonical)),
                canonical
            );
        }
    }

    #[test]
    fn canonical_closure_expression_operands_roundtrip() {
        for source in [
            "fn(value:Float)->Float{return value;}",
            "[fn(value:Int)->Int{return value;}]",
            "accept(fn(value:Int)->Int{return value;})",
            "(fn(value:Int)->Int{return value;})(42)",
        ] {
            let original = crate::in_lang_parse::parse_expr(source);
            assert!(
                match &original {
                    Expr::Closure { .. } => true,
                    Expr::ArrayLit(items) => matches!(items.as_slice(), [Expr::Closure { .. }]),
                    Expr::Call { callee, args } =>
                        matches!(callee.as_ref(), Expr::Closure { .. })
                            || matches!(args.as_slice(), [Expr::Closure { .. }]),
                    _ => false,
                },
                "closure was not parsed: {source}: {original:?}"
            );
            let canonical = format_expr(&original);
            assert_eq!(
                crate::in_lang_parse::parse_expr(&canonical),
                original,
                "{canonical}"
            );
        }
    }

    #[test]
    fn canonical_classes_preserve_fields_methods_and_contracts() {
        for source in [
            "class Holder{flags:[Bool];amounts:[Float];words:[String];fn change()->Int{flags[0]=true;amounts[0]=2.5;words[0]=\"λ雪\";return 0;}}\nfn main()->Int{let h=Holder{flags:[false],amounts:[0.0],words:[\"\"]};h.change();if h.flags[0] && h.amounts[0]==2.5 && h.words[0]==\"λ雪\"{return 42;}return 0;}",
            "interface Readable{fn read()->Int}\nclass Base{value:Int;fn read()->Int{return value;}}\nclass Child extends Base implements Readable{value:Int;fn new(value:Int)->Child{return Child{value:value};} fn read()->Int{return value;}}\nfn main()->Int{let value=Child{value:42};return value.read();}",
        ] {
            let parsed = parse_in_source(source).unwrap();
            let canonical = canonicalize_in_source(source).unwrap();
            let after = parse_in_source(&canonical).unwrap();
            assert_eq!(after, parsed, "{canonical}");
            assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
            for decl in &after.decls {
                if let Decl::Class {
                    name: owner,
                    fields,
                    methods,
                    ..
                } = decl
                {
                    assert!(!methods.is_empty());
                    if owner == "Holder" {
                        assert_eq!(fields.len(), 3);
                        let Decl::Function {
                            name, params, body, ..
                        } = &methods[0]
                        else {
                            panic!("missing change method")
                        };
                        assert_eq!(name, "change");
                        assert!(params.is_empty());
                        assert!(
                            matches!(body.first(), Some(Stmt::IndexAssign { base, .. }) if base == &Expr::Ident("flags".into()))
                        );
                    }
                    for method in methods {
                        let Decl::Function {
                            name, params, body, ..
                        } = method
                        else {
                            panic!("non-function method")
                        };
                        assert!(!name.contains("Holder_") && !name.contains("Child_"));
                        assert!(!params.iter().any(|(name, _)| name == "self"));
                        assert!(!body.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn canonical_float_literals_preserve_type_and_bits() {
        for literal in [
            "0.0",
            "-0.0",
            "1.0",
            "-42.0",
            "1.5",
            "1e20",
            "1e-20",
            "1.7976931348623157e308",
            "2.2250738585072014e-308",
            "5e-324",
            "1e999",
            "-1e999",
        ] {
            let original = crate::in_lang_parse::parse_expr(literal);
            assert!(matches!(original, Expr::FloatLit(_)), "{literal}");
            let rendered = format_expr(&original);
            assert_eq!(
                crate::in_lang_parse::parse_expr(&rendered),
                original,
                "{rendered}"
            );
            let source = format!(
                "var amount:Float={literal};\nfn main()->Float{{amount={literal};return amount;}}"
            );
            let canonical = canonicalize_in_source(&source).unwrap();
            assert_eq!(
                parse_in_source(&source).unwrap(),
                parse_in_source(&canonical).unwrap(),
                "{canonical}"
            );
            assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        }
    }

    #[test]
    fn canonical_globals_keep_mutability_types_and_initializers() {
        let source =
            "const base:Int=20;\nvar total:Int=22;\nfn main()->Int{total+=base;return total;}";
        let before = parse_in_source(source).unwrap();
        let canonical = canonicalize_in_source(source).unwrap();
        let after = parse_in_source(&canonical).unwrap();
        assert_eq!(before.decls, after.decls, "{canonical}");
        assert_eq!(
            after
                .decls
                .iter()
                .filter(|decl| matches!(decl, Decl::Global { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn canonicalizes_example_style_source() {
        let src = r#"
fn main() { printone(); }
fn printone() {
    print("1");
}
"#;

        let canonical = canonicalize_in_source(src).expect("canonicalize");

        assert_eq!(
            canonical,
            r#"fn main() -> void {
  printone()
}

fn printone() -> void {
  print("1")
}
"#
        );
    }

    #[test]
    fn canonical_output_remains_parseable() {
        let src = r#"
struct Session {
  Int id
  String label
}
fn note(text: String) -> VOID { return; }
fn main() {
  let seed: Int = 0;
  note("ready");
  return;
}
"#;

        let canonical = canonicalize_in_source(src).expect("canonicalize");

        parse_in_source(&canonical).expect("canonical output parses");
        assert!(canonical.contains("  Int id\n"));
        assert!(canonical.contains("  String label\n"));
        assert!(canonical.contains("fn note(text: String) -> void {\n"));
        assert!(canonical.contains("  let seed: Int = 0\n"));
        assert!(!canonical.contains(';'));
    }

    #[test]
    fn handles_empty_input() {
        let result = canonicalize_in_source("");
        assert!(result.is_err());
    }

    #[test]
    fn returns_error_on_invalid_syntax() {
        let result = canonicalize_in_source("!@#$");
        assert!(result.is_err());
    }

    #[test]
    fn canonicalizes_complex_source_constructs() {
        let src = r#"
struct Point {
  Int x
  Int y
}

fn compute() -> Int {
  let p: Point = Point { x: 1, y: 2 };
  p.x = 3;
  let arr: [Int] = [1, 2, 3];
  arr[0] = 4;
  while p.x < 10 {
    p.x = p.x + 1;
  }
  match p.y {
    1 { return 1; }
    2 { return 2; }
  }
  if p.x == 10 {
    return 10;
  } else {
    return 0;
  }
  while true {
    break;
  }
  return p.x;
}

fn main() { return; }
"#;
        let canonical = canonicalize_in_source(src).expect("canonicalize");
        assert!(canonical.contains("struct Point {\n  Int x\n  Int y\n}"));
        assert!(canonical.contains("let p: Point = Point { x: 1, y: 2 }"));
        assert!(canonical.contains("p.x = 3"));
        assert!(canonical.contains("let arr: [Int] = [1, 2, 3]"));
        assert!(canonical.contains("arr[0] = 4"));
        assert!(canonical.contains("while ((p.x < 10)) {"));
        assert!(canonical.contains("match (p.y) {"));
        assert!(canonical.contains("if ((p.x == 10)) {"));
        assert!(canonical.contains("while (true) {"));
        assert_eq!(
            parse_in_source(&canonical).unwrap(),
            parse_in_source(src).unwrap()
        );
    }
}

#[cfg(test)]
mod surface_preservation_tests {
    use super::*;
    use crate::in_lang_parse::{parse_in_source, parse_in_surface_info};

    #[test]
    fn canonical_preserves_surface_facts_and_generated_binding_provenance() {
        let source = r#"package demo;module demo.main;import std.io;use database.postgres;bind database.postgres as postgres;capability console.write;enable trace;@pure;fn helper()->Int{
return 42;
}parallel {helper();}system graph {capability hidden;extern c fn hidden()->Int;}extern c fn external(value:Int)->Int requires console.write;fn main()->Int{return helper();}"#;
        let before = parse_in_source(source).unwrap();
        let facts = parse_in_surface_info(source).unwrap();
        let canonical = canonicalize_in_source(source).unwrap();
        assert_eq!(parse_in_source(&canonical).unwrap(), before);
        assert_eq!(parse_in_surface_info(&canonical).unwrap(), facts);
        assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        assert!(canonical.contains("extern c fn external"));
        assert!(!canonical.contains("fn print("));
        assert!(canonical.contains("system graph {capability hidden;extern c fn hidden()->Int;}"));
    }

    #[test]
    fn canonical_keeps_annotation_order_and_interrupt_modifier() {
        let source = "@pure;@gpu;fn helper()->Int{return 42;}@parallel-safe;var value:Int=42;interrupt fn irq()->Void{return;}fn main()->Int{return value;}";
        let canonical = canonicalize_in_source(source).unwrap();
        assert_eq!(
            parse_in_surface_info(&canonical).unwrap(),
            parse_in_surface_info(source).unwrap()
        );
        assert!(canonical.contains("interrupt fn irq"));
        assert_eq!(
            parse_in_source(&canonical).unwrap(),
            parse_in_source(source).unwrap()
        );
        assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
    }

    #[test]
    fn canonical_retains_local_imports_for_file_resolution() {
        let directory = std::env::temp_dir().join(format!(
            "in-canonical-imports-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("helper.in"), "fn helper()->Int{return 42;}").unwrap();
        let path = directory.join("main.in");
        let source =
            r#"import "./helper.in";capability console.write;fn main()->Int{return helper();}"#;
        std::fs::write(&path, source).unwrap();
        let before = crate::in_lang_parse::parse_in_file(&path).unwrap();
        let canonical = canonicalize_in_source(source).unwrap();
        std::fs::write(&path, &canonical).unwrap();
        assert_eq!(crate::in_lang_parse::parse_in_file(&path).unwrap(), before);
        assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn canonical_preserves_struct_method_expansion_provenance() {
        let source = "struct Pair {Int value; fn read()->Int{return self.value;} fn add(extra:Int)->Int{return self.value+extra;}}fn main()->Int{let pair=Pair{value:20};return pair.add(22);}";
        let before = parse_in_source(source).unwrap();
        let canonical = canonicalize_in_source(source).unwrap();
        assert_eq!(parse_in_source(&canonical).unwrap(), before);
        assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        assert_eq!(canonical.matches("fn read(").count(), 1);
        assert!(!canonical.contains("fn Pair_read("));
    }
}

#[cfg(test)]
mod private_field_modify_canonical_tests {
    use super::*;
    #[test]
    fn all_compound_fields_roundtrip_without_exposing_private_marker() {
        for op in ["+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>"] {
            let source = format!(
                "struct Row{{Int x}}\nfn at()->Int{{return 0;}}\nfn rhs()->Int{{return 2;}}\nfn main()->Int{{let rows:[Row]=[Row{{x:40}}];rows[at()].x{op}=rhs();return rows[0].x;}}"
            );
            let before = crate::in_lang_parse::parse_in_source(&source).unwrap();
            let canonical = canonicalize_in_source(&source).unwrap();
            assert!(!canonical.contains(crate::core_ir::FIELD_MODIFY_PLACE_FIRST));
            assert_eq!(
                crate::in_lang_parse::parse_in_source(&canonical).unwrap(),
                before
            );
            assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        }
    }
    #[test]
    fn unicode_compound_fields_preserve_source_roundtrip() {
        for field in ["café", "变量", "🦀", "field_1"] {
            let source = format!(
                "struct Row{{Int {field}}}\nfn main()->Int{{let row=Row{{{field}:40}};row.{field}+=2;return row.{field};}}"
            );
            let before = crate::in_lang_parse::parse_in_source(&source).unwrap();
            let canonical = canonicalize_in_source(&source).unwrap();
            assert!(!canonical.contains(crate::core_ir::FIELD_MODIFY_PLACE_FIRST));
            assert_eq!(
                crate::in_lang_parse::parse_in_source(&canonical).unwrap(),
                before
            );
            assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        }
    }
    #[test]
    fn malformed_private_marker_remains_private_and_refused() {
        for args in [
            vec![Expr::IntLit(0)],
            vec![
                Expr::Ident("row".into()),
                Expr::StringLit("x".into()),
                Expr::IntLit(1),
                Expr::StringLit("invalid".into()),
            ],
            vec![
                Expr::Ident("row".into()),
                Expr::StringLit("x; injected()".into()),
                Expr::IntLit(1),
                Expr::StringLit("+".into()),
            ],
        ] {
            let stmt = Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::FIELD_MODIFY_PLACE_FIRST.into())),
                args,
            });
            let mut text = String::new();
            format_stmt(&stmt, 0, &mut text);
            assert!(text.contains(crate::core_ir::FIELD_MODIFY_PLACE_FIRST));
        }
    }
}

#[cfg(test)]
mod private_index_modify_canonical_tests {
    use super::*;
    #[test]
    fn indexed_compounds_roundtrip_nested_effectful_places_and_all_operators() {
        for op in ["+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>"] {
            let source = format!(
                "fn get()->[Int]{{return [40];}}fn at()->Int{{return 0;}}fn rhs()->Int{{return 2;}}fn main()->Int{{get()[at()]{op}=rhs();return 42;}}"
            );
            let before = crate::in_lang_parse::parse_in_source(&source).unwrap();
            let canonical = canonicalize_in_source(&source).unwrap();
            assert!(!canonical.contains(crate::core_ir::INDEX_MODIFY_PLACE_FIRST));
            assert_eq!(
                crate::in_lang_parse::parse_in_source(&canonical).unwrap(),
                before
            );
            assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        }
        let source = "fn main()->Int{let rows:[[Int]]=[[40]];rows[0][0]+=2;return rows[0][0];}";
        let canonical = canonicalize_in_source(source).unwrap();
        assert_eq!(
            crate::in_lang_parse::parse_in_source(&canonical).unwrap(),
            crate::in_lang_parse::parse_in_source(source).unwrap()
        );
        assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
    }
    #[test]
    fn private_value_first_calls_preserve_explicit_identity_and_order_contract() {
        for (name, args) in [
            (crate::core_ir::INDEX_ASSIGN_VALUE_FIRST, "values,0,42"),
            (crate::core_ir::INDEX_MODIFY_VALUE_FIRST, "values,0,2,\"+\""),
            (
                crate::core_ir::FIELD_MODIFY_VALUE_FIRST,
                "row,\"x\",2,\"+\"",
            ),
            (crate::core_ir::MODIFY_VALUE_FIRST, "value,2,\"+\""),
            (crate::core_ir::ARRAY_REPEAT, "42,1"),
        ] {
            let source = format!(
                "struct Row{{Int x}}fn main()->Int{{let values:[Int]=[40];let row=Row{{x:40}};let value:Int=40;{name}({args});return 42;}}"
            );
            let canonical = canonicalize_in_source(&source).unwrap();
            assert!(canonical.contains(name), "{name}: {canonical}");
            assert_eq!(canonicalize_in_source(&canonical).unwrap(), canonical);
        }
    }
    #[test]
    fn malformed_index_compound_stays_private_for_refusal() {
        for args in [
            vec![Expr::IntLit(0)],
            vec![
                Expr::Ident("values".into()),
                Expr::IntLit(0),
                Expr::IntLit(1),
                Expr::StringLit("invalid".into()),
            ],
        ] {
            let mut text = String::new();
            format_stmt(
                &Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::INDEX_MODIFY_PLACE_FIRST.into())),
                    args,
                }),
                0,
                &mut text,
            );
            assert!(text.contains(crate::core_ir::INDEX_MODIFY_PLACE_FIRST));
        }
    }
}
