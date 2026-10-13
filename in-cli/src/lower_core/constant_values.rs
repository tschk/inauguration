//! Copy compile-time values at their uses without allocating global storage.
//! Runtime read-only globals keep their storage and initialization semantics.

use crate::core_ir::{Decl, Expr, Stmt};
use std::collections::{HashMap, HashSet};

pub(super) fn has_named_constants(decls: &[Decl]) -> bool {
    decls.iter().any(|decl| {
        matches!(
            decl,
            Decl::Global {
                constant_value: true,
                ..
            }
        )
    })
}

pub(crate) fn lower_named_constants(decls: &mut [Decl]) {
    if !has_named_constants(decls) {
        return;
    }
    // Normalize lexical bindings before replacing global value references.
    crate::core_bindings::normalize_decls(decls);
    let mut definitions = HashMap::new();
    for decl in decls.iter() {
        if let Decl::Global {
            name,
            init,
            constant_value: true,
            ..
        } = decl
        {
            if definitions.contains_key(name) {
                definitions.insert(name.clone(), None);
            } else {
                definitions.insert(name.clone(), init.as_deref().cloned());
            }
        }
    }
    let mut values = HashMap::new();
    let mut names: Vec<_> = definitions.keys().cloned().collect();
    names.sort_unstable();
    for name in names {
        resolve_name(&name, &definitions, &mut HashSet::new(), &mut values);
    }
    for decl in decls {
        match decl {
            Decl::Function { body, .. } => rewrite_body(body, &values),
            Decl::Global {
                init: Some(expr),
                constant_value: false,
                ..
            } => rewrite_expr(expr, &values),
            _ => {}
        }
    }
}

fn resolve(
    expr: &Expr,
    defs: &HashMap<String, Option<Expr>>,
    active: &mut HashSet<String>,
    cache: &mut HashMap<String, Option<Expr>>,
) -> Option<Expr> {
    let value = match expr {
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::BoolLit(_) | Expr::StringLit(_) => expr.clone(),
        Expr::Ident(name) => return resolve_name(name, defs, active, cache),
        Expr::Unary { op, expr } => Expr::Unary {
            op: op.clone(),
            expr: Box::new(resolve(expr, defs, active, cache)?),
        },
        Expr::Binary { op, lhs, rhs } => Expr::Binary {
            op: op.clone(),
            lhs: Box::new(resolve(lhs, defs, active, cache)?),
            rhs: Box::new(resolve(rhs, defs, active, cache)?),
        },
        Expr::ArrayLit(items) => Expr::ArrayLit(
            items
                .iter()
                .map(|expr| resolve(expr, defs, active, cache))
                .collect::<Option<Vec<_>>>()?,
        ),
        Expr::StructInit { name, fields } => Expr::StructInit {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|(name, value)| Some((name.clone(), resolve(value, defs, active, cache)?)))
                .collect::<Option<Vec<_>>>()?,
        },
        Expr::Field { base, name } => {
            let Expr::StructInit { fields, .. } = resolve(base, defs, active, cache)? else {
                return None;
            };
            return fields
                .into_iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value);
        }
        Expr::Index { base, index } => {
            let Expr::ArrayLit(items) = resolve(base, defs, active, cache)? else {
                return None;
            };
            let index =
                usize::try_from(super::const_int(&resolve(index, defs, active, cache)?)?).ok()?;
            return items.get(index).cloned();
        }
        Expr::Call { callee, args } if matches!(callee.as_ref(), Expr::Ident(name) if matches!(name.as_str(), "array-repeat" | "range-exclusive" | "range-inclusive" | "range-from" | "range-to" | "range-to-inclusive" | "range-full")) => {
            Expr::Call {
                callee: callee.clone(),
                args: args
                    .iter()
                    .map(|expr| resolve(expr, defs, active, cache))
                    .collect::<Option<Vec<_>>>()?,
            }
        }
        _ => return None,
    };
    // Fold each resolved node before caching it, so repeated scalar aliases
    // form a DAG of small values rather than exponentially growing expressions.
    let value = crate::core_opt::fold_expr(value);
    let mut pending = vec![&value];
    let mut nodes = 0;
    while let Some(expr) = pending.pop() {
        nodes += 1;
        if nodes > 4096 {
            return None;
        }
        crate::core_ir::for_each_expr_child(expr, &mut |child| pending.push(child));
    }
    Some(if let Some(value) = super::const_bool(&value) {
        Expr::BoolLit(value)
    } else {
        value
    })
}

fn resolve_name(
    name: &str,
    defs: &HashMap<String, Option<Expr>>,
    active: &mut HashSet<String>,
    cache: &mut HashMap<String, Option<Expr>>,
) -> Option<Expr> {
    if let Some(value) = cache.get(name) {
        return value.clone();
    }
    if active.len() >= 64 || !active.insert(name.to_string()) {
        return None;
    }
    let value = defs
        .get(name)
        .and_then(Option::as_ref)
        .and_then(|expr| resolve(expr, defs, active, cache));
    active.remove(name);
    cache.insert(name.to_string(), value.clone());
    value
}

fn rewrite_expr(expr: &mut Expr, values: &HashMap<String, Option<Expr>>) {
    if let Expr::Ident(name) = expr
        && let Some(value) = values.get(name)
    {
        *expr = value.clone().unwrap_or_else(|| Expr::Call {
            callee: Box::new(Expr::Ident(format!("__unsupported_constant_{name}"))),
            args: vec![],
        });
        return;
    }
    if let Expr::Closure { body, .. } = expr {
        rewrite_body(body, values);
    } else {
        crate::core_ir::for_each_expr_child_mut(expr, &mut |child| rewrite_expr(child, values));
    }
}

fn rewrite_body(body: &mut [Stmt], values: &HashMap<String, Option<Expr>>) {
    for stmt in body {
        match stmt {
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::Expr(expr)
            | Stmt::Throw(expr)
            | Stmt::Return(Some(expr)) => rewrite_expr(expr, values),
            Stmt::IndexAssign { base, index, value } => {
                for expr in [base, index, value] {
                    rewrite_expr(expr, values);
                }
            }
            Stmt::FieldAssign { base, value, .. } => {
                for expr in [base, value] {
                    rewrite_expr(expr, values);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                rewrite_expr(cond, values);
                rewrite_body(then_body, values);
                rewrite_body(else_body, values);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    rewrite_expr(cond, values);
                }
                rewrite_body(body, values);
            }
            Stmt::Match { scrutinee, arms } => {
                rewrite_expr(scrutinee, values);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        rewrite_expr(guard, values);
                    }
                    rewrite_body(&mut arm.body, values);
                }
            }
            Stmt::Try { body, catches } => {
                rewrite_body(body, values);
                for catch in catches {
                    rewrite_body(&mut catch.body, values);
                }
            }
            _ => {}
        }
    }
}
