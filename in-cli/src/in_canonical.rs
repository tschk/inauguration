use crate::core_ir::{Decl, Expr, LoopKind, MatchArm, Stmt, Typ, UnifiedModule};
use crate::in_lang_parse::parse_in_source;

pub fn canonicalize_in_source(source: &str) -> Result<String, String> {
    let module = parse_in_source(source)?;
    Ok(format_module(&module))
}

fn format_module(module: &UnifiedModule) -> String {
    let mut out = String::new();
    for (idx, decl) in module.decls.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
            out.push('\n');
        }
        format_decl(decl, &mut out);
    }
    out.push('\n');
    out
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
        Decl::Class { .. } | Decl::Interface { .. } | Decl::Component { .. } => {}
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
        Expr::FloatLit(f) => f.0.to_string(),
        Expr::StringLit(value) => format!("{value:?}"),
        Expr::BoolLit(value) => value.to_string(),
        Expr::Ident(name) => name.clone(),
        Expr::Unary { op, expr, .. } => {
            let mut out = op.clone();
            out.push_str(&format_expr(expr));
            out
        }
        Expr::Binary { op, lhs, rhs, .. } => {
            format!("{} {} {}", format_expr(lhs), op, format_expr(rhs))
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
            format_value_branch(else_expr)
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
        Expr::Closure { .. } => "closure".to_string(),
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
    out.push_str("if ");
    out.push_str(&format_expr(cond));
    out.push_str(" {\n");
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
    out.push_str("while ");
    out.push_str(&format_expr(cond));
    out.push_str(" {\n");
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
    out.push_str("match ");
    out.push_str(&format_expr(scrutinee));
    out.push_str(" {\n");
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
    use super::*;
    use crate::in_lang_parse::parse_in_source;

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
        assert!(canonical.contains("while p.x < 10 {"));
        assert!(canonical.contains("match p.y {"));
        assert!(canonical.contains("if p.x == 10 {"));
        assert!(canonical.contains("while true {"));
    }
}
