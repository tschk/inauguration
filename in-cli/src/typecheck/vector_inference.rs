use super::{Facts, TypeChecker, closure_inference, resolved_closure_type};
use crate::core_ir::{Expr, Stmt, Typ};
use std::collections::{HashMap, HashSet};

pub(super) fn infer(
    checker: &TypeChecker,
    body: &mut [Stmt],
    facts: &Facts,
    initial: &HashMap<String, Typ>,
    return_type: Option<&Typ>,
) {
    while has_candidates(body) {
        let mut candidates = HashSet::new();
        let mut constraints = HashMap::new();
        collect(
            checker,
            body,
            facts,
            &mut initial.clone(),
            &mut candidates,
            &mut constraints,
            return_type,
        );
        if !apply(body, &constraints) {
            break;
        }
    }
}

fn has_candidates(body: &[Stmt]) -> bool {
    body.iter().any(|stmt| match stmt {
        Stmt::Let(_, Some(Typ::Vector(item)), _) => {
            matches!(item.as_ref(), Typ::Generic(name) if name == "_")
        }
        Stmt::If {
            then_body,
            else_body,
            ..
        } => has_candidates(then_body) || has_candidates(else_body),
        Stmt::Loop { body, .. } => has_candidates(body),
        Stmt::Match { arms, .. } => arms.iter().any(|arm| has_candidates(&arm.body)),
        Stmt::Try { body, catches } => {
            has_candidates(body) || catches.iter().any(|catch| has_candidates(&catch.body))
        }
        _ => false,
    })
}

fn constrain(
    name: &str,
    typ: Typ,
    candidates: &HashSet<String>,
    constraints: &mut HashMap<String, Option<Typ>>,
) {
    if !candidates.contains(name) || !resolved_closure_type(&typ) {
        return;
    }
    let typ = typ.canonical();
    constraints
        .entry(name.into())
        .and_modify(|existing| {
            if existing.as_ref() != Some(&typ) {
                *existing = None;
            }
        })
        .or_insert(Some(typ));
}

fn expression(
    checker: &TypeChecker,
    expr: &Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    candidates: &HashSet<String>,
    constraints: &mut HashMap<String, Option<Typ>>,
) {
    if matches!(expr, Expr::Closure { .. }) {
        return;
    }
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        expression(checker, child, facts, env, candidates, constraints)
    });
    let Expr::Call { callee, args } = expr else {
        return;
    };
    let Expr::Ident(callee) = callee.as_ref() else {
        return;
    };
    if let Some((params, _)) = facts.functions.get(callee) {
        for ((_, expected), arg) in params.iter().zip(args) {
            if let (Typ::Vector(item), Expr::Ident(name)) = (expected, arg) {
                constrain(name, (**item).clone(), candidates, constraints);
            }
        }
    }
    if matches!(
        callee.as_str(),
        "#receiver-method:extend" | "Vec::extend" | "vector-extend"
    ) && let [destination, source] = args.as_slice()
    {
        let source_type = closure_inference::argument_type(checker, source, facts, env);
        let destination_type = closure_inference::argument_type(checker, destination, facts, env);
        if let Some((params, _)) = facts.functions.get("extend")
            && let [(_, Typ::Vector(item)), (_, expected)] = params.as_slice()
            && source_type.as_ref().is_some_and(|actual| {
                super::is_compatible(expected, actual)
                    || expected.accepts_sequence_literal(source, actual)
            })
        {
            if let Expr::Ident(name) = destination {
                constrain(name, (**item).clone(), candidates, constraints);
            }
            return;
        }
        if let (Expr::Ident(name), Some(Typ::Vector(item) | Typ::Array(item))) =
            (destination, source_type)
        {
            constrain(name, *item, candidates, constraints);
        }
        if let (Expr::Ident(name), Some(Typ::Vector(item))) = (source, destination_type) {
            constrain(name, *item, candidates, constraints);
        }
    }
    if matches!(
        callee.as_str(),
        "#receiver-method:push" | "Vec::push" | "vector-push"
    ) && let [Expr::Ident(name), value] = args.as_slice()
        && matches!(env.get(name), Some(Typ::Vector(_)))
        && let Some(typ) = closure_inference::argument_type(checker, value, facts, env)
    {
        if let Some((params, _)) = facts.functions.get("push")
            && let [(_, Typ::Vector(item)), (_, expected)] = params.as_slice()
            && (super::is_compatible(expected, &typ)
                || expected.accepts_sequence_literal(value, &typ))
        {
            constrain(name, (**item).clone(), candidates, constraints);
        } else {
            constrain(name, typ, candidates, constraints);
        }
    }
}

fn collect(
    checker: &TypeChecker,
    body: &[Stmt],
    facts: &Facts,
    env: &mut HashMap<String, Typ>,
    candidates: &mut HashSet<String>,
    constraints: &mut HashMap<String, Option<Typ>>,
    return_type: Option<&Typ>,
) {
    for stmt in body {
        match stmt {
            Stmt::Let(name, typ, value) => {
                expression(checker, value, facts, env, candidates, constraints);
                if matches!(typ, Some(Typ::Vector(item)) if matches!(item.as_ref(), Typ::Generic(name) if name == "_"))
                {
                    candidates.insert(name.clone());
                    if let Some(Typ::Array(item) | Typ::Vector(item)) =
                        closure_inference::argument_type(checker, value, facts, env)
                    {
                        constrain(name, *item, candidates, constraints);
                    }
                }
                if let Some(typ) = typ
                    .clone()
                    .or_else(|| closure_inference::argument_type(checker, value, facts, env))
                {
                    env.insert(name.clone(), typ);
                }
            }
            Stmt::Assign(name, value) => {
                expression(checker, value, facts, env, candidates, constraints);
                if let Some(Typ::Array(item) | Typ::Vector(item)) =
                    closure_inference::argument_type(checker, value, facts, env)
                {
                    constrain(name, *item, candidates, constraints);
                }
            }
            Stmt::Return(Some(value)) => {
                expression(checker, value, facts, env, candidates, constraints);
                if let (Some(Typ::Vector(item)), Expr::Ident(name)) = (return_type, value) {
                    constrain(name, (**item).clone(), candidates, constraints);
                }
            }
            Stmt::Expr(value) | Stmt::Throw(value) => {
                expression(checker, value, facts, env, candidates, constraints)
            }
            Stmt::IndexAssign { base, index, value } => {
                for expr in [base, index, value] {
                    expression(checker, expr, facts, env, candidates, constraints);
                }
                if let Expr::Ident(name) = base
                    && let Some(typ) = closure_inference::argument_type(checker, value, facts, env)
                {
                    constrain(name, typ, candidates, constraints);
                }
            }
            Stmt::FieldAssign { base, value, .. } => {
                for expr in [base, value] {
                    expression(checker, expr, facts, env, candidates, constraints);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                expression(checker, cond, facts, env, candidates, constraints);
                for branch in [then_body, else_body] {
                    collect(
                        checker,
                        branch,
                        facts,
                        &mut env.clone(),
                        candidates,
                        constraints,
                        return_type,
                    );
                }
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    expression(checker, cond, facts, env, candidates, constraints);
                }
                collect(
                    checker,
                    body,
                    facts,
                    &mut env.clone(),
                    candidates,
                    constraints,
                    return_type,
                );
            }
            Stmt::Match { scrutinee, arms } => {
                expression(checker, scrutinee, facts, env, candidates, constraints);
                for arm in arms {
                    let mut inner = checker
                        .expr_type(scrutinee, facts, env)
                        .and_then(|typ| checker.pattern_environment(&arm.pattern, &typ, facts, env))
                        .unwrap_or_else(|| env.clone());
                    if let Some(guard) = &arm.guard {
                        expression(checker, guard, facts, &inner, candidates, constraints);
                    }
                    collect(
                        checker,
                        &arm.body,
                        facts,
                        &mut inner,
                        candidates,
                        constraints,
                        return_type,
                    );
                }
            }
            Stmt::Try { body, catches } => {
                collect(
                    checker,
                    body,
                    facts,
                    &mut env.clone(),
                    candidates,
                    constraints,
                    return_type,
                );
                for catch in catches {
                    let mut inner = checker
                        .pattern_environment(&catch.pattern, &Typ::Named("Any".into()), facts, env)
                        .unwrap_or_else(|| env.clone());
                    collect(
                        checker,
                        &catch.body,
                        facts,
                        &mut inner,
                        candidates,
                        constraints,
                        return_type,
                    );
                }
            }
            _ => {}
        }
    }
}

fn apply(body: &mut [Stmt], constraints: &HashMap<String, Option<Typ>>) -> bool {
    let mut changed = false;
    for stmt in body {
        match stmt {
            Stmt::Let(name, Some(Typ::Vector(item)), _) if matches!(item.as_ref(), Typ::Generic(name) if name == "_") => {
                if let Some(Some(typ)) = constraints.get(name) {
                    *item = Box::new(typ.clone());
                    changed = true;
                }
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                changed |= apply(then_body, constraints);
                changed |= apply(else_body, constraints);
            }
            Stmt::Loop { body, .. } => changed |= apply(body, constraints),
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    changed |= apply(&mut arm.body, constraints);
                }
            }
            Stmt::Try { body, catches } => {
                changed |= apply(body, constraints);
                for catch in catches {
                    changed |= apply(&mut catch.body, constraints);
                }
            }
            _ => {}
        }
    }
    changed
}
