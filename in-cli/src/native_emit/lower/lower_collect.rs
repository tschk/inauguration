use super::lower_util::canonical_type;
use super::{EntryReturn, FunctionInfo};
use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule};
use std::collections::HashMap;

pub(crate) fn collect_functions(
    module: &UnifiedModule,
) -> Result<HashMap<String, FunctionInfo>, String> {
    let mut functions = HashMap::new();
    let mut name_counts: HashMap<String, u32> = HashMap::new();
    for decl in &module.decls {
        let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        else {
            continue;
        };
        let unique_name = if functions.contains_key(name) {
            let count = name_counts.entry(name.clone()).or_insert(1);
            *count += 1;
            format!("{name}__dup{count}")
        } else {
            name_counts.insert(name.clone(), 1);
            name.clone()
        };
        functions.insert(
            unique_name.clone(),
            FunctionInfo {
                name: unique_name,
                params: params
                    .iter()
                    .map(|(name, typ)| (name.clone(), canonical_type(typ)))
                    .collect(),
                ret: canonical_type(ret),
                body: body.clone(),
            },
        );
    }
    // Build disambiguation map: original name → unique name
    let mut name_map: HashMap<String, String> = HashMap::new();
    for unique in functions.keys() {
        // Strip the __dup suffix to get original
        let orig = unique.split("__dup").next().unwrap_or(unique).to_string();
        name_map.insert(orig, unique.clone());
    }
    // Update call targets in all function bodies
    for func in functions.values_mut() {
        rename_calls(&mut func.body, &name_map);
    }
    if functions.is_empty() {
        return Err("native-lower: module has no functions".to_string());
    }
    Ok(functions)
}

pub(crate) fn rename_calls(stmts: &mut [Stmt], name_map: &HashMap<String, String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Expr(expr) | Stmt::Return(Some(expr)) => rename_call_expr(expr, name_map),
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::FieldAssign { value: expr, .. } => rename_call_expr(expr, name_map),
            Stmt::If {
                then_body,
                else_body,
                cond,
                ..
            } => {
                rename_call_expr(cond, name_map);
                rename_calls(then_body, name_map);
                rename_calls(else_body, name_map);
            }
            Stmt::Loop { body, cond, .. } => {
                if let Some(cond) = cond {
                    rename_call_expr(cond, name_map);
                }
                rename_calls(body, name_map);
            }
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    rename_calls(&mut arm.body, name_map);
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn rename_call_expr(expr: &mut Expr, name_map: &HashMap<String, String>) {
    match expr {
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(name) = callee.as_mut() {
                if let Some(new_name) = name_map.get(name.as_str()) {
                    *name = new_name.clone();
                } else if let Some(idx) = name.rfind("::") {
                    let last = &name[idx + 2..];
                    if let Some(new_name) = name_map.get(last) {
                        *name = new_name.clone();
                    }
                }
            }
            rename_call_expr(callee, name_map);
            for arg in args {
                rename_call_expr(arg, name_map);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rename_call_expr(lhs, name_map);
            rename_call_expr(rhs, name_map);
        }
        Expr::Unary { expr, .. } => rename_call_expr(expr, name_map),
        Expr::Field { base, .. } => rename_call_expr(base, name_map),
        Expr::Index { base, index } => {
            rename_call_expr(base, name_map);
            rename_call_expr(index, name_map);
        }
        Expr::StructInit { fields, .. } => {
            for (_, field_expr) in fields {
                rename_call_expr(field_expr, name_map);
            }
        }
        _ => {}
    }
}

pub(crate) fn entry_return_kind(ret: &Typ) -> EntryReturn {
    match canonical_type(ret) {
        Typ::Int | Typ::Float | Typ::Bool => EntryReturn::IntLike,
        Typ::String
        | Typ::Void
        | Typ::Array(_)
        | Typ::Vector(_)
        | Typ::Named(_)
        | Typ::Generic(_) => EntryReturn::VoidOrReference,
    }
}

pub(crate) fn collect_structs(module: &UnifiedModule) -> HashMap<String, Vec<(String, Typ)>> {
    let mut structs: HashMap<String, Vec<(String, Typ)>> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Struct { name, fields, .. } => Some((name.clone(), fields.clone())),
            _ => None,
        })
        .collect();
    // Add synthetic struct defs for common Rust std types and tuples
    if !structs.contains_key("Vec") {
        structs.insert(
            "Vec".into(),
            vec![
                ("ptr".into(), Typ::Int),
                ("len".into(), Typ::Int),
                ("cap".into(), Typ::Int),
            ],
        );
    }
    if !structs.contains_key("String") {
        structs.insert(
            "String".into(),
            vec![("vec".into(), Typ::Named("Vec".into()))],
        );
    }
    if !structs.contains_key("Box") {
        structs.insert("Box".into(), vec![("ptr".into(), Typ::Int)]);
    }
    if !structs.contains_key("Option") {
        structs.insert(
            "Option".into(),
            vec![("tag".into(), Typ::Int), ("value".into(), Typ::Int)],
        );
    }
    if !structs.contains_key("Result") {
        structs.insert(
            "Result".into(),
            vec![
                ("tag".into(), Typ::Int),
                ("ok".into(), Typ::Int),
                ("err".into(), Typ::Int),
            ],
        );
    }
    if !structs.contains_key("HashMap") {
        structs.insert("HashMap".into(), vec![("ptr".into(), Typ::Int)]);
    }
    if !structs.contains_key("PathBuf") {
        structs.insert(
            "PathBuf".into(),
            vec![("vec".into(), Typ::Named("Vec".into()))],
        );
    }
    if !structs.contains_key("Path") {
        structs.insert(
            "Path".into(),
            vec![("ptr".into(), Typ::Int), ("len".into(), Typ::Int)],
        );
    }
    structs
}

pub(crate) fn collect_strings(module: &UnifiedModule) -> HashMap<String, i64> {
    let mut values = Vec::new();
    for decl in &module.decls {
        if let Decl::Function { body, .. } = decl {
            collect_body_strings(body, &mut values);
        }
    }
    values.sort();
    values.dedup();
    values
        .into_iter()
        .filter(|value| !value.is_empty())
        .enumerate()
        .map(|(idx, value)| (value, idx as i64 + 1))
        .collect()
}

fn collect_body_strings(body: &[Stmt], values: &mut Vec<String>) {
    for stmt in body {
        match stmt {
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::FieldAssign { value: expr, .. }
            | Stmt::Return(Some(expr))
            | Stmt::Expr(expr) => collect_expr_strings(expr, values),
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                collect_expr_strings(base, values);
                collect_expr_strings(index, values);
                collect_expr_strings(value, values);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                collect_expr_strings(cond, values);
                collect_body_strings(then_body, values);
                collect_body_strings(else_body, values);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    collect_expr_strings(cond, values);
                }
                collect_body_strings(body, values);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                collect_expr_strings(scrutinee, values);
                for arm in arms {
                    collect_body_strings(&arm.body, values);
                    collect_pattern_strings(&arm.pattern, values);
                }
            }
            Stmt::Return(None) => {}

            Stmt::Throw(expr) => collect_expr_strings(expr, values),
            Stmt::Try { body, catches, .. } => {
                collect_body_strings(body, values);
                for catch in catches {
                    collect_body_strings(&catch.body, values);
                }
            }
            Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
        }
    }
}

fn collect_pattern_strings(pattern: &str, values: &mut Vec<String>) {
    let trimmed = pattern.trim().trim_end_matches(':').trim();
    let trimmed = trimmed.strip_prefix("case ").unwrap_or(trimmed).trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let content = &trimmed[1..trimmed.len() - 1];
        values.push(content.to_string());
    }
}

fn collect_expr_strings(expr: &Expr, values: &mut Vec<String>) {
    match expr {
        Expr::StringLit(value) => values.push(value.clone()),
        Expr::Unary { expr, .. } => collect_expr_strings(expr, values),
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr_strings(lhs, values);
            collect_expr_strings(rhs, values);
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                collect_expr_strings(expr, values);
            }
        }
        Expr::Field { base, .. } => collect_expr_strings(base, values),
        Expr::ArrayLit(items) => {
            for item in items {
                collect_expr_strings(item, values);
            }
        }
        Expr::Index { base, index, .. } => {
            collect_expr_strings(base, values);
            collect_expr_strings(index, values);
        }
        Expr::Call { callee, args, .. } => {
            collect_expr_strings(callee, values);
            for arg in args {
                collect_expr_strings(arg, values);
            }
        }
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_)
        | Expr::Closure { .. } => {}
    }
}
