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

fn has_candidate_expr(expr: &Expr) -> bool {
    if let Expr::Block { body, result } = expr {
        return has_candidates(body) || result.as_deref().is_some_and(has_candidate_expr);
    }
    if matches!(expr, Expr::Closure { .. }) {
        return false;
    }
    let mut found = false;
    crate::core_ir::for_each_expr_child(expr, &mut |child| found |= has_candidate_expr(child));
    found
}

fn any_stmt_expression(stmt: &Stmt, predicate: &mut impl FnMut(&Expr) -> bool) -> bool {
    match stmt {
        Stmt::Let(_, _, value)
        | Stmt::Assign(_, value)
        | Stmt::Expr(value)
        | Stmt::Throw(value)
        | Stmt::Return(Some(value)) => predicate(value),
        Stmt::FieldAssign { base, value, .. } => predicate(base) || predicate(value),
        Stmt::IndexAssign { base, index, value } => {
            predicate(base) || predicate(index) || predicate(value)
        }
        Stmt::If { cond, .. } => predicate(cond),
        Stmt::Loop { cond, .. } => cond.as_ref().is_some_and(predicate),
        Stmt::Match { scrutinee, arms } => {
            predicate(scrutinee)
                || arms
                    .iter()
                    .filter_map(|arm| arm.guard.as_ref())
                    .any(predicate)
        }
        _ => false,
    }
}

fn has_candidates(body: &[Stmt]) -> bool {
    body.iter().any(|stmt| {
        any_stmt_expression(stmt, &mut has_candidate_expr)
            || match stmt {
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
            }
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
    let typ = typ.inference_type();
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
    return_type: Option<&Typ>,
) {
    if matches!(expr, Expr::Closure { .. }) {
        return;
    }
    if let Expr::MatchValue { scrutinee, arms } = expr {
        expression(
            checker,
            scrutinee,
            facts,
            env,
            candidates,
            constraints,
            return_type,
        );
        if let Some(typ) = checker.expr_type(scrutinee, facts, env) {
            for arm in arms {
                if let Some(inner) = checker.pattern_environment(&arm.pattern, &typ, facts, env) {
                    if let Some(guard) = &arm.guard {
                        expression(
                            checker,
                            guard,
                            facts,
                            &inner,
                            candidates,
                            constraints,
                            return_type,
                        );
                    }
                    expression(
                        checker,
                        &arm.value,
                        facts,
                        &inner,
                        candidates,
                        constraints,
                        return_type,
                    );
                }
            }
        }
        return;
    }
    if let Expr::Block { body, result } = expr {
        let mut inner = env.clone();
        let mut inner_candidates = candidates.clone();
        collect(
            checker,
            body,
            facts,
            &mut inner,
            &mut inner_candidates,
            constraints,
            return_type,
        );
        if let Some(result) = result {
            expression(
                checker,
                result,
                facts,
                &inner,
                &inner_candidates,
                constraints,
                return_type,
            );
        }
        return;
    }
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        expression(
            checker,
            child,
            facts,
            env,
            candidates,
            constraints,
            return_type,
        )
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
            Stmt::Declare(name, typ) => {
                env.insert(name.clone(), typ.clone());
            }
            Stmt::Let(name, typ, value) => {
                expression(
                    checker,
                    value,
                    facts,
                    env,
                    candidates,
                    constraints,
                    return_type,
                );
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
                expression(
                    checker,
                    value,
                    facts,
                    env,
                    candidates,
                    constraints,
                    return_type,
                );
                if let Some(Typ::Array(item) | Typ::Vector(item)) =
                    closure_inference::argument_type(checker, value, facts, env)
                {
                    constrain(name, *item, candidates, constraints);
                }
            }
            Stmt::Return(Some(value)) => {
                expression(
                    checker,
                    value,
                    facts,
                    env,
                    candidates,
                    constraints,
                    return_type,
                );
                if let (Some(Typ::Vector(item)), Expr::Ident(name)) = (return_type, value) {
                    constrain(name, (**item).clone(), candidates, constraints);
                }
            }
            Stmt::Expr(value) | Stmt::Throw(value) => expression(
                checker,
                value,
                facts,
                env,
                candidates,
                constraints,
                return_type,
            ),
            Stmt::IndexAssign { base, index, value } => {
                for expr in [base, index, value] {
                    expression(
                        checker,
                        expr,
                        facts,
                        env,
                        candidates,
                        constraints,
                        return_type,
                    );
                }
                if let Expr::Ident(name) = base
                    && let Some(typ) = closure_inference::argument_type(checker, value, facts, env)
                {
                    constrain(name, typ, candidates, constraints);
                }
            }
            Stmt::FieldAssign { base, value, .. } => {
                for expr in [base, value] {
                    expression(
                        checker,
                        expr,
                        facts,
                        env,
                        candidates,
                        constraints,
                        return_type,
                    );
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                expression(
                    checker,
                    cond,
                    facts,
                    env,
                    candidates,
                    constraints,
                    return_type,
                );
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
                    expression(
                        checker,
                        cond,
                        facts,
                        env,
                        candidates,
                        constraints,
                        return_type,
                    );
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
                expression(
                    checker,
                    scrutinee,
                    facts,
                    env,
                    candidates,
                    constraints,
                    return_type,
                );
                for arm in arms {
                    let mut inner = checker
                        .expr_type(scrutinee, facts, env)
                        .and_then(|typ| checker.pattern_environment(&arm.pattern, &typ, facts, env))
                        .unwrap_or_else(|| env.clone());
                    if let Some(guard) = &arm.guard {
                        expression(
                            checker,
                            guard,
                            facts,
                            &inner,
                            candidates,
                            constraints,
                            return_type,
                        );
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

fn apply_expression(expr: &mut Expr, constraints: &HashMap<String, Option<Typ>>) -> bool {
    if let Expr::Block { body, result } = expr {
        let mut changed = apply(body, constraints);
        if let Some(result) = result {
            changed |= apply_expression(result, constraints);
        }
        return changed;
    }
    if matches!(expr, Expr::Closure { .. }) {
        return false;
    }
    let mut changed = false;
    crate::core_ir::for_each_expr_child_mut(expr, &mut |child| {
        changed |= apply_expression(child, constraints)
    });
    changed
}

fn apply(body: &mut [Stmt], constraints: &HashMap<String, Option<Typ>>) -> bool {
    let mut changed = false;
    for stmt in body {
        match stmt {
            Stmt::Let(_, _, value)
            | Stmt::Assign(_, value)
            | Stmt::Expr(value)
            | Stmt::Throw(value)
            | Stmt::Return(Some(value)) => changed |= apply_expression(value, constraints),
            Stmt::FieldAssign { base, value, .. } => {
                changed |= apply_expression(base, constraints);
                changed |= apply_expression(value, constraints);
            }
            Stmt::IndexAssign { base, index, value } => {
                for expr in [base, index, value] {
                    changed |= apply_expression(expr, constraints);
                }
            }
            Stmt::If { cond, .. } => changed |= apply_expression(cond, constraints),
            Stmt::Loop {
                cond: Some(cond), ..
            } => changed |= apply_expression(cond, constraints),
            Stmt::Match { scrutinee, arms } => {
                changed |= apply_expression(scrutinee, constraints);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        changed |= apply_expression(guard, constraints);
                    }
                }
            }
            _ => {}
        }
        match stmt {
            Stmt::Let(name, Some(Typ::Vector(item)), _) if matches!(item.as_ref(), Typ::Generic(name) if name == "_") => {
                if let Some(Some(typ)) = constraints.get(name) {
                    **item = typ.clone();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_local_vector_candidates_are_found_and_updated() {
        let checker = TypeChecker::new();
        let facts = checker.collect_facts(&crate::core_ir::UnifiedModule::new(vec![]));
        let mut body = vec![Stmt::Let(
            "answer".into(),
            None,
            Expr::Block {
                body: vec![
                    Stmt::Let(
                        "items".into(),
                        Some(Typ::Vector(Box::new(Typ::Generic("_".into())))),
                        Expr::ArrayLit(vec![]),
                    ),
                    Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("vector-push".into())),
                        args: vec![Expr::Ident("items".into()), Expr::IntLit(42)],
                    }),
                ],
                result: Some(Box::new(Expr::IntLit(42))),
            },
        )];
        infer(
            &checker,
            &mut body,
            &facts,
            &HashMap::new(),
            Some(&Typ::Int),
        );
        let Stmt::Let(_, _, Expr::Block { body, .. }) = &body[0] else {
            panic!("block lost");
        };
        assert!(
            matches!(&body[0], Stmt::Let(_, Some(Typ::Vector(item)), _) if item.as_ref() == &Typ::Int)
        );
    }
}
