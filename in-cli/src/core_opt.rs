//! Core IR optimization passes.
//!
//! These run before lowering. All frontends / backends benefit.
//! Order: inlining → constant folding → constant propagation → DCE.
//! Profile-specific passes (lean / harden) layer on top via
//! [`optimize_with_profile`].

use crate::core_ir::{CatchArm, Decl, Expr, LoopKind, MatchArm, Stmt, Typ};
use crate::emit_profile::EmitProfile;
use crate::integer_semantics::IntegerSemantics;
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
    optimize_with_linkage_and_semantics(
        decls,
        entry,
        profile,
        keep_all_functions,
        IntegerSemantics::Word64,
    );
}

pub fn optimize_with_linkage_and_semantics(
    decls: &mut Vec<Decl>,
    entry: Option<&str>,
    profile: EmitProfile,
    keep_all_functions: bool,
    semantics: IntegerSemantics,
) {
    optimize_with_linkage_and_semantics_and_normalizer(
        decls,
        entry,
        profile,
        keep_all_functions,
        semantics,
        None,
    );
}

/// Normalize frontend-owned operations after substitution and before folding or
/// reachability removes their potential method targets. Ordinary callers use None.
pub fn optimize_with_linkage_and_semantics_and_normalizer(
    decls: &mut Vec<Decl>,
    entry: Option<&str>,
    profile: EmitProfile,
    keep_all_functions: bool,
    semantics: IntegerSemantics,
    mut normalize_after_inline: Option<&mut dyn FnMut(&mut Vec<Decl>)>,
) {
    crate::lower_core::lower_guarded_decls(decls);
    crate::lower_core::lower_named_constants_with_semantics(decls, semantics);
    crate::lower_core::lower_value_spills(decls);
    crate::core_bindings::normalize_decls(decls);
    match profile {
        EmitProfile::Default => {
            // Fast owned path: more inlining than a textbook compiler, then fold/DCE.
            inline_small_functions_with(decls, INLINE_THRESHOLD, DEFAULT_INLINE_DEPTH);
            inline_small_functions_with(decls, INLINE_THRESHOLD, DEFAULT_INLINE_DEPTH);
            if let Some(normalize) = normalize_after_inline.as_mut() {
                normalize(decls);
            }
            algebraic_simplify(decls, semantics);
            fold_constants_in_decls(decls, semantics);
            propagate_constants(decls);
            fold_constants_in_decls(decls, semantics);
            dead_code_eliminate(decls);
            if !keep_all_functions {
                remove_dead_functions(decls, entry);
            }
        }
        EmitProfile::Lean => {
            // Aggressive inlining + deeper recursion, then standard cleanup.
            inline_small_functions_with(decls, LEAN_INLINE_THRESHOLD, LEAN_INLINE_DEPTH);
            inline_small_functions_with(decls, LEAN_INLINE_THRESHOLD, LEAN_INLINE_DEPTH);
            if let Some(normalize) = normalize_after_inline.as_mut() {
                normalize(decls);
            }
            algebraic_simplify(decls, semantics);
            fold_constants_in_decls(decls, semantics);
            propagate_constants(decls);
            fold_constants_in_decls(decls, semantics);
            dead_code_eliminate(decls);
            if !keep_all_functions {
                remove_dead_functions(decls, entry);
            }
        }
        EmitProfile::Harden => {
            // Normal opts first so harden noise is not immediately folded away.
            inline_small_functions_with(decls, INLINE_THRESHOLD, DEFAULT_INLINE_DEPTH);
            if let Some(normalize) = normalize_after_inline.as_mut() {
                normalize(decls);
            }
            algebraic_simplify(decls, semantics);
            fold_constants_in_decls(decls, semantics);
            propagate_constants(decls);
            fold_constants_in_decls(decls, semantics);
            dead_code_eliminate(decls);
            if !keep_all_functions {
                remove_dead_functions(decls, entry);
            }
            // Preserve completion before opaque predicates hide feasible paths.
            crate::core_ir_verifier::preserve_nonvoid_completion(decls);
            // Anti-decomp transforms (must run after fold/dce).
            harden_mba_arithmetic(decls);
            harden_obscure_literals(decls, semantics);
            harden_opaque_predicates(decls);
            harden_bogus_blocks(decls);
            // Flatten eligible bodies into a pc dispatcher before junk pads
            // so each state still receives junk noise afterward at the outer level.
            harden_cfg_dispatch_lite(decls);
            harden_junk_stmts(decls);
            // Keep names stable when every retained function is a potential export.
            if !keep_all_functions {
                harden_hash_symbols(decls, entry);
            }
        }
    }
}

/// Qualify frontend constant references after lexical binding normalization.
pub(crate) fn rewrite_constant_references(decls: &mut [Decl], names: &HashMap<String, String>) {
    let mut rewrite = |expr: &mut Expr| {
        if let Expr::Ident(name) = expr
            && let Some(qualified) = names.get(name)
        {
            *name = qualified.clone();
        }
    };
    map_decl_exprs_mut(decls, &mut rewrite);
}

pub(crate) fn map_decl_exprs_mut(decls: &mut [Decl], rewrite: &mut impl FnMut(&mut Expr)) {
    for decl in decls {
        match decl {
            Decl::Function { body, .. } => {
                for stmt in body {
                    map_stmt_mut(stmt, rewrite);
                }
            }
            Decl::Global {
                init: Some(expr), ..
            } => map_expr_mut(expr, rewrite),
            Decl::Class { methods, .. } => map_decl_exprs_mut(methods, rewrite),
            _ => {}
        }
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn fn_bodies_mut(decls: &mut [Decl]) -> impl Iterator<Item = &mut Vec<Stmt>> {
    let mut protected: HashSet<_> = crate::typecheck::TypeChecker::new()
        .typed_proof_functions(decls)
        .into_keys()
        .collect();
    protected.extend(crate::core_ir_verifier::initialization_errors_by_function(decls).into_keys());
    decls.iter_mut().filter_map(move |d| match d {
        Decl::Function { name, body, .. } if !protected.contains(name) => Some(body),
        _ => None,
    })
}

fn walk_expr<F: FnMut(&Expr)>(e: &Expr, f: &mut F) {
    f(e);
    if let Expr::Block { body, .. } = e {
        for stmt in body {
            let mut stmt = stmt.clone();
            map_stmt_mut(&mut stmt, &mut |expr| f(expr));
        }
    }
    crate::core_ir::for_each_expr_child(e, &mut |child| walk_expr(child, f));
}

/// Whether evaluation can observe effects or leave the current function.
/// Checked indexing, division and remainder can return early on failure, just as
/// calls and structured values can carry effects or nonlocal exits. Retain their
/// evaluation when removing bindings or substituting function bodies.
pub(crate) fn expr_has_effects(e: &Expr) -> bool {
    let mut has = false;
    walk_expr(e, &mut |n| {
        // Cloning allocates; substitution must not duplicate its storage or evaluation.
        if matches!(
            n,
            Expr::Call { .. }
                | Expr::Index { .. }
                | Expr::Block { .. }
                | Expr::Conditional { .. }
                | Expr::MatchValue { .. }
        ) || matches!(n, Expr::Unary {op, ..} if op == "vector-clone" || crate::core_ir::array_copy::count(op).is_some())
            || matches!(n, Expr::Binary {op, ..} if matches!(op.as_str(), "/" | "%" | "div" | "mod" | "udiv" | "urem"))
        {
            has = true;
        }
    });
    has
}

pub(crate) fn map_expr_mut<F: FnMut(&mut Expr)>(e: &mut Expr, f: &mut F) {
    match e {
        Expr::MatchValue { scrutinee, arms } => {
            map_expr_mut(scrutinee, f);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    map_expr_mut(guard, f);
                }
                map_expr_mut(&mut arm.value, f);
            }
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            map_expr_mut(cond, f);
            map_expr_mut(then_expr, f);
            map_expr_mut(else_expr, f);
        }
        Expr::Block { body, result } => {
            for stmt in body {
                map_stmt_mut(stmt, f);
            }
            if let Some(result) = result {
                map_expr_mut(result, f);
            }
        }
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

pub(crate) fn map_stmt_exprs_mut(stmt: &mut Stmt, rewrite: &mut impl FnMut(&mut Expr)) {
    map_stmt_mut(stmt, rewrite);
}

fn map_stmt_mut<F: FnMut(&mut Expr)>(s: &mut Stmt, f: &mut F) {
    match s {
        Stmt::Declare(_, _) => {}
        Stmt::Let(_, _, e) => map_expr_mut(e, f),
        Stmt::Assign(_, e) => map_expr_mut(e, f),
        Stmt::FieldAssign { base, value, .. } => {
            map_expr_mut(base, f);
            map_expr_mut(value, f);
        }
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
                if let Some(guard) = &mut arm.guard {
                    map_expr_mut(guard, f);
                }
                for s in arm.body.iter_mut() {
                    map_stmt_mut(s, f);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn map_expr<F: FnMut(Expr) -> Expr>(e: Expr, f: &mut F) -> Expr {
    let mapped = match e {
        Expr::MatchValue { scrutinee, arms } => Expr::MatchValue {
            scrutinee: Box::new(map_expr(*scrutinee, f)),
            arms: arms
                .into_iter()
                .map(|mut arm| {
                    arm.guard = arm.guard.map(|guard| map_expr(guard, f));
                    arm.value = map_expr(arm.value, f);
                    arm
                })
                .collect(),
        },
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => Expr::Conditional {
            cond: Box::new(map_expr(*cond, f)),
            then_expr: Box::new(map_expr(*then_expr, f)),
            else_expr: Box::new(map_expr(*else_expr, f)),
        },
        Expr::Block { body, result } => Expr::Block {
            body: body.into_iter().map(|s| map_stmt(s, f)).collect(),
            result: result.map(|e| Box::new(map_expr(*e, f))),
        },
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

fn map_stmt<F: FnMut(Expr) -> Expr>(s: Stmt, f: &mut F) -> Stmt {
    match s {
        Stmt::Declare(n, t) => Stmt::Declare(n, t),
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
                    guard: a.guard.map(|guard| map_expr(guard, f)),
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

/// A bounded extra pass for frontend operations exposed by substitution.
/// Ordinary optimizer callers never use this path.
pub(crate) fn inline_for_frontend_normalization(decls: &mut [Decl], profile: EmitProfile) -> bool {
    use std::hash::{Hash, Hasher};
    fn fingerprint(decls: &[Decl]) -> u64 {
        let mut state = std::collections::hash_map::DefaultHasher::new();
        decls.hash(&mut state);
        state.finish()
    }
    let before = fingerprint(decls);
    let (threshold, depth) = match profile {
        EmitProfile::Lean => (LEAN_INLINE_THRESHOLD, LEAN_INLINE_DEPTH),
        EmitProfile::Default | EmitProfile::Harden => (INLINE_THRESHOLD, DEFAULT_INLINE_DEPTH),
    };
    inline_small_functions_with(decls, threshold, depth);
    fingerprint(decls) != before
}

pub(crate) fn frontend_inline_round_limit(profile: EmitProfile) -> u32 {
    match profile {
        EmitProfile::Lean => LEAN_INLINE_DEPTH,
        EmitProfile::Default | EmitProfile::Harden => DEFAULT_INLINE_DEPTH,
    }
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

    // An inlined body must not erase an invalid/unsupported source type.
    // This applies to pure and zero-argument calls as well as snapshots.
    let typed_proofs = crate::typecheck::TypeChecker::new().typed_proof_functions(decls);
    let initialization_errors = crate::core_ir_verifier::initialization_errors_by_function(decls);
    let candidates: Vec<&String> = functions
        .iter()
        .filter(|(n, d)| {
            matches!(
                d,
                Decl::Function { body, ret, .. }
                    // An empty body is an extern binding (asm/zig/rust), not a
                    // function to inline: "inlining" it would delete the call
                    // to the external symbol, silently dropping MMIO writes,
                    // context switches, and other side effects.
                    if !body.is_empty()
                        // Substitution loses the vector return type and its storage ABI.
                        && !matches!(ret, Typ::Vector(_))
                        && body.len() <= threshold
                        && !ptr_refs.contains(n)
                        && !has_cf(body)
                        && !typed_proofs.contains_key(n.as_str())
                        && !initialization_errors.contains_key(n.as_str())
            )
        })
        .map(|(n, _)| n)
        .collect();
    if candidates.is_empty() {
        return;
    }

    let mut names = InlineArgumentNames::new(decls);
    // Compute scoped caller/callee eligibility once per inline pass, only when
    // this pass can use the new effectful argument path.
    let mut may_snapshot = false;
    map_decl_exprs_mut(decls, &mut |expr| {
        if !may_snapshot
            && let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if candidates.contains(&name))
            && args.iter().any(expr_has_effects)
        {
            may_snapshot = true;
        }
    });
    if may_snapshot {
        names.checked_functions =
            crate::typecheck::TypeChecker::new().checked_inline_functions(decls);
    }
    for decl in decls.iter_mut() {
        if let Decl::Function { name, body, .. } = decl {
            names.caller_checked = names.checked_functions.contains(name);
            if initialization_errors.contains_key(name) || typed_proofs.contains_key(name) {
                continue;
            }
            *body = inline_body_limited(
                std::mem::take(body),
                &candidates,
                &functions,
                0,
                max_depth,
                &mut names,
            );
        }
    }
}

/// Fold a straight-line let* + return body into a single expression for inlining.
fn fold_body_to_expr(body: &[Stmt], params: &[(String, Typ)]) -> Option<Expr> {
    if body.is_empty() {
        return None;
    }
    let mut env: HashMap<String, Expr> = HashMap::new();
    for stmt in &body[..body.len() - 1] {
        match stmt {
            Stmt::Let(name, _, e) if !expr_has_effects(e) => {
                let e = replace_in_expr(e.clone(), &env);
                env.insert(name.clone(), e);
            }
            _ => return None,
        }
    }
    match body.last() {
        // ponytail: effectful returns need parameter/local snapshots before substitution is safe.
        Some(Stmt::Return(Some(ret))) if !expr_has_effects(ret) => {
            let ret = replace_in_expr(ret.clone(), &env);
            let mut bound = true;
            walk_expr(&ret, &mut |e| match e {
                Expr::Ident(name) => bound &= params.iter().any(|(param, _)| param == name),
                // Free identifiers and closure captures require scope-aware inlining.
                Expr::Closure { .. } => bound = false,
                _ => {}
            });
            bound.then_some(ret)
        }
        _ => None,
    }
}

/// Reserve all source binders and references, including globals, before
/// introducing block-local argument snapshots. Names are unique across this pass.
struct InlineArgumentNames {
    reserved: HashSet<String>,
    next: usize,
    checked_functions: HashSet<String>,
    caller_checked: bool,
}

impl InlineArgumentNames {
    fn new(decls: &[Decl]) -> Self {
        let mut reserved = crate::core_bindings::module_names(decls);
        for decl in decls {
            match decl {
                Decl::Function { params, body, .. } => {
                    reserved.extend(params.iter().map(|(name, _)| name.clone()));
                    crate::core_bindings::reserve_body(body, &mut reserved);
                }
                Decl::Global {
                    init: Some(value), ..
                } => crate::core_bindings::reserve_expr(value, &mut reserved),
                _ => {}
            }
        }
        Self {
            reserved,
            next: 0,
            checked_functions: HashSet::new(),
            caller_checked: false,
        }
    }
    fn fresh(&mut self) -> String {
        loop {
            let name = format!("__in_inline_arg_{}", self.next);
            self.next += 1;
            if self.reserved.insert(name.clone()) {
                return name;
            }
        }
    }
}

/// Only fold already-proven pure, straight-line callees. Effectful arguments
/// still execute exactly once, in source order, even when the callee ignores or
/// duplicates a parameter. Snapshot every argument when any one is effectful:
/// an earlier global read must precede a later argument's global write.
fn inline_call_expression(
    expression: &Expr,
    candidates: &[&String],
    functions: &HashMap<String, Decl>,
    names: &mut InlineArgumentNames,
) -> Option<Expr> {
    let Expr::Call { callee, args } = expression else {
        return None;
    };
    let Expr::Ident(name) = callee.as_ref() else {
        return None;
    };
    if !candidates.contains(&name) {
        return None;
    }
    let Decl::Function { params, body, .. } = functions.get(name)? else {
        return None;
    };
    if params.len() != args.len() {
        return None;
    }
    let result = fold_body_to_expr(body, params)?;
    if !args.iter().any(expr_has_effects) {
        let substitution = params
            .iter()
            .zip(args)
            .map(|((name, _), value)| (name.as_str(), value))
            .collect();
        return Some(substitute_expr(&result, &substitution));
    }
    if !names.caller_checked || !names.checked_functions.contains(name) {
        return None;
    }
    let snapshots: Vec<_> = args.iter().map(|_| names.fresh()).collect();
    let values: Vec<_> = snapshots.iter().cloned().map(Expr::Ident).collect();
    let substitution = params
        .iter()
        .zip(&values)
        .map(|((name, _), value)| (name.as_str(), value))
        .collect();
    Some(Expr::Block {
        body: snapshots
            .into_iter()
            .zip(args)
            .zip(params)
            .map(|((name, value), (_, formal))| {
                // Preserve concrete context (for example empty array literals).
                // A reference snapshot uses the ordinary erased value runtime,
                // not the contextual receiver ABI of the now-inlined callee.
                let annotation = match formal {
                    Typ::Generic(_) => None,
                    Typ::Named(name) => {
                        if let Some(mut reference) = crate::core_ir::rust_references::decode(name) {
                            reference.runtime = crate::core_ir::rust_references::erase_type(
                                reference.referent.clone(),
                            );
                            Some(crate::core_ir::rust_references::typ(reference))
                        } else {
                            Some(formal.clone())
                        }
                    }
                    formal => Some(formal.clone()),
                };
                Stmt::Let(name, annotation, value.clone())
            })
            .collect(),
        result: Some(Box::new(substitute_expr(&result, &substitution))),
    })
}

fn inline_body_limited(
    stmts: Vec<Stmt>,
    cand: &[&String],
    fns: &HashMap<String, Decl>,
    depth: u32,
    max_depth: u32,
    names: &mut InlineArgumentNames,
) -> Vec<Stmt> {
    if depth > max_depth {
        return stmts;
    }
    let mut r = Vec::new();
    for stmt in stmts {
        match stmt {
            Stmt::Let(n, t, e) => {
                let e = fold_call_ret(
                    inline_in_expr_limited(e, cand, fns, depth + 1, max_depth, names),
                    cand,
                    fns,
                    names,
                );
                r.push(Stmt::Let(n, t, e));
            }
            Stmt::Return(Some(e)) => {
                let e = fold_call_ret(
                    inline_in_expr_limited(e, cand, fns, depth + 1, max_depth, names),
                    cand,
                    fns,
                    names,
                );
                r.push(Stmt::Return(Some(e)));
            }
            Stmt::Expr(e) => {
                let e = inline_in_expr_limited(e, cand, fns, depth + 1, max_depth, names);
                // ponytail: void-body splicing needs hygienic bindings and argument snapshots.
                r.push(Stmt::Expr(e));
            }
            s => r.push(map_stmt(s, &mut |e| {
                inline_in_expr_limited(e, cand, fns, depth + 1, max_depth, names)
            })),
        }
    }
    r
}

fn inline_in_expr_limited(
    expression: Expr,
    candidates: &[&String],
    functions: &HashMap<String, Decl>,
    depth: u32,
    max_depth: u32,
    names: &mut InlineArgumentNames,
) -> Expr {
    map_expr(expression, &mut |expression| {
        if depth < max_depth {
            inline_call_expression(&expression, candidates, functions, names).unwrap_or(expression)
        } else {
            expression
        }
    })
}

/// Replace a call-to-small-fn with its return value (for let-bindings).
fn fold_call_ret(
    expression: Expr,
    candidates: &[&String],
    functions: &HashMap<String, Decl>,
    names: &mut InlineArgumentNames,
) -> Expr {
    inline_call_expression(&expression, candidates, functions, names).unwrap_or(expression)
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

// Callers normalize lexical bindings before collecting value references.
pub(crate) fn detect_ptr_refs(decls: &[Decl], out: &mut Vec<String>) {
    for decl in decls {
        match decl {
            Decl::Function { body, .. } => ptr_in_stmts(body, out),
            Decl::Global {
                init: Some(expr), ..
            } => ptr_in_expr(expr, out),
            Decl::Class { methods, .. } => detect_ptr_refs(methods, out),
            _ => {}
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
                cond,
                then_body,
                else_body,
                ..
            } => {
                ptr_in_expr(cond, out);
                ptr_in_stmts(then_body, out);
                ptr_in_stmts(else_body, out);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    ptr_in_expr(cond, out);
                }
                ptr_in_stmts(body, out);
            }
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
                    if let Some(guard) = &arm.guard {
                        ptr_in_expr(guard, out);
                    }
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
fn ptr_in_expr(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Ident(name) => {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        Expr::Call { callee, args } => {
            if !matches!(callee.as_ref(), Expr::Ident(_)) {
                ptr_in_expr(callee, out);
            }
            for arg in args {
                ptr_in_expr(arg, out);
            }
        }
        Expr::Closure { body, .. } => ptr_in_stmts(body, out),
        Expr::Block { body, result } => {
            ptr_in_stmts(body, out);
            if let Some(result) = result {
                ptr_in_expr(result, out);
            }
        }
        _ => crate::core_ir::for_each_expr_child(expr, &mut |child| ptr_in_expr(child, out)),
    }
}

// ─── Algebraic Simplification ──────────────────────────────────────────────

/// x+0→x, x*1→x, x&-1→x, x|0→x, x^0→x, x<<0→x, etc.
fn algebraic_simplify(decls: &mut [Decl], semantics: IntegerSemantics) {
    for body in fn_bodies_mut(decls) {
        let old = std::mem::take(body);
        *body = old
            .into_iter()
            .map(|s| map_stmt(s, &mut |e| simplify_expr_with_semantics(e, semantics)))
            .collect();
    }
}

fn simplify_binary_expr(
    original: Expr,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    semantics: IntegerSemantics,
) -> Expr {
    if [lhs, rhs].into_iter().any(
        |expr| matches!(expr, Expr::IntLit(value) if semantics.normalize_word(*value).is_none()),
    ) {
        return original;
    }
    if matches!(op, "shl" | "<<" | "shr" | ">>" | "lshr")
        && let Expr::IntLit(count) = rhs
        && !(0..if semantics == IntegerSemantics::Word32 {
            32
        } else {
            64
        })
            .contains(count)
    {
        return original;
    }
    // Absorbing identities must retain calls, mutation, and nonlocal block exits.
    if expr_has_effects(lhs) || expr_has_effects(rhs) {
        return original;
    }
    if let (Expr::IntLit(a), Expr::IntLit(b)) = (lhs, rhs) {
        if let Some(folded) = semantics.fold_binary(op, *a, *b) {
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
        "&&" | "||" => {
            // Only the LHS can suppress evaluation. Preserve other expressions:
            // replacing them with an operand can lose effects or 0/1 normalization.
            let truth = |e: &Expr| match e {
                Expr::BoolLit(v) => Some(*v),
                Expr::IntLit(v) => Some(*v != 0),
                _ => None,
            };
            if let Some(left) = truth(lhs) {
                let decided = (op == "&&" && !left) || (op == "||" && left);
                let value = if decided { Some(left) } else { truth(rhs) };
                if let Some(value) = value {
                    return if matches!(lhs, Expr::BoolLit(_)) || matches!(rhs, Expr::BoolLit(_)) {
                        Expr::BoolLit(value)
                    } else {
                        Expr::IntLit(i64::from(value))
                    };
                }
            }
        }
        "land" => {
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
        "lor" => {
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
            if lhs == rhs && matches!(lhs, Expr::IntLit(_)) {
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
            if lhs == rhs && matches!(lhs, Expr::IntLit(_)) {
                return Expr::IntLit(0);
            }
        }
        "==" => {
            if lhs == rhs && matches!(lhs, Expr::IntLit(_) | Expr::BoolLit(_) | Expr::StringLit(_))
            {
                return Expr::BoolLit(true);
            }
        }
        "!=" => {
            if lhs == rhs && matches!(lhs, Expr::IntLit(_) | Expr::BoolLit(_) | Expr::StringLit(_))
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
            if lhs == rhs && matches!(lhs, Expr::IntLit(_)) {
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
        "shl" | "<<" | "shr" | ">>" | "lshr" => {
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
        // !!x normalizes numeric truth values to Bool; untyped x is not equivalent.
        _ => {}
    }
    original
}

#[cfg(test)]
fn simplify_expr(e: Expr) -> Expr {
    simplify_expr_with_semantics(e, IntegerSemantics::Word64)
}

fn simplify_expr_with_semantics(e: Expr, semantics: IntegerSemantics) -> Expr {
    // Keep invalid target-word literals visible even inside absorbing identities.
    let mut invalid_word = false;
    walk_expr(&e, &mut |expr| {
        if let Expr::IntLit(value) = expr {
            invalid_word |= semantics.normalize_word(*value).is_none();
        }
    });
    if invalid_word {
        return e;
    }
    // Clone-and-match on owned value to avoid borrow gymnastics
    match e.clone() {
        Expr::Binary { op, lhs, rhs, .. } => simplify_binary_expr(e, &op, &lhs, &rhs, semantics),
        Expr::Unary { op, expr, .. } => simplify_unary_expr(e, &op, &expr),
        _ => e,
    }
}

// ─── Dead Function Elimination ─────────────────────────────────────────────

/// Remove functions that are never called and not referenced as pointers.
fn remove_dead_functions(decls: &mut Vec<Decl>, entry: Option<&str>) {
    // Calls inside an unreachable function do not make their callees roots.
    // Index bodies once, then visit each reachable function at most once.
    let mut functions = HashMap::new();
    let mut called = HashSet::new();
    for declaration in decls.iter() {
        match declaration {
            Decl::Function { name, body, .. } => {
                functions.entry(name.as_str()).or_insert(body.as_slice());
            }
            // Global declarations remain in the module, so their initializer
            // calls (and transitive dependencies) must remain available too.
            Decl::Global {
                init: Some(value), ..
            } => collect_calls_in_expr(value, &mut called),
            Decl::Class { methods, .. } => {
                // Classes are retained by this pass. Their method bodies and
                // any nested retained declarations are executable roots too.
                retained_declaration_calls(methods, &mut called);
            }
            _ => {}
        }
    }
    // Preserve the existing conservative address-taken root policy, including
    // references in global initializers and class methods.
    let mut ptr_refs = Vec::new();
    detect_ptr_refs(decls, &mut ptr_refs);
    called.extend(ptr_refs);
    if let Some(entry) = entry {
        called.insert(entry.to_string());
    } else {
        called.extend(["kernel_entry".into(), "kernel-entry".into(), "main".into()]);
    }
    let mut work = called.iter().cloned().collect::<Vec<_>>();
    while let Some(name) = work.pop() {
        let Some(body) = functions.get(name.as_str()) else {
            continue;
        };
        let mut dependencies = HashSet::new();
        for statement in *body {
            collect_calls_in_stmt(statement, &mut dependencies);
        }
        for dependency in dependencies {
            if called.insert(dependency.clone()) {
                work.push(dependency);
            }
        }
    }
    decls.retain(|declaration| match declaration {
        Decl::Function { name, .. } => called.contains(name),
        _ => true,
    });
}

fn retained_declaration_calls(declarations: &[Decl], roots: &mut HashSet<String>) {
    for declaration in declarations {
        match declaration {
            Decl::Function { body, .. } => {
                for statement in body {
                    collect_calls_in_stmt(statement, roots);
                }
            }
            Decl::Global {
                init: Some(value), ..
            } => collect_calls_in_expr(value, roots),
            Decl::Class { methods, .. } => retained_declaration_calls(methods, roots),
            // Struct/interface/component declarations contain no executable IR.
            _ => {}
        }
    }
}

pub(crate) fn collect_calls_in_stmt(s: &Stmt, out: &mut HashSet<String>) {
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
                if let Some(guard) = &arm.guard {
                    collect_calls_in_expr(guard, out);
                }
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
        if let Expr::Closure { body, .. } | Expr::Block { body, .. } = e {
            for s in body {
                collect_calls_in_stmt(s, out);
            }
        }
    });
}

// ─── Constant Folding ──────────────────────────────────────────────────────

/// Evaluate compile-time integer expressions: `3 + 4` → `7`.
fn fold_constants_in_decls(decls: &mut [Decl], semantics: IntegerSemantics) {
    let proofs = crate::typecheck::TypeChecker::new().typed_proof_globals(decls);
    let initialization_errors = crate::core_ir_verifier::initialization_errors_by_global(decls);
    for decl in decls.iter_mut() {
        if let Decl::Global { name, init, .. } = decl {
            if proofs.contains_key(name) || initialization_errors.contains_key(name) {
                continue;
            }
            *init = init.take().map(|expr| {
                Box::new(map_expr(*expr, &mut |e| {
                    fold_expr_with_semantics(e, semantics)
                }))
            });
        }
    }
    for body in fn_bodies_mut(decls) {
        let old = std::mem::take(body);
        *body = old
            .into_iter()
            .flat_map(|s| {
                fold_stmt_matches(
                    map_stmt(s, &mut |e| fold_expr_with_semantics(e, semantics)),
                    semantics,
                )
            })
            .collect();
    }
}

fn fold_stmt_matches(s: Stmt, semantics: IntegerSemantics) -> Vec<Stmt> {
    match s {
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            use crate::core_ir::MatchPattern;
            let invalid_word = matches!(&scrutinee, Expr::IntLit(value) if semantics.normalize_word(*value).is_none());
            // An early wildcard must not hide invalid or unsupported patterns.
            let valid_patterns = arms
                .iter()
                .all(|arm| match MatchPattern::parse(&arm.pattern) {
                    Ok(MatchPattern::IntPat(value)) => semantics.normalize_word(value).is_some(),
                    Ok(
                        MatchPattern::WildPat
                        | MatchPattern::StringPat(_)
                        | MatchPattern::BoolPat(_),
                    ) => true,
                    _ => false,
                });
            if !invalid_word
                && valid_patterns
                && matches!(
                    &scrutinee,
                    Expr::IntLit(_) | Expr::StringLit(_) | Expr::BoolLit(_)
                )
            {
                for arm in &arms {
                    use crate::core_ir::MatchPattern;
                    let matches = match (&scrutinee, MatchPattern::parse(&arm.pattern)) {
                        (_, Ok(MatchPattern::WildPat)) => true,
                        (Expr::IntLit(value), Ok(MatchPattern::IntPat(pattern))) => semantics
                            .normalize_word(*value)
                            .zip(semantics.normalize_word(pattern))
                            .is_some_and(|(value, pattern)| value == pattern),
                        (Expr::BoolLit(value), Ok(MatchPattern::BoolPat(pattern))) => {
                            *value == pattern
                        }
                        (Expr::StringLit(value), Ok(MatchPattern::StringPat(pattern))) => {
                            *value == pattern
                        }
                        // Keep unsupported and incompatible patterns visible to lowering.
                        _ => break,
                    };
                    if matches {
                        return arm.body.clone();
                    }
                }
            }
            vec![Stmt::Match { scrutinee, arms }]
        }
        other => vec![other],
    }
}

pub(crate) fn fold_expr(e: Expr) -> Expr {
    fold_expr_with_semantics(e, IntegerSemantics::Word64)
}

pub(crate) fn fold_expr_with_semantics(e: Expr, semantics: IntegerSemantics) -> Expr {
    match &e {
        Expr::Binary { op, lhs, rhs, .. } => {
            if let (Expr::BoolLit(lhs), Expr::BoolLit(rhs)) = (lhs.as_ref(), rhs.as_ref()) {
                let value = match op.as_str() {
                    "&" | "band" => Some(*lhs & *rhs),
                    "|" | "bor" => Some(*lhs | *rhs),
                    "^" | "xor" => Some(*lhs ^ *rhs),
                    _ => None,
                };
                if let Some(value) = value {
                    return Expr::BoolLit(value);
                }
            }
            if let (Expr::FloatLit(a), Expr::FloatLit(b)) = (lhs.as_ref(), rhs.as_ref()) {
                let av = a.0;
                let bv = b.0;
                let comparison = match op.as_str() {
                    "==" | "eq" => Some(av == bv),
                    "!=" | "neq" => Some(av != bv),
                    "<" | "lt" => Some(av < bv),
                    ">" | "gt" => Some(av > bv),
                    "<=" | "le" => Some(av <= bv),
                    ">=" | "ge" => Some(av >= bv),
                    _ => None,
                };
                if let Some(value) = comparison {
                    return Expr::BoolLit(value);
                }
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
                match op.as_str() {
                    "+" | "add" => return Expr::StringLit(format!("{a}{b}")),
                    "==" | "eq" => return Expr::BoolLit(a == b),
                    "!=" | "neq" => return Expr::BoolLit(a != b),
                    _ => {}
                }
            }
            if let (Expr::IntLit(a), Expr::IntLit(b)) = (lhs.as_ref(), rhs.as_ref()) {
                if let Some(value) = semantics.fold_binary(op, *a, *b) {
                    return value;
                }
            }
            e
        }
        Expr::Unary { op, expr, .. } => {
            if crate::core_ir::integer_cast_spec(op).is_some() {
                let value = match expr.as_ref() {
                    Expr::IntLit(value) => Some(Expr::IntLit(*value)),
                    Expr::BoolLit(value) => Some(Expr::IntLit(i64::from(*value))),
                    _ => None,
                };
                if let Some(value) = value {
                    return map_expr(
                        crate::core_ir::integer_cast_expr(value, op).unwrap(),
                        &mut |expr| fold_expr_with_semantics(expr, semantics),
                    );
                }
            }
            if let Some(width) = crate::core_ir::float_to_bits_width(op)
                && semantics == IntegerSemantics::Word64
                && let Some(bits) = crate::core_ir::constant_float_bits(expr)
            {
                return Expr::IntLit(if width == 32 {
                    i64::from(crate::core_ir::narrow_f64_bits(bits))
                } else {
                    bits as i64
                });
            }
            if let Some(width) = crate::core_ir::bits_to_float_width(op)
                && semantics == IntegerSemantics::Word64
                && let Expr::IntLit(bits) = expr.as_ref()
            {
                return crate::core_ir::float_bits_expr(if width == 32 {
                    crate::core_ir::embed_f32_bits(*bits as u32)
                } else {
                    *bits as u64
                });
            }
            if matches!(op.as_str(), "float-cast-f32" | "float-cast-f64")
                && let Some(bits) = crate::core_ir::constant_float_bits(&e)
            {
                return crate::core_ir::float_bits_expr(bits);
            }
            if crate::core_ir::integer_float_cast_spec(op).is_some()
                && let Expr::IntLit(value) = expr.as_ref()
            {
                // New unsigned carriers require Word64; retain legacy signed word folding.
                let value = if op == "int-to-float" {
                    semantics.normalize_word(*value)
                } else {
                    (semantics == IntegerSemantics::Word64).then_some(*value)
                };
                if let Some(value) =
                    value.and_then(|value| crate::core_ir::fold_integer_float_cast(op, value))
                {
                    return Expr::FloatLit(crate::core_ir::FloatVal(value));
                }
            }
            if let Some((bits, signed)) = crate::core_ir::saturating_float_cast_spec(op)
                && let Expr::FloatLit(value) = expr.as_ref()
            {
                return Expr::IntLit(crate::core_ir::saturating_float_cast(value.0, bits, signed));
            }
            if op == "float-to-int"
                && let Expr::FloatLit(value) = expr.as_ref()
                && value.0.is_finite()
                && value.0 >= i64::MIN as f64
                && value.0 < -(i64::MIN as f64)
            {
                return Expr::IntLit(value.0 as i64);
            }
            if op == "bool-to-int"
                && let Expr::BoolLit(value) = expr.as_ref()
            {
                return Expr::IntLit(i64::from(*value));
            }
            if matches!(op.as_str(), "!" | "not")
                && let Expr::FloatLit(value) = expr.as_ref()
            {
                return Expr::BoolLit(value.0 == 0.0);
            }
            if matches!(op.as_str(), "!" | "not")
                && let Expr::BoolLit(value) = expr.as_ref()
            {
                return Expr::BoolLit(!*value);
            }
            if let Expr::IntLit(n) = expr.as_ref() {
                if let Some(value) = semantics.fold_unary(op, *n) {
                    return value;
                }
            }
            e
        }
        Expr::Index { base, index, .. } => {
            if let (Expr::ArrayLit(items), Expr::IntLit(i)) = (base.as_ref(), index.as_ref()) {
                if !items.iter().all(|item| {
                    matches!(
                        item,
                        Expr::IntLit(_) | Expr::BoolLit(_) | Expr::FloatLit(_) | Expr::StringLit(_)
                    )
                }) {
                    return e;
                }
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
            if let Expr::StructInit { fields, .. } = base.as_ref()
                && fields.iter().all(|(_, value)| {
                    matches!(
                        value,
                        Expr::IntLit(_) | Expr::BoolLit(_) | Expr::FloatLit(_) | Expr::StringLit(_)
                    )
                })
            {
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
/// Scalars substitute on every use. Aggregates retain storage: array indexing
/// and struct receiver calls can observe an address, even without assignments.
fn propagate_constants(decls: &mut [Decl]) {
    for body in fn_bodies_mut(decls) {
        propagate_in_body(body);
    }
}

/// Resolve immutable local function values before direct-call validation.
/// Only unannotated aliases of exact module functions are eligible; no scalar
/// folding or annotated binding obligation is removed by this source pass.
pub(crate) fn propagate_function_values(decls: &mut [Decl]) {
    crate::core_bindings::normalize_decls(decls);
    let globals: HashSet<_> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let functions: HashSet<_> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } if !globals.contains(name) => Some(name.clone()),
            _ => None,
        })
        .collect();
    if functions.is_empty() {
        return;
    }
    for body in fn_bodies_mut(decls) {
        let mut aliases = HashMap::new();
        let mut assigned = HashSet::new();
        let mut observed = HashSet::new();
        for statement in body.iter() {
            if let Stmt::Let(name, None, Expr::Ident(target)) = statement {
                if functions.contains(target) || aliases.contains_key(target) {
                    aliases.insert(name.clone(), Expr::Ident(target.clone()));
                }
            }
            collect_assigned_names(statement, &mut assigned);
            let mut statement = statement.clone();
            map_stmt_mut(&mut statement, &mut |expression| {
                if let Expr::Closure { body, captures, .. } = expression {
                    observed.extend(captures.iter().cloned());
                    crate::core_bindings::reserve_body(body, &mut observed);
                }
                match expression {
                    Expr::Unary { op, expr }
                        if op == "&"
                            || crate::core_ir::rust_references::value_kind(op).is_some() =>
                    {
                        crate::core_bindings::reserve_expr(expr, &mut observed);
                    }
                    Expr::Call { callee, args } if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::BORROW) => {
                        if let Some(value) = args.first() {
                            crate::core_bindings::reserve_expr(value, &mut observed);
                        }
                    }
                    _ => {}
                }
            });
        }
        aliases.retain(|name, _| !assigned.contains(name) && !observed.contains(name));
        let aliases: HashMap<_, _> = aliases
            .keys()
            .filter_map(|name| {
                let mut current = name.as_str();
                let mut visited = HashSet::new();
                while visited.insert(current) {
                    let Expr::Ident(target) = aliases.get(current)? else {
                        return None;
                    };
                    if functions.contains(target) {
                        return Some((name.clone(), Expr::Ident(target.clone())));
                    }
                    current = target;
                }
                None
            })
            .collect();
        if aliases.is_empty() {
            continue;
        }
        for statement in body.iter_mut() {
            *statement = replace_in_stmt(statement, &aliases);
        }
        body.retain(|statement| !matches!(statement, Stmt::Let(name, None, _) if aliases.contains_key(name)));
    }
}

fn is_propagatable_const(e: &Expr) -> bool {
    matches!(
        e,
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::BoolLit(_) | Expr::StringLit(_)
    )
}

pub(crate) fn place_root(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Ident(name) => Some(name),
        Expr::Field { base, .. } | Expr::Index { base, .. } => place_root(base),
        Expr::Unary { expr, .. } => place_root(expr),
        _ => None,
    }
}

pub(crate) fn collect_assigned_names(s: &Stmt, assigned: &mut HashSet<String>) {
    let mut stmt = s.clone();
    map_stmt_mut(&mut stmt, &mut |expr| {
        if let Expr::Block { body, .. } = expr {
            for inner in body {
                collect_assigned_names(inner, assigned);
            }
        }
    });
    match s {
        Stmt::Assign(n, _) => {
            assigned.insert(n.clone());
        }
        // `arr[i] = v` and `s.f = v` mutate the base binding, so the base must
        // be treated as assigned (otherwise constant propagation would inline
        // the initializer and the store would write to a discarded temporary).
        Stmt::IndexAssign { base, .. } | Stmt::FieldAssign { base, .. } => {
            if let Some(name) = place_root(base) {
                assigned.insert(name.to_owned());
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
    stmts.retain(|s| {
        !matches!(s, Stmt::Declare(n, _) if !used.contains(n))
            && !matches!(s, Stmt::Let(n, _, e) if !used.contains(n) && !expr_has_effects(e))
    });

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
        if let Expr::Block { body, .. } = e {
            collect_used(body, out);
        }
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
    for decl in decls {
        let Decl::Function { params, body, .. } = decl else {
            continue;
        };
        // ponytail: prove parameter types only in binding-free returns; extend with scoped facts before rewriting locals.
        let unshadowed = match body.as_slice() {
            [Stmt::Return(Some(expr))] => {
                let mut closure = false;
                walk_expr(expr, &mut |e| {
                    closure |= matches!(e, Expr::Closure { .. } | Expr::Block { .. })
                });
                !closure
            }
            _ => false,
        };
        let integer_leaf = |e: &Expr| match e {
            Expr::IntLit(_) => true,
            Expr::Ident(name) if unshadowed => params
                .iter()
                .any(|(param, typ)| param == name && *typ == Typ::Int),
            _ => false,
        };
        for stmt in body.iter_mut() {
            map_stmt_mut(stmt, &mut |e| {
                if let Expr::Binary { op, lhs, rhs } = e {
                    // MBA duplicates operands and requires integer bit operations.
                    if !integer_leaf(lhs) || !integer_leaf(rhs) {
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
                        "^" | "xor" => {
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
fn harden_obscure_literals(decls: &mut [Decl], semantics: IntegerSemantics) {
    let mut counter = 0u64;
    for body in fn_bodies_mut(decls) {
        for stmt in body.iter_mut() {
            map_stmt_mut(stmt, &mut |e| {
                obscure_expr_literals(e, &mut counter, semantics);
            });
        }
    }
}

fn obscure_expr_literals(e: &mut Expr, counter: &mut u64, semantics: IntegerSemantics) {
    match e {
        Expr::IntLit(v) => {
            *counter = counter.wrapping_add(1);
            let k = ((*counter).wrapping_mul(0x9E3779B97F4A7C15) ^ (*v as u64)) as i64 | 1;
            let (value, k) = if semantics == IntegerSemantics::Word32 {
                let Some(value) = semantics.normalize_word(*v) else {
                    return;
                };
                (value, i64::from(k as i32))
            } else {
                (*v, k)
            };
            let masked = if semantics == IntegerSemantics::Word32 {
                i64::from((value as i32) ^ (k as i32))
            } else {
                value ^ k
            };
            // ((v ^ k) ^ k) | (k & 0) == v. Extra `| 0` is noise, not a third XOR of k.
            *e = bin_expr(
                "|",
                bin_expr("^", Expr::IntLit(masked), Expr::IntLit(k)),
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
/// flow or redirect loop jumps into the synthetic dispatcher.
fn stmts_forbid_cfg_dispatch(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        let mut structured = false;
        let mut copied = stmt.clone();
        map_stmt_mut(&mut copied, &mut |expr| {
            structured |= matches!(expr, Expr::Block { .. })
        });
        if structured {
            return true;
        }
        match stmt {
            Stmt::Declare(..)
            | Stmt::Loop { .. }
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
    crate::core_ir_verifier::preserve_nonvoid_completion(decls);
    for decl in decls {
        let Decl::Function { body, .. } = decl else {
            continue;
        };
        if body.len() < 2 {
            continue;
        }
        if stmts_forbid_cfg_dispatch(body) {
            continue;
        }
        let proven_complete = matches!(body.last(), Some(Stmt::Expr(Expr::Call { callee, args }))
            if args.is_empty() && matches!(callee.as_ref(), Expr::Ident(name)
                if name == crate::core_ir::COMPILER_UNREACHABLE));
        let original: Vec<Stmt> = std::mem::take(body);
        // Prefer short states when binding lifetimes permit.
        let chunk = if original.len() > 8 { 2 } else { 1 };
        let mut last_use = HashMap::new();
        let mut used = HashSet::new();
        for (index, stmt) in original.iter().enumerate() {
            collect_used(std::slice::from_ref(stmt), &mut used);
            for name in used.drain() {
                last_use.insert(name, index);
            }
            if let Stmt::Let(name, ..) = stmt {
                last_use.entry(name.clone()).or_insert(index);
            }
        }
        let mut pc = "_pc".to_string();
        while last_use.contains_key(&pc) {
            pc.push('_');
        }
        let mut chunks: Vec<Vec<Stmt>> = Vec::new();
        let mut cur = Vec::new();
        let mut needed_through = 0;
        for (index, stmt) in original.into_iter().enumerate() {
            if let Stmt::Let(name, ..) = &stmt {
                needed_through = needed_through.max(last_use.get(name).copied().unwrap_or(index));
            }
            cur.push(stmt);
            // State scopes must retain a local until its last read or write.
            if cur.len() >= chunk && index >= needed_through {
                chunks.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            chunks.push(cur);
        }
        let state_count = chunks.len() as i64;
        fn dispatch_tree(
            mut chunks: Vec<Vec<Stmt>>,
            first: i64,
            pc: &str,
            state_count: i64,
        ) -> Stmt {
            if chunks.len() == 1 {
                let mut then_body = chunks.pop().expect("nonempty dispatcher leaf");
                then_body.push(Stmt::Assign(pc.to_string(), Expr::IntLit(first + 1)));
                return Stmt::If {
                    cond: bin_expr("==", Expr::Ident(pc.to_string()), Expr::IntLit(first)),
                    then_body,
                    else_body: vec![Stmt::Assign(pc.to_string(), Expr::IntLit(state_count))],
                };
            }
            let middle = chunks.len() / 2;
            let right = chunks.split_off(middle);
            let middle_state = first + middle as i64;
            Stmt::If {
                cond: bin_expr("<", Expr::Ident(pc.to_string()), Expr::IntLit(middle_state)),
                then_body: vec![dispatch_tree(chunks, first, pc, state_count)],
                else_body: vec![dispatch_tree(right, middle_state, pc, state_count)],
            }
        }
        // A linear else chain makes a wide source body recursively deep in
        // every later IR walker/lowerer. Balanced routing retains one guarded
        // state per iteration with logarithmic synthetic nesting instead.
        let dispatch = dispatch_tree(chunks, 0, &pc, state_count);
        *body = vec![
            Stmt::Let(pc.clone(), Some(Typ::Int), Expr::IntLit(0)),
            Stmt::Loop {
                kind: LoopKind::While,
                cond: Some(Expr::Binary {
                    op: "<".into(),
                    lhs: Box::new(Expr::Ident(pc.clone())),
                    rhs: Box::new(Expr::IntLit(state_count)),
                }),
                body: vec![dispatch],
            },
        ];
        if proven_complete {
            // Carry the original completion proof across synthetic loop exits.
            body.push(Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::COMPILER_UNREACHABLE.into())),
                args: vec![],
            }));
        }
    }
}

/// Hash-mangle internal function names beyond normal ABI.
fn harden_hash_symbols(decls: &mut [Decl], entry: Option<&str>) {
    let entry = entry.unwrap_or("main");
    let mut rename: HashMap<String, String> = HashMap::new();
    for d in decls.iter() {
        if let Decl::Function { name, body, .. } = d {
            if body.is_empty()
                || name == entry
                || is_stdlib_or_reserved(name)
                || name.starts_with("_H")
            {
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
        if let Decl::Function { name, .. } = d {
            if let Some(new) = rename.get(name) {
                *name = new.clone();
            }
        }
    }
    map_decl_exprs_mut(decls, &mut |expr| {
        if let Expr::Ident(name) = expr {
            if let Some(new) = rename.get(name) {
                *name = new.clone();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardened_link_surfaces_preserve_exports_and_external_names() {
        let source = "extern asm fn external_answer()->Int\nfn other_export()->Int{return 20;}\nfn main()->Int{return external_answer()+other_export();}";
        for keep_all in [false, true] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            optimize_with_linkage(
                &mut module.decls,
                Some("main"),
                EmitProfile::Harden,
                keep_all,
            );
            assert!(module.decls.iter().any(|decl| matches!(decl, Decl::Function {name,body,..} if name=="external_answer" && body.is_empty())));
            if keep_all {
                assert!(
                    module.decls.iter().any(
                        |decl| matches!(decl, Decl::Function {name,..} if name=="other_export")
                    )
                );
            }
            let mut calls = HashSet::new();
            for body in fn_bodies_mut(&mut module.decls) {
                for statement in body {
                    map_stmt_mut(statement, &mut |expression| {
                        if let Expr::Call { callee, .. } = expression
                            && let Expr::Ident(name) = callee.as_ref()
                        {
                            calls.insert(name.clone());
                        }
                    });
                }
            }
            assert!(calls.contains("external_answer"));
        }
    }

    #[test]
    fn checked_indexing_retains_bounds_failure_exits() {
        let indexed = Expr::Index {
            base: Box::new(ident("values")),
            index: Box::new(Expr::IntLit(9)),
        };
        assert!(expr_has_effects(&indexed));
        let absorbed = bin("*", Expr::IntLit(0), indexed.clone());
        assert_eq!(simplify_expr(absorbed.clone()), absorbed);
        let mut body = vec![
            Stmt::Let("ignored".into(), Some(Typ::Int), indexed),
            Stmt::Return(Some(Expr::IntLit(42))),
        ];
        assert!(fold_body_to_expr(&body, &[]).is_none());
        dce_body(&mut body);
        assert!(matches!(body.first(), Some(Stmt::Let(..))));
        assert_eq!(
            fold_expr(Expr::Index {
                base: Box::new(Expr::ArrayLit(vec![Expr::IntLit(42)])),
                index: Box::new(Expr::IntLit(0))
            }),
            Expr::IntLit(42)
        );
    }

    #[test]
    fn checked_integer_operations_retain_failure_exits_through_optimization() {
        for op in ["/", "%", "div", "mod", "udiv", "urem"] {
            let checked = bin(op, ident("value"), ident("zero"));
            assert!(expr_has_effects(&checked));
            for expression in [
                bin("*", Expr::IntLit(0), checked.clone()),
                bin("*", checked.clone(), Expr::IntLit(0)),
            ] {
                assert_eq!(simplify_expr(expression.clone()), expression, "{op}");
            }
            let mut body = vec![
                Stmt::Let("unused".into(), Some(Typ::Int), checked.clone()),
                Stmt::Return(Some(Expr::IntLit(42))),
            ];
            assert!(
                fold_body_to_expr(
                    &body,
                    &[("value".into(), Typ::Int), ("zero".into(), Typ::Int)]
                )
                .is_none()
            );
            dce_body(&mut body);
            assert!(matches!(body.first(), Some(Stmt::Let(_, _, value)) if value == &checked));
            // Successful literal operations still fold before effect-sensitive DCE.
            assert_eq!(
                fold_expr(bin(op, Expr::IntLit(8), Expr::IntLit(2))),
                Expr::IntLit(if matches!(op, "%" | "mod" | "urem") {
                    0
                } else {
                    4
                })
            );
        }
    }

    #[test]
    fn word32_optimizations_keep_invalid_carriers_visible() {
        let semantics = IntegerSemantics::Word32;
        let invalid = bin(
            "*",
            bin("+", Expr::IntLit(0x100000000), ident("value")),
            Expr::IntLit(0),
        );
        assert_eq!(
            map_expr(invalid.clone(), &mut |expr| simplify_expr_with_semantics(
                expr, semantics
            )),
            invalid
        );
        for (scrutinee, pattern) in [(0x100000000, "_"), (42, "4294967296")] {
            let statement = Stmt::Match {
                scrutinee: Expr::IntLit(scrutinee),
                arms: vec![
                    MatchArm {
                        pattern: "_".into(),
                        guard: None,
                        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    },
                    MatchArm {
                        pattern: pattern.into(),
                        guard: None,
                        body: vec![],
                    },
                ],
            };
            assert_eq!(
                fold_stmt_matches(statement.clone(), semantics),
                vec![statement]
            );
        }
    }

    #[test]
    fn word32_folding_and_harden_noise_preserve_machine_word_values() {
        let semantics = IntegerSemantics::Word32;
        let expression = bin("+", Expr::IntLit(0x7fffffff), Expr::IntLit(1));
        assert_eq!(
            fold_expr_with_semantics(expression.clone(), semantics),
            Expr::IntLit(i32::MIN.into())
        );
        assert_eq!(fold_expr(expression), Expr::IntLit(0x80000000));
        for value in [0, 42, -1, i32::MIN.into(), 0xffffffff] {
            let mut obscured = Expr::IntLit(value);
            obscure_expr_literals(&mut obscured, &mut 0, semantics);
            walk_expr(&obscured, &mut |expr| {
                if let Expr::IntLit(value) = expr {
                    assert!(
                        semantics.normalize_word(*value).is_some(),
                        "illegal harden literal {value}"
                    );
                }
            });
            let folded = map_expr(obscured, &mut |expr| {
                fold_expr_with_semantics(expr, semantics)
            });
            assert_eq!(
                folded,
                Expr::IntLit(semantics.normalize_word(value).unwrap())
            );
        }
        for count in [-1, 32, 64] {
            let expression = bin("<<", Expr::IntLit(0), Expr::IntLit(count));
            assert_eq!(
                simplify_expr_with_semantics(expression.clone(), semantics),
                expression
            );
        }
    }

    #[test]
    fn every_profile_uses_word32_folding_for_globals_and_propagated_values() {
        let source = "var result: Int = 2147483647 + 1\nfn main()->Int{let value:Int=4294967295;return value+1;}";
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            optimize_with_linkage_and_semantics(
                &mut module.decls,
                Some("main"),
                profile,
                false,
                IntegerSemantics::Word32,
            );
            let global = module
                .decls
                .iter()
                .find_map(|decl| match decl {
                    Decl::Global { init, .. } => init.as_deref(),
                    _ => None,
                })
                .unwrap();
            assert_eq!(global, &Expr::IntLit(i32::MIN.into()), "{profile:?}");
            for body in fn_bodies_mut(&mut module.decls) {
                let mut body = body.clone();
                for stmt in &mut body {
                    map_stmt_mut(stmt, &mut |expr| {
                        *expr = fold_expr_with_semantics(expr.clone(), IntegerSemantics::Word32);
                    });
                }
                let mut pending: Vec<_> = body.iter().collect();
                let mut returns = Vec::new();
                while let Some(stmt) = pending.pop() {
                    match stmt {
                        Stmt::Return(value) => returns.push(value.as_ref()),
                        Stmt::Loop { body, .. } => pending.extend(body),
                        Stmt::If {
                            then_body,
                            else_body,
                            ..
                        } => {
                            pending.extend(then_body);
                            pending.extend(else_body);
                        }
                        Stmt::Match { arms, .. } => {
                            for arm in arms {
                                pending.extend(&arm.body);
                            }
                        }
                        Stmt::Try { body, catches } => {
                            pending.extend(body);
                            for catch in catches {
                                pending.extend(&catch.body);
                            }
                        }
                        _ => {}
                    }
                }
                assert!(
                    !returns.is_empty(),
                    "missing return for {profile:?}: {body:?}"
                );
                assert!(
                    returns
                        .iter()
                        .all(|value| matches!(value, Some(Expr::IntLit(0)))),
                    "{profile:?}: {body:?}"
                );
            }
        }
    }

    #[test]
    fn hardened_block_array_keeps_local_type_and_result_scope() {
        let mut decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![
                Stmt::Let(
                    "values".into(),
                    Some(Typ::Array(Box::new(Typ::Int))),
                    Expr::Block {
                        body: vec![Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(20))],
                        result: Some(Box::new(Expr::ArrayLit(vec![
                            Expr::Ident("x".into()),
                            Expr::IntLit(22),
                        ]))),
                    },
                ),
                Stmt::Return(Some(Expr::Index {
                    base: Box::new(Expr::Ident("values".into())),
                    index: Box::new(Expr::IntLit(0)),
                })),
            ],
        }];
        optimize_with_profile(&mut decls, Some("main"), EmitProfile::Harden);
        let mut found = false;
        map_decl_exprs_mut(&mut decls, &mut |expr| {
            if let Expr::Block {
                body,
                result: Some(result),
            } = expr
            {
                let [Stmt::Let(local, Some(Typ::Int), value)] = body.as_slice() else {
                    panic!("block local type lost: {body:?}")
                };
                assert!(matches!(value, Expr::Binary { .. }));
                let Expr::ArrayLit(items) = result.as_ref() else {
                    panic!("block result shape lost")
                };
                assert_eq!(&items[0], &Expr::Ident(local.clone()));
                assert!(matches!(&items[1], Expr::Binary { .. }));
                found = true;
            }
        });
        assert!(found);
    }

    #[test]
    fn structured_exits_survive_absorbing_identities_and_unused_bindings() {
        let exit = Expr::Block {
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            result: Some(Box::new(Expr::IntLit(7))),
        };
        for op in ["*", "&", "|", "&&", "||"] {
            let rhs = if op == "|" {
                Expr::IntLit(-1)
            } else if op == "||" {
                Expr::BoolLit(true)
            } else if op == "&&" {
                Expr::BoolLit(false)
            } else {
                Expr::IntLit(0)
            };
            let original = bin_expr(op, exit.clone(), rhs.clone());
            assert_eq!(
                simplify_binary_expr(original.clone(), op, &exit, &rhs, IntegerSemantics::Word64),
                original
            );
        }
        let mut body = vec![
            Stmt::Let("unused".into(), Some(Typ::Int), exit),
            Stmt::Return(Some(Expr::IntLit(0))),
        ];
        dce_body(&mut body);
        assert!(matches!(
            body.first(),
            Some(Stmt::Let(_, _, Expr::Block { .. }))
        ));
        assert!(fold_body_to_expr(&body, &[]).is_none());
        assert!(stmts_forbid_cfg_dispatch(&body));
    }

    #[test]
    fn structured_values_keep_outer_mutations_and_live_calls() {
        let mut body = vec![
            Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(20)),
            Stmt::Let(
                "unused".into(),
                Some(Typ::Int),
                Expr::Block {
                    body: vec![
                        Stmt::Assign("x".into(), Expr::IntLit(42)),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![],
                        }),
                    ],
                    result: Some(Box::new(Expr::IntLit(0))),
                },
            ),
            Stmt::Return(Some(Expr::Ident("x".into()))),
        ];
        propagate_in_body(&mut body);
        dce_body(&mut body);
        assert!(matches!(body.last(), Some(Stmt::Return(Some(Expr::Ident(name)))) if name == "x"));
        assert!(
            body.iter()
                .any(|stmt| matches!(stmt, Stmt::Let(_, _, Expr::Block { .. })))
        );
        let mut calls = HashSet::new();
        for stmt in &body {
            collect_calls_in_stmt(stmt, &mut calls);
        }
        assert!(calls.contains("helper"));
    }

    use crate::core_ir::{CatchArm, Decl, Expr, LoopKind, Stmt, Typ};
    use crate::emit_profile::EmitProfile;
    use crate::integer_semantics::IntegerSemantics;

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
    fn struct_field_fold_preserves_unselected_effects_and_failures() {
        for discarded in [
            call("effect", vec![]),
            bin("div", Expr::IntLit(1), Expr::IntLit(0)),
            Expr::Index {
                base: Box::new(ident("values")),
                index: Box::new(Expr::IntLit(-1)),
            },
        ] {
            let expr = Expr::Field {
                base: Box::new(Expr::StructInit {
                    name: "Point".into(),
                    fields: vec![("x".into(), Expr::IntLit(42)), ("y".into(), discarded)],
                }),
                name: "x".into(),
            };
            assert_eq!(fold_expr(expr.clone()), expr);
        }
        let expr = Expr::Field {
            base: Box::new(Expr::StructInit {
                name: "Point".into(),
                fields: vec![
                    ("x".into(), Expr::IntLit(42)),
                    ("y".into(), Expr::IntLit(0)),
                ],
            }),
            name: "x".into(),
        };
        assert_eq!(fold_expr(expr), Expr::IntLit(42));
    }

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
    fn fold_negation_overflow_unchanged() {
        for op in ["neg", "-"] {
            let original = unary(op, Expr::IntLit(i64::MIN));
            assert_eq!(fold_expr(original.clone()), original);
        }
    }

    #[test]
    fn fold_globals_preserves_nonconstant_initializers() {
        let mut decls: Vec<_> = [
            None,
            Some(call("init", vec![])),
            Some(bin("/", Expr::IntLit(1), Expr::IntLit(0))),
            Some(ident("other")),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, init)| Decl::Global {
            name: format!("value{i}"),
            typ: Typ::Int,
            init: init.map(Box::new),
            mutable: true,
            constant_value: false,
        })
        .collect();
        let original = decls.clone();
        fold_constants_in_decls(&mut decls, IntegerSemantics::Word64);
        assert_eq!(decls, original);
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
            Expr::BoolLit(true)
        );
        assert_eq!(
            fold_expr(bin("eq", Expr::IntLit(5), Expr::IntLit(3))),
            Expr::BoolLit(false)
        );
        assert_eq!(
            fold_expr(bin("neq", Expr::IntLit(1), Expr::IntLit(2))),
            Expr::BoolLit(true)
        );
        assert_eq!(
            fold_expr(bin("lt", Expr::IntLit(3), Expr::IntLit(5))),
            Expr::BoolLit(true)
        );
        assert_eq!(
            fold_expr(bin("gt", Expr::IntLit(5), Expr::IntLit(3))),
            Expr::BoolLit(true)
        );
        assert_eq!(
            fold_expr(bin("le", Expr::IntLit(5), Expr::IntLit(5))),
            Expr::BoolLit(true)
        );
        assert_eq!(
            fold_expr(bin("ge", Expr::IntLit(5), Expr::IntLit(5))),
            Expr::BoolLit(true)
        );
    }

    #[test]
    fn fold_string_equality_compares_complete_literal_content() {
        for (left, right, equal) in [
            ("λ雪", "λ雪", true),
            ("λ雪", "λ霜", false),
            ("", "", true),
            ("", "λ", false),
            ("a\0雪", "a\0雪", true),
            ("a\0雪", "a\0霜", false),
        ] {
            for op in ["==", "eq", "!=", "neq"] {
                let expected = if matches!(op, "==" | "eq") {
                    equal
                } else {
                    !equal
                };
                assert_eq!(
                    fold_expr(bin(
                        op,
                        Expr::StringLit(left.into()),
                        Expr::StringLit(right.into())
                    )),
                    Expr::BoolLit(expected),
                    "{left:?} {op} {right:?}"
                );
            }
        }
        for op in ["<", ">", "===", "!==", "*", "unknown"] {
            let expr = bin(op, Expr::StringLit("λ".into()), Expr::StringLit("λ".into()));
            assert_eq!(fold_expr(expr.clone()), expr);
        }
        for op in ["==", "eq", "!=", "neq"] {
            for expr in [
                bin(op, call("effect", vec![]), Expr::StringLit("λ".into())),
                bin(op, Expr::StringLit("λ".into()), call("effect", vec![])),
                bin(op, call("effect", vec![]), call("effect", vec![])),
            ] {
                assert_eq!(fold_expr(expr.clone()), expr);
            }
        }
    }

    #[test]
    fn fold_logical_ops() {
        assert_eq!(
            fold_expr(bin("land", Expr::IntLit(1), Expr::IntLit(1))),
            Expr::BoolLit(true)
        );
        assert_eq!(
            fold_expr(bin("land", Expr::IntLit(0), Expr::IntLit(1))),
            Expr::BoolLit(false)
        );
        assert_eq!(
            fold_expr(bin("lor", Expr::IntLit(0), Expr::IntLit(1))),
            Expr::BoolLit(true)
        );
        assert_eq!(
            fold_expr(bin("lor", Expr::IntLit(0), Expr::IntLit(0))),
            Expr::BoolLit(false)
        );
    }

    #[test]
    fn fold_unary_neg() {
        assert_eq!(fold_expr(unary("neg", Expr::IntLit(5))), Expr::IntLit(-5));
    }

    #[test]
    fn fold_unary_not() {
        assert_eq!(
            fold_expr(unary("not", Expr::IntLit(0))),
            Expr::BoolLit(true)
        );
        assert_eq!(
            fold_expr(unary("not", Expr::IntLit(42))),
            Expr::BoolLit(false)
        );
    }

    #[test]
    fn fold_non_literal_unchanged() {
        let e = bin("add", ident("x"), Expr::IntLit(1));
        assert_eq!(fold_expr(e.clone()), e);
    }

    // ─── Algebraic Simplification ──────────────────────────────────────

    #[test]
    fn simplify_preserves_undefined_integer_division_and_remainder() {
        for op in ["%", "mod", "/", "div"] {
            for (lhs, rhs) in [(i64::MIN, -1), (42, 0)] {
                let expr = bin(op, Expr::IntLit(lhs), Expr::IntLit(rhs));
                assert_eq!(simplify_expr(expr.clone()), expr, "{op}: {lhs}, {rhs}");
                assert_eq!(fold_expr(expr.clone()), expr, "{op}: {lhs}, {rhs}");
            }
        }
    }

    #[test]
    fn simplify_folds_signed_remainders() {
        for op in ["%", "mod"] {
            for (lhs, rhs, expected) in [(-7, 3, -1), (7, -3, 1), (-7, -3, -1)] {
                let expr = bin(op, Expr::IntLit(lhs), Expr::IntLit(rhs));
                assert_eq!(simplify_expr(expr.clone()), Expr::IntLit(expected));
                assert_eq!(fold_expr(expr), Expr::IntLit(expected));
            }
        }
    }

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
        assert_eq!(simplify_expr(e.clone()), e);
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
    fn optimize_folds_global_initializers_in_every_profile() {
        let source = "var sentinel: Int = -1\nvar nested: Int = -(2 + 3)\nconst mask = (1 << 5) | 3\nfn main() -> Int { sentinel = 7; return sentinel; }";
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut module = crate::in_lang_parse::parse_in_source(source).expect("parse");
            optimize_with_profile(&mut module.decls, Some("main"), profile);
            let values: Vec<_> = module
                .decls
                .iter()
                .filter_map(|decl| match decl {
                    Decl::Global { name, init, .. } => Some((name.as_str(), init.as_deref())),
                    _ => None,
                })
                .collect();
            assert_eq!(
                values,
                vec![
                    ("sentinel", Some(&Expr::IntLit(-1))),
                    ("nested", Some(&Expr::IntLit(-5))),
                    ("mask", Some(&Expr::IntLit(35))),
                ],
                "profile {profile:?}"
            );
        }
    }

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
    fn fold_float_comparisons_preserve_nan_and_signed_zero() {
        for (lhs, rhs) in [
            (f64::NAN, 0.0),
            (f64::NAN, f64::NAN),
            (-0.0, 0.0),
            (1.5, 0.0),
        ] {
            for (op, expected) in [
                ("==", lhs == rhs),
                ("!=", lhs != rhs),
                ("<", lhs < rhs),
                (">", lhs > rhs),
                ("<=", lhs <= rhs),
                (">=", lhs >= rhs),
            ] {
                assert_eq!(
                    fold_expr(bin(
                        op,
                        Expr::FloatLit(crate::core_ir::FloatVal(lhs)),
                        Expr::FloatLit(crate::core_ir::FloatVal(rhs))
                    )),
                    Expr::BoolLit(expected),
                    "{lhs:?} {op} {rhs:?}"
                );
            }
        }
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
    fn remove_dead_functions_uses_entry_reachability_not_dead_caller_edges() {
        let mut declarations = vec![
            make_fn("main", vec![Stmt::Expr(call("live_a", vec![]))]),
            make_fn("live_a", vec![Stmt::Expr(call("live_b", vec![]))]),
            make_fn("live_b", vec![Stmt::Return(None)]),
            make_fn("dead_a", vec![Stmt::Expr(call("dead_b", vec![]))]),
            make_fn("dead_b", vec![Stmt::Return(None)]),
            make_fn("cycle_a", vec![Stmt::Expr(call("cycle_b", vec![]))]),
            make_fn("cycle_b", vec![Stmt::Expr(call("cycle_a", vec![]))]),
        ];
        remove_dead_functions(&mut declarations, Some("main"));
        let names = declarations
            .iter()
            .filter_map(|decl| {
                if let Decl::Function { name, .. } = decl {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();
        assert_eq!(names, HashSet::from(["main", "live_a", "live_b"]));
    }
    #[test]
    fn remove_dead_functions_preserves_pointer_global_and_match_guard_roots() {
        let mut declarations = vec![
            make_fn(
                "main",
                vec![Stmt::Match {
                    scrutinee: Expr::BoolLit(true),
                    arms: vec![MatchArm {
                        pattern: "true".into(),
                        guard: Some(call("guard", vec![])),
                        body: vec![],
                    }],
                }],
            ),
            make_fn("guard", vec![Stmt::Return(None)]),
            make_fn(
                "global_init",
                vec![Stmt::Expr(call("global_child", vec![]))],
            ),
            make_fn("global_child", vec![Stmt::Return(None)]),
            make_fn("address", vec![Stmt::Expr(call("pointer_child", vec![]))]),
            make_fn("pointer_child", vec![Stmt::Return(None)]),
            make_fn("dead", vec![Stmt::Return(None)]),
            Decl::Global {
                name: "TABLE".into(),
                typ: Typ::Int,
                init: Some(Box::new(ident("address"))),
                mutable: false,
                constant_value: false,
            },
            Decl::Global {
                name: "VALUE".into(),
                typ: Typ::Int,
                init: Some(Box::new(call("global_init", vec![]))),
                mutable: false,
                constant_value: false,
            },
        ];
        remove_dead_functions(&mut declarations, Some("main"));
        let names = declarations
            .iter()
            .filter_map(|decl| {
                if let Decl::Function { name, .. } = decl {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();
        assert_eq!(
            names,
            HashSet::from([
                "main",
                "guard",
                "global_init",
                "global_child",
                "address",
                "pointer_child"
            ])
        );
        assert!(
            declarations
                .iter()
                .any(|decl| matches!(decl,Decl::Global{name,..}if name=="TABLE"))
        );
    }
    #[test]
    fn retained_class_method_bodies_keep_their_top_level_dependencies() {
        let class = |name: &str, methods: Vec<Decl>| Decl::Class {
            name: name.into(),
            fields: vec![],
            methods,
            visibility: crate::core_ir::Visibility::Pub,
            extends: None,
            implements: vec![],
            type_params: vec![],
        };
        let mut declarations = vec![
            make_fn("main", vec![Stmt::Return(None)]),
            make_fn(
                "method_helper",
                vec![Stmt::Expr(call("method_leaf", vec![]))],
            ),
            make_fn("method_leaf", vec![Stmt::Return(None)]),
            make_fn("nested_helper", vec![Stmt::Return(None)]),
            make_fn("unused", vec![Stmt::Return(None)]),
            class(
                "Root",
                vec![
                    make_fn("method", vec![Stmt::Expr(call("method_helper", vec![]))]),
                    class(
                        "Nested",
                        vec![make_fn(
                            "nested_method",
                            vec![Stmt::Expr(call("nested_helper", vec![]))],
                        )],
                    ),
                ],
            ),
        ];
        remove_dead_functions(&mut declarations, Some("main"));
        let names = declarations
            .iter()
            .filter_map(|decl| {
                if let Decl::Function { name, .. } = decl {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();
        assert_eq!(
            names,
            HashSet::from(["main", "method_helper", "method_leaf", "nested_helper"])
        );
        assert!(
            declarations
                .iter()
                .any(|decl| matches!(decl,Decl::Class{name,..}if name=="Root"))
        );
    }
    #[test]
    fn keep_all_linkage_retains_unreachable_callers_and_cycles() {
        let mut declarations = vec![
            make_fn("main", vec![Stmt::Return(None)]),
            make_fn("cycle_a", vec![Stmt::Expr(call("cycle_b", vec![]))]),
            make_fn("cycle_b", vec![Stmt::Expr(call("cycle_a", vec![]))]),
        ];
        optimize_with_linkage(&mut declarations, Some("main"), EmitProfile::Default, true);
        assert_eq!(
            declarations
                .iter()
                .filter(|decl| matches!(decl, Decl::Function { .. }))
                .count(),
            3
        );
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
    fn optimize_keeps_default_entry() {
        let mut decls = vec![
            make_fn("kernel_entry", vec![Stmt::Return(None)]),
            make_fn("unused", vec![Stmt::Return(None)]),
        ];
        optimize(&mut decls);
        assert_eq!(decls.len(), 1);
        if let Decl::Function { name, .. } = &decls[0] {
            assert_eq!(name, "kernel_entry");
        }
    }

    #[test]
    fn optimize_with_entry_keeps_specified_entry() {
        let mut decls = vec![
            make_fn("custom_entry", vec![Stmt::Return(None)]),
            make_fn("unused", vec![Stmt::Return(None)]),
        ];
        optimize_with_entry(&mut decls, Some("custom_entry"));
        assert_eq!(decls.len(), 1);
        if let Decl::Function { name, .. } = &decls[0] {
            assert_eq!(name, "custom_entry");
        }
    }

    #[test]
    fn optimize_performs_inlining_and_dce() {
        let mut decls = vec![
            make_fn_with_params(
                "inline_me",
                vec![],
                Typ::Int,
                vec![
                    Stmt::Let("a".into(), Some(Typ::Int), Expr::IntLit(10)),
                    Stmt::Return(Some(ident("a"))),
                ],
            ),
            make_fn_with_params(
                "kernel_entry",
                vec![],
                Typ::Int,
                vec![
                    Stmt::Return(Some(call("inline_me", vec![]))),
                    Stmt::Let("b".into(), Some(Typ::Int), Expr::IntLit(20)),
                ],
            ),
        ];
        optimize(&mut decls);
        assert_eq!(decls.len(), 1);
        let Decl::Function { name, body, .. } = &decls[0] else {
            panic!("expected entry function");
        };
        assert_eq!(name, "kernel_entry");
        assert_eq!(body, &vec![Stmt::Return(Some(Expr::IntLit(10)))]);
    }

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

    #[test]
    fn optimize_empty_decls() {
        let mut decls: Vec<Decl> = vec![];
        optimize(&mut decls);
        assert!(decls.is_empty());
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
    fn hardened_symbol_names_follow_global_addresses_and_match_guards() {
        let mut decls = vec![
            make_fn("target", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
            make_fn(
                "main",
                vec![Stmt::Match {
                    scrutinee: Expr::IntLit(0),
                    arms: vec![MatchArm {
                        pattern: "_".into(),
                        guard: Some(bin_expr("!=", ident("target"), Expr::IntLit(0))),
                        body: vec![Stmt::FieldAssign {
                            base: ident("target"),
                            name: "value".into(),
                            value: Expr::IntLit(0),
                        }],
                    }],
                }],
            ),
            Decl::Global {
                name: "pointer".into(),
                typ: Typ::Int,
                init: Some(Box::new(ident("target"))),
                mutable: false,
                constant_value: false,
            },
        ];
        harden_hash_symbols(&mut decls, Some("main"));
        let Decl::Function { name: renamed, .. } = &decls[0] else {
            panic!()
        };
        let renamed = renamed.clone();
        assert!(renamed.starts_with("_H"));
        let Decl::Global {
            init: Some(init), ..
        } = &decls[2]
        else {
            panic!()
        };
        assert_eq!(init.as_ref(), &ident(&renamed));
        let Decl::Function { body, .. } = &decls[1] else {
            panic!()
        };
        let Stmt::Match { arms, .. } = &body[0] else {
            panic!()
        };
        let Some(Expr::Binary { lhs, .. }) = &arms[0].guard else {
            panic!()
        };
        assert_eq!(lhs.as_ref(), &ident(&renamed));
        let Stmt::FieldAssign { base, .. } = &arms[0].body[0] else {
            panic!()
        };
        assert_eq!(base, &ident(&renamed));
    }

    #[test]
    fn optimization_keeps_function_addresses_in_every_value_position() {
        let cases = vec![
            vec![
                Stmt::Let("pointer".into(), Some(Typ::Int), ident("target")),
                Stmt::Return(Some(ident("pointer"))),
            ],
            vec![Stmt::Return(Some(ident("target")))],
            vec![
                Stmt::Let(
                    "items".into(),
                    Some(Typ::Array(Box::new(Typ::Int))),
                    Expr::ArrayLit(vec![ident("target")]),
                ),
                Stmt::Return(Some(Expr::Index {
                    base: Box::new(ident("items")),
                    index: Box::new(Expr::IntLit(0)),
                })),
            ],
            vec![Stmt::If {
                cond: bin_expr("!=", ident("target"), Expr::IntLit(0)),
                then_body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                else_body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
            }],
            vec![Stmt::Match {
                scrutinee: Expr::IntLit(0),
                arms: vec![
                    MatchArm {
                        pattern: "0".into(),
                        guard: Some(bin_expr("!=", ident("target"), Expr::IntLit(0))),
                        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    },
                    MatchArm {
                        pattern: "_".into(),
                        guard: None,
                        body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                    },
                ],
            }],
            vec![Stmt::Return(Some(call(
                "invoke",
                vec![Expr::Closure {
                    params: vec![],
                    ret: Typ::Int,
                    captures: vec![],
                    body: vec![Stmt::Return(Some(ident("target")))],
                }],
            )))],
        ];
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for body in &cases {
                let mut decls = vec![
                    make_fn("target", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                    make_fn("main", body.clone()),
                ];
                optimize_with_profile(&mut decls, Some("main"), profile);
                assert_eq!(
                    decls
                        .iter()
                        .filter(|decl| matches!(decl, Decl::Function { .. }))
                        .count(),
                    2,
                    "{profile:?} {body:?}"
                );
            }
            let mut decls = vec![
                make_fn("target", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                make_fn("main", vec![Stmt::Return(Some(Expr::IntLit(0)))]),
                Decl::Global {
                    name: "pointer".into(),
                    typ: Typ::Int,
                    init: Some(Box::new(ident("target"))),
                    mutable: false,
                    constant_value: false,
                },
            ];
            optimize_with_profile(&mut decls, Some("main"), profile);
            assert_eq!(
                decls
                    .iter()
                    .filter(|decl| matches!(decl, Decl::Function { .. }))
                    .count(),
                2
            );
        }
    }

    #[test]
    fn shadowed_values_do_not_escape_functions_or_disable_direct_call_inlining() {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut decls = vec![
                make_fn("target", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                make_fn(
                    "main",
                    vec![
                        Stmt::Let("target".into(), Some(Typ::Int), Expr::IntLit(7)),
                        Stmt::Return(Some(ident("target"))),
                    ],
                ),
            ];
            optimize_with_profile(&mut decls, Some("main"), profile);
            assert_eq!(
                decls
                    .iter()
                    .filter(|decl| matches!(decl, Decl::Function { .. }))
                    .count(),
                1
            );
            let mut decls = vec![
                make_fn("target", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                make_fn("main", vec![Stmt::Return(Some(call("target", vec![])))]),
            ];
            optimize_with_profile(&mut decls, Some("main"), profile);
            assert_eq!(
                decls
                    .iter()
                    .filter(|decl| matches!(decl, Decl::Function { .. }))
                    .count(),
                1
            );
            let mut decls = vec![
                make_fn("target", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                make_fn("main", vec![Stmt::Return(Some(Expr::IntLit(0)))]),
                Decl::Global {
                    name: "pointer".into(),
                    typ: Typ::Int,
                    init: Some(Box::new(Expr::Block {
                        body: vec![Stmt::Let("target".into(), Some(Typ::Int), Expr::IntLit(7))],
                        result: Some(Box::new(ident("target"))),
                    })),
                    mutable: false,
                    constant_value: false,
                },
            ];
            optimize_with_profile(&mut decls, Some("main"), profile);
            assert_eq!(
                decls
                    .iter()
                    .filter(|decl| matches!(decl, Decl::Function { .. }))
                    .count(),
                1
            );
        }
    }

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
        let double_neg = Expr::Unary {
            op: "!".into(),
            expr: Box::new(Expr::Unary {
                op: "!".into(),
                expr: Box::new(ident("x")),
            }),
        };
        // Numeric operands require both logical operations to produce canonical Bool.
        assert_eq!(simplify_expr(double_neg.clone()), double_neg);

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
        // An untyped identifier may be boolean, so keep its result type.
        assert_eq!(self_xor, bin("^", ident("a"), ident("a")));
        assert_eq!(
            fold_expr(bin("^", Expr::BoolLit(true), Expr::BoolLit(true))),
            Expr::BoolLit(false)
        );
        assert_eq!(
            simplify_expr(bin("^", Expr::IntLit(7), Expr::IntLit(7))),
            Expr::IntLit(0)
        );

        let self_sub = simplify_expr(Expr::Binary {
            op: "-".into(),
            lhs: Box::new(ident("a")),
            rhs: Box::new(ident("a")),
        });
        // An untyped identifier may be infinite or NaN, making x - x NaN.
        assert_eq!(self_sub, bin("-", ident("a"), ident("a")));

        // `a == a` must NOT fold when `a`'s type is unknown: a Float NaN
        // operand makes `==` false and `!=` true (IEEE 754), so only
        // type-known literals qualify for the tautology fold.
        let self_eq = simplify_expr(Expr::Binary {
            op: "==".into(),
            lhs: Box::new(ident("a")),
            rhs: Box::new(ident("a")),
        });
        assert_eq!(
            self_eq,
            Expr::Binary {
                op: "==".into(),
                lhs: Box::new(ident("a")),
                rhs: Box::new(ident("a")),
            }
        );
        let lit_eq = simplify_expr(Expr::Binary {
            op: "==".into(),
            lhs: Box::new(Expr::IntLit(2)),
            rhs: Box::new(Expr::IntLit(2)),
        });
        assert_eq!(lit_eq, Expr::BoolLit(true));
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
            bin("&&", ident("x"), Expr::BoolLit(true))
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
    fn harden_mba_preserves_unproven_and_effectful_operands() {
        for (params, prefix, lhs) in [
            (
                vec![("x".into(), Typ::Float)],
                vec![],
                Expr::Ident("x".into()),
            ),
            (
                vec![],
                vec![],
                Expr::Call {
                    callee: Box::new(Expr::Ident("next".into())),
                    args: vec![],
                },
            ),
            (
                vec![("x".into(), Typ::Int)],
                vec![Stmt::Let(
                    "x".into(),
                    Some(Typ::Float),
                    Expr::FloatLit(crate::core_ir::FloatVal(1.25)),
                )],
                Expr::Ident("x".into()),
            ),
        ] {
            let mut body = prefix;
            body.push(Stmt::Return(Some(bin_expr("+", lhs, Expr::IntLit(2)))));
            let mut decls = vec![Decl::Function {
                name: "main".into(),
                params,
                ret: Typ::Int,
                body,
                type_params: vec![],
            }];
            let before = decls.clone();
            harden_mba_arithmetic(&mut decls);
            assert_eq!(decls, before);
        }
    }

    #[test]
    fn harden_mba_xor_alias_matches_caret() {
        let mut caret = crate::in_lang_parse::parse_in_source(
            "fn mix(left:Int,right:Int)->Int{return left^right;}fn main()->Int{return mix(63,21);}",
        )
        .unwrap()
        .decls;
        let original = caret.clone();
        let mut alias = caret.clone();
        map_decl_exprs_mut(&mut alias, &mut |expr| {
            if let Expr::Binary { op, .. } = expr
                && op == "^"
            {
                *op = "xor".into();
            }
        });
        harden_mba_arithmetic(&mut caret);
        harden_mba_arithmetic(&mut alias);
        assert_ne!(caret, original);
        assert_eq!(alias, caret);
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
    #[test]
    fn folds_numeric_conversions_only_in_defined_integer_range() {
        assert_eq!(
            fold_expr(unary("int-to-float", Expr::IntLit(42))),
            Expr::FloatLit(crate::core_ir::FloatVal(42.0))
        );
        for (value, expected) in [
            (42.9, 42),
            (-42.9, -42),
            (-0.0, 0),
            (i64::MIN as f64, i64::MIN),
        ] {
            assert_eq!(
                fold_expr(unary(
                    "float-to-int",
                    Expr::FloatLit(crate::core_ir::FloatVal(value))
                )),
                Expr::IntLit(expected)
            );
        }
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -(i64::MIN as f64),
        ] {
            let expr = unary(
                "float-to-int",
                Expr::FloatLit(crate::core_ir::FloatVal(value)),
            );
            assert_eq!(fold_expr(expr.clone()), expr);
        }
    }
    #[test]
    fn saturating_float_constants_match_rust_casts() {
        for (target, bits, signed) in [
            ("i8", 8, true),
            ("i64", 64, true),
            ("u8", 8, false),
            ("u64", 64, false),
        ] {
            for value in [
                42.9,
                -42.9,
                1000.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                2.0f64.powi(63),
                2.0f64.powi(64),
            ] {
                assert_eq!(
                    fold_expr(Expr::Unary {
                        op: format!("float-saturate-{target}"),
                        expr: Box::new(Expr::FloatLit(crate::core_ir::FloatVal(value)))
                    }),
                    Expr::IntLit(crate::core_ir::saturating_float_cast(value, bits, signed))
                );
            }
        }
    }

    #[test]
    fn integer_cast_constants_match_truncation_and_extension() {
        for (target, value, expected) in [
            ("u8", 298, 42),
            ("i8", 170, -86),
            ("i8", -129, 127),
            ("u16", -1, 65535),
            ("i16", 65535, -1),
            ("u32", -1, 4294967295),
            ("i32", 2147483648, -2147483648),
            ("u64", i64::MIN, i64::MIN),
        ] {
            assert_eq!(
                fold_expr(unary(&format!("int-cast-{target}"), Expr::IntLit(value))),
                Expr::IntLit(expected)
            );
        }
        assert_eq!(
            fold_expr(unary("int-cast-u8", Expr::BoolLit(true))),
            Expr::IntLit(1)
        );
    }
    #[test]
    fn floating_width_folding_matches_ieee_single_rounding() {
        for value in [
            0.0f64,
            -0.0,
            0.1,
            16777217.0,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::INFINITY,
        ] {
            let expected = f64::from(value as f32);
            let folded = fold_expr(unary(
                "float-cast-f32",
                Expr::FloatLit(crate::core_ir::FloatVal(value)),
            ));
            assert_eq!(
                crate::core_ir::constant_float_bits(&folded),
                Some(expected.to_bits())
            );
        }
    }
    #[test]
    fn harden_preserves_completion_without_sealing_invalid_functions() {
        for (source, complete) in [
            ("fn main()->Int{if true{return 42;}}", true),
            ("fn main()->Int{print(20);print(22);}", false),
            (
                "fn main(flag:Bool)->Int{if flag{return 42;}print(0);}",
                false,
            ),
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            optimize_with_linkage(&mut module.decls, Some("main"), EmitProfile::Harden, false);
            assert_eq!(
                crate::core_ir_verifier::check_nonvoid_completion(&module).is_ok(),
                complete,
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn cfg_dispatch_retains_binding_lifetimes_and_private_counter_names() {
        for source in [
            "fn main() -> Int { var value: Int = 40; value += 2; return value; }",
            "fn main() -> Int { let _pc: Int = 40; var value: Int = _pc; value += 2; return value; }",
            "fn consume(value: Int) -> Int { return value; }\nfn main() -> Int { let value: Int = 40; if true { consume(value); } consume(1); return 42; }",
            "fn consume(value: Int) -> Int { return value; }\nfn main() -> Int { let _pc: Int = 20; let _pc_: Int = 20; consume(_pc + _pc_); consume(1); consume(2); consume(3); consume(4); consume(5); consume(6); return 42; }",
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            harden_cfg_dispatch_lite(&mut module.decls);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{source}: {verified:?}: {:?}", module.decls);
            assert_eq!(
                crate::native_emit::inisa::eval_module(&module, "main").unwrap(),
                42
            );
            if source.starts_with("fn consume") {
                let body = module
                    .decls
                    .iter()
                    .find_map(|decl| match decl {
                        Decl::Function { name, body, .. } if name == "main" => Some(body),
                        _ => None,
                    })
                    .unwrap();
                let Stmt::Loop {
                    cond: Some(Expr::Binary { rhs, .. }),
                    ..
                } = &body[1]
                else {
                    panic!("dispatcher");
                };
                assert!(
                    matches!(rhs.as_ref(), Expr::IntLit(states) if *states > 1),
                    "must split after the local dies: {body:?}"
                );
                if source.contains("let _pc_") {
                    assert!(
                        matches!(&body[0], Stmt::Let(name, ..) if name != "_pc" && name != "_pc_")
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod self_subtraction_tests {
    use super::*;

    #[test]
    fn untyped_identifier_subtraction_keeps_ieee_semantics() {
        for op in ["-", "sub"] {
            let operand = Expr::Ident("value".into());
            let original = bin_expr(op, operand.clone(), operand.clone());
            assert_eq!(
                simplify_binary_expr(
                    original.clone(),
                    op,
                    &operand,
                    &operand,
                    IntegerSemantics::Word64
                ),
                original
            );
        }
        for value in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 42.5, -0.0] {
            let original = bin_expr(
                "-",
                Expr::FloatLit(crate::core_ir::FloatVal(value)),
                Expr::FloatLit(crate::core_ir::FloatVal(value)),
            );
            let Expr::FloatLit(result) = fold_expr(original) else {
                panic!("float subtraction lost its result type");
            };
            if value.is_finite() {
                assert_eq!(result.0, 0.0);
            } else {
                assert!(result.0.is_nan());
            }
        }
        assert_eq!(
            fold_expr(bin_expr("-", Expr::IntLit(42), Expr::IntLit(42))),
            Expr::IntLit(0)
        );
    }

    #[test]
    fn every_profile_preserves_dynamic_float_subtraction() {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut declarations = vec![Decl::Function {
                name: "difference".into(),
                params: vec![("value".into(), Typ::Float)],
                ret: Typ::Float,
                body: vec![Stmt::Return(Some(bin_expr(
                    "-",
                    Expr::Ident("value".into()),
                    Expr::Ident("value".into()),
                )))],
                type_params: vec![],
            }];
            optimize_with_profile(&mut declarations, Some("difference"), profile);
            let mut found = false;
            map_decl_exprs_mut(&mut declarations, &mut |expr| {
                found |= matches!(expr, Expr::Binary{op,lhs,rhs} if op=="-" && lhs==rhs && matches!(lhs.as_ref(),Expr::Ident(_)));
            });
            assert!(found, "{profile:?}: {declarations:?}");
        }
    }
}

#[cfg(test)]
mod logical_normalization_tests {
    use super::*;
    #[test]
    fn double_logical_not_preserves_dynamic_values_and_one_effectful_operand() {
        for outer in ["!", "not"] {
            for inner in ["!", "not"] {
                for value in [
                    Expr::Ident("value".into()),
                    Expr::Call {
                        callee: Box::new(Expr::Ident("next".into())),
                        args: vec![],
                    },
                ] {
                    let expr = Expr::Unary {
                        op: outer.into(),
                        expr: Box::new(Expr::Unary {
                            op: inner.into(),
                            expr: Box::new(value),
                        }),
                    };
                    for semantics in [IntegerSemantics::Word32, IntegerSemantics::Word64] {
                        assert_eq!(simplify_expr_with_semantics(expr.clone(), semantics), expr);
                    }
                }
            }
        }
    }
    #[test]
    fn literal_double_not_keeps_boolean_result_type_and_truth_normalization() {
        for value in [0, 1, 42, -1, i64::MIN] {
            let expr = Expr::Unary {
                op: "!".into(),
                expr: Box::new(Expr::Unary {
                    op: "!".into(),
                    expr: Box::new(Expr::IntLit(value)),
                }),
            };
            assert_eq!(map_expr(expr, &mut fold_expr), Expr::BoolLit(value != 0));
        }
        for value in [true, false] {
            let expr = Expr::Unary {
                op: "!".into(),
                expr: Box::new(Expr::Unary {
                    op: "!".into(),
                    expr: Box::new(Expr::BoolLit(value)),
                }),
            };
            assert_eq!(map_expr(expr, &mut fold_expr), Expr::BoolLit(value));
        }
    }
}

#[cfg(test)]
mod float_literal_logical_not_tests {
    use super::*;

    #[test]
    fn float_not_folds_numeric_zero_for_both_word_widths() {
        for semantics in [IntegerSemantics::Word32, IntegerSemantics::Word64] {
            for op in ["!", "not"] {
                for (value, expected) in [
                    (0.0, true),
                    (-0.0, true),
                    (2.5, false),
                    (-2.5, false),
                    (f64::INFINITY, false),
                    (f64::NEG_INFINITY, false),
                    (f64::NAN, false),
                ] {
                    let expr = Expr::Unary {
                        op: op.into(),
                        expr: Box::new(Expr::FloatLit(crate::core_ir::FloatVal(value))),
                    };
                    assert_eq!(
                        fold_expr_with_semantics(expr, semantics),
                        Expr::BoolLit(expected)
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod balanced_dispatch_tests {
    use super::*;

    fn if_depth(body: &[Stmt]) -> usize {
        body.iter()
            .map(|stmt| match stmt {
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => 1 + if_depth(then_body).max(if_depth(else_body)),
                Stmt::Loop { body, .. } => if_depth(body),
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn wide_dispatch_is_logarithmic_and_preserves_short_lived_values() {
        let calls = (0..300)
            .map(|index| {
                format!(
                    "let value{index}:Int=step({index});if value{index}!={}{{return 7;}}",
                    index + 1
                )
            })
            .collect::<String>();
        let source = format!(
            "fn step(value:Int)->Int{{return value+1;}}fn main()->Int{{let _pc:Int=20;let _pc_:Int=21;let first:Int=step(_pc+_pc_);if first!=42{{return 7;}}{calls}return 42;}}"
        );
        let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
        assert_eq!(
            crate::native_emit::inisa::eval_module(&module, "main").unwrap(),
            42
        );
        harden_cfg_dispatch_lite(&mut module.decls);
        let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
        assert!(verified.ok, "{verified:?}");
        assert_eq!(
            crate::native_emit::inisa::eval_module(&module, "main").unwrap(),
            42
        );
        let body = module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .unwrap();
        assert!(matches!(&body[0],Stmt::Let(name,..) if name!="_pc"&&name!="_pc_"));
        let Stmt::Loop {
            cond: Some(Expr::Binary { rhs, .. }),
            body: dispatch,
            ..
        } = &body[1]
        else {
            panic!("dispatcher")
        };
        let Expr::IntLit(states) = rhs.as_ref() else {
            panic!("state count")
        };
        assert!(*states > 100, "must split after local bindings expire");
        let routing_levels = (*states as usize).next_power_of_two().ilog2() as usize;
        assert!(
            if_depth(dispatch) <= routing_levels + 2,
            "{} states plus one source if must have logarithmic depth: {}",
            states,
            if_depth(dispatch)
        );
    }
}

#[cfg(test)]
mod post_inline_normalizer_tests {
    use super::*;

    #[test]
    fn normalization_can_create_real_method_edges_before_reachability_dce() {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut module = crate::in_lang_parse::parse_in_source(
                "fn target(value:Int)->Int{return 77;}fn main(value:Int)->Int{return -value;}",
            )
            .unwrap();
            let mut called = false;
            let mut normalize = |decls: &mut Vec<Decl>| {
                assert!(
                    decls.iter().any(
                        |decl| matches!(decl, Decl::Function { name, .. } if name == "target")
                    )
                );
                for decl in decls {
                    if let Decl::Function { name, body, .. } = decl
                        && name == "main"
                    {
                        let Some(Stmt::Return(Some(Expr::Unary { op, expr }))) = body.last() else {
                            panic!("normalizer must run before folding");
                        };
                        assert_eq!(op, "-");
                        let argument = expr.as_ref().clone();
                        *body.last_mut().unwrap() = Stmt::Return(Some(Expr::Call {
                            callee: Box::new(Expr::Ident("target".into())),
                            args: vec![argument],
                        }));
                        called = true;
                    }
                }
            };
            optimize_with_linkage_and_semantics_and_normalizer(
                &mut module.decls,
                Some("main"),
                profile,
                false,
                IntegerSemantics::Word64,
                Some(&mut normalize),
            );
            assert!(called);
            assert_eq!(
                module
                    .decls
                    .iter()
                    .filter(|decl| matches!(decl, Decl::Function { .. }))
                    .count(),
                2,
                "{profile:?}: actual method target must survive DCE"
            );
        }
    }

    #[test]
    fn absent_normalizer_preserves_existing_profile_output() {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut original =
                crate::in_lang_parse::parse_in_source("fn main(value:Int)->Int{return value+42;}")
                    .unwrap();
            let mut hooked = original.clone();
            optimize_with_linkage_and_semantics(
                &mut original.decls,
                Some("main"),
                profile,
                false,
                IntegerSemantics::Word64,
            );
            optimize_with_linkage_and_semantics_and_normalizer(
                &mut hooked.decls,
                Some("main"),
                profile,
                false,
                IntegerSemantics::Word64,
                None,
            );
            assert_eq!(original, hooked);
        }
    }
}

#[cfg(test)]
mod inline_argument_snapshot_tests {
    use super::*;
    fn function(name: &str, params: Vec<(String, Typ)>, value: Expr) -> Decl {
        Decl::Function {
            name: name.into(),
            params,
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(value))],
            type_params: vec![],
        }
    }
    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args,
        }
    }
    fn pure() -> Decl {
        function(
            "pure",
            vec![("left".into(), Typ::Int), ("right".into(), Typ::Int)],
            Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::Ident("left".into())),
                rhs: Box::new(Expr::Ident("left".into())),
            },
        )
    }
    #[test]
    fn effectful_call_snapshots_all_arguments_in_order_and_uses_unique_locals() {
        let declaration = pure();
        let functions = HashMap::from([("pure".into(), declaration.clone())]);
        let key = "pure".to_string();
        let arguments = vec![
            Expr::Ident("__in_inline_arg_0".into()),
            call("write_global", vec![]),
        ];
        let invocation = call("pure", arguments.clone());
        let context = function(
            "caller",
            vec![("__in_inline_arg_1".into(), Typ::Int)],
            invocation.clone(),
        );
        let mut names = InlineArgumentNames::new(&[declaration, context]);
        names.caller_checked = true;
        names.checked_functions.insert(key.clone());
        let inlined = inline_call_expression(&invocation, &[&key], &functions, &mut names).unwrap();
        let Expr::Block { body, result } = inlined else {
            panic!("expected snapshots")
        };
        assert_eq!(body.len(), 2);
        let Stmt::Let(first, Some(Typ::Int), left) = &body[0] else {
            panic!("preserve concrete argument context")
        };
        let Stmt::Let(second, Some(Typ::Int), right) = &body[1] else {
            panic!("preserve concrete argument context")
        };
        assert_eq!(left, &arguments[0]);
        assert_eq!(right, &arguments[1]);
        assert_ne!(first, second);
        assert!(!["__in_inline_arg_0", "__in_inline_arg_1"].contains(&first.as_str()));
        assert!(!["__in_inline_arg_0", "__in_inline_arg_1"].contains(&second.as_str()));
        assert_eq!(
            *result.unwrap(),
            Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::Ident(first.clone())),
                rhs: Box::new(Expr::Ident(first.clone()))
            }
        );
    }
    #[test]
    fn unused_arguments_and_nonlocal_exit_remain_at_the_call_position() {
        let declaration = function(
            "constant",
            vec![("ignored".into(), Typ::Int)],
            Expr::IntLit(7),
        );
        let functions = HashMap::from([("constant".into(), declaration.clone())]);
        let key = "constant".to_string();
        let exit = Expr::Block {
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            result: None,
        };
        let invocation = call("constant", vec![exit.clone()]);
        let mut names = InlineArgumentNames::new(&[declaration]);
        names.caller_checked = true;
        names.checked_functions.insert(key.clone());
        let inlined = inline_call_expression(&invocation, &[&key], &functions, &mut names).unwrap();
        let Expr::Block { body, result } = inlined else {
            panic!()
        };
        assert!(matches!(&body[..],[Stmt::Let(_,Some(Typ::Int),argument)]if argument==&exit));
        assert_eq!(*result.unwrap(), Expr::IntLit(7));
    }
    #[test]
    fn exact_arity_zero_args_and_pure_callee_boundary_are_preserved() {
        let declaration = pure();
        let functions = HashMap::from([("pure".into(), declaration.clone())]);
        let key = "pure".to_string();
        let mut names = InlineArgumentNames::new(&[declaration]);
        for args in [
            vec![Expr::IntLit(1)],
            vec![Expr::IntLit(1), Expr::IntLit(2), call("effect", vec![])],
        ] {
            assert!(
                inline_call_expression(&call("pure", args), &[&key], &functions, &mut names)
                    .is_none()
            );
        }
        let zero = function("zero", vec![], Expr::IntLit(42));
        let impure = function("impure", vec![], call("effect", vec![]));
        let functions = HashMap::from([("zero".into(), zero), ("impure".into(), impure)]);
        let zero = "zero".to_string();
        let impure = "impure".to_string();
        assert_eq!(
            inline_call_expression(&call("zero", vec![]), &[&zero], &functions, &mut names),
            Some(Expr::IntLit(42))
        );
        assert!(
            inline_call_expression(&call("impure", vec![]), &[&impure], &functions, &mut names)
                .is_none()
        );
    }
}

#[cfg(test)]
mod snapshot_reference_context_tests {
    use super::*;
    #[test]
    fn borrowed_snapshot_retains_source_proof_with_ordinary_value_runtime() {
        use crate::core_ir::rust_references::{self, ReferenceType};
        let formal = rust_references::typ(ReferenceType {
            mutable: false,
            referent: Typ::Named("Row".into()),
            runtime: Typ::Named("&Row".into()),
        });
        let declaration = Decl::Function {
            name: "borrowed_neg".into(),
            params: vec![("self".into(), formal)],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(88)))],
            type_params: vec![],
        };
        let key = "borrowed_neg".to_string();
        let functions = HashMap::from([(key.clone(), declaration.clone())]);
        let mut names = InlineArgumentNames::new(&[declaration]);
        names.caller_checked = true;
        names.checked_functions.insert(key.clone());
        let argument = Expr::Call {
            callee: Box::new(Expr::Ident("conditional_reference".into())),
            args: vec![Expr::BoolLit(true)],
        };
        let call = Expr::Call {
            callee: Box::new(Expr::Ident(key.clone())),
            args: vec![argument.clone()],
        };
        let inlined = inline_call_expression(&call, &[&key], &functions, &mut names).unwrap();
        let Expr::Block { body, result } = inlined else {
            panic!()
        };
        let [Stmt::Let(_, Some(Typ::Named(annotation)), actual)] = body.as_slice() else {
            panic!()
        };
        let proof = rust_references::decode(annotation).unwrap();
        assert_eq!(proof.referent, Typ::Named("Row".into()));
        assert_eq!(proof.runtime, Typ::Named("Row".into()));
        assert!(!proof.mutable);
        assert_eq!(actual, &argument);
        assert_eq!(*result.unwrap(), Expr::IntLit(88));
    }
}

#[cfg(test)]
mod checked_snapshot_driver_tests {
    use super::*;
    fn function(name: &str, params: Vec<(String, Typ)>, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params,
            ret: Typ::Int,
            body,
            type_params: vec![],
        }
    }
    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args,
        }
    }
    fn contains_call(declaration: &mut Decl, target: &str) -> bool {
        let mut found = false;
        map_decl_exprs_mut(std::slice::from_mut(declaration), &mut |value| {
            found |= matches!(value,Expr::Call{callee,..}if matches!(callee.as_ref(),Expr::Ident(name)if name==target));
        });
        found
    }
    #[test]
    fn unchecked_caller_or_callee_keeps_original_effectful_call() {
        let producer = function(
            "produce",
            vec![],
            vec![Stmt::If {
                cond: Expr::BoolLit(true),
                then_body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
                else_body: vec![Stmt::Return(Some(Expr::IntLit(2)))],
            }],
        );
        for bad_callee in 0..3 {
            let formal = if bad_callee != 0 {
                Typ::Int
            } else {
                Typ::Array(Box::new(Typ::Int))
            };
            let mut body = vec![];
            if bad_callee == 1 {
                body.push(Stmt::Let(
                    "wrong".into(),
                    Some(Typ::String),
                    Expr::Ident("value".into()),
                ));
            }
            body.push(Stmt::Return(Some(if bad_callee == 2 {
                Expr::StringLit("wrong return".into())
            } else {
                Expr::IntLit(42)
            })));
            let mut declarations = vec![
                function("ignore", vec![("value".into(), formal)], body),
                producer.clone(),
                function(
                    "main",
                    vec![],
                    vec![Stmt::Return(Some(call(
                        "ignore",
                        vec![call("produce", vec![])],
                    )))],
                ),
            ];
            inline_small_functions(&mut declarations);
            assert!(
                contains_call(&mut declarations[2], "ignore"),
                "bad callee={bad_callee}"
            );
        }
    }
    #[test]
    fn surviving_closure_does_not_inherit_parent_snapshot_permission() {
        let producer = function(
            "produce",
            vec![],
            vec![Stmt::If {
                cond: Expr::BoolLit(true),
                then_body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
                else_body: vec![],
            }],
        );
        let closure = Expr::Closure {
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(call(
                "ignore",
                vec![call("produce", vec![])],
            )))],
            captures: vec![],
        };
        let mut declarations = vec![
            function(
                "ignore",
                vec![("value".into(), Typ::Int)],
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            producer,
            function(
                "main",
                vec![],
                vec![Stmt::Expr(closure), Stmt::Return(Some(Expr::IntLit(42)))],
            ),
        ];
        inline_small_functions(&mut declarations);
        assert!(contains_call(&mut declarations[2], "ignore"));
    }
}

#[cfg(test)]
mod associated_type_inline_barrier_tests {
    use super::*;
    #[test]
    fn private_type_failures_block_pure_zeroarg_and_literal_argument_inline_paths() {
        for proof in [
            crate::core_ir::type_proofs::ResolutionFailure::Invalid("cycle".into()),
            crate::core_ir::type_proofs::ResolutionFailure::Unsupported("projection".into()),
        ] {
            for argc in [0, 1] {
                let call = Expr::Call {
                    callee: Box::new(Expr::Ident("bad".into())),
                    args: if argc == 0 {
                        vec![]
                    } else {
                        vec![Expr::IntLit(42)]
                    },
                };
                let mut declarations = vec![
                    Decl::Function {
                        name: "bad".into(),
                        params: if argc == 0 {
                            vec![]
                        } else {
                            vec![("unused".into(), Typ::Int)]
                        },
                        ret: proof.clone().typ(),
                        body: vec![Stmt::Return(Some(Expr::IntLit(77)))],
                        type_params: vec![],
                    },
                    Decl::Function {
                        name: "main".into(),
                        params: vec![],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(call.clone()))],
                        type_params: vec![],
                    },
                ];
                inline_small_functions_with(&mut declarations, usize::MAX, 4);
                let Decl::Function { body, .. } = &declarations[1] else {
                    panic!()
                };
                assert_eq!(body, &vec![Stmt::Return(Some(call))]);
            }
        }
    }
}

#[cfg(test)]
mod declaration_optimization_tests {
    use super::*;
    #[test]
    fn harden_preserves_branch_assignment_followed_by_a_conditional_value() {
        let module = crate::compiler::rust_front::parse_rust_source(
            "fn apply(flag:bool)->i64 {let value:i64;if flag {value=42;} else {value=7;} if flag {value} else {value+35}} fn main()->i64 {apply(true)}"
        ).unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
        optimize(&mut module, EmitProfile::Harden);
        let checked = crate::core_ir_verifier::check_execution_contracts(&module);
        assert!(checked.is_ok(), "{checked:?}\n{module:#?}");
    }
    fn function(name: &str, params: Vec<(String, Typ)>, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params,
            ret: Typ::Int,
            body,
            type_params: vec![],
        }
    }
    fn optimize(module: &mut crate::core_ir::UnifiedModule, profile: EmitProfile) {
        optimize_with_linkage_and_semantics(
            &mut module.decls,
            Some("main"),
            profile,
            true,
            IntegerSemantics::Word64,
        );
    }
    #[test]
    fn profiles_preserve_valid_declaration_assignment_control_flow() {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut module = crate::core_ir::UnifiedModule::new(vec![function(
                "main",
                vec![],
                vec![
                    Stmt::Declare("value".into(), Typ::Int),
                    Stmt::Assign("value".into(), Expr::IntLit(42)),
                    Stmt::Return(Some(Expr::Ident("value".into()))),
                ],
            )]);
            optimize(&mut module, profile);
            crate::core_ir_verifier::check_execution_contracts(&module).unwrap();
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{profile:?}: {report:?}");
        }
    }
    #[test]
    fn optimize_with_profile_applies_inlining_and_dce() {
        let mut decls = vec![
            function(
                "small_func",
                vec![],
                vec![Stmt::Return(Some(Expr::IntLit(10)))],
            ),
            function(
                "dead_func",
                vec![],
                vec![Stmt::Return(Some(Expr::IntLit(0)))],
            ),
            function(
                "main",
                vec![],
                vec![
                    Stmt::Let(
                        "x".into(),
                        None,
                        Expr::Call {
                            callee: Box::new(Expr::Ident("small_func".into())),
                            args: vec![],
                        },
                    ),
                    Stmt::Let(
                        "y".into(),
                        None,
                        Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("x".into())),
                            rhs: Box::new(Expr::IntLit(32)),
                        },
                    ),
                    Stmt::Return(Some(Expr::Ident("y".into()))),
                ],
            ),
        ];

        super::optimize_with_profile(&mut decls, Some("main"), EmitProfile::Default);

        assert_eq!(decls.len(), 1);
        let Decl::Function { name, body, .. } = &decls[0] else {
            panic!("main not found")
        };
        assert_eq!(name, "main");
        assert_eq!(
            body,
            &vec![
                Stmt::Let("y".into(), None, Expr::IntLit(42)),
                Stmt::Return(Some(Expr::Ident("y".into()))),
            ]
        );
    }

    #[test]
    fn profiles_cannot_erase_an_uninitialized_ignored_argument() {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut module = crate::core_ir::UnifiedModule::new(vec![
                function(
                    "ignore",
                    vec![("unused".into(), Typ::Int)],
                    vec![Stmt::Return(Some(Expr::IntLit(42)))],
                ),
                function(
                    "main",
                    vec![],
                    vec![
                        Stmt::Declare("value".into(), Typ::Int),
                        Stmt::Return(Some(Expr::Call {
                            callee: Box::new(Expr::Ident("ignore".into())),
                            args: vec![Expr::Ident("value".into())],
                        })),
                    ],
                ),
            ]);
            optimize(&mut module, profile);
            assert!(
                crate::core_ir_verifier::check_execution_contracts(&module).is_err(),
                "{profile:?}"
            );
        }
    }
    #[test]
    fn profiles_preserve_global_reads_and_unresolved_annotation_obligations() {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for annotation in [
                Typ::Int,
                crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                    "associated output".into(),
                )
                .typ(),
            ] {
                let mut module = crate::core_ir::UnifiedModule::new(vec![
                    Decl::Global {
                        name: "answer".into(),
                        typ: Typ::Int,
                        mutable: false,
                        constant_value: false,
                        init: Some(Box::new(Expr::Block {
                            body: vec![Stmt::Declare("value".into(), annotation)],
                            result: Some(Box::new(Expr::Binary {
                                op: "*".into(),
                                lhs: Box::new(Expr::Ident("value".into())),
                                rhs: Box::new(Expr::IntLit(0)),
                            })),
                        })),
                    },
                    function(
                        "main",
                        vec![],
                        vec![Stmt::Return(Some(Expr::Ident("answer".into())))],
                    ),
                ]);
                optimize(&mut module, profile);
                assert!(
                    crate::core_ir_verifier::check_execution_contracts(&module).is_err(),
                    "{profile:?}"
                );
            }
            let mut module = crate::core_ir::UnifiedModule::new(vec![function(
                "main",
                vec![],
                vec![
                    Stmt::Declare(
                        "unused".into(),
                        crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                            "associated output".into(),
                        )
                        .typ(),
                    ),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            )]);
            optimize(&mut module, profile);
            assert!(
                !crate::typecheck::TypeChecker::new()
                    .typed_proof_functions(&module.decls)
                    .is_empty(),
                "{profile:?}"
            );
            assert!(crate::core_ir_verifier::check_execution_contracts(&module).is_err());
        }
    }
}

#[cfg(test)]
mod function_value_propagation_tests {
    use super::*;
    fn function(name: &str, params: Vec<(String, Typ)>, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params,
            body,
            ret: Typ::Int,
            type_params: vec![],
        }
    }
    fn call(name: &str) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args: vec![Expr::IntLit(42)],
        }
    }
    fn fixture(body: Vec<Stmt>) -> Vec<Decl> {
        vec![
            function(
                "identity",
                vec![("value".into(), Typ::Int)],
                vec![Stmt::Return(Some(Expr::Ident("value".into())))],
            ),
            function("main", vec![], body),
        ]
    }
    #[test]
    fn unannotated_function_value_alias_becomes_exact_direct_call() {
        let mut decls = fixture(vec![
            Stmt::Let("pick".into(), None, Expr::Ident("identity".into())),
            Stmt::Return(Some(call("pick"))),
        ]);
        propagate_function_values(&mut decls);
        let Decl::Function { body, .. } = &decls[1] else {
            panic!("main")
        };
        assert_eq!(body, &vec![Stmt::Return(Some(call("identity")))]);
    }
    #[test]
    fn numeric_casts_and_immutable_alias_chains_preserve_direct_targets() {
        let mut decls = fixture(vec![
            Stmt::Let("first".into(), None, Expr::Ident("identity".into())),
            Stmt::Let("pick".into(), None, Expr::Ident("first".into())),
            Stmt::Return(Some(Expr::Unary {
                op: "int-cast-u8".into(),
                expr: Box::new(call("pick")),
            })),
        ]);
        propagate_function_values(&mut decls);
        let Decl::Function { body, .. } = &decls[1] else {
            panic!("main")
        };
        assert_eq!(
            body,
            &vec![Stmt::Return(Some(Expr::Unary {
                op: "int-cast-u8".into(),
                expr: Box::new(call("identity"))
            }))]
        );
        let mut decls = fixture(vec![
            Stmt::Let("first".into(), None, Expr::Ident("identity".into())),
            Stmt::Let("pick".into(), None, Expr::Ident("first".into())),
            Stmt::Assign("first".into(), Expr::IntLit(7)),
            Stmt::Return(Some(call("pick"))),
        ]);
        propagate_function_values(&mut decls);
        let Decl::Function { body, .. } = &decls[1] else {
            panic!("main")
        };
        assert_eq!(body.len(), 4);
    }

    #[test]
    fn assignments_annotations_and_borrows_preserve_alias_storage() {
        for extra in [
            Stmt::Assign("pick".into(), Expr::IntLit(7)),
            Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::BORROW.into())),
                args: vec![Expr::Ident("pick".into())],
            }),
        ] {
            let mut decls = fixture(vec![
                Stmt::Let("pick".into(), None, Expr::Ident("identity".into())),
                extra,
                Stmt::Return(Some(call("pick"))),
            ]);
            propagate_function_values(&mut decls);
            let Decl::Function { body, .. } = &decls[1] else {
                panic!("main")
            };
            assert!(matches!(&body[0], Stmt::Let(_, None, Expr::Ident(_))));
        }
        let mut decls = fixture(vec![
            Stmt::Let(
                "pick".into(),
                Some(Typ::Int),
                Expr::Ident("identity".into()),
            ),
            Stmt::Return(Some(call("pick"))),
        ]);
        let original = decls.clone();
        propagate_function_values(&mut decls);
        assert_eq!(decls, original);
    }
    #[test]
    fn local_and_global_target_collisions_do_not_invent_function_proofs() {
        let mut decls = fixture(vec![
            Stmt::Let("identity".into(), None, Expr::IntLit(7)),
            Stmt::Let("pick".into(), None, Expr::Ident("identity".into())),
            Stmt::Return(Some(call("pick"))),
        ]);
        propagate_function_values(&mut decls);
        let Decl::Function { body, .. } = &decls[1] else {
            panic!("main")
        };
        assert_eq!(body.len(), 3);
        let mut decls = fixture(vec![
            Stmt::Let("pick".into(), None, Expr::Ident("identity".into())),
            Stmt::Return(Some(call("pick"))),
        ]);
        decls.push(Decl::Global {
            name: "identity".into(),
            typ: Typ::Int,
            init: Some(Box::new(Expr::IntLit(7))),
            mutable: false,
            constant_value: false,
        });
        propagate_function_values(&mut decls);
        let Decl::Function { body, .. } = &decls[1] else {
            panic!("main")
        };
        assert_eq!(body.len(), 2);
    }
}

#[cfg(test)]
mod array_copy_effect_tests {
    use super::*;

    #[test]
    fn fresh_array_storage_cannot_be_duplicated_or_discarded_as_a_pure_value() {
        for length in [0, 2] {
            let copy = Expr::Unary {
                op: crate::core_ir::array_copy::operation(length),
                expr: Box::new(Expr::Ident("source".into())),
            };
            assert!(expr_has_effects(&copy));
            assert!(!is_propagatable_const(&copy));
        }
        assert!(!expr_has_effects(&Expr::Ident("source".into())));
    }
}

#[cfg(test)]
mod integer_float_conversion_tests {
    use super::*;
    #[test]
    fn integer_float_folding_uses_direct_precision_and_retains_word32_boundary() {
        for (op, value, expected) in [
            (
                "int-to-float-f32",
                4611686293305294849_i64,
                f64::from(f32::from_bits(0x5e80_0001)),
            ),
            ("uint-to-float-f32", -1, 18446744073709551616.0),
            ("uint-to-float", -1, 18446744073709551616.0),
        ] {
            let cast = |value| Expr::Unary {
                op: op.into(),
                expr: Box::new(Expr::IntLit(value)),
            };
            assert_eq!(
                fold_expr_with_semantics(cast(value), IntegerSemantics::Word64),
                Expr::FloatLit(crate::core_ir::FloatVal(expected))
            );
            assert_eq!(
                fold_expr_with_semantics(cast(-1), IntegerSemantics::Word32),
                cast(-1)
            );
        }
    }
}

#[cfg(test)]
mod ieee_bit_folding_tests {
    use super::*;
    #[test]
    fn folding_keeps_nonfinite_constants_as_exact_integer_bit_constructors() {
        for (width, bits) in [
            (32, 0x7f800001u64),
            (32, 0xffc12345),
            (64, 0x7ff0000000000001),
            (64, 0xfff8123456789abc),
        ] {
            let value = Expr::Unary {
                op: format!("bits-to-float-f{width}"),
                expr: Box::new(Expr::IntLit(bits as i64)),
            };
            let folded = fold_expr(value);
            assert!(
                matches!(&folded,Expr::Unary {op,expr} if op == "bits-to-float-f64" && matches!(expr.as_ref(),Expr::IntLit(_)))
            );
            let extracted = fold_expr(Expr::Unary {
                op: format!("float-to-bits-f{width}"),
                expr: Box::new(folded),
            });
            assert_eq!(extracted, Expr::IntLit(bits as i64));
        }
    }
}
