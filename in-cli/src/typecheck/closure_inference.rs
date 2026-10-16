use super::{Facts, TypeChecker, resolved_closure_type};
use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule};
use std::collections::{HashMap, HashSet};

pub(super) fn infer(
    checker: &TypeChecker,
    module: &mut UnifiedModule,
    closures: &HashMap<String, String>,
) -> bool {
    let generated: HashSet<_> = closures.values().cloned().collect();
    if generated.is_empty() {
        return false;
    }
    let mut facts = checker.collect_facts(module);
    facts.closures = closures.clone();
    let globals: HashMap<_, _> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect();
    let mut changed = false;
    // Caller constraints reach parent closures before their nested children.
    for closure_phase in [false, true] {
        for decl in module.decls.iter().rev() {
            if let Decl::Function { name, body, .. } = decl
                && generated.contains(name) == closure_phase
            {
                let mut env = globals.clone();
                if let Some((params, _)) = facts.functions.get(name) {
                    env.extend(params.iter().cloned());
                }
                body_constraints(
                    checker,
                    body,
                    &mut facts,
                    &generated,
                    &mut env,
                    &mut changed,
                );
            }
        }
    }
    if changed {
        for decl in &mut module.decls {
            if let Decl::Function { name, params, .. } = decl
                && generated.contains(name)
                && let Some((inferred, _)) = facts.functions.get(name)
            {
                *params = inferred.clone();
            }
        }
    }
    changed
}

pub(super) fn argument_type(
    checker: &TypeChecker,
    expr: &Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) -> Option<Typ> {
    if crate::core_ir::empty_sequence_literal(expr) {
        return None;
    }
    let mut complete = true;
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            let typ = argument_type(checker, scrutinee, facts, env)?;
            return crate::core_ir::match_value_type(
                arms,
                arms.iter().map(|arm| {
                    let inner = checker.pattern_environment(&arm.pattern, &typ, facts, env)?;
                    argument_type(checker, &arm.value, facts, &inner)
                }),
            )
            .filter(resolved_closure_type);
        }
        Expr::Block { body, result } => {
            let mut inner = env.clone();
            for stmt in body {
                if let Stmt::Let(name, typ, value) = stmt {
                    let typ = typ
                        .clone()
                        .or_else(|| argument_type(checker, value, facts, &inner));
                    inner.remove(name);
                    if let Some(typ) = typ {
                        inner.insert(name.clone(), typ);
                    }
                }
            }
            return result.as_deref().map_or(Some(Typ::Void), |value| {
                argument_type(checker, value, facts, &inner)
            });
        }
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => {
            return crate::core_ir::conditional_type(
                then_expr,
                argument_type(checker, then_expr, facts, env),
                else_expr,
                argument_type(checker, else_expr, facts, env),
            )
            .filter(resolved_closure_type);
        }
        Expr::Call { args, .. } => {
            for arg in args {
                complete &= argument_type(checker, arg, facts, env).is_some();
            }
        }
        _ => crate::core_ir::for_each_expr_child(expr, &mut |child| {
            complete &= argument_type(checker, child, facts, env).is_some();
        }),
    }
    if !complete {
        return None;
    }
    checker
        .expr_type(expr, facts, env)
        .map(|typ| typ.inference_type())
        .filter(resolved_closure_type)
}

fn expression_constraints(
    checker: &TypeChecker,
    expr: &Expr,
    facts: &mut Facts,
    generated: &HashSet<String>,
    env: &HashMap<String, Typ>,
    changed: &mut bool,
) {
    if matches!(expr, Expr::Closure { .. }) {
        return;
    }
    if let Expr::MatchValue { scrutinee, arms } = expr {
        expression_constraints(checker, scrutinee, facts, generated, env, changed);
        if let Some(typ) = argument_type(checker, scrutinee, facts, env) {
            for arm in arms {
                if let Some(inner) = checker.pattern_environment(&arm.pattern, &typ, facts, env) {
                    if let Some(guard) = &arm.guard {
                        expression_constraints(checker, guard, facts, generated, &inner, changed);
                    }
                    expression_constraints(checker, &arm.value, facts, generated, &inner, changed);
                }
            }
        }
        return;
    }
    if let Expr::Block { body, result } = expr {
        let mut inner = env.clone();
        body_constraints(checker, body, facts, generated, &mut inner, changed);
        if let Some(result) = result {
            expression_constraints(checker, result, facts, generated, &inner, changed);
        }
        return;
    }
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        expression_constraints(checker, child, facts, generated, env, changed)
    });
    let Expr::Call { callee, args } = expr else {
        return;
    };
    let (target, skip) = match callee.as_ref() {
        Expr::Ident(name) if generated.contains(name) => (name.clone(), 0),
        callee => {
            let captures = match callee {
                Expr::Ident(local) => match env.get(local) {
                    Some(Typ::Named(name)) => name,
                    _ => return,
                },
                Expr::StructInit { name, fields } if fields.is_empty() => name,
                _ => return,
            };
            if !facts
                .structs
                .get(captures)
                .is_some_and(|fields| fields.is_empty())
            {
                return;
            }
            let Some(target) = facts.closures.get(captures) else {
                return;
            };
            (target.clone(), 1)
        }
    };
    let actual: Vec<_> = args
        .iter()
        .map(|arg| argument_type(checker, arg, facts, env))
        .collect();
    let Some((params, _)) = facts.functions.get_mut(&target) else {
        return;
    };
    if params.len() != args.len() + skip {
        return;
    }
    for ((_, expected), actual) in params.iter_mut().skip(skip).zip(actual) {
        // Named generics require shared type-variable constraints, not independent slots.
        if matches!(expected, Typ::Generic(name) if name == "_")
            && let Some(actual) = actual
        {
            *expected = actual;
            *changed = true;
        }
    }
}

fn body_constraints(
    checker: &TypeChecker,
    body: &[Stmt],
    facts: &mut Facts,
    generated: &HashSet<String>,
    env: &mut HashMap<String, Typ>,
    changed: &mut bool,
) {
    for stmt in body {
        match stmt {
            Stmt::Let(name, typ, value) => {
                expression_constraints(checker, value, facts, generated, env, changed);
                if let Some(typ) = typ
                    .clone()
                    .or_else(|| argument_type(checker, value, facts, env))
                {
                    env.insert(name.clone(), typ);
                } else {
                    env.remove(name);
                }
            }
            Stmt::Assign(_, value)
            | Stmt::Return(Some(value))
            | Stmt::Expr(value)
            | Stmt::Throw(value) => {
                expression_constraints(checker, value, facts, generated, env, changed)
            }
            Stmt::FieldAssign { base, value, .. } => {
                for value in [base, value] {
                    expression_constraints(checker, value, facts, generated, env, changed);
                }
            }
            Stmt::IndexAssign { base, index, value } => {
                for value in [base, index, value] {
                    expression_constraints(checker, value, facts, generated, env, changed);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                expression_constraints(checker, cond, facts, generated, env, changed);
                body_constraints(
                    checker,
                    then_body,
                    facts,
                    generated,
                    &mut env.clone(),
                    changed,
                );
                body_constraints(
                    checker,
                    else_body,
                    facts,
                    generated,
                    &mut env.clone(),
                    changed,
                );
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    expression_constraints(checker, cond, facts, generated, env, changed);
                }
                body_constraints(checker, body, facts, generated, &mut env.clone(), changed);
            }
            Stmt::Match { scrutinee, arms } => {
                expression_constraints(checker, scrutinee, facts, generated, env, changed);
                let Some(typ) = argument_type(checker, scrutinee, facts, env) else {
                    continue;
                };
                for arm in arms {
                    if let Some(mut inner) =
                        checker.pattern_environment(&arm.pattern, &typ, facts, env)
                    {
                        if let Some(guard) = &arm.guard {
                            expression_constraints(
                                checker, guard, facts, generated, &inner, changed,
                            );
                        }
                        body_constraints(checker, &arm.body, facts, generated, &mut inner, changed);
                    }
                }
            }
            Stmt::Try { body, catches } => {
                body_constraints(checker, body, facts, generated, &mut env.clone(), changed);
                for catch in catches {
                    if let Some(mut inner) = checker.pattern_environment(
                        &catch.pattern,
                        &Typ::Named("Any".into()),
                        facts,
                        env,
                    ) {
                        body_constraints(
                            checker,
                            &catch.body,
                            facts,
                            generated,
                            &mut inner,
                            changed,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_argument_type_uses_its_local_scope() {
        let checker = TypeChecker::new();
        let facts = checker.collect_facts(&UnifiedModule::new(vec![]));
        let env = HashMap::from([("value".into(), Typ::Bool)]);
        let expr = Expr::Block {
            body: vec![Stmt::Let("value".into(), None, Expr::IntLit(42))],
            result: Some(Box::new(Expr::Ident("value".into()))),
        };
        assert_eq!(argument_type(&checker, &expr, &facts, &env), Some(Typ::Int));
        assert_eq!(env.get("value"), Some(&Typ::Bool));
    }
}
