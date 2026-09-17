#![allow(clippy::redundant_closure)]
//! AST scans for C backend: InVec need + emit size/depth DoS guards.

use crate::core_ir::{Decl, Expr, LoopKind, Stmt, Typ, UnifiedModule};

const MAX_EMIT_DEPTH: u32 = 256;
const MAX_EMIT_NODES: usize = 100_000;

pub(super) fn module_needs_invec(module: &UnifiedModule) -> bool {
    module.decls.iter().any(decl_needs_invec)
}

fn typ_needs_invec(t: &Typ) -> bool {
    match t.canonical() {
        Typ::Vector(_) => true,
        Typ::Named(n) if n == "Vec" || n == "InVec" => true,
        Typ::Array(inner) => typ_needs_invec(&inner),
        _ => false,
    }
}

fn decl_needs_invec(d: &Decl) -> bool {
    match d {
        Decl::Struct { fields, .. } => fields.iter().any(|(_, t)| typ_needs_invec(t)),
        Decl::Class {
            fields, methods, ..
        } => fields.iter().any(|(_, t)| typ_needs_invec(t)) || methods.iter().any(decl_needs_invec),
        Decl::Function {
            params, ret, body, ..
        } => {
            params.iter().any(|(_, t)| typ_needs_invec(t))
                || typ_needs_invec(ret)
                || stmts_need_invec(body)
        }
        Decl::Global { typ, init, .. } => {
            typ_needs_invec(typ) || init.as_ref().is_some_and(|e| expr_needs_invec(e))
        }
        Decl::Interface { methods, .. } => methods
            .iter()
            .any(|m| m.params.iter().any(|(_, t)| typ_needs_invec(t)) || typ_needs_invec(&m.ret)),
        Decl::Component { .. } => false,
    }
}

fn stmts_need_invec(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_needs_invec)
}

fn stmt_needs_invec(s: &Stmt) -> bool {
    match s {
        Stmt::Let(_, ty, e) => ty.as_ref().map_or(false, typ_needs_invec) || expr_needs_invec(e),
        Stmt::Assign(_, e) | Stmt::Return(Some(e)) | Stmt::Throw(e) | Stmt::Expr(e) => {
            expr_needs_invec(e)
        }
        Stmt::IndexAssign { base, index, value } => {
            expr_needs_invec(base) || expr_needs_invec(index) || expr_needs_invec(value)
        }
        Stmt::FieldAssign { base, value, .. } => expr_needs_invec(base) || expr_needs_invec(value),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => expr_needs_invec(cond) || stmts_need_invec(then_body) || stmts_need_invec(else_body),
        Stmt::Loop { kind, cond, body } => {
            matches!(kind, LoopKind::For { .. })
                || cond.as_ref().map_or(false, expr_needs_invec)
                || stmts_need_invec(body)
        }
        Stmt::Match { scrutinee, arms } => {
            expr_needs_invec(scrutinee) || arms.iter().any(|a| stmts_need_invec(&a.body))
        }
        Stmt::Try { body, catches } => {
            stmts_need_invec(body) || catches.iter().any(|c| stmts_need_invec(&c.body))
        }
        Stmt::Return(None) | Stmt::Propagate | Stmt::Break | Stmt::Continue => false,
    }
}

fn expr_needs_invec(e: &Expr) -> bool {
    match e {
        Expr::Unary { expr, .. } => expr_needs_invec(expr),
        Expr::Binary { lhs, rhs, .. } => expr_needs_invec(lhs) || expr_needs_invec(rhs),
        Expr::StructInit { fields, .. } => fields.iter().any(|(_, e)| expr_needs_invec(e)),
        Expr::Field { base, .. } => expr_needs_invec(base),
        Expr::ArrayLit(items) => items.iter().any(expr_needs_invec),
        Expr::Index { base, index } => expr_needs_invec(base) || expr_needs_invec(index),
        Expr::Call { callee, args } => {
            expr_needs_invec(callee) || args.iter().any(expr_needs_invec)
        }
        Expr::Closure {
            params, ret, body, ..
        } => {
            params.iter().any(|(_, t)| typ_needs_invec(t))
                || typ_needs_invec(ret)
                || stmts_need_invec(body)
        }
        _ => false,
    }
}

pub(super) fn check_module_limits(module: &UnifiedModule) -> Result<(), String> {
    let mut nodes = 0usize;
    for d in &module.decls {
        count_decl(d, 0, &mut nodes)?;
    }
    Ok(())
}

fn bump(nodes: &mut usize) -> Result<(), String> {
    *nodes += 1;
    if *nodes > MAX_EMIT_NODES {
        return Err(format!(
            "emit aborted: Core IR exceeds {MAX_EMIT_NODES} nodes (DoS guard)"
        ));
    }
    Ok(())
}

fn depth_ok(depth: u32) -> Result<(), String> {
    if depth > MAX_EMIT_DEPTH {
        return Err(format!(
            "emit aborted: Core IR nesting exceeds {MAX_EMIT_DEPTH} (DoS guard)"
        ));
    }
    Ok(())
}

fn count_decl(d: &Decl, depth: u32, nodes: &mut usize) -> Result<(), String> {
    depth_ok(depth)?;
    bump(nodes)?;
    match d {
        Decl::Function { body, .. } => count_stmts(body, depth + 1, nodes),
        Decl::Class { methods, .. } => {
            for m in methods {
                count_decl(m, depth + 1, nodes)?;
            }
            Ok(())
        }
        Decl::Global { init, .. } => {
            if let Some(e) = init {
                count_expr(e, depth + 1, nodes)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn count_stmts(stmts: &[Stmt], depth: u32, nodes: &mut usize) -> Result<(), String> {
    for s in stmts {
        count_stmt(s, depth, nodes)?;
    }
    Ok(())
}

fn count_stmt(s: &Stmt, depth: u32, nodes: &mut usize) -> Result<(), String> {
    depth_ok(depth)?;
    bump(nodes)?;
    match s {
        Stmt::Let(_, _, e) | Stmt::Assign(_, e) | Stmt::Throw(e) | Stmt::Expr(e) => {
            count_expr(e, depth + 1, nodes)
        }
        Stmt::Return(Some(e)) => count_expr(e, depth + 1, nodes),
        Stmt::Return(None) | Stmt::Propagate | Stmt::Break | Stmt::Continue => Ok(()),
        Stmt::IndexAssign { base, index, value } => {
            count_expr(base, depth + 1, nodes)?;
            count_expr(index, depth + 1, nodes)?;
            count_expr(value, depth + 1, nodes)
        }
        Stmt::FieldAssign { base, value, .. } => {
            count_expr(base, depth + 1, nodes)?;
            count_expr(value, depth + 1, nodes)
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            count_expr(cond, depth + 1, nodes)?;
            count_stmts(then_body, depth + 1, nodes)?;
            count_stmts(else_body, depth + 1, nodes)
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(c) = cond {
                count_expr(c, depth + 1, nodes)?;
            }
            count_stmts(body, depth + 1, nodes)
        }
        Stmt::Match { scrutinee, arms } => {
            count_expr(scrutinee, depth + 1, nodes)?;
            for a in arms {
                count_stmts(&a.body, depth + 1, nodes)?;
            }
            Ok(())
        }
        Stmt::Try { body, catches } => {
            count_stmts(body, depth + 1, nodes)?;
            for c in catches {
                count_stmts(&c.body, depth + 1, nodes)?;
            }
            Ok(())
        }
    }
}

fn count_expr(e: &Expr, depth: u32, nodes: &mut usize) -> Result<(), String> {
    depth_ok(depth)?;
    bump(nodes)?;
    match e {
        Expr::Unary { expr, .. } => count_expr(expr, depth + 1, nodes),
        Expr::Binary { lhs, rhs, .. } => {
            count_expr(lhs, depth + 1, nodes)?;
            count_expr(rhs, depth + 1, nodes)
        }
        Expr::StructInit { fields, .. } => {
            for (_, fe) in fields {
                count_expr(fe, depth + 1, nodes)?;
            }
            Ok(())
        }
        Expr::Field { base, .. } => count_expr(base, depth + 1, nodes),
        Expr::ArrayLit(items) => {
            for it in items {
                count_expr(it, depth + 1, nodes)?;
            }
            Ok(())
        }
        Expr::Index { base, index } => {
            count_expr(base, depth + 1, nodes)?;
            count_expr(index, depth + 1, nodes)
        }
        Expr::Call { callee, args } => {
            count_expr(callee, depth + 1, nodes)?;
            for a in args {
                count_expr(a, depth + 1, nodes)?;
            }
            Ok(())
        }
        Expr::Closure { body, .. } => count_stmts(body, depth + 1, nodes),
        _ => Ok(()),
    }
}
