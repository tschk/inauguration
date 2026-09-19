//! Core IR optimization passes.
//!
//! These run before lowering. All frontends / backends benefit.
//! Order: inlining → constant folding → constant propagation → DCE.
//! Profile-specific passes (lean / harden) layer on top via
//! [`optimize_with_profile`].

use crate::core_ir::{CatchArm, Decl, Expr, LoopKind, MatchArm, Stmt, Typ};
use crate::emit_profile::EmitProfile;
use std::collections::{HashMap, HashSet};

pub fn optimize(decls: &mut Vec<Decl>) {
    optimize_with_entry(decls, None);
}

pub fn optimize_with_entry(decls: &mut Vec<Decl>, entry: Option<&str>) {
    optimize_with_profile(decls, entry, EmitProfile::Default);
}

/// Profile-aware IR optimize entry point used by `compile_owned`.
pub fn optimize_with_profile(decls: &mut Vec<Decl>, entry: Option<&str>, profile: EmitProfile) {
    optimize_with_linkage(decls, entry, profile, false);
}

/// Linkage-aware variant: for static-lib linkage every function is a
/// potential export — callers outside this module (assembly capsules, other
/// objects) may resolve them by symbol. Function-level removal must not run;
/// statement-level DCE and folding above are safe.
pub fn optimize_with_linkage(
    decls: &mut Vec<Decl>,
    entry: Option<&str>,
    profile: EmitProfile,
    keep_all_functions: bool,
) {
    match profile {
        EmitProfile::Default => {
            // Fast owned path: more inlining than a textbook compiler, then fold/DCE.
            inline_small_functions_with(decls, INLINE_THRESHOLD, DEFAULT_INLINE_DEPTH);
            inline_small_functions_with(decls, INLINE_THRESHOLD, DEFAULT_INLINE_DEPTH);
            algebraic_simplify(decls);
            fold_constants_in_decls(decls);
            propagate_constants(decls);
            fold_constants_in_decls(decls);
            dead_code_eliminate(decls);
            if !keep_all_functions {
                remove_dead_functions(decls, entry);
            }
        }
        EmitProfile::Lean => {
            // Aggressive inlining + deeper recursion, then standard cleanup.
            inline_small_functions_with(decls, LEAN_INLINE_THRESHOLD, LEAN_INLINE_DEPTH);
            inline_small_functions_with(decls, LEAN_INLINE_THRESHOLD, LEAN_INLINE_DEPTH);
            algebraic_simplify(decls);
            fold_constants_in_decls(decls);
            propagate_constants(decls);
            fold_constants_in_decls(decls);
            dead_code_eliminate(decls);
            if !keep_all_functions {
                remove_dead_functions(decls, entry);
            }
        }
        EmitProfile::Harden => {
            // Normal opts first so harden noise is not immediately folded away.
            inline_small_functions_with(decls, INLINE_THRESHOLD, DEFAULT_INLINE_DEPTH);
            algebraic_simplify(decls);
            fold_constants_in_decls(decls);
            propagate_constants(decls);
            fold_constants_in_decls(decls);
            dead_code_eliminate(decls);
            if !keep_all_functions {
                remove_dead_functions(decls, entry);
            }
            // Anti-decomp transforms (must run after fold/dce).
            harden_mba_arithmetic(decls);
            harden_obscure_literals(decls);
            harden_opaque_predicates(decls);
            harden_bogus_blocks(decls);
            // Flatten eligible bodies into a pc dispatcher before junk pads
            // so each state still receives junk noise afterward at the outer level.
            harden_cfg_dispatch_lite(decls);
            harden_junk_stmts(decls);
            harden_hash_symbols(decls, entry);
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn fn_bodies_mut(decls: &mut [Decl]) -> impl Iterator<Item = &mut Vec<Stmt>> {
    decls.iter_mut().filter_map(|d| match d {
        Decl::Function { body, .. } => Some(body),
        _ => None,
    })
}

fn walk_expr<F: FnMut(&Expr)>(e: &Expr, f: &mut F) {
    f(e);
    crate::core_ir::for_each_expr_child(e, &mut |child| walk_expr(child, f));
}

/// Whether an expression (or any subexpression) contains a function call.
/// Calls may have observable side effects (I/O, MMIO, allocation), so an
/// unused binding whose initializer calls must not be dead-code-eliminated.
fn expr_has_call(e: &Expr) -> bool {
    let mut has = false;
    walk_expr(e, &mut |n| {
        if matches!(n, Expr::Call { .. }) {
            has = true;
        }
    });
    has
}

fn map_expr_mut<F: FnMut(&mut Expr)>(e: &mut Expr, f: &mut F) {
    match e {
        Expr::Call { callee, args, .. } => {
            map_expr_mut(callee, f);
            for a in args.iter_mut() {
                map_expr_mut(a, f);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            map_expr_mut(lhs, f);
            map_expr_mut(rhs, f);
        }
        Expr::Unary { expr, .. } => map_expr_mut(expr, f),
        Expr::Field { base, .. } => map_expr_mut(base, f),
        Expr::Index { base, index, .. } => {
            map_expr_mut(base, f);
            map_expr_mut(index, f);
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields.iter_mut() {
                map_expr_mut(expr, f);
            }
        }
        Expr::ArrayLit(args) => {
            for a in args.iter_mut() {
                map_expr_mut(a, f);
            }
        }
        Expr::Closure { body, .. } => {
            for s in body.iter_mut() {
                map_stmt_mut(s, f);
            }
        }
        _ => {}
    }
    f(e);
}

fn map_stmt_mut<F: FnMut(&mut Expr)>(s: &mut Stmt, f: &mut F) {
    match s {
        Stmt::Let(_, _, e) => map_expr_mut(e, f),
        Stmt::Assign(_, e) | Stmt::FieldAssign { value: e, .. } => map_expr_mut(e, f),
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            map_expr_mut(base, f);
            map_expr_mut(index, f);
            map_expr_mut(value, f);
        }
        Stmt::Return(Some(e)) => map_expr_mut(e, f),
        Stmt::Return(None) => {}
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            map_expr_mut(cond, f);
            for s in then_body.iter_mut() {
                map_stmt_mut(s, f);
            }
            for s in else_body.iter_mut() {
                map_stmt_mut(s, f);
            }
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(c) = cond {
                map_expr_mut(c, f);
            }
            for s in body.iter_mut() {
                map_stmt_mut(s, f);
            }
        }
        Stmt::Expr(e) => map_expr_mut(e, f),
        Stmt::Throw(e) => map_expr_mut(e, f),
        Stmt::Try { body, catches, .. } => {
            for s in body.iter_mut() {
                map_stmt_mut(s, f);
            }
            for c in catches.iter_mut() {
                for s in c.body.iter_mut() {
                    map_stmt_mut(s, f);
                }
            }
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            map_expr_mut(scrutinee, f);
            for arm in arms.iter_mut() {
                for s in arm.body.iter_mut() {
                    map_stmt_mut(s, f);
                }
            }
        }
        _ => {}
    }
}

fn map_expr<F: FnMut(Expr) -> Expr + Copy>(e: Expr, f: &mut F) -> Expr {
    let mapped = match e {
        Expr::Call { callee, args, .. } => Expr::Call {
            callee: Box::new(map_expr(*callee, f)),
            args: args.into_iter().map(|a| map_expr(a, f)).collect(),
        },
        Expr::Binary { op, lhs, rhs, .. } => Expr::Binary {
            op,
            lhs: Box::new(map_expr(*lhs, f)),
            rhs: Box::new(map_expr(*rhs, f)),
        },
        Expr::Unary { op, expr, .. } => Expr::Unary {
            op,
            expr: Box::new(map_expr(*expr, f)),
        },
        Expr::Field { base, name, .. } => Expr::Field {
            base: Box::new(map_expr(*base, f)),
            name,
        },
        Expr::Index { base, index, .. } => Expr::Index {
            base: Box::new(map_expr(*base, f)),
            index: Box::new(map_expr(*index, f)),
        },
        Expr::StructInit { name, fields, .. } => Expr::StructInit {
            name,
            fields: fields
                .into_iter()
                .map(|(n, e)| (n, map_expr(e, f)))
                .collect(),
        },
        Expr::ArrayLit(args) => Expr::ArrayLit(args.into_iter().map(|a| map_expr(a, f)).collect()),
        Expr::Closure {
            params,
            ret,
            body,
            captures,
        } => Expr::Closure {
            params,
            ret,
            body: body.into_iter().map(|s| map_stmt(s, f)).collect(),
            captures,
        },
        other => other,
    };
    f(mapped)
}

fn map_stmt<F: FnMut(Expr) -> Expr + Copy>(s: Stmt, f: &mut F) -> Stmt {
    match s {
        Stmt::Let(n, t, e) => Stmt::Let(n, t, map_expr(e, f)),
        Stmt::Assign(n, e) => Stmt::Assign(n, map_expr(e, f)),
        Stmt::FieldAssign {
            base, name, value, ..
        } => Stmt::FieldAssign {
            base: map_expr(base, f),
            name,
            value: map_expr(value, f),
        },
        Stmt::IndexAssign {
            base, index, value, ..
        } => Stmt::IndexAssign {
            base: map_expr(base, f),
            index: map_expr(index, f),
            value: map_expr(value, f),
        },
        Stmt::Return(e) => Stmt::Return(e.map(|e| map_expr(e, f))),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => Stmt::If {
            cond: map_expr(cond, f),
            then_body: then_body.into_iter().map(|s| map_stmt(s, f)).collect(),
            else_body: else_body.into_iter().map(|s| map_stmt(s, f)).collect(),
        },
        Stmt::Loop {
            kind, cond, body, ..
        } => Stmt::Loop {
            kind,
            cond: cond.map(|c| map_expr(c, f)),
            body: body.into_iter().map(|s| map_stmt(s, f)).collect(),
        },
        Stmt::Expr(e) => Stmt::Expr(map_expr(e, f)),
        Stmt::Throw(e) => Stmt::Throw(map_expr(e, f)),
        Stmt::Try { body, catches } => Stmt::Try {
            body: body.into_iter().map(|s| map_stmt(s, f)).collect(),
            catches: catches
                .into_iter()
                .map(|c| CatchArm {
                    pattern: c.pattern,
                    body: c.body.into_iter().map(|s| map_stmt(s, f)).collect(),
                })
                .collect(),
        },
        Stmt::Match { scrutinee, arms } => Stmt::Match {
            scrutinee: map_expr(scrutinee, f),
            arms: arms
                .into_iter()
                .map(|a| MatchArm {
                    pattern: a.pattern,
                    body: a.body.into_iter().map(|s| map_stmt(s, f)).collect(),
                })
                .collect(),
        },
        other => other,
    }
}

// ─── Inlining ──────────────────────────────────────────────────────────────

const INLINE_THRESHOLD: usize = 6;
const DEFAULT_INLINE_DEPTH: u32 = 16;
const LEAN_INLINE_THRESHOLD: usize = 12;
const LEAN_INLINE_DEPTH: u32 = 24;

#[cfg_attr(not(test), allow(dead_code))]
fn inline_small_functions(decls: &mut [Decl]) {
    inline_small_functions_with(decls, INLINE_THRESHOLD, DEFAULT_INLINE_DEPTH);
}

fn inline_small_functions_with(decls: &mut [Decl], threshold: usize, max_depth: u32) {
    let mut functions: HashMap<String, Decl> = HashMap::new();
    let mut ptr_refs: Vec<String> = Vec::new();
    for d in decls.iter() {
        if let Decl::Function { name, .. } = d {
            functions.insert(name.clone(), d.clone());
        }
    }
    detect_ptr_refs(decls, &mut ptr_refs);

    let candidates: Vec<String> = functions
        .iter()
        .filter(|(n, d)| {
            matches!(
                d,
                Decl::Function { body, .. }
                    // An empty body is an extern binding (asm/zig/rust), not a
                    // function to inline: "inlining" it would delete the call
                    // to the external symbol, silently dropping MMIO writes,
                    // context switches, and other side effects.
                    if !body.is_empty()
                        && body.len() <= threshold
                        && !ptr_refs.contains(n)
                        && !has_cf(body)
            )
        })
        .map(|(n, _)| n.clone())
        .collect();
    if candidates.is_empty() {
        return;
    }

    for decl in decls.iter_mut() {
        if let Decl::Function { body, .. } = decl {
            *body =
                inline_body_limited(std::mem::take(body), &candidates, &functions, 0, max_depth);
        }
    }
}

/// Fold a straight-line let* + return body into a single expression for inlining.
fn fold_body_to_expr(body: &[Stmt]) -> Option<Expr> {
    if body.is_empty() {
        return None;
    }
    let mut env: HashMap<String, Expr> = HashMap::new();
    for stmt in &body[..body.len() - 1] {
        match stmt {
            Stmt::Let(name, _, e) if !expr_has_call(e) => {
                let e = replace_in_expr(e.clone(), &env);
                env.insert(name.clone(), e);
            }
            _ => return None,
        }
    }
    match body.last() {
        Some(Stmt::Return(Some(ret))) => Some(replace_in_expr(ret.clone(), &env)),
        _ => None,
    }
}

fn inline_body_limited(
    stmts: Vec<Stmt>,
    cand: &[String],
    fns: &HashMap<String, Decl>,
    depth: u32,
    max_depth: u32,
) -> Vec<Stmt> {
    if depth > max_depth {
        return stmts;
    }
    let mut r = Vec::new();
    for stmt in stmts {
        match stmt {
            Stmt::Let(n, t, e) => {
                let e = fold_call_ret(
                    inline_in_expr_limited(e, cand, fns, depth + 1, max_depth),
                    cand,
                    fns,
                );
                r.push(Stmt::Let(n, t, e));
            }
            Stmt::Return(Some(e)) => {
                let e = fold_call_ret(
                    inline_in_expr_limited(e, cand, fns, depth + 1, max_depth),
                    cand,
                    fns,
                );
                r.push(Stmt::Return(Some(e)));
            }
            Stmt::Expr(e) => {
                let e = inline_in_expr_limited(e, cand, fns, depth + 1, max_depth);
                match try_inline_void(&e, cand, fns) {
                    Some(s) => r.extend(s),
                    None => r.push(Stmt::Expr(e)),
                }
            }
            s => r.push(map_stmt(s, &mut |e| {
                inline_in_expr_limited(e, cand, fns, depth + 1, max_depth)
            })),
        }
    }
    r
}

fn inline_in_expr_limited(
    e: Expr,
    cand: &[String],
    fns: &HashMap<String, Decl>,
    depth: u32,
    max_depth: u32,
) -> Expr {
    map_expr(e, &mut |e| match e {
        Expr::Call { callee, args, .. } if depth < max_depth => {
            let name = match *callee {
                Expr::Ident(ref n) => n.clone(),
                other => {
                    return Expr::Call {
                        callee: Box::new(other),
                        args,
                    };
                }
            };
            if cand.contains(&name) {
                if let Some(Decl::Function { body, params, .. }) = fns.get(&name) {
                    if let Some(ret) = fold_body_to_expr(body) {
                        let mut sub = HashMap::new();
                        for (i, (p, _)) in params.iter().enumerate() {
                            if i < args.len() {
                                sub.insert(p.as_str(), &args[i]);
                            }
                        }
                        return substitute_expr(&ret, &sub);
                    }
                }
            }
            Expr::Call {
                callee: Box::new(Expr::Ident(name)),
                args,
            }
        }
        other => other,
    })
}

/// Replace a call-to-small-fn with its return value (for let-bindings).
fn fold_call_ret(e: Expr, cand: &[String], fns: &HashMap<String, Decl>) -> Expr {
    if let Expr::Call { callee, args, .. } = &e {
        if let Expr::Ident(name) = callee.as_ref() {
            if cand.contains(name) {
                if let Some(Decl::Function { body, params, .. }) = fns.get(name) {
                    if let Some(ret) = fold_body_to_expr(body) {
                        let mut sub = HashMap::new();
                        for (i, (p, _)) in params.iter().enumerate() {
                            if i < args.len() {
                                sub.insert(p.as_str(), &args[i]);
                            }
                        }
                        return substitute_expr(&ret, &sub);
                    }
                }
            }
        }
    }
    e
}

fn try_inline_void(e: &Expr, cand: &[String], fns: &HashMap<String, Decl>) -> Option<Vec<Stmt>> {
    if let Expr::Call { callee, args, .. } = e {
        if let Expr::Ident(name) = callee.as_ref() {
            if cand.contains(name) {
                if let Some(Decl::Function {
                    body, params, ret, ..
                }) = fns.get(name)
                {
                    if *ret != Typ::Void {
                        return None;
                    }
                    let mut sub = HashMap::new();
                    for (i, (p, _)) in params.iter().enumerate() {
                        if i < args.len() {
                            sub.insert(p.as_str(), &args[i]);
                        }
                    }
                    let r: Vec<Stmt> = substitute_params(body, &sub)
                        .into_iter()
                        .filter(|s| !matches!(s, Stmt::Return(None)))
                        .collect();
                    return Some(r);
                }
            }
        }
    }
    None
}

fn substitute_params(stmts: &[Stmt], sub: &HashMap<&str, &Expr>) -> Vec<Stmt> {
    stmts.iter().map(|s| subst_stmt(s, sub)).collect()
}
fn subst_stmt(s: &Stmt, sub: &HashMap<&str, &Expr>) -> Stmt {
    match s {
        Stmt::Let(n, t, e) => Stmt::Let(n.clone(), t.clone(), substitute_expr(e, sub)),
        Stmt::Assign(n, e) => Stmt::Assign(n.clone(), substitute_expr(e, sub)),
        Stmt::FieldAssign {
            base, name, value, ..
        } => Stmt::FieldAssign {
            base: substitute_expr(base, sub),
            name: name.clone(),
            value: substitute_expr(value, sub),
        },
        Stmt::IndexAssign {
            base, index, value, ..
        } => Stmt::IndexAssign {
            base: substitute_expr(base, sub),
            index: substitute_expr(index, sub),
            value: substitute_expr(value, sub),
        },
        Stmt::Return(e) => Stmt::Return(e.as_ref().map(|e| substitute_expr(e, sub))),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => Stmt::If {
            cond: substitute_expr(cond, sub),
            then_body: substitute_params(then_body, sub),
            else_body: substitute_params(else_body, sub),
        },
        Stmt::Loop {
            kind, cond, body, ..
        } => Stmt::Loop {
            kind: kind.clone(),
            cond: cond.as_ref().map(|c| substitute_expr(c, sub)),
            body: substitute_params(body, sub),
        },
        Stmt::Expr(e) => Stmt::Expr(substitute_expr(e, sub)),
        Stmt::Throw(e) => Stmt::Throw(substitute_expr(e, sub)),
        Stmt::Try { body, catches } => Stmt::Try {
            body: substitute_params(body, sub),
            catches: catches
                .iter()
                .map(|c| CatchArm {
                    pattern: c.pattern.clone(),
                    body: substitute_params(&c.body, sub),
                })
                .collect(),
        },
        Stmt::Match { scrutinee, arms } => Stmt::Match {
            scrutinee: substitute_expr(scrutinee, sub),
            arms: arms
                .iter()
                .map(|a| MatchArm {
                    pattern: a.pattern.clone(),
                    body: substitute_params(&a.body, sub),
                })
                .collect(),
        },
        Stmt::Break => Stmt::Break,
        Stmt::Continue => Stmt::Continue,
        Stmt::Propagate => Stmt::Propagate,
    }
}
fn substitute_expr(e: &Expr, sub: &HashMap<&str, &Expr>) -> Expr {
    let mut e = e.clone();
    map_expr_mut(&mut e, &mut |expr_mut| {
        if let Expr::Ident(n) = expr_mut {
            if let Some(&new_expr) = sub.get(n.as_str()) {
                *expr_mut = new_expr.clone();
            }
        }
    });
    e
}
fn has_cf(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| {
        matches!(
            s,
            Stmt::If { .. }
                | Stmt::Loop { .. }
                | Stmt::Match { .. }
                | Stmt::Throw(_)
                | Stmt::Try { .. }
                | Stmt::Propagate
                | Stmt::Break
                | Stmt::Continue
        )
    })
}

fn detect_ptr_refs(decls: &[Decl], out: &mut Vec<String>) {
    for d in decls {
        if let Decl::Function { body, .. } = d {
            ptr_in_stmts(body, out);
        }
    }
}
fn ptr_in_stmts(stmts: &[Stmt], out: &mut Vec<String>) {
    for s in stmts {
        match s {
            Stmt::Let(_, _, e) | Stmt::Assign(_, e) | Stmt::Return(Some(e)) | Stmt::Expr(e) => {
                ptr_in_expr(e, out)
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                ptr_in_expr(base, out);
                ptr_in_expr(index, out);
                ptr_in_expr(value, out);
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                ptr_in_stmts(then_body, out);
                ptr_in_stmts(else_body, out);
            }
            Stmt::Loop { body, .. } => ptr_in_stmts(body, out),
            Stmt::Throw(e) => ptr_in_expr(e, out),
            Stmt::Try { body, catches, .. } => {
                ptr_in_stmts(body, out);
                for c in catches {
                    ptr_in_stmts(&c.body, out);
                }
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                ptr_in_expr(scrutinee, out);
                for arm in arms {
                    ptr_in_stmts(&arm.body, out);
                }
            }
            Stmt::FieldAssign { base, value, .. } => {
                ptr_in_expr(base, out);
                ptr_in_expr(value, out);
            }
            _ => {}
        }
    }
}
fn ptr_in_expr(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(name) = callee.as_ref() {
                if matches!(name.as_str(), "invoke" | "invoke1" | "invoke2") {
                    if let Some(Expr::Ident(fn_name)) = args.first() {
                        if !out.contains(fn_name) {
                            out.push(fn_name.clone());
                        }
                    }
                }
            }
            for arg in args {
                if let Expr::Ident(name) = arg {
                    if !out.contains(name) {
                        out.push(name.clone());
                    }
                }
                ptr_in_expr(arg, out);
            }
            ptr_in_expr(callee, out);
        }
        Expr::Binary { lhs, rhs, .. } => {
            ptr_in_expr(lhs, out);
            ptr_in_expr(rhs, out);
        }
        Expr::Unary { expr, .. } => ptr_in_expr(expr, out),
        Expr::Field { base, .. } => ptr_in_expr(base, out),
        Expr::Index { base, index, .. } => {
            ptr_in_expr(base, out);
            ptr_in_expr(index, out);
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                ptr_in_expr(e, out);
            }
        }
        _ => {}
    }
}

// ─── Algebraic Simplification ──────────────────────────────────────────────

/// x+0→x, x*1→x, x&-1→x, x|0→x, x^0→x, x<<0→x, etc.
fn algebraic_simplify(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        let old = std::mem::take(body);
        *body = old
            .into_iter()
            .map(|s| map_stmt(s, &mut |e| simplify_expr(e)))
            .collect();
    }
}

fn fold_int_constants(op: &str, a: i64, b: i64) -> Option<Expr> {
    match op {
        "add" | "+" => a.checked_add(b).map(Expr::IntLit),
        "sub" | "-" => a.checked_sub(b).map(Expr::IntLit),
        "mul" | "*" => a.checked_mul(b).map(Expr::IntLit),
        "div" | "/" if b != 0 && !(a == i64::MIN && b == -1) => Some(Expr::IntLit(a / b)),
        "mod" | "%" if b != 0 => Some(Expr::IntLit(a % b)),
        _ => None,
    }
}

fn simplify_binary_expr(original: Expr, op: &str, lhs: &Expr, rhs: &Expr) -> Expr {
    if let (Expr::IntLit(a), Expr::IntLit(b)) = (lhs, rhs) {
        if let Some(folded) = fold_int_constants(op, *a, *b) {
            return folded;
        }
    }

    let is_zero = |e: &Expr| matches!(e, Expr::IntLit(0));
    let is_one = |e: &Expr| matches!(e, Expr::IntLit(1));
    let is_neg1 = |e: &Expr| matches!(e, Expr::IntLit(-1));
    let is_true = |e: &Expr| matches!(e, Expr::BoolLit(true));
    let is_false = |e: &Expr| matches!(e, Expr::BoolLit(false));

    match op {
        "add" | "+" => {
            if is_zero(lhs) {
                return rhs.clone();
            }
            if is_zero(rhs) {
                return lhs.clone();
            }
        }
        "bor" | "|" => {
            if is_neg1(lhs) || is_neg1(rhs) {
                return Expr::IntLit(-1);
            }
            if is_zero(lhs) {
                return rhs.clone();
            }
            if is_zero(rhs) {
                return lhs.clone();
            }
        }
        "land" | "&&" => {
            if is_false(lhs) || is_false(rhs) {
                return Expr::BoolLit(false);
            }
            if is_true(lhs) {
                return rhs.clone();
            }
            if is_true(rhs) {
                return lhs.clone();
            }
            if is_zero(lhs) || is_zero(rhs) {
                return Expr::IntLit(0);
            }
            if is_one(lhs) {
                return rhs.clone();
            }
            if is_one(rhs) {
                return lhs.clone();
            }
        }
        "lor" | "||" => {
            if is_true(lhs) || is_true(rhs) {
                return Expr::BoolLit(true);
            }
            if is_false(lhs) {
                return rhs.clone();
            }
            if is_false(rhs) {
                return lhs.clone();
            }
            if is_one(lhs) || is_one(rhs) {
                return Expr::IntLit(1);
            }
            if is_zero(lhs) {
                return rhs.clone();
            }
            if is_zero(rhs) {
                return lhs.clone();
            }
        }
        "sub" | "-" => {
            if is_zero(rhs) {
                return lhs.clone();
            }
            if lhs == rhs && matches!(lhs, Expr::Ident(_) | Expr::IntLit(_)) {
                return Expr::IntLit(0);
            }
        }
        "xor" | "^" => {
            if is_zero(lhs) {
                return rhs.clone();
            }
            if is_zero(rhs) {
                return lhs.clone();
            }
            if lhs == rhs && matches!(lhs, Expr::Ident(_) | Expr::IntLit(_) | Expr::BoolLit(_)) {
                return Expr::IntLit(0);
            }
        }
        "==" => {
            if lhs == rhs
                && matches!(
                    lhs,
                    Expr::Ident(_) | Expr::IntLit(_) | Expr::BoolLit(_) | Expr::StringLit(_)
                )
            {
                return Expr::BoolLit(true);
            }
        }
        "!=" => {
            if lhs == rhs
                && matches!(
                    lhs,
                    Expr::Ident(_) | Expr::IntLit(_) | Expr::BoolLit(_) | Expr::StringLit(_)
                )
            {
                return Expr::BoolLit(false);
            }
        }
        ">" | "<" | "gt" | "lt" => {
            if lhs == rhs && matches!(lhs, Expr::Ident(_) | Expr::IntLit(_)) {
                return Expr::BoolLit(false);
            }
        }
        ">=" | "<=" | "ge" | "le" => {
            if lhs == rhs && matches!(lhs, Expr::Ident(_) | Expr::IntLit(_)) {
                return Expr::BoolLit(true);
            }
        }
        "mul" | "*" => {
            if is_zero(lhs) || is_zero(rhs) {
                return Expr::IntLit(0);
            }
            if is_one(lhs) {
                return rhs.clone();
            }
            if is_one(rhs) {
                return lhs.clone();
            }
        }
        "div" | "/" => {
            if is_one(rhs) {
                return lhs.clone();
            }
        }
        "band" | "&" => {
            if is_zero(lhs) || is_zero(rhs) {
                return Expr::IntLit(0);
            }
            if is_neg1(lhs) {
                return rhs.clone();
            }
            if is_neg1(rhs) {
                return lhs.clone();
            }
        }
        "shl" | "<<" | "shr" | ">>" => {
            if is_zero(rhs) {
                return lhs.clone();
            }
            if is_zero(lhs) {
                return Expr::IntLit(0);
            }
        }
        _ => {}
    }
    original
}

fn simplify_unary_expr(original: Expr, op: &str, expr: &Expr) -> Expr {
    match op {
        "neg" | "-" => {
            if let Expr::Unary {
                op: ref inner_op,
                expr: ref inner_expr,
            } = *expr
            {
                if inner_op == "neg" || inner_op == "-" {
                    return *inner_expr.clone();
                }
            }
        }
        "not" | "!" => {
            if let Expr::Unary {
                op: ref inner_op,
                expr: ref inner_expr2,
            } = *expr
            {
                if inner_op == "not" || inner_op == "!" {
                    return *inner_expr2.clone();
                }
            }
        }
        _ => {}
    }
    original
}

fn simplify_expr(e: Expr) -> Expr {
    // Clone-and-match on owned value to avoid borrow gymnastics
    match e.clone() {
        Expr::Binary { op, lhs, rhs, .. } => simplify_binary_expr(e, &op, &lhs, &rhs),
        Expr::Unary { op, expr, .. } => simplify_unary_expr(e, &op, &expr),
        _ => e,
    }
}

// ─── Dead Function Elimination ─────────────────────────────────────────────

/// Remove functions that are never called and not referenced as pointers.
fn remove_dead_functions(decls: &mut Vec<Decl>, entry: Option<&str>) {
    // Collect all called function names
    let mut called: HashSet<String> = HashSet::new();
    for d in decls.iter() {
        if let Decl::Function { body, .. } = d {
            for s in body {
                collect_calls_in_stmt(s, &mut called);
            }
        }
    }
    // Entry function and ptr-refs are always kept
    let mut ptr_refs: Vec<String> = Vec::new();
    detect_ptr_refs(decls, &mut ptr_refs);
    for n in &ptr_refs {
        called.insert(n.clone());
    }

    // Keep entry, remove the rest
    if let Some(e) = entry {
        called.insert(e.to_string());
    } else {
        // Fallback for callers that don't know the entry name
        called.insert("kernel_entry".to_string());
        called.insert("kernel-entry".to_string());
        called.insert("main".to_string());
    }
    // Transitive closure: aliases / wrappers that only show up as callees.
    let mut work: Vec<String> = called.iter().cloned().collect();
    while let Some(name) = work.pop() {
        let Some(Decl::Function { body, .. }) = decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name: n, .. } if n == &name))
        else {
            continue;
        };
        let mut extra = HashSet::new();
        for s in body {
            collect_calls_in_stmt(s, &mut extra);
        }
        for n in extra {
            if called.insert(n.clone()) {
                work.push(n);
            }
        }
    }
    decls.retain(|d| match d {
        Decl::Function { name, .. } => called.contains(name),
        _ => true,
    });
}

fn collect_calls_in_stmt(s: &Stmt, out: &mut HashSet<String>) {
    match s {
        Stmt::Let(_, _, e) | Stmt::Assign(_, e) | Stmt::Return(Some(e)) | Stmt::Expr(e) => {
            collect_calls_in_expr(e, out)
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            collect_calls_in_expr(base, out);
            collect_calls_in_expr(index, out);
            collect_calls_in_expr(value, out);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            collect_calls_in_expr(cond, out);
            for s in then_body {
                collect_calls_in_stmt(s, out);
            }
            for s in else_body {
                collect_calls_in_stmt(s, out);
            }
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(c) = cond {
                collect_calls_in_expr(c, out);
            }
            for s in body {
                collect_calls_in_stmt(s, out);
            }
        }
        Stmt::FieldAssign { base, value, .. } => {
            collect_calls_in_expr(base, out);
            collect_calls_in_expr(value, out);
        }
        Stmt::Throw(e) => collect_calls_in_expr(e, out),
        Stmt::Try { body, catches, .. } => {
            for s in body {
                collect_calls_in_stmt(s, out);
            }
            for c in catches {
                for s in &c.body {
                    collect_calls_in_stmt(s, out);
                }
            }
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            collect_calls_in_expr(scrutinee, out);
            for arm in arms {
                for s in &arm.body {
                    collect_calls_in_stmt(s, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_calls_in_expr(e: &Expr, out: &mut HashSet<String>) {
    walk_expr(e, &mut |e| {
        if let Expr::Call { callee, .. } = e {
            if let Expr::Ident(name) = callee.as_ref() {
                out.insert(name.clone());
            }
        }
        if let Expr::Closure { body, .. } = e {
            for s in body {
                collect_calls_in_stmt(s, out);
            }
        }
    });
}

// ─── Constant Folding ──────────────────────────────────────────────────────

/// Evaluate compile-time integer expressions: `3 + 4` → `7`.
fn fold_constants_in_decls(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        let old = std::mem::take(body);
        *body = old
            .into_iter()
            .flat_map(|s| fold_stmt_matches(map_stmt(s, &mut |e| fold_expr(e))))
            .collect();
    }
}

fn fold_stmt_matches(s: Stmt) -> Vec<Stmt> {
    match s {
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            let chosen = match &scrutinee {
                Expr::IntLit(v) => arms.iter().find(|a| {
                    parse_folded_int_pat(&a.pattern)
                        .map(|p| p == *v)
                        .unwrap_or(false)
                }),
                Expr::StringLit(v) => arms.iter().find(|a| {
                    parse_folded_string_pat(&a.pattern)
                        .as_ref()
                        .map(|p| p == v)
                        .unwrap_or(false)
                }),
                Expr::BoolLit(v) => arms.iter().find(|a| {
                    let t = a.pattern.trim();
                    (*v && t == "true") || (!*v && t == "false")
                }),
                _ => None,
            };
            if let Some(arm) = chosen {
                return arm.body.clone();
            }
            if matches!(
                &scrutinee,
                Expr::IntLit(_) | Expr::StringLit(_) | Expr::BoolLit(_)
            ) {
                if let Some(arm) = arms
                    .iter()
                    .find(|a| matches!(a.pattern.trim(), "_" | "-" | "else" | "default"))
                {
                    return arm.body.clone();
                }
            }
            vec![Stmt::Match { scrutinee, arms }]
        }
        other => vec![other],
    }
}

fn parse_folded_int_pat(pattern: &str) -> Option<i64> {
    let trimmed = pattern.trim().trim_end_matches(':').trim();
    let trimmed = trimmed.strip_prefix("case ").unwrap_or(trimmed).trim();
    trimmed.parse::<i64>().ok()
}

fn parse_folded_string_pat(pattern: &str) -> Option<String> {
    let trimmed = pattern.trim().trim_end_matches(':').trim();
    let trimmed = trimmed.strip_prefix("case ").unwrap_or(trimmed).trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        Some(trimmed[1..trimmed.len() - 1].to_string())
    } else {
        None
    }
}

fn fold_expr(e: Expr) -> Expr {
    match &e {
        Expr::Binary { op, lhs, rhs, .. } => {
            if let (Expr::FloatLit(a), Expr::FloatLit(b)) = (lhs.as_ref(), rhs.as_ref()) {
                let av = a.0;
                let bv = b.0;
                let result = match op.as_str() {
                    "+" | "add" => Some(av + bv),
                    "-" | "sub" => Some(av - bv),
                    "*" | "mul" => Some(av * bv),
                    "/" | "div" if bv != 0.0 => Some(av / bv),
                    _ => None,
                };
                if let Some(v) = result {
                    return Expr::FloatLit(crate::core_ir::FloatVal(v));
                }
            }
            if let (Expr::StringLit(a), Expr::StringLit(b)) = (lhs.as_ref(), rhs.as_ref()) {
                if matches!(op.as_str(), "+" | "add") {
                    return Expr::StringLit(format!("{a}{b}"));
                }
            }
            if let (Expr::IntLit(a), Expr::IntLit(b)) = (lhs.as_ref(), rhs.as_ref()) {
                // ponytail: skip div-by-zero (would change runtime behavior)
                let result = match op.as_str() {
                    "add" | "+" => a.checked_add(*b),
                    "sub" | "-" => a.checked_sub(*b),
                    "mul" | "*" => a.checked_mul(*b),
                    "div" | "/" if *b != 0 => a.checked_div(*b),
                    "mod" | "%" if *b != 0 => a.checked_rem(*b),
                    "band" | "&" => Some(a & b),
                    "bor" | "|" => Some(a | b),
                    "xor" | "^" => Some(a ^ b),
                    "shl" | "<<" if *b >= 0 && *b < 64 => a.checked_shl(*b as u32),
                    "shr" | ">>" if *b >= 0 && *b < 64 => a.checked_shr(*b as u32),
                    "eq" | "==" => Some(if a == b { 1 } else { 0 }),
                    "neq" | "!=" => Some(if a != b { 1 } else { 0 }),
                    "lt" | "<" => Some(if a < b { 1 } else { 0 }),
                    "gt" | ">" => Some(if a > b { 1 } else { 0 }),
                    "le" | "<=" => Some(if a <= b { 1 } else { 0 }),
                    "ge" | ">=" => Some(if a >= b { 1 } else { 0 }),
                    "land" | "&&" => Some(if *a != 0 && *b != 0 { 1 } else { 0 }),
                    "lor" | "||" => Some(if *a != 0 || *b != 0 { 1 } else { 0 }),
                    _ => None,
                };
                if let Some(v) = result {
                    return Expr::IntLit(v);
                }
            }
            e
        }
        Expr::Unary { op, expr, .. } => {
            if let Expr::IntLit(n) = expr.as_ref() {
                let result = match op.as_str() {
                    "neg" => Some(-n),
                    "not" => Some(if *n == 0 { 1 } else { 0 }),
                    _ => None,
                };
                if let Some(v) = result {
                    return Expr::IntLit(v);
                }
            }
            e
        }
        Expr::Index { base, index, .. } => {
            if let (Expr::ArrayLit(items), Expr::IntLit(i)) = (base.as_ref(), index.as_ref()) {
                if *i >= 0 {
                    let idx = *i as usize;
                    if idx < items.len() {
                        return items[idx].clone();
                    }
                }
            }
            e
        }
        Expr::Field { base, name, .. } => {
            if let Expr::StructInit { fields, .. } = base.as_ref() {
                if let Some((_, value)) = fields.iter().find(|(n, _)| n == name) {
                    return value.clone();
                }
            }
            e
        }
        _ => e,
    }
}

// ─── Constant Propagation ──────────────────────────────────────────────────

/// Replace `let x = C; ... x ...` with `... C ...` when `x` is never assigned.
/// Scalars and structs substitute on every use; array literals are excluded
/// because backends materialize arrays as stack slots — inlining the literal
/// into `arr[i]` would leave `arr` unbound and turn variable-index reads into
/// a form no owned backend can execute.
fn propagate_constants(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        propagate_in_body(body);
    }
}

fn is_propagatable_const(e: &Expr) -> bool {
    match e {
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::BoolLit(_) | Expr::StringLit(_) => true,
        Expr::StructInit { fields, .. } => fields.iter().all(|(_, v)| is_propagatable_const(v)),
        _ => false,
    }
}

fn collect_assigned_names(s: &Stmt, assigned: &mut HashSet<String>) {
    match s {
        Stmt::Assign(n, _) => {
            assigned.insert(n.clone());
        }
        // `arr[i] = v` and `s.f = v` mutate the base binding, so the base must
        // be treated as assigned (otherwise constant propagation would inline
        // the initializer and the store would write to a discarded temporary).
        Stmt::IndexAssign { base, .. } | Stmt::FieldAssign { base, .. } => {
            if let Expr::Ident(n) = base {
                assigned.insert(n.clone());
            }
        }
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            for inner in then_body.iter().chain(else_body.iter()) {
                collect_assigned_names(inner, assigned);
            }
        }
        Stmt::Loop { body, .. } => {
            for inner in body {
                collect_assigned_names(inner, assigned);
            }
        }
        Stmt::Match { arms, .. } => {
            for arm in arms {
                for inner in &arm.body {
                    collect_assigned_names(inner, assigned);
                }
            }
        }
        Stmt::Try { body, catches, .. } => {
            for inner in body {
                collect_assigned_names(inner, assigned);
            }
            for c in catches {
                for inner in &c.body {
                    collect_assigned_names(inner, assigned);
                }
            }
        }
        _ => {}
    }
}

fn propagate_in_body(stmts: &mut Vec<Stmt>) {
    let mut consts: HashMap<String, Expr> = HashMap::new();
    let mut assigned: HashSet<String> = HashSet::new();
    for s in stmts.iter() {
        if let Stmt::Let(n, _, e) = s {
            if is_propagatable_const(e) {
                consts.entry(n.clone()).or_insert_with(|| e.clone());
            }
        }
        collect_assigned_names(s, &mut assigned);
    }
    consts.retain(|n, _| !assigned.contains(n));
    if consts.is_empty() {
        return;
    }

    for s in stmts.iter_mut() {
        *s = replace_in_stmt(s, &consts);
    }
    stmts.retain(|s| !matches!(s, Stmt::Let(n, _, _) if consts.contains_key(n)));
}

fn replace_in_stmt(s: &Stmt, sub: &HashMap<String, Expr>) -> Stmt {
    map_stmt(s.clone(), &mut |e| replace_in_expr(e, sub))
}

fn replace_in_expr(e: Expr, sub: &HashMap<String, Expr>) -> Expr {
    map_expr(e, &mut |e| match e {
        Expr::Ident(n) => sub.get(&n).cloned().unwrap_or(Expr::Ident(n)),
        other => other,
    })
}

// ─── Dead Code Elimination ─────────────────────────────────────────────────

fn dead_code_eliminate(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        dce_body(body);
    }
}

fn dce_body(stmts: &mut Vec<Stmt>) {
    // Collapse duplicate void-returns: `return; return;` → `return;`
    let mut cleaned: Vec<Stmt> = Vec::with_capacity(stmts.len());
    for s in stmts.iter() {
        if matches!(s, Stmt::Return(None)) {
            if cleaned
                .last()
                .map_or(false, |p| matches!(p, Stmt::Return(None)))
            {
                continue;
            }
        }
        cleaned.push(s.clone());
    }
    *stmts = cleaned;

    // Remove unused let bindings — but never drop one whose initializer may
    // have observable side effects (a function call: I/O, MMIO, allocation).
    // Dropping `let x = side_effectful();` would silently skip the call.
    let used: HashSet<String> = {
        let mut s = HashSet::new();
        collect_used(stmts, &mut s);
        s
    };
    stmts.retain(|s| !matches!(s, Stmt::Let(n, _, e) if !used.contains(n) && !expr_has_call(e)));

    // Recurse
    for s in stmts.iter_mut() {
        match s {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                dce_body(then_body);
                dce_body(else_body);
            }
            Stmt::Loop { body, .. } => dce_body(body),
            Stmt::Try { body, catches, .. } => {
                dce_body(body);
                for c in catches.iter_mut() {
                    dce_body(&mut c.body);
                }
            }
            Stmt::Match { arms, .. } => {
                for arm in arms.iter_mut() {
                    dce_body(&mut arm.body);
                }
            }
            Stmt::FieldAssign { .. } => {}
            _ => {}
        }
    }
}

fn collect_used(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match s {
            Stmt::Assign(n, e) => {
                out.insert(n.clone());
                collect_used_in_expr(e, out);
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                collect_used_in_expr(base, out);
                collect_used_in_expr(index, out);
                collect_used_in_expr(value, out);
            }
            Stmt::Let(_, _, e) | Stmt::Return(Some(e)) | Stmt::Expr(e) => {
                collect_used_in_expr(e, out)
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                collect_used_in_expr(cond, out);
                collect_used(then_body, out);
                collect_used(else_body, out);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(c) = cond {
                    collect_used_in_expr(c, out);
                }
                collect_used(body, out);
            }
            Stmt::FieldAssign { base, value, .. } => {
                collect_used_in_expr(base, out);
                collect_used_in_expr(value, out);
            }
            Stmt::Throw(e) => collect_used_in_expr(e, out),
            Stmt::Try { body, catches, .. } => {
                collect_used(body, out);
                for c in catches {
                    collect_used(&c.body, out);
                }
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                collect_used_in_expr(scrutinee, out);
                for arm in arms {
                    collect_used(&arm.body, out);
                }
            }
            _ => {}
        }
    }
}

fn collect_used_in_expr(e: &Expr, out: &mut HashSet<String>) {
    walk_expr(e, &mut |e| {
        if let Expr::Ident(n) = e {
            out.insert(n.clone());
        }
    });
}

/// Table-driven x86-64 instruction length decoder.
/// Returns the total byte length of the instruction starting at `code[pos]`.
/// Used by the peephole to correctly identify instruction boundaries.
fn x86_64_insn_length(code: &[u8], pos: usize) -> usize {
    let mut p = pos;

    // ── Skip legacy prefixes ──
    while p < code.len() {
        match code[p] {
            // Group 1: lock, repne, repe
            0xF0 | 0xF2 | 0xF3 => {
                p += 1;
            }
            // Group 2: segment overrides, branch hints
            0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => {
                p += 1;
            }
            // Group 3: operand size override
            0x66 => {
                p += 1;
            }
            // Group 4: address size override
            0x67 => {
                p += 1;
            }
            // REX prefix (0x40-0x4F)
            _ if (0x40..=0x4F).contains(&code[p]) => {
                p += 1;
            }
            _ => break,
        }
    }

    if p >= code.len() {
        return code.len() - pos;
    }

    // ── Opcode ──
    let op1 = code[p];
    p += 1;

    // Two-byte opcode (0x0F prefix)
    let op2: Option<u8> = if op1 == 0x0F && p < code.len() {
        let o2 = code[p];
        p += 1;
        Some(o2)
    } else {
        None
    };

    // Three-byte opcode (0x0F 0x38/0x3A)
    let _op3: Option<u8> = if let Some(0x38 | 0x3A) = op2 {
        if p < code.len() {
            let o3 = code[p];
            p += 1;
            Some(o3)
        } else {
            None
        }
    } else {
        None
    };

    // ── Determine if ModRM follows ──
    // Most non-immediate, non-relative opcodes use ModRM.
    // Exceptions: opcodes with implicit operands.
    let opcode_total = if let Some(o2) = op2 { o2 } else { op1 };
    let two_byte = op2.is_some();

    let has_modrm = match (two_byte, opcode_total) {
        // Immediate-only: push/pop, mov al/ax/eax/rax, etc.
        (false, 0x50..=0x5F) => false, // push r64 / pop r64
        (false, 0x60..=0x6F) => true,  // pusha/pusha/pushad/pop variants
        (false, 0x70..=0x7F) => false, // jcc rel8 (handled by caller)
        (false, 0x9C) => false,        // pushfq
        (false, 0x9D) => false,        // popfq
        (false, 0x9E) => false,        // sahf
        (false, 0x9F) => false,        // lahf
        (false, 0xA0..=0xAF) => true,  // mov al,[addr] etc.
        (false, 0xB0..=0xBF) => false, // mov r8..r15, imm8/32/64
        (false, 0xC0..=0xC1) => true,  // shift by imm8
        (false, 0xC2) => false,        // ret near imm16
        (false, 0xC3) => false,        // ret
        (false, 0xC6..=0xC7) => true,  // mov r/m, imm
        (false, 0xCA) => false,        // ret far imm16
        (false, 0xCB) => false,        // ret far
        (false, 0xCC) => false,        // int3
        (false, 0xCD) => false,        // int imm8
        (false, 0xCE) => false,        // into
        (false, 0xCF) => false,        // iret
        (false, 0xD0..=0xD3) => true,  // shift by 1/cl
        (false, 0xD4..=0xD5) => true,  // aam/aad
        (false, 0xD6) => false,        // salc (undefined)
        (false, 0xD7) => true,         // xlat
        (false, 0xE0..=0xE3) => false, // loop/loope/loopne/jecxz (rel8)
        (false, 0xE4..=0xE7) => false, // in/out imm8
        (false, 0xE8..=0xEB) => false, // call/jmp rel32, jmp rel8 (handled by caller)
        (false, 0xEC..=0xEF) => false, // in/out dx
        (false, 0xF4) => false,        // hlt
        (false, 0xF5) => false,        // cmc
        (false, 0xF6..=0xF7) => true,  // test/not/neg/mul/imul/div/idiv r/m
        (false, 0xF8) => false,        // clc
        (false, 0xF9) => false,        // stc
        (false, 0xFA) => false,        // cli
        (false, 0xFB) => false,        // sti
        (false, 0xFC) => false,        // cld
        (false, 0xFD) => false,        // std
        (false, 0xFE..=0xFF) => true,  // inc/dec/call/jmp/push r/m
        // Two-byte opcodes
        (true, 0x00..=0x7F) => true,  // most 0F-prefixed instructions
        (true, 0x80..=0x8F) => false, // jcc rel32 (handled by caller)
        (true, 0x90..=0x9F) => false, // setcc (modrm after)
        (true, 0xA0..=0xA7) => false, // push fs/gs
        (true, 0xA8..=0xAF) => false, // swapgs, rdtscp
        (true, 0xB0..=0xBF) => true,  // cmpxchg
        (true, 0xC0..=0xC1) => true,  // xadd
        (true, 0xC2) => true,         // cmpss/cmpsd/cmpps/cmppd
        (true, 0xC3..=0xC6) => true,  // movnti, pinsrw, shufps/pd
        (true, 0xC7..=0xCF) => true,  // cmovcc
        (true, 0xD0..=0xDF) => true,  // SSE1
        (true, 0xE0..=0xEF) => true,  // SSE1
        (true, 0xF0..=0xFF) => true,  // SSE1/SSE2
        _ => true,                    // Conservative: assume ModRM
    };

    // ── ModRM byte ──
    let mut modrm: u8 = 0;
    if has_modrm && p < code.len() {
        modrm = code[p];
        p += 1;
    }

    if has_modrm {
        let mod_field = modrm >> 6;
        let rm_field = modrm & 7;

        // ── SIB byte ──
        let has_sib = mod_field != 3 && rm_field == 4;
        if has_sib && p < code.len() {
            p += 1; // skip SIB
        }

        // ── Displacement ──
        if mod_field == 1 {
            p += 1; // disp8
        } else if mod_field == 2 {
            p += 4; // disp32
        } else if mod_field == 0 && rm_field == 5 && !has_sib && !two_byte {
            p += 4; // disp32 (RIP-relative)
        } else if mod_field == 0 && rm_field == 5 && two_byte {
            p += 4; // disp32 (two-byte opcode RIP-relative)
        }
    }

    // ── Immediate ──
    let immediate_size = match (two_byte, opcode_total) {
        // MOV r8..r15, imm64
        (false, 0xB8..=0xBF)
            if code[pos..p]
                .iter()
                .any(|&b| (0x40..=0x4F).contains(&b) && (b & 8) != 0) =>
        {
            8
        } // REX.W + mov r64, imm64
        (false, 0xB8..=0xBF) => {
            // Without REX.W or with REX but not W=1: 32-bit sign-extended
            if code[pos..p].contains(&0x48) { 4 } else { 4 }
        }
        // MOV r/m64, imm32 (REX.W + C7 /0)
        (false, 0xC7) => {
            // opcode extension in reg field: /0 = mov, /1 = xbegin
            let reg_ext = (modrm >> 3) & 7;
            if reg_ext == 0 {
                // need to check for REX.W for 64-bit
                let rex_w = code[pos..p - 2].contains(&0x48);
                if rex_w { 4 } else { 4 }
            } else {
                4
            }
        }
        // shift/rotate by imm8 (C0/C1)
        (false, 0xC0) | (false, 0xC1) => 1,
        // shifts by imm8 (D0/D1/D2/D3)
        (false, 0xD0) | (false, 0xD1) | (false, 0xD2) | (false, 0xD3) => 0,
        // enter: imm16 + imm8
        (false, 0xC8) => 4, // enter imm16, imm8 (3 actually, but we need 4 for 2 immediates)
        // ret near imm16
        (false, 0xC2) => 2,
        // int imm8
        (false, 0xCD) => 1,
        // AAM/AAD: imm8
        (false, 0xD4) | (false, 0xD5) => 1,
        // IN/OUT imm8
        (false, 0xE4) | (false, 0xE5) | (false, 0xE6) | (false, 0xE7) => 1,
        // PUSH imm8/imm32
        (false, 0x6A) => 1, // push imm8
        (false, 0x68) => 4, // push imm32
        (false, 0x6B) => 1, // imul r64, r/m, imm8
        (false, 0x69) => 4, // imul r64, r/m, imm32
        // ARPL
        (false, 0x63) if !code[pos..p].iter().any(|&b| (0x40..=0x4F).contains(&b)) => 0, // not MOVSXD (without REX)
        // MOVSXD
        (false, 0x63) => 0,
        _ => 0,
    };
    p += immediate_size;

    p - pos
}

// ─── x86_64 Peephole ──────────────────────────────────────────────────────

/// A relative jump/call instruction to re-patch after byte removal.
#[derive(Debug)]
struct RelJump {
    pos: usize,         // start position in code buffer
    len: usize,         // instruction length (2, 5, or 6 bytes)
    offset_byte: usize, // position of the offset bytes (pos+1 for most, pos+2 for 0F 8x)
    offset_value: i32,  // original signed offset
    target: usize,      // absolute target position after instruction
}

#[derive(Debug)]
struct RemoveRange {
    start: usize,
    len: usize,
}

/// Scan the code buffer and remove redundant `mov r, r` instructions where
/// source and destination registers are the same. Also removes trailing NOPs.
///
/// Handles offset re-patching for all relative jump/call instructions so that
/// conditional branches, loop jumps, and function calls remain correct after
/// byte removal.
pub fn peephole_x86_64(code: &mut Vec<u8>) {
    if code.is_empty() {
        return;
    }

    let orig_len = code.len();

    let jumps = find_relative_jumps(code);
    let mut remove = find_removable_instructions(code, &jumps);

    // Nothing to do?
    if remove.is_empty() {
        return;
    }

    adjust_jump_offsets(code, &jumps, &remove);

    // ── Pass 5: remove bytes (highest first to avoid shifting) ──
    remove.sort_by_key(|r| std::cmp::Reverse(r.start));
    for r in remove {
        code.drain(r.start..r.start + r.len);
    }

    let removed = orig_len - code.len();
    if removed > 0 {
        eprintln!("peephole: removed {removed} bytes");
    }
}

fn find_relative_jumps(code: &[u8]) -> Vec<RelJump> {
    let mut jumps = Vec::new();
    let mut i = 0;
    while i < code.len() {
        let b = code[i];
        let (len, is_rel, offset_idx, offset_size) = match b {
            0xE8 => (5, true, 1, 4),        // call rel32
            0xE9 => (5, true, 1, 4),        // jmp rel32
            0xEB => (2, true, 1, 1),        // jmp rel8
            0x70..=0x7F => (2, true, 1, 1), // jcc rel8
            0x0F if i + 1 < code.len() && (0x80..=0x8F).contains(&code[i + 1]) => {
                (6, true, 2, 4) // jcc rel32 (0F 8x ...)
            }
            _ => {
                let insn_len = x86_64_insn_length(code, i);
                (insn_len, false, 0, 0)
            }
        };
        if is_rel {
            let offset_value: i32 = if offset_size == 4 {
                i32::from_le_bytes([
                    code[i + offset_idx],
                    code[i + offset_idx + 1],
                    code[i + offset_idx + 2],
                    code[i + offset_idx + 3],
                ])
            } else {
                // offset_size == 1 (rel8) → sign-extend
                (code[i + offset_idx] as i8) as i32
            };
            let target = (i + len).wrapping_add(offset_value as usize);
            jumps.push(RelJump {
                pos: i,
                len,
                offset_byte: i + offset_idx,
                offset_value,
                target,
            });
        }
        i += len;
    }
    jumps
}

fn find_removable_instructions(code: &[u8], jumps: &[RelJump]) -> Vec<RemoveRange> {
    let target_set: std::collections::HashSet<usize> = jumps.iter().map(|j| j.target).collect();
    let mut remove: Vec<RemoveRange> = Vec::new();

    let mut i = 0;
    while i < code.len() {
        let is_redundant_mov = if i + 1 < code.len() {
            let (opcode, modrm) = if code[i] == 0x48 && i + 2 < code.len() {
                (code[i + 1], code[i + 2])
            } else {
                (code[i], code[i + 1])
            };
            let mod_field = modrm >> 6; // bits 7:6
            let reg_field = (modrm >> 3) & 7; // bits 5:3
            let rm_field = modrm & 7; // bits 2:0
            if mod_field == 3 && reg_field == rm_field {
                matches!(opcode, 0x89 | 0x8B)
            } else {
                false
            }
        } else {
            false
        };
        if is_redundant_mov {
            let mov_len = if i + 2 < code.len() && code[i] == 0x48 {
                3
            } else {
                2
            };
            // Safety: only remove if no jump targets this instruction
            let is_jump_target = (0..mov_len).any(|delta| target_set.contains(&(i + delta)));
            if !is_jump_target {
                remove.push(RemoveRange {
                    start: i,
                    len: mov_len,
                });
            }
            i += mov_len;
        } else {
            i += 1;
        }
    }

    let mut i = 0;
    while i < code.len() {
        let is_redundant_mov = if i + 1 < code.len() {
            let (opcode, modrm) = if code[i] == 0x48 && i + 2 < code.len() {
                (code[i + 1], code[i + 2])
            } else {
                (code[i], code[i + 1])
            };
            let mod_field = modrm >> 6; // bits 7:6
            let reg_field = (modrm >> 3) & 7; // bits 5:3
            let rm_field = modrm & 7; // bits 2:0
            if mod_field == 3 && reg_field == rm_field {
                matches!(opcode, 0x89 | 0x8B)
            } else {
                false
            }
        } else {
            false
        };
        if is_redundant_mov {
            let mov_len = if i + 2 < code.len() && code[i] == 0x48 {
                3
            } else {
                2
            };
            remove.push(RemoveRange {
                start: i,
                len: mov_len,
            });
            i += mov_len;
        } else {
            i += 1;
        }
    }

    // Remove trailing `66 90` (2-byte NOP) and `90` (single NOP)
    let mut trailing = 0;
    let mut j = code.len();
    while j >= 2 && code[j - 2] == 0x66 && code[j - 1] == 0x90 {
        trailing += 2;
        j -= 2;
    }
    if j >= 1 && code[j - 1] == 0x90 {
        trailing += 1;
    }
    if trailing > 0 {
        remove.push(RemoveRange {
            start: code.len() - trailing,
            len: trailing,
        });
    }

    // Sort remove ranges by position (ascending) and merge overlaps
    remove.sort_by_key(|r| r.start);
    let mut merged: Vec<RemoveRange> = Vec::new();
    for r in remove {
        if let Some(last) = merged.last_mut() {
            if r.start <= last.start + last.len {
                // Overlap or adjacent → extend
                let end = std::cmp::max(last.start + last.len, r.start + r.len);
                last.len = end - last.start;
                continue;
            }
        }
        merged.push(r);
    }
    merged
}

fn adjust_jump_offsets(code: &mut [u8], jumps: &[RelJump], remove: &[RemoveRange]) {
    for jmp in jumps {
        let old_target = jmp.target;
        let old_offset = jmp.offset_value;

        // Calculate how many bytes are removed BETWEEN the jump instruction
        // and its target (after jump end, before target start)
        let jump_end = jmp.pos + jmp.len;
        let mut removed_between: usize = 0;
        for r in remove {
            let r_end = r.start + r.len;
            // Removal is after jump end and before target
            if r.start >= jump_end && r_end <= old_target {
                removed_between += r.len;
            } else if r.start < old_target && r_end > old_target && r.start >= jump_end {
                // Partial overlap: only bytes before target count
                removed_between += old_target - r.start;
            }
        }

        // New offset: original offset minus bytes removed between jump and target.
        // Bytes removed before the jump shift both instruction and target equally
        // so the relative offset stays the same.
        let new_offset = old_offset as isize - removed_between as isize;

        // Write the adjusted offset into the buffer
        // Bytes removed before the jump shift the offset byte position
        let mut removed_before_jump: usize = 0;
        for r in remove {
            let r_end = r.start + r.len;
            if r_end <= jmp.pos {
                removed_before_jump += r.len;
            }
        }
        let offset_byte = jmp.offset_byte - removed_before_jump;

        if jmp.len == 2 {
            // rel8: 1 byte offset
            code[offset_byte] = (new_offset as i8) as u8;
        } else {
            // rel32: 4 byte offset
            let new_offset_i32 = new_offset as i32;
            code[offset_byte..offset_byte + 4].copy_from_slice(&new_offset_i32.to_le_bytes());
        }
    }
}

// ─── Harden (anti-decomp) IR transforms ────────────────────────────────────

fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn is_stdlib_or_reserved(name: &str) -> bool {
    matches!(
        name,
        "main"
            | "print"
            | "join"
            | "chain"
            | "extend"
            | "push"
            | "push-str"
            | "read-file"
            | "write-file"
            | "fs-exists"
            | "create-dir"
            | "remove-file"
            | "process-run"
            | "env-get"
            | "env-set"
            | "env-has"
            | "env-temp-dir"
            | "env-current-dir"
            | "path-join"
            | "path-dirname"
            | "path-basename"
            | "path-extname"
            | "path-normalize"
            | "str-concat"
            | "str-eq"
            | "json-stringify"
            | "str-table-has"
            | "str-table-get-int"
            | "str-contains"
            | "str-starts-with"
            | "str-ends-with"
            | "str-index-of"
            | "str-is-int"
            | "str-slice"
            | "str-split-lines"
            | "str-split-spaces"
            | "str-tokenize-expr"
            | "str-to-int"
            | "str-trim"
            | "to-string"
    ) || name.starts_with("in_")
        || name.starts_with("std.")
}

fn bin_expr(op: &str, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op: op.into(),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// Mixed-boolean arithmetic that Hex-Rays / Ghidra rarely fold:
/// `x + y` → `(x ^ y) + 2*(x & y)`
/// `x ^ y` → `(x | y) - (x & y)`
/// `x | y` → `(x & y) + (x ^ y)`
fn harden_mba_arithmetic(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        for stmt in body.iter_mut() {
            map_stmt_mut(stmt, &mut |e| {
                if let Expr::Binary { op, lhs, rhs } = e {
                    // One MBA layer on leaf operands only — nested binaries would
                    // explode after the bottom-up walk rewrote the children.
                    if matches!(&**lhs, Expr::Binary { .. })
                        || matches!(&**rhs, Expr::Binary { .. })
                    {
                        return;
                    }
                    let l = *lhs.clone();
                    let r = *rhs.clone();
                    match op.as_str() {
                        "+" => {
                            *e = bin_expr(
                                "+",
                                bin_expr("^", l.clone(), r.clone()),
                                bin_expr("*", Expr::IntLit(2), bin_expr("&", l, r)),
                            );
                        }
                        "^" => {
                            *e = bin_expr(
                                "-",
                                bin_expr("|", l.clone(), r.clone()),
                                bin_expr("&", l, r),
                            );
                        }
                        "|" => {
                            *e = bin_expr(
                                "+",
                                bin_expr("&", l.clone(), r.clone()),
                                bin_expr("^", l, r),
                            );
                        }
                        _ => {}
                    }
                }
            });
        }
    }
}

/// Obscure integer/bool literals as `(x ^ k) ^ k` / equivalent forms.
fn harden_obscure_literals(decls: &mut [Decl]) {
    let mut counter = 0u64;
    for body in fn_bodies_mut(decls) {
        for stmt in body.iter_mut() {
            map_stmt_mut(stmt, &mut |e| {
                obscure_expr_literals(e, &mut counter);
            });
        }
    }
}

fn obscure_expr_literals(e: &mut Expr, counter: &mut u64) {
    match e {
        Expr::IntLit(v) => {
            *counter = counter.wrapping_add(1);
            let k = ((*counter).wrapping_mul(0x9E3779B97F4A7C15) ^ (*v as u64)) as i64 | 1;
            // ((v ^ k) ^ k) | (k & 0) == v. Extra `| 0` is noise, not a third XOR of k.
            *e = bin_expr(
                "|",
                bin_expr("^", Expr::IntLit(*v ^ k), Expr::IntLit(k)),
                bin_expr("&", Expr::IntLit(k), Expr::IntLit(0)),
            );
        }
        Expr::BoolLit(b) => {
            // Opaque comparisons: true as `1 != 0`, false as `1 == 0` (never `0 == 0`).
            let one = Expr::IntLit(1);
            let zero = Expr::IntLit(0);
            *e = if *b {
                bin_expr("!=", one, zero)
            } else {
                bin_expr("==", one, zero)
            };
        }
        Expr::StringLit(s) => {
            // Chunk into XOR-masked reconstruction via str-concat of single-char
            // pieces when short; otherwise leave (stdlib may be unavailable).
            if s.is_empty() || s.len() > 32 {
                return;
            }
            // Represent as identity concat of itself split — mild fingerprint noise.
            // Full XOR decode needs runtime helpers; emit opaque Int length side-bind instead.
            let _ = s;
        }
        _ => {}
    }
}

/// Wrap `if` conditions with always-true opaque predicates.
fn harden_opaque_predicates(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        harden_opaque_in_stmts(body);
    }
}

fn harden_opaque_in_stmts(stmts: &mut [Stmt]) {
    for stmt in stmts.iter_mut() {
        match stmt {
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                let original = std::mem::replace(cond, Expr::BoolLit(true));
                // 7*7 - 49 == 0  (always true) AND original.
                let opaque = bin_expr(
                    "==",
                    bin_expr(
                        "-",
                        bin_expr("*", Expr::IntLit(7), Expr::IntLit(7)),
                        Expr::IntLit(49),
                    ),
                    Expr::IntLit(0),
                );
                *cond = bin_expr("&&", opaque, original);
                harden_opaque_in_stmts(then_body);
                harden_opaque_in_stmts(else_body);
            }
            Stmt::Loop { body, .. } => harden_opaque_in_stmts(body),
            Stmt::Try { body, catches } => {
                harden_opaque_in_stmts(body);
                for c in catches.iter_mut() {
                    harden_opaque_in_stmts(&mut c.body);
                }
            }
            Stmt::Match { arms, .. } => {
                for arm in arms.iter_mut() {
                    harden_opaque_in_stmts(&mut arm.body);
                }
            }
            _ => {}
        }
    }
}

/// Insert never-taken bogus blocks that look like real control flow.
fn harden_bogus_blocks(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        if body.is_empty() {
            continue;
        }
        // Never-taken: (1 == 0) && (x*x < 0). Looks live to pattern matchers.
        let bogey = Stmt::If {
            cond: bin_expr(
                "&&",
                bin_expr("==", Expr::IntLit(1), Expr::IntLit(0)),
                bin_expr(
                    "<",
                    bin_expr("*", Expr::IntLit(5), Expr::IntLit(5)),
                    Expr::IntLit(0),
                ),
            ),
            then_body: vec![
                Stmt::Let("_bogus".into(), Some(Typ::Int), Expr::IntLit(0xDEAD)),
                Stmt::Assign(
                    "_bogus".into(),
                    bin_expr("+", Expr::Ident("_bogus".into()), Expr::IntLit(1)),
                ),
            ],
            else_body: vec![],
        };
        // Insert after first statement so entry still looks "real".
        body.insert(1.min(body.len()), bogey);
    }
}

/// Semantics-preserving junk lets/assigns.
fn harden_junk_stmts(decls: &mut [Decl]) {
    let mut n = 0usize;
    for body in fn_bodies_mut(decls) {
        let mut out = Vec::with_capacity(body.len() * 2);
        for stmt in body.drain(..) {
            n += 1;
            let jname = format!("_j{n}");
            out.push(Stmt::Let(jname.clone(), Some(Typ::Int), Expr::IntLit(0)));
            out.push(Stmt::Assign(
                jname,
                bin_expr(
                    "^",
                    bin_expr("&", Expr::IntLit(0x55), Expr::IntLit(0xAA)),
                    bin_expr("|", Expr::IntLit(0), Expr::IntLit(0)),
                ),
            ));
            out.push(stmt);
        }
        *body = out;
    }
}

/// Lite control-flow flattening: wrap eligible bodies in a `while _pc < N`
/// dispatcher of `if _pc == i` states.
///
/// Nested `if` is allowed (then/else stay inside a state). Skips loops /
/// breaks / try / match / throw / propagate so we do not fight real control
/// flow or rely on `Break` (native lower currently treats break as a no-op).
fn stmts_forbid_cfg_dispatch(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::Loop { .. }
            | Stmt::Break
            | Stmt::Continue
            | Stmt::Try { .. }
            | Stmt::Match { .. }
            | Stmt::Throw(_)
            | Stmt::Propagate => return true,
            Stmt::If {
                then_body,
                else_body,
                ..
            } if stmts_forbid_cfg_dispatch(then_body) || stmts_forbid_cfg_dispatch(else_body) => {
                return true;
            }
            Stmt::If { .. } => {}
            _ => {}
        }
    }
    false
}

fn harden_cfg_dispatch_lite(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        if body.len() < 2 {
            continue;
        }
        if stmts_forbid_cfg_dispatch(body) {
            continue;
        }
        let original: Vec<Stmt> = std::mem::take(body);
        // Chunk large bodies so the dispatcher stays bounded.
        let chunk = if original.len() > 8 { 2 } else { 1 };
        let mut chunks: Vec<Vec<Stmt>> = Vec::new();
        let mut cur = Vec::new();
        for stmt in original {
            cur.push(stmt);
            if cur.len() >= chunk {
                chunks.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            chunks.push(cur);
        }
        let state_count = chunks.len() as i64;
        let mut chain: Option<Stmt> = None;
        for (i, chunk_stmts) in chunks.into_iter().enumerate().rev() {
            let i = i as i64;
            let mut then_body = chunk_stmts;
            then_body.push(Stmt::Assign("_pc".into(), Expr::IntLit(i + 1)));
            let cond = Expr::Binary {
                op: "==".into(),
                lhs: Box::new(Expr::Ident("_pc".into())),
                rhs: Box::new(Expr::IntLit(i)),
            };
            let else_body = match chain.take() {
                Some(s) => vec![s],
                None => vec![Stmt::Assign("_pc".into(), Expr::IntLit(state_count))],
            };
            chain = Some(Stmt::If {
                cond,
                then_body,
                else_body,
            });
        }
        let dispatch = chain.expect("chunks non-empty");
        *body = vec![
            Stmt::Let("_pc".into(), Some(Typ::Int), Expr::IntLit(0)),
            Stmt::Loop {
                kind: LoopKind::While,
                cond: Some(Expr::Binary {
                    op: "<".into(),
                    lhs: Box::new(Expr::Ident("_pc".into())),
                    rhs: Box::new(Expr::IntLit(state_count)),
                }),
                body: vec![dispatch],
            },
        ];
    }
}

/// Hash-mangle internal function names beyond normal ABI.
fn harden_hash_symbols(decls: &mut [Decl], entry: Option<&str>) {
    let entry = entry.unwrap_or("main");
    let mut rename: HashMap<String, String> = HashMap::new();
    for d in decls.iter() {
        if let Decl::Function { name, .. } = d {
            if name == entry || is_stdlib_or_reserved(name) || name.starts_with("_H") {
                continue;
            }
            let h = fnv1a64(name);
            rename.insert(name.clone(), format!("_H{h:016x}"));
        }
    }
    if rename.is_empty() {
        return;
    }
    for d in decls.iter_mut() {
        if let Decl::Function { name, body, .. } = d {
            if let Some(new) = rename.get(name) {
                *name = new.clone();
            }
            rename_calls_in_stmts(body, &rename);
        }
    }
}

fn rename_calls_in_stmts(stmts: &mut [Stmt], rename: &HashMap<String, String>) {
    for stmt in stmts.iter_mut() {
        match stmt {
            Stmt::Let(_, _, e)
            | Stmt::Assign(_, e)
            | Stmt::FieldAssign { value: e, .. }
            | Stmt::Return(Some(e))
            | Stmt::Throw(e)
            | Stmt::Expr(e) => rename_calls_in_expr(e, rename),
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                rename_calls_in_expr(base, rename);
                rename_calls_in_expr(index, rename);
                rename_calls_in_expr(value, rename);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                rename_calls_in_expr(cond, rename);
                rename_calls_in_stmts(then_body, rename);
                rename_calls_in_stmts(else_body, rename);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(c) = cond {
                    rename_calls_in_expr(c, rename);
                }
                rename_calls_in_stmts(body, rename);
            }
            Stmt::Match { scrutinee, arms } => {
                rename_calls_in_expr(scrutinee, rename);
                for arm in arms.iter_mut() {
                    rename_calls_in_stmts(&mut arm.body, rename);
                }
            }
            Stmt::Try { body, catches } => {
                rename_calls_in_stmts(body, rename);
                for c in catches.iter_mut() {
                    rename_calls_in_stmts(&mut c.body, rename);
                }
            }
            Stmt::Return(None) | Stmt::Propagate | Stmt::Break | Stmt::Continue => {}
        }
    }
}

fn rename_calls_in_expr(e: &mut Expr, rename: &HashMap<String, String>) {
    match e {
        Expr::Ident(name) => {
            if let Some(new) = rename.get(name) {
                *name = new.clone();
            }
        }
        Expr::Call { callee, args, .. } => {
            rename_calls_in_expr(callee, rename);
            for a in args.iter_mut() {
                rename_calls_in_expr(a, rename);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rename_calls_in_expr(lhs, rename);
            rename_calls_in_expr(rhs, rename);
        }
        Expr::Unary { expr, .. } | Expr::Field { base: expr, .. } => {
            rename_calls_in_expr(expr, rename);
        }
        Expr::Index { base, index, .. } => {
            rename_calls_in_expr(base, rename);
            rename_calls_in_expr(index, rename);
        }
        Expr::StructInit { fields, .. } => {
            for (_, fe) in fields.iter_mut() {
                rename_calls_in_expr(fe, rename);
            }
        }
        Expr::ArrayLit(args) => {
            for a in args.iter_mut() {
                rename_calls_in_expr(a, rename);
            }
        }
        Expr::Closure { body, .. } => rename_calls_in_stmts(body, rename),
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::StringLit(_) | Expr::BoolLit(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{CatchArm, Decl, Expr, LoopKind, Stmt, Typ};
    use crate::emit_profile::EmitProfile;

    fn make_fn(name: &str, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.to_string(),
            params: vec![],
            ret: Typ::Void,
            body,
            type_params: vec![],
        }
    }

    fn make_fn_with_params(
        name: &str,
        params: Vec<(&str, Typ)>,
        ret: Typ,
        body: Vec<Stmt>,
    ) -> Decl {
        Decl::Function {
            name: name.to_string(),
            params: params
                .into_iter()
                .map(|(n, t)| (n.to_string(), t))
                .collect(),
            ret,
            body,
            type_params: vec![],
        }
    }

    fn bin(op: &str, lhs: Expr, rhs: Expr) -> Expr {
        Expr::Binary {
            op: op.to_string(),
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    fn unary(op: &str, e: Expr) -> Expr {
        Expr::Unary {
            op: op.to_string(),
            expr: Box::new(e),
        }
    }

    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident(name.to_string())),
            args,
        }
    }

    fn ident(n: &str) -> Expr {
        Expr::Ident(n.to_string())
    }

    // ─── Constant Folding ──────────────────────────────────────────────

    #[test]
    fn fold_add() {
        let e = fold_expr(bin("add", Expr::IntLit(3), Expr::IntLit(4)));
        assert_eq!(e, Expr::IntLit(7));
    }

    #[test]
    fn fold_sub() {
        let e = fold_expr(bin("sub", Expr::IntLit(10), Expr::IntLit(3)));
        assert_eq!(e, Expr::IntLit(7));
    }

    #[test]
    fn fold_mul() {
        let e = fold_expr(bin("mul", Expr::IntLit(6), Expr::IntLit(7)));
        assert_eq!(e, Expr::IntLit(42));
    }

    #[test]
    fn fold_div() {
        let e = fold_expr(bin("div", Expr::IntLit(20), Expr::IntLit(4)));
        assert_eq!(e, Expr::IntLit(5));
    }

    #[test]
    fn fold_div_by_zero_unchanged() {
        let orig = bin("div", Expr::IntLit(10), Expr::IntLit(0));
        let e = fold_expr(orig.clone());
        assert_eq!(e, orig);
    }

    #[test]
    fn fold_mod() {
        let e = fold_expr(bin("mod", Expr::IntLit(17), Expr::IntLit(5)));
        assert_eq!(e, Expr::IntLit(2));
    }

    #[test]
    fn fold_bitwise_ops() {
        assert_eq!(
            fold_expr(bin("band", Expr::IntLit(0xFF), Expr::IntLit(0x0F))),
            Expr::IntLit(0x0F)
        );
        assert_eq!(
            fold_expr(bin("bor", Expr::IntLit(0xF0), Expr::IntLit(0x0F))),
            Expr::IntLit(0xFF)
        );
        assert_eq!(
            fold_expr(bin("xor", Expr::IntLit(0xFF), Expr::IntLit(0xFF))),
            Expr::IntLit(0)
        );
    }

    #[test]
    fn fold_shift() {
        assert_eq!(
            fold_expr(bin("shl", Expr::IntLit(1), Expr::IntLit(4))),
            Expr::IntLit(16)
        );
        assert_eq!(
            fold_expr(bin("shr", Expr::IntLit(16), Expr::IntLit(2))),
            Expr::IntLit(4)
        );
    }

    #[test]
    fn fold_comparison() {
        assert_eq!(
            fold_expr(bin("eq", Expr::IntLit(5), Expr::IntLit(5))),
            Expr::IntLit(1)
        );
        assert_eq!(
            fold_expr(bin("eq", Expr::IntLit(5), Expr::IntLit(3))),
            Expr::IntLit(0)
        );
        assert_eq!(
            fold_expr(bin("neq", Expr::IntLit(1), Expr::IntLit(2))),
            Expr::IntLit(1)
        );
        assert_eq!(
            fold_expr(bin("lt", Expr::IntLit(3), Expr::IntLit(5))),
            Expr::IntLit(1)
        );
        assert_eq!(
            fold_expr(bin("gt", Expr::IntLit(5), Expr::IntLit(3))),
            Expr::IntLit(1)
        );
        assert_eq!(
            fold_expr(bin("le", Expr::IntLit(5), Expr::IntLit(5))),
            Expr::IntLit(1)
        );
        assert_eq!(
            fold_expr(bin("ge", Expr::IntLit(5), Expr::IntLit(5))),
            Expr::IntLit(1)
        );
    }

    #[test]
    fn fold_logical_ops() {
        assert_eq!(
            fold_expr(bin("land", Expr::IntLit(1), Expr::IntLit(1))),
            Expr::IntLit(1)
        );
        assert_eq!(
            fold_expr(bin("land", Expr::IntLit(0), Expr::IntLit(1))),
            Expr::IntLit(0)
        );
        assert_eq!(
            fold_expr(bin("lor", Expr::IntLit(0), Expr::IntLit(1))),
            Expr::IntLit(1)
        );
        assert_eq!(
            fold_expr(bin("lor", Expr::IntLit(0), Expr::IntLit(0))),
            Expr::IntLit(0)
        );
    }

    #[test]
    fn fold_unary_neg() {
        assert_eq!(fold_expr(unary("neg", Expr::IntLit(5))), Expr::IntLit(-5));
    }

    #[test]
    fn fold_unary_not() {
        assert_eq!(fold_expr(unary("not", Expr::IntLit(0))), Expr::IntLit(1));
        assert_eq!(fold_expr(unary("not", Expr::IntLit(42))), Expr::IntLit(0));
    }

    #[test]
    fn fold_non_literal_unchanged() {
        let e = bin("add", ident("x"), Expr::IntLit(1));
        assert_eq!(fold_expr(e.clone()), e);
    }

    // ─── Algebraic Simplification ──────────────────────────────────────

    #[test]
    fn simplify_add_zero_identity() {
        assert_eq!(
            simplify_expr(bin("add", Expr::IntLit(0), ident("x"))),
            ident("x")
        );
        assert_eq!(
            simplify_expr(bin("add", ident("x"), Expr::IntLit(0))),
            ident("x")
        );
    }

    #[test]
    fn simplify_sub_zero() {
        assert_eq!(
            simplify_expr(bin("sub", ident("x"), Expr::IntLit(0))),
            ident("x")
        );
    }

    #[test]
    fn simplify_mul_zero() {
        assert_eq!(
            simplify_expr(bin("mul", Expr::IntLit(0), ident("x"))),
            Expr::IntLit(0)
        );
        assert_eq!(
            simplify_expr(bin("mul", ident("x"), Expr::IntLit(0))),
            Expr::IntLit(0)
        );
    }

    #[test]
    fn simplify_mul_one() {
        assert_eq!(
            simplify_expr(bin("mul", Expr::IntLit(1), ident("x"))),
            ident("x")
        );
        assert_eq!(
            simplify_expr(bin("mul", ident("x"), Expr::IntLit(1))),
            ident("x")
        );
    }

    #[test]
    fn simplify_div_one() {
        assert_eq!(
            simplify_expr(bin("div", ident("x"), Expr::IntLit(1))),
            ident("x")
        );
    }

    #[test]
    fn simplify_band_zero() {
        assert_eq!(
            simplify_expr(bin("band", Expr::IntLit(0), ident("x"))),
            Expr::IntLit(0)
        );
    }

    #[test]
    fn simplify_band_neg1() {
        assert_eq!(
            simplify_expr(bin("band", Expr::IntLit(-1), ident("x"))),
            ident("x")
        );
        assert_eq!(
            simplify_expr(bin("band", ident("x"), Expr::IntLit(-1))),
            ident("x")
        );
    }

    #[test]
    fn simplify_bor_zero() {
        assert_eq!(
            simplify_expr(bin("bor", Expr::IntLit(0), ident("x"))),
            ident("x")
        );
        assert_eq!(
            simplify_expr(bin("bor", ident("x"), Expr::IntLit(0))),
            ident("x")
        );
    }

    #[test]
    fn simplify_xor_zero() {
        assert_eq!(
            simplify_expr(bin("xor", Expr::IntLit(0), ident("x"))),
            ident("x")
        );
        assert_eq!(
            simplify_expr(bin("xor", ident("x"), Expr::IntLit(0))),
            ident("x")
        );
    }

    #[test]
    fn simplify_shl_zero() {
        assert_eq!(
            simplify_expr(bin("shl", ident("x"), Expr::IntLit(0))),
            ident("x")
        );
    }

    #[test]
    fn simplify_shr_zero() {
        assert_eq!(
            simplify_expr(bin("shr", ident("x"), Expr::IntLit(0))),
            ident("x")
        );
    }

    #[test]
    fn simplify_land_short_circuit() {
        assert_eq!(
            simplify_expr(bin("land", Expr::IntLit(0), ident("x"))),
            Expr::IntLit(0)
        );
        assert_eq!(
            simplify_expr(bin("land", ident("x"), Expr::IntLit(0))),
            Expr::IntLit(0)
        );
    }

    #[test]
    fn simplify_land_identity() {
        assert_eq!(
            simplify_expr(bin("land", Expr::IntLit(1), ident("x"))),
            ident("x")
        );
        assert_eq!(
            simplify_expr(bin("land", ident("x"), Expr::IntLit(1))),
            ident("x")
        );
    }

    #[test]
    fn simplify_lor_short_circuit() {
        assert_eq!(
            simplify_expr(bin("lor", Expr::IntLit(1), ident("x"))),
            Expr::IntLit(1)
        );
        assert_eq!(
            simplify_expr(bin("lor", ident("x"), Expr::IntLit(1))),
            Expr::IntLit(1)
        );
    }

    #[test]
    fn simplify_lor_identity() {
        assert_eq!(
            simplify_expr(bin("lor", Expr::IntLit(0), ident("x"))),
            ident("x")
        );
        assert_eq!(
            simplify_expr(bin("lor", ident("x"), Expr::IntLit(0))),
            ident("x")
        );
    }

    #[test]
    fn simplify_double_neg() {
        let e = unary("neg", unary("neg", ident("x")));
        assert_eq!(simplify_expr(e), ident("x"));
    }

    #[test]
    fn simplify_double_not() {
        let e = unary("not", unary("not", ident("x")));
        assert_eq!(simplify_expr(e), ident("x"));
    }

    // ─── Dead Code Elimination ──────────────────────────────────────────

    #[test]
    fn dce_removes_unused_let() {
        let mut body = vec![
            Stmt::Let("unused".into(), Some(Typ::Int), Expr::IntLit(42)),
            Stmt::Return(Some(Expr::IntLit(0))),
        ];
        dce_body(&mut body);
        assert_eq!(body.len(), 1);
        assert!(matches!(&body[0], Stmt::Return(Some(Expr::IntLit(0)))));
    }

    #[test]
    fn dce_keeps_used_let() {
        let mut body = vec![
            Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(42)),
            Stmt::Return(Some(ident("x"))),
        ];
        dce_body(&mut body);
        assert_eq!(body.len(), 2);
    }

    #[test]
    fn dce_keeps_side_effecting_call_let() {
        // An unused `let` whose RHS is a call must NOT be removed: the call has
        // observable side effects (serial I/O, MMIO, allocation).
        let mut body = vec![
            Stmt::Let(
                "unused".into(),
                Some(Typ::Int),
                call("serial-put", vec![ident("port"), Expr::IntLit(90)]),
            ),
            Stmt::Return(None),
        ];
        dce_body(&mut body);
        assert_eq!(body.len(), 2, "side-effecting call let must be preserved");
    }

    #[test]
    fn dce_collapses_duplicate_void_returns() {
        let mut body = vec![Stmt::Return(None), Stmt::Return(None), Stmt::Return(None)];
        dce_body(&mut body);
        assert_eq!(body.len(), 1);
    }

    // ─── Constant Propagation ──────────────────────────────────────────

    #[test]
    fn propagate_single_use_constant() {
        let mut body = vec![
            Stmt::Let("c".into(), Some(Typ::Int), Expr::IntLit(99)),
            Stmt::Return(Some(ident("c"))),
        ];
        propagate_in_body(&mut body);
        assert_eq!(body.len(), 1);
        assert!(matches!(&body[0], Stmt::Return(Some(Expr::IntLit(99)))));
    }

    #[test]
    fn propagate_skips_loop_assigned() {
        let mut body = vec![
            Stmt::Let("i".into(), Some(Typ::Int), Expr::IntLit(0)),
            Stmt::Loop {
                kind: LoopKind::While,
                cond: Some(bin("<", ident("i"), Expr::IntLit(5))),
                body: vec![Stmt::Assign(
                    "i".into(),
                    bin("+", ident("i"), Expr::IntLit(1)),
                )],
            },
            Stmt::Return(Some(ident("i"))),
        ];
        propagate_in_body(&mut body);
        assert!(matches!(&body[0], Stmt::Let(n, _, Expr::IntLit(0)) if n == "i"));
        assert!(
            matches!(&body[1], Stmt::Loop { cond: Some(Expr::Binary { lhs, .. }), .. }
            if matches!(lhs.as_ref(), Expr::Ident(n) if n == "i"))
        );
    }

    #[test]
    fn propagate_multi_use_literal() {
        let mut body = vec![
            Stmt::Let("c".into(), Some(Typ::Int), Expr::IntLit(5)),
            Stmt::Expr(bin("add", ident("c"), ident("c"))),
        ];
        propagate_in_body(&mut body);
        assert_eq!(body.len(), 1);
        assert!(matches!(
            &body[0],
            Stmt::Expr(Expr::Binary { lhs, rhs, .. })
                if matches!(lhs.as_ref(), Expr::IntLit(5)) && matches!(rhs.as_ref(), Expr::IntLit(5))
        ));
    }

    #[test]
    fn fold_const_array_index() {
        let e = fold_expr(Expr::Index {
            base: Box::new(Expr::ArrayLit(vec![
                Expr::IntLit(1),
                Expr::IntLit(2),
                Expr::IntLit(3),
            ])),
            index: Box::new(Expr::IntLit(2)),
        });
        assert_eq!(e, Expr::IntLit(3));
    }

    #[test]
    fn fold_float_add() {
        let e = fold_expr(bin(
            "+",
            Expr::FloatLit(crate::core_ir::FloatVal(2.5)),
            Expr::FloatLit(crate::core_ir::FloatVal(3.5)),
        ));
        assert_eq!(e, Expr::FloatLit(crate::core_ir::FloatVal(6.0)));
    }

    // ─── Dead Function Elimination ──────────────────────────────────────

    #[test]
    fn dce_follows_alias_wrappers() {
        let mut decls = vec![
            Decl::Function {
                name: "helper".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::If {
                    cond: Expr::BoolLit(true),
                    then_body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
                    else_body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                }],
                type_params: vec![],
            },
            Decl::Function {
                name: "alias".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::If {
                    cond: Expr::BoolLit(true),
                    then_body: vec![Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".into())),
                        args: vec![],
                    }))],
                    else_body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                }],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("alias".into())),
                    args: vec![],
                }))],
                type_params: vec![],
            },
        ];
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Default);
        let names: Vec<_> = decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            names.contains(&"helper"),
            "alias must keep helper: {names:?}"
        );
        assert!(names.contains(&"alias"), "{names:?}");
    }

    #[test]
    fn static_lib_keeps_unreferenced_functions() {
        // Static-lib linkage: every function is a potential export resolved by
        // assembly capsules or sibling objects (e.g. IRQ handlers called only
        // from asm). Function-level removal must not run.
        let mut decls = vec![
            make_fn("subspace_main", vec![Stmt::Return(Some(Expr::IntLit(0)))]),
            make_fn("subspace_systick_handler", vec![Stmt::Return(None)]),
        ];
        optimize_with_linkage(
            &mut decls,
            Some("subspace_main"),
            EmitProfile::Default,
            true,
        );
        let names: Vec<&str> = decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            names.contains(&"subspace_systick_handler"),
            "static-lib must keep asm-called handlers: {names:?}"
        );
    }

    #[test]
    fn inline_keeps_calls_to_extern_bindings() {
        // Extern bindings (asm/zig/rust) lower as Decl::Function with an empty
        // body. Inlining an empty body deletes the call, silently dropping
        // side effects (MMIO stores, context switches). A statement-level call
        // to an empty-bodied extern must survive optimization.
        let mut extern_touch = make_fn("ext_touch", vec![]);
        if let Decl::Function { params, .. } = &mut extern_touch {
            *params = vec![("x".to_string(), Typ::Int)];
        }
        let mut decls = vec![
            make_fn(
                "kernel_entry",
                vec![Stmt::Expr(call("ext_touch", vec![ident("x")]))],
            ),
            extern_touch,
            make_fn(
                "uses_result",
                vec![
                    Stmt::Let(
                        "v".to_string(),
                        Some(Typ::Int),
                        call("ext_value", vec![Expr::IntLit(3)]),
                    ),
                    Stmt::Return(Some(ident("v"))),
                ],
            ),
            make_fn_with_params("ext_value", vec![("x", Typ::Int)], Typ::Int, vec![]),
        ];
        optimize_with_linkage(
            &mut decls,
            Some("kernel_entry"),
            EmitProfile::Default,
            false,
        );
        let entry = decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "kernel_entry" => Some(body.clone()),
                _ => None,
            })
            .expect("kernel_entry");
        let has_extern_call = entry.iter().any(|s| match s {
            Stmt::Expr(e) => matches!(e, Expr::Call { callee, .. }
                if matches!(callee.as_ref(), Expr::Ident(n) if n == "ext_touch")),
            _ => false,
        });
        assert!(
            has_extern_call,
            "optimizer must keep statement calls to extern bindings: {entry:?}"
        );
    }

    #[test]
    fn remove_dead_functions_keeps_called() {
        let mut decls = vec![
            make_fn("kernel_entry", vec![Stmt::Expr(call("helper", vec![]))]),
            make_fn("helper", vec![Stmt::Return(None)]),
            make_fn("unused", vec![Stmt::Return(None)]),
        ];
        remove_dead_functions(&mut decls, None);
        let names: Vec<&str> = decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"kernel_entry"));
        assert!(names.contains(&"helper"));
        assert!(!names.contains(&"unused"));
    }

    #[test]
    fn remove_dead_functions_keeps_entry() {
        let mut decls = vec![
            make_fn("kernel_entry", vec![Stmt::Return(None)]),
            make_fn("dead", vec![Stmt::Return(None)]),
        ];
        remove_dead_functions(&mut decls, Some("kernel_entry"));
        assert_eq!(decls.len(), 1);
    }

    // ─── Inlining ──────────────────────────────────────────────────────

    #[test]
    fn inline_small_return_function() {
        let mut decls = vec![
            make_fn_with_params(
                "small",
                vec![],
                Typ::Int,
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            make_fn(
                "kernel_entry",
                vec![
                    Stmt::Let("x".into(), Some(Typ::Int), call("small", vec![])),
                    Stmt::Return(Some(ident("x"))),
                ],
            ),
        ];
        inline_small_functions(&mut decls);
        if let Decl::Function { body, .. } = &decls[1] {
            if let Stmt::Let(_, _, expr) = &body[0] {
                assert_eq!(*expr, Expr::IntLit(42));
            }
        }
    }

    #[test]
    fn inline_skips_large_functions() {
        let large_body: Vec<Stmt> = (0..10).map(|i| Stmt::Expr(Expr::IntLit(i))).collect();
        let mut decls = vec![
            make_fn("big", large_body),
            make_fn(
                "kernel_entry",
                vec![Stmt::Expr(call("big", vec![])), Stmt::Return(None)],
            ),
        ];
        let before = decls[1].clone();
        inline_small_functions(&mut decls);
        assert_eq!(decls[1], before);
    }

    // ─── Full Optimize Pipeline ────────────────────────────────────────

    #[test]
    fn optimize_folds_and_propagates() {
        let mut decls = vec![make_fn(
            "kernel_entry",
            vec![
                Stmt::Let("a".into(), Some(Typ::Int), Expr::IntLit(99)),
                Stmt::Return(Some(ident("a"))),
            ],
        )];
        optimize(&mut decls);
        if let Decl::Function { body, .. } = &decls[0] {
            assert_eq!(body.len(), 1);
            assert!(matches!(&body[0], Stmt::Return(Some(Expr::IntLit(99)))));
        }
    }

    #[test]
    fn optimize_removes_dead_code() {
        let mut decls = vec![make_fn(
            "kernel_entry",
            vec![
                Stmt::Let("dead".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Return(Some(Expr::IntLit(1))),
            ],
        )];
        optimize(&mut decls);
        if let Decl::Function { body, .. } = &decls[0] {
            assert_eq!(body.len(), 1);
        }
    }

    // ─── has_cf ────────────────────────────────────────────────────────

    #[test]
    fn has_cf_detects_if() {
        let stmts = vec![Stmt::If {
            cond: Expr::BoolLit(true),
            then_body: vec![],
            else_body: vec![],
        }];
        assert!(has_cf(&stmts));
    }

    #[test]
    fn has_cf_detects_loop() {
        let stmts = vec![Stmt::Loop {
            kind: LoopKind::While,
            cond: Some(Expr::BoolLit(true)),
            body: vec![],
        }];
        assert!(has_cf(&stmts));
    }

    #[test]
    fn has_cf_false_for_flat_code() {
        let stmts = vec![Stmt::Return(None)];
        assert!(!has_cf(&stmts));
    }

    #[test]
    fn has_cf_detects_throw_and_try() {
        assert!(has_cf(&[Stmt::Throw(Expr::IntLit(1))]));
        assert!(has_cf(&[Stmt::Try {
            body: vec![],
            catches: vec![],
        }]));
    }

    #[test]
    fn remove_dead_keeps_callees_inside_try() {
        let mut decls = vec![
            Decl::Function {
                name: "inner".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Throw(Expr::StringLit("nope".into()))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Try {
                    body: vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("inner".into())),
                        args: vec![],
                    })],
                    catches: vec![CatchArm {
                        pattern: "e".into(),
                        body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
                    }],
                }],
                type_params: vec![],
            },
        ];
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Default);
        assert!(
            decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "inner")),
            "inner must survive DFE when only called from inside try"
        );
    }

    // ─── x86_64 Peephole ──────────────────────────────────────────────

    #[test]
    fn peephole_removes_trailing_nop() {
        let mut code = vec![0xC3, 0x90]; // ret, nop
        peephole_x86_64(&mut code);
        assert_eq!(code, vec![0xC3]);
    }

    #[test]
    fn peephole_removes_two_byte_nops() {
        let mut code = vec![0xC3, 0x66, 0x90]; // ret, 2-byte nop
        peephole_x86_64(&mut code);
        assert_eq!(code, vec![0xC3]);
    }

    #[test]
    fn peephole_empty_code() {
        let mut code: Vec<u8> = vec![];
        peephole_x86_64(&mut code);
        assert!(code.is_empty());
    }

    #[test]
    fn peephole_no_change_when_clean() {
        let mut code = vec![0xC3]; // just ret
        peephole_x86_64(&mut code);
        assert_eq!(code, vec![0xC3]);
    }

    // ─── x86_64 Instruction Length ─────────────────────────────────────

    #[test]
    fn insn_length_ret() {
        assert_eq!(x86_64_insn_length(&[0xC3], 0), 1);
    }

    #[test]
    fn insn_length_nop() {
        assert_eq!(x86_64_insn_length(&[0x90], 0), 1);
    }

    #[test]
    fn insn_length_push_r64() {
        assert_eq!(x86_64_insn_length(&[0x50], 0), 1); // push rax
        assert_eq!(x86_64_insn_length(&[0x55], 0), 1); // push rbp
    }

    #[test]
    fn insn_length_pop_r64() {
        assert_eq!(x86_64_insn_length(&[0x58], 0), 1); // pop rax
        assert_eq!(x86_64_insn_length(&[0x5D], 0), 1); // pop rbp
    }

    #[test]
    fn insn_length_int3() {
        // CC = int3, single byte
        assert_eq!(x86_64_insn_length(&[0xCC], 0), 1);
    }

    #[test]
    fn insn_length_hlt() {
        // F4 = hlt, single byte
        assert_eq!(x86_64_insn_length(&[0xF4], 0), 1);
    }

    // ─── detect_ptr_refs ──────────────────────────────────────────────

    #[test]
    fn detect_ptr_refs_finds_invoke_targets() {
        let decls = vec![make_fn(
            "main",
            vec![Stmt::Expr(call("invoke", vec![ident("target_fn")]))],
        )];
        let mut refs = Vec::new();
        detect_ptr_refs(&decls, &mut refs);
        assert!(refs.contains(&"target_fn".to_string()));
    }

    #[test]
    fn detect_ptr_refs_finds_arg_idents() {
        let decls = vec![make_fn(
            "main",
            vec![Stmt::Expr(call("foo", vec![ident("bar")]))],
        )];
        let mut refs = Vec::new();
        detect_ptr_refs(&decls, &mut refs);
        assert!(refs.contains(&"bar".to_string()));
    }

    #[test]
    fn algebraic_simplify_double_negation_and_self_identities() {
        let double_neg = simplify_expr(Expr::Unary {
            op: "!".into(),
            expr: Box::new(Expr::Unary {
                op: "!".into(),
                expr: Box::new(ident("x")),
            }),
        });
        assert_eq!(double_neg, ident("x"));

        let double_minus = simplify_expr(Expr::Unary {
            op: "-".into(),
            expr: Box::new(Expr::Unary {
                op: "-".into(),
                expr: Box::new(ident("n")),
            }),
        });
        assert_eq!(double_minus, ident("n"));

        let self_xor = simplify_expr(Expr::Binary {
            op: "^".into(),
            lhs: Box::new(ident("a")),
            rhs: Box::new(ident("a")),
        });
        assert_eq!(self_xor, Expr::IntLit(0));

        let self_sub = simplify_expr(Expr::Binary {
            op: "-".into(),
            lhs: Box::new(ident("a")),
            rhs: Box::new(ident("a")),
        });
        assert_eq!(self_sub, Expr::IntLit(0));

        let self_eq = simplify_expr(Expr::Binary {
            op: "==".into(),
            lhs: Box::new(ident("a")),
            rhs: Box::new(ident("a")),
        });
        assert_eq!(self_eq, Expr::BoolLit(true));
    }

    #[test]
    fn simplify_arithmetic_constant_folding() {
        assert_eq!(
            simplify_expr(bin("+", Expr::IntLit(2), Expr::IntLit(3))),
            Expr::IntLit(5)
        );
        assert_eq!(
            simplify_expr(bin("-", Expr::IntLit(10), Expr::IntLit(3))),
            Expr::IntLit(7)
        );
        assert_eq!(
            simplify_expr(bin("*", Expr::IntLit(4), Expr::IntLit(5))),
            Expr::IntLit(20)
        );
        // Overflow must not fold (i64::MAX + 1 would panic/wrap otherwise).
        assert_eq!(
            simplify_expr(bin("+", Expr::IntLit(i64::MAX), Expr::IntLit(1))),
            bin("+", Expr::IntLit(i64::MAX), Expr::IntLit(1))
        );
        assert_eq!(
            simplify_expr(bin("/", Expr::IntLit(i64::MIN), Expr::IntLit(-1))),
            bin("/", Expr::IntLit(i64::MIN), Expr::IntLit(-1))
        );
    }

    #[test]
    fn simplify_boolean() {
        assert_eq!(
            simplify_expr(bin("&&", ident("x"), Expr::BoolLit(true))),
            ident("x")
        );
    }

    #[test]
    fn simplify_multiplication_identity_explicit() {
        assert_eq!(
            simplify_expr(bin("*", ident("x"), Expr::IntLit(1))),
            ident("x")
        );
    }

    #[test]
    fn harden_renames_internal_symbols() {
        // Use an if so the inliner will not erase `helper` before rename.
        let mut decls = vec![
            Decl::Function {
                name: "helper".into(),
                params: vec![("n".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::If {
                    cond: Expr::Binary {
                        op: ">".into(),
                        lhs: Box::new(Expr::Ident("n".into())),
                        rhs: Box::new(Expr::IntLit(0)),
                    },
                    then_body: vec![Stmt::Return(Some(Expr::Ident("n".into())))],
                    else_body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                }],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![Expr::IntLit(3)],
                }))],
                type_params: vec![],
            },
        ];
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Harden);
        let names: Vec<_> = decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"main"));
        assert!(names.iter().any(|n| n.starts_with("_H")));
        assert!(!names.contains(&"helper"));
    }

    fn straight_line_sum_decls() -> Vec<Decl> {
        vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![
                Stmt::Let("a".into(), Some(Typ::Int), Expr::IntLit(1)),
                Stmt::Let("b".into(), Some(Typ::Int), Expr::IntLit(2)),
                Stmt::Let(
                    "c".into(),
                    Some(Typ::Int),
                    Expr::Binary {
                        op: "+".into(),
                        lhs: Box::new(Expr::Ident("a".into())),
                        rhs: Box::new(Expr::Ident("b".into())),
                    },
                ),
                Stmt::Return(Some(Expr::Ident("c".into()))),
            ],
            type_params: vec![],
        }]
    }

    fn walk_has_pc_dispatch(stmts: &[Stmt]) -> (bool, bool) {
        let mut has_loop = false;
        let mut has_pc = false;
        for s in stmts {
            match s {
                Stmt::Let(name, ..) if name == "_pc" => has_pc = true,
                Stmt::Assign(name, _) if name == "_pc" => has_pc = true,
                Stmt::Loop { body, .. } => {
                    has_loop = true;
                    let (l, p) = walk_has_pc_dispatch(body);
                    has_loop |= l;
                    has_pc |= p;
                }
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    let (l1, p1) = walk_has_pc_dispatch(then_body);
                    let (l2, p2) = walk_has_pc_dispatch(else_body);
                    has_loop |= l1 | l2;
                    has_pc |= p1 | p2;
                }
                _ => {}
            }
        }
        (has_loop, has_pc)
    }

    fn function_body(decls: &[Decl]) -> &[Stmt] {
        match &decls[0] {
            Decl::Function { body, .. } => body,
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn harden_cfg_dispatch_wraps_straight_line() {
        let mut decls = straight_line_sum_decls();
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Harden);
        let body = function_body(&decls);
        let (has_loop, has_pc) = walk_has_pc_dispatch(body);
        assert!(
            has_loop,
            "expected dispatcher loop in harden body: {body:?}"
        );
        assert!(has_pc, "expected _pc state var in harden body: {body:?}");
    }

    #[test]
    fn runtime_profile_has_no_cfg_pc_dispatch() {
        for profile in [EmitProfile::Default, EmitProfile::Lean] {
            let mut decls = straight_line_sum_decls();
            optimize_with_profile(&mut decls, Some("main"), profile);
            let body = function_body(&decls);
            let (_has_loop, has_pc) = walk_has_pc_dispatch(body);
            assert!(
                !has_pc,
                "{profile} runtime profile must not insert CFG _pc dispatch: {body:?}"
            );
        }
    }

    #[test]
    fn harden_profile_keeps_cfg_pc_dispatch() {
        let mut decls = straight_line_sum_decls();
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Harden);
        let body = function_body(&decls);
        let (has_loop, has_pc) = walk_has_pc_dispatch(body);
        assert!(
            has_loop && has_pc,
            "harden profile must keep CFG _pc dispatch: {body:?}"
        );
    }

    #[test]
    fn remove_dead_functions_keeps_main_without_explicit_entry() {
        let mut decls = vec![
            Decl::Function {
                name: "unused".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Return(None)],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Return(None)],
                type_params: vec![],
            },
        ];
        optimize_with_profile(&mut decls, None, EmitProfile::Default);
        let names: Vec<_> = decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            names.contains(&"main"),
            "implicit JIT entry `main` must survive DCE: {names:?}"
        );
        assert!(
            !names.contains(&"unused"),
            "uncalled helpers should still be removed: {names:?}"
        );
    }

    #[test]
    fn lean_inlines_larger_helpers() {
        // Body of 7 stmts exceeds default threshold (6) but fits lean (12).
        let helper_body = vec![
            Stmt::Let("a".into(), None, Expr::IntLit(1)),
            Stmt::Let("b".into(), None, Expr::IntLit(2)),
            Stmt::Let("c".into(), None, Expr::IntLit(3)),
            Stmt::Let("d".into(), None, Expr::IntLit(4)),
            Stmt::Let("e".into(), None, Expr::IntLit(5)),
            Stmt::Let("f".into(), None, Expr::IntLit(6)),
            Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::Ident("a".into())),
                rhs: Box::new(Expr::Ident("b".into())),
            })),
        ];
        let mut decls = vec![
            Decl::Function {
                name: "helper".into(),
                params: vec![],
                ret: Typ::Int,
                body: helper_body,
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![],
                }))],
                type_params: vec![],
            },
        ];
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Lean);
        let main = decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .unwrap();
        let src = format!("{main:?}");
        assert!(
            !src.contains("Ident(\"helper\")"),
            "lean should inline helper into main: {src}"
        );
    }

    #[test]
    fn default_inlines_medium_helpers() {
        let helper_body = vec![
            Stmt::Let("a".into(), None, Expr::IntLit(1)),
            Stmt::Let("b".into(), None, Expr::IntLit(2)),
            Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::Ident("a".into())),
                rhs: Box::new(Expr::Ident("b".into())),
            })),
        ];
        let mut decls = vec![
            Decl::Function {
                name: "helper".into(),
                params: vec![],
                ret: Typ::Int,
                body: helper_body,
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![],
                }))],
                type_params: vec![],
            },
        ];
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Default);
        let main = decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .unwrap();
        let src = format!("{main:?}");
        assert!(
            !src.contains("Ident(\"helper\")"),
            "default should inline 3-stmt helper: {src}"
        );
    }

    #[test]
    fn harden_mba_rewrites_add() {
        let mut decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::Ident("x".into())),
                rhs: Box::new(Expr::Ident("y".into())),
            }))],
            type_params: vec![],
        }];
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Harden);
        let src = format!("{:?}", function_body(&decls));
        assert!(
            src.contains("\"^\"") && src.contains("\"&\""),
            "harden MBA should rewrite x+y into xor/and form: {src}"
        );
    }
}
