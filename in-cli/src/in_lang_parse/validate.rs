use crate::core_ir::{
    looks_like_int_literal, Decl, Expr, MethodSig, Stmt, Typ, UnifiedModule,
};
use std::collections::{HashMap, HashSet};

pub(crate) fn collect_top_level_type_names(module: &UnifiedModule) -> Vec<String> {
    module
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Struct { name, .. } | Decl::Class { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn duplicate_top_level_names(module: &UnifiedModule) -> Vec<String> {
    let mut names = Vec::new();
    for d in &module.decls {
        match d {
            Decl::Struct { name, .. } | Decl::Class { name, .. } => names.push(name.clone()),
            Decl::Function { name, .. } => names.push(name.clone()),
            Decl::Interface { .. } => {}
            Decl::Component { name, .. } => names.push(name.clone()),
            Decl::Global { name, .. } => names.push(name.clone()),
        }
    }
    let mut seen = HashSet::new();
    let mut dups = Vec::new();
    for n in names {
        if !seen.insert(n.clone()) {
            dups.push(n);
        }
    }
    dups
}

pub(crate) fn type_known(structs: &HashSet<&str>, t: &Typ) -> bool {
    match t {
        Typ::Named(n) => structs.contains(n.as_str()),
        Typ::Array(item) => type_known(structs, item),
        Typ::Vector(item) => type_known(structs, item),
        Typ::Int | Typ::Float | Typ::String | Typ::Bool | Typ::Void => true,
        Typ::Generic(_) => false,
    }
}

pub(crate) fn method_sig_matches(required: &MethodSig, actual: &Decl) -> bool {
    match actual {
        Decl::Function {
            name, params, ret, ..
        } => name == &required.name && params == &required.params && ret == &required.ret,
        _ => false,
    }
}

pub(crate) fn validate_class_contracts(module: &UnifiedModule) -> Result<(), String> {
    type ClassContract<'a> = (&'a Option<String>, &'a Vec<String>, &'a Vec<Decl>);
    let classes: HashMap<&str, ClassContract<'_>> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Class {
                name,
                extends,
                implements,
                methods,
                ..
            } => Some((name.as_str(), (extends, implements, methods))),
            _ => None,
        })
        .collect();
    let interfaces: HashMap<&str, &Vec<MethodSig>> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Interface { name, methods, .. } => Some((name.as_str(), methods)),
            _ => None,
        })
        .collect();

    for (class_name, (extends, implements, methods)) in classes {
        if let Some(parent) = extends
            && !module.decls.iter().any(|decl| {
                matches!(decl, Decl::Class { name, .. } if name == parent)
                    || matches!(decl, Decl::Struct { name, .. } if name == parent)
            })
        {
            return Err(format!(
                ".in: class `{class_name}` extends unknown class `{parent}`"
            ));
        }
        for iface in implements {
            let Some(required_methods) = interfaces.get(iface.as_str()) else {
                return Err(format!(
                    ".in: class `{class_name}` implements unknown interface `{iface}`"
                ));
            };
            for required in *required_methods {
                if !methods
                    .iter()
                    .any(|method| method_sig_matches(required, method))
                {
                    return Err(format!(
                        ".in: class `{class_name}` does not implement `{iface}.{}`",
                        required.name
                    ));
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn validate_expr_shapes(
    fn_name: &str,
    structs: &HashMap<String, Vec<String>>,
    expr: &Expr,
) -> Result<(), String> {
    match expr {
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_) => Ok(()),
        Expr::Ident(name) => {
            // The expression parser turns tokens it cannot classify into
            // identifiers. A numeric-looking token that failed the i64 parse
            // is an out-of-range literal, not a name: refuse it here so it
            // never lowers to a silent 0.
            if looks_like_int_literal(name) {
                return Err(format!(
                    ".in: integer literal `{name}` out of i64 range in fn {fn_name}"
                ));
            }
            Ok(())
        }
        Expr::Unary { expr, .. } => validate_expr_shapes(fn_name, structs, expr),
        Expr::Binary { lhs, rhs, .. } => {
            validate_expr_shapes(fn_name, structs, lhs)?;
            validate_expr_shapes(fn_name, structs, rhs)
        }
        Expr::ArrayLit(items) => {
            for item in items {
                validate_expr_shapes(fn_name, structs, item)?;
            }
            Ok(())
        }
        Expr::Index { base, index, .. } => {
            validate_expr_shapes(fn_name, structs, base)?;
            validate_expr_shapes(fn_name, structs, index)
        }
        Expr::Call { callee, args, .. } => {
            validate_expr_shapes(fn_name, structs, callee)?;
            for arg in args {
                validate_expr_shapes(fn_name, structs, arg)?;
            }
            Ok(())
        }
        Expr::Field { base, .. } => validate_expr_shapes(fn_name, structs, base),
        Expr::StructInit { name, fields, .. } => {
            let schema = structs.get(name).ok_or(format!(
                ".in: unknown struct initializer `{name}` in fn {fn_name}"
            ))?;
            let mut seen = HashSet::new();
            for (field, expr) in fields {
                if !seen.insert(field.as_str()) {
                    return Err(format!(
                        ".in: duplicate field `{name}.{field}` in fn {fn_name}"
                    ));
                }
                if !schema.iter().any(|known| known == field) {
                    return Err(format!(
                        ".in: unknown field `{name}.{field}` in fn {fn_name}"
                    ));
                }
                validate_expr_shapes(fn_name, structs, expr)?;
            }
            for field in schema {
                if !seen.contains(field.as_str()) {
                    return Err(format!(
                        ".in: missing field `{name}.{field}` in fn {fn_name}"
                    ));
                }
            }
            Ok(())
        }
        Expr::Closure { .. } => Ok(()),
    }
}

pub(crate) fn validate_stmt_types(
    fn_name: &str,
    structs: &HashSet<&str>,
    struct_fields: &HashMap<String, Vec<String>>,
    stmt: &Stmt,
) -> Result<(), String> {
    match stmt {
        Stmt::Let(_, Some(ty), expr) => {
            if !type_known(structs, ty) {
                return Err(format!(
                    ".in: unknown type in `let` annotation in fn {fn_name}"
                ));
            }
            validate_expr_shapes(fn_name, struct_fields, expr)?;
        }
        Stmt::Let(_, None, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Return(Some(expr))
        | Stmt::Expr(expr) => {
            validate_expr_shapes(fn_name, struct_fields, expr)?;
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            validate_expr_shapes(fn_name, struct_fields, base)?;
            validate_expr_shapes(fn_name, struct_fields, index)?;
            validate_expr_shapes(fn_name, struct_fields, value)?;
        }
        Stmt::FieldAssign { base, value, .. } => {
            validate_expr_shapes(fn_name, struct_fields, base)?;
            validate_expr_shapes(fn_name, struct_fields, value)?;
        }
        Stmt::Return(None) => {}
        Stmt::Break | Stmt::Propagate => {}
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            validate_expr_shapes(fn_name, struct_fields, cond)?;
            for nested in then_body {
                validate_stmt_types(fn_name, structs, struct_fields, nested)?;
            }
            for nested in else_body {
                validate_stmt_types(fn_name, structs, struct_fields, nested)?;
            }
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(cond) = cond {
                validate_expr_shapes(fn_name, struct_fields, cond)?;
            }
            for nested in body {
                validate_stmt_types(fn_name, structs, struct_fields, nested)?;
            }
        }
        Stmt::Match { scrutinee, arms } => {
            validate_expr_shapes(fn_name, struct_fields, scrutinee)?;
            for arm in arms {
                for nested in &arm.body {
                    validate_stmt_types(fn_name, structs, struct_fields, nested)?;
                }
            }
        }
        Stmt::Throw(expr) => {
            validate_expr_shapes(fn_name, struct_fields, expr)?;
        }
        Stmt::Try { body, catches, .. } => {
            for stmt in body {
                validate_stmt_types(fn_name, structs, struct_fields, stmt)?;
            }
            for catch in catches {
                for stmt in &catch.body {
                    validate_stmt_types(fn_name, structs, struct_fields, stmt)?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn desugar_method_calls(module: &mut UnifiedModule) {
    let struct_fields: HashMap<String, HashMap<String, Typ>> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Struct { name, fields, .. } | Decl::Class { name, fields, .. } => Some((
                name.clone(),
                fields
                    .iter()
                    .map(|(field, typ)| (field.clone(), typ.clone()))
                    .collect(),
            )),
            _ => None,
        })
        .collect();
    let fn_rets: HashMap<String, Typ> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, ret, .. } => Some((name.clone(), ret.clone())),
            _ => None,
        })
        .collect();

    for decl in &mut module.decls {
        if let Decl::Function { params, body, .. } = decl {
            let mut env: HashMap<String, Typ> = params.iter().cloned().collect();
            desugar_method_calls_in_body(body, &mut env, &struct_fields, &fn_rets);
        }
    }
}

pub(crate) fn desugar_method_calls_in_body(
    body: &mut [Stmt],
    env: &mut HashMap<String, Typ>,
    structs: &HashMap<String, HashMap<String, Typ>>,
    fn_rets: &HashMap<String, Typ>,
) {
    for stmt in body {
        match stmt {
            Stmt::Let(name, typ, expr) => {
                desugar_method_calls_in_expr(expr, env, structs, fn_rets);
                if let Some(typ) = typ
                    .clone()
                    .or_else(|| infer_in_expr_type(expr, env, structs, fn_rets))
                {
                    env.insert(name.clone(), typ);
                }
            }
            Stmt::Assign(_, expr) | Stmt::Return(Some(expr)) | Stmt::Expr(expr) => {
                desugar_method_calls_in_expr(expr, env, structs, fn_rets);
            }
            Stmt::FieldAssign { base, value, .. } => {
                desugar_method_calls_in_expr(base, env, structs, fn_rets);
                desugar_method_calls_in_expr(value, env, structs, fn_rets);
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                desugar_method_calls_in_expr(base, env, structs, fn_rets);
                desugar_method_calls_in_expr(index, env, structs, fn_rets);
                desugar_method_calls_in_expr(value, env, structs, fn_rets);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                desugar_method_calls_in_expr(cond, env, structs, fn_rets);
                let mut then_env = env.clone();
                desugar_method_calls_in_body(then_body, &mut then_env, structs, fn_rets);
                let mut else_env = env.clone();
                desugar_method_calls_in_body(else_body, &mut else_env, structs, fn_rets);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    desugar_method_calls_in_expr(cond, env, structs, fn_rets);
                }
                let mut loop_env = env.clone();
                desugar_method_calls_in_body(body, &mut loop_env, structs, fn_rets);
            }
            Stmt::Match { scrutinee, arms } => {
                desugar_method_calls_in_expr(scrutinee, env, structs, fn_rets);
                for arm in arms {
                    let mut arm_env = env.clone();
                    desugar_method_calls_in_body(&mut arm.body, &mut arm_env, structs, fn_rets);
                }
            }
            Stmt::Return(None) => {}
            Stmt::Break | Stmt::Propagate => {}
            Stmt::Throw(expr) => {
                desugar_method_calls_in_expr(expr, env, structs, fn_rets);
            }
            Stmt::Try { body, catches, .. } => {
                let mut try_env = env.clone();
                desugar_method_calls_in_body(body, &mut try_env, structs, fn_rets);
                for catch in catches {
                    let mut catch_env = env.clone();
                    desugar_method_calls_in_body(&mut catch.body, &mut catch_env, structs, fn_rets);
                }
            }
        }
    }
}

pub(crate) fn desugar_method_calls_in_expr(
    expr: &mut Expr,
    env: &HashMap<String, Typ>,
    structs: &HashMap<String, HashMap<String, Typ>>,
    fn_rets: &HashMap<String, Typ>,
) {
    match expr {
        Expr::Unary { expr, .. } => desugar_method_calls_in_expr(expr, env, structs, fn_rets),
        Expr::Binary { lhs, rhs, .. } => {
            desugar_method_calls_in_expr(lhs, env, structs, fn_rets);
            desugar_method_calls_in_expr(rhs, env, structs, fn_rets);
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                desugar_method_calls_in_expr(expr, env, structs, fn_rets);
            }
        }
        Expr::Field { base, .. } => desugar_method_calls_in_expr(base, env, structs, fn_rets),
        Expr::ArrayLit(items) => {
            for item in items {
                desugar_method_calls_in_expr(item, env, structs, fn_rets);
            }
        }
        Expr::Index { base, index, .. } => {
            desugar_method_calls_in_expr(base, env, structs, fn_rets);
            desugar_method_calls_in_expr(index, env, structs, fn_rets);
        }
        Expr::Call { callee, args, .. } => {
            for arg in args.iter_mut() {
                desugar_method_calls_in_expr(arg, env, structs, fn_rets);
            }
            if let Expr::Ident(name) = callee.as_ref()
                && let Some(method) = name.strip_prefix("__method__")
                && let Some(base) = args.first()
                && let Some(base_typ) = infer_in_expr_type(base, env, structs, fn_rets)
            {
                match base_typ {
                    Typ::Named(struct_name) => {
                        **callee = Expr::Ident(format!("{struct_name}-{method}"));
                    }
                    Typ::Int | Typ::Float | Typ::Bool | Typ::String if method == "toStr" => {
                        **callee = Expr::Ident("to-string".to_string());
                    }
                    _ => {}
                }
            }
        }
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_) => {}
        Expr::Closure { body, .. } => {
            let mut closure_env = env.clone();
            desugar_method_calls_in_body(body, &mut closure_env, structs, fn_rets);
        }
    }
}

pub(crate) fn infer_in_expr_type(
    expr: &Expr,
    env: &HashMap<String, Typ>,
    structs: &HashMap<String, HashMap<String, Typ>>,
    fn_rets: &HashMap<String, Typ>,
) -> Option<Typ> {
    match expr {
        Expr::IntLit(_) => Some(Typ::Int),
        Expr::FloatLit(_) => Some(Typ::Float),
        Expr::StringLit(_) => Some(Typ::String),
        Expr::BoolLit(_) => Some(Typ::Bool),
        Expr::Ident(name) => env.get(name).cloned(),
        Expr::StructInit { name, .. } => Some(Typ::Named(name.clone())),
        Expr::Field { base, name, .. } => {
            if let Some(Typ::Named(struct_name)) = infer_in_expr_type(base, env, structs, fn_rets) {
                structs
                    .get(&struct_name)
                    .and_then(|fields| fields.get(name))
                    .cloned()
            } else {
                None
            }
        }
        Expr::ArrayLit(items) => Some(Typ::Array(Box::new(
            items
                .iter()
                .find_map(|item| infer_in_expr_type(item, env, structs, fn_rets))
                .unwrap_or(Typ::Void),
        ))),
        Expr::Index { base, .. } => {
            if let Some(Typ::Array(item)) = infer_in_expr_type(base, env, structs, fn_rets) {
                Some(*item)
            } else {
                None
            }
        }
        Expr::Unary { op, expr, .. } => match op.as_str() {
            "!" => Some(Typ::Bool),
            "-" => Some(Typ::Int),
            _ => infer_in_expr_type(expr, env, structs, fn_rets),
        },
        Expr::Binary { op, .. } => match op.as_str() {
            "+" | "-" | "*" | "/" | "%" | "^" | "<<" | ">>" | "&" | "|" => Some(Typ::Int),
            "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => Some(Typ::Bool),
            _ => None,
        },
        Expr::Call { callee, .. } => {
            if let Expr::Ident(name) = callee.as_ref() {
                fn_rets.get(name).cloned().or(match name.as_str() {
                    "to-string" => Some(Typ::String),
                    _ => None,
                })
            } else {
                None
            }
        }
        Expr::Closure { .. } => None,
    }
}

fn replace_idents(expr: &mut Expr, consts: &std::collections::HashMap<String, Expr>) {
    match expr {
        Expr::Ident(name) => {
            if let Some(replacement) = consts.get(name) {
                *expr = replacement.clone();
            }
        }
        Expr::Unary { expr: inner, .. } => replace_idents(inner, consts),
        Expr::Binary { lhs, rhs, .. } => {
            replace_idents(lhs, consts);
            replace_idents(rhs, consts);
        }
        Expr::Call { callee, args, .. } => {
            replace_idents(callee, consts);
            for arg in args {
                replace_idents(arg, consts);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                replace_idents(expr, consts);
            }
        }
        Expr::Field { base, .. } => replace_idents(base, consts),
        Expr::ArrayLit(items) => {
            for item in items {
                replace_idents(item, consts);
            }
        }
        Expr::Index { base, index, .. } => {
            replace_idents(base, consts);
            replace_idents(index, consts);
        }
        Expr::Closure { body, .. } => {
            for stmt in body {
                replace_stmt_idents(stmt, consts);
            }
        }
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::StringLit(_) | Expr::BoolLit(_) => {}
    }
}

fn replace_stmt_idents(stmt: &mut Stmt, consts: &std::collections::HashMap<String, Expr>) {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Return(Some(expr))
        | Stmt::Expr(expr)
        | Stmt::Throw(expr) => replace_idents(expr, consts),
        Stmt::FieldAssign { base, value, .. } => {
            replace_idents(base, consts);
            replace_idents(value, consts);
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            replace_idents(base, consts);
            replace_idents(index, consts);
            replace_idents(value, consts);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            replace_idents(cond, consts);
            for s in then_body {
                replace_stmt_idents(s, consts);
            }
            for s in else_body {
                replace_stmt_idents(s, consts);
            }
        }
        Stmt::Loop {
            cond: Some(cond),
            body,
            ..
        } => {
            replace_idents(cond, consts);
            for s in body {
                replace_stmt_idents(s, consts);
            }
        }
        Stmt::Loop {
            cond: None, body, ..
        } => {
            for s in body {
                replace_stmt_idents(s, consts);
            }
        }
        Stmt::Match { scrutinee, arms } => {
            replace_idents(scrutinee, consts);
            for arm in arms {
                for s in &mut arm.body {
                    replace_stmt_idents(s, consts);
                }
            }
        }
        Stmt::Try { body, catches, .. } => {
            for s in body {
                replace_stmt_idents(s, consts);
            }
            for catch in catches {
                for s in &mut catch.body {
                    replace_stmt_idents(s, consts);
                }
            }
        }
        Stmt::Return(None) => {}
        Stmt::Break | Stmt::Propagate => {}
    }
}

pub fn inline_const_values(module: &mut UnifiedModule) {
    // Collect const init values: name -> cloned init expression
    let consts: std::collections::HashMap<String, Expr> = module
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Global {
                name,
                init,
                mutable: false,
                ..
            } => init.as_ref().map(|expr| (name.clone(), *expr.clone())),
            _ => None,
        })
        .collect();

    if consts.is_empty() {
        return;
    }

    // Walk all function bodies and replace const references
    for decl in &mut module.decls {
        if let Decl::Function { body, .. } = decl {
            for stmt in body.iter_mut() {
                replace_stmt_idents(stmt, &consts);
            }
        }
    }
}
