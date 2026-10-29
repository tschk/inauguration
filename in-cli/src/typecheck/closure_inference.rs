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
            if let Decl::Struct { name, fields, .. } = decl
                && closures.contains_key(name)
                && let Some(inferred) = facts.structs.get(name)
            {
                *fields = inferred.clone();
            }
        }
    }
    changed
}

/// The current record ABI supports immutable scalar capture snapshots only.
/// Mutable/by-reference environments need a real shared-place ABI before dispatch.
pub(super) fn proven_environments(
    module: &mut UnifiedModule,
    facts: &Facts,
    closures: &HashMap<String, String>,
) -> HashSet<String> {
    fn scalar(typ: &Typ) -> bool {
        matches!(typ, Typ::Int | Typ::Float | Typ::Bool)
            || matches!(typ, Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some())
            || crate::core_ir::rust_floats::kind(typ).is_some()
            || crate::core_ir::rust_callables::signature_of(typ).is_some_and(|signature| {
                signature.ordinary_rust_form()
                    && crate::core_ir::type_proofs::failure_in_type(typ).is_none()
            })
    }
    fn borrowed(expr: &Expr) -> Option<&str> {
        match expr {
            Expr::Unary { op, expr }
                if crate::core_ir::rust_references::value_kind(op).is_some() =>
            {
                crate::core_opt::place_root(expr)
            }
            Expr::Call { callee, args } if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::BORROW) => {
                args.first().and_then(crate::core_opt::place_root)
            }
            _ => None,
        }
    }
    let mut candidates: HashSet<_> = closures
        .keys()
        .filter(|name| {
            facts.structs.get(*name).is_some_and(|fields| {
                !fields.is_empty() && fields.iter().all(|(_, typ)| scalar(typ))
            })
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        return candidates;
    }
    let globals: HashSet<_> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let mut seen = HashSet::new();
    let mut vetoed = HashSet::new();
    let mut readonly = HashSet::new();
    for decl in &mut module.decls {
        if let Decl::Global {
            init: Some(init), ..
        } = decl
        {
            // A global constructor has no enclosing local snapshot proof.
            crate::core_opt::map_expr_mut(init, &mut |expr| {
                if let Expr::StructInit { name, .. } = expr
                    && closures.contains_key(name)
                {
                    vetoed.insert(name.clone());
                }
            });
        }
        let Decl::Function {
            name, params, body, ..
        } = decl
        else {
            continue;
        };
        let mut assigned = HashSet::new();
        for stmt in body.iter() {
            crate::core_opt::collect_assigned_names(stmt, &mut assigned);
        }
        let mut borrows = HashSet::new();
        for stmt in body.iter_mut() {
            crate::core_opt::map_stmt_exprs_mut(stmt, &mut |expr| {
                if let Some(name) = borrowed(expr) {
                    borrows.insert(name.to_owned());
                }
            });
        }
        if let Some((receiver, _)) = params.first()
            && !assigned.contains(receiver)
            && !borrows.contains(receiver)
        {
            readonly.insert(name.clone());
        }
        for stmt in body.iter_mut() {
            crate::core_opt::map_stmt_exprs_mut(stmt, &mut |expr| {
                let Expr::StructInit { name, fields } = expr else {
                    return;
                };
                if !closures.contains_key(name) {
                    return;
                }
                seen.insert(name.clone());
                let schema = facts.structs.get(name);
                let mut field_names = HashSet::new();
                let safe = schema.is_some_and(|schema| {
                    fields.len() == schema.len()
                        && fields.iter().all(|(field, value)| {
                            field_names.insert(field)
                                && schema.iter().any(|(name, _)| name == field)
                                && matches!(value, Expr::Ident(source) if !assigned.contains(source) && !borrows.contains(source) && !globals.contains(source))
                        })
                });
                if !safe {
                    vetoed.insert(name.clone());
                }
            });
        }
    }
    candidates.retain(|schema| {
        seen.contains(schema)
            && !vetoed.contains(schema)
            && closures
                .get(schema)
                .is_some_and(|target| readonly.contains(target))
    });
    candidates
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
                match stmt {
                    Stmt::Declare(name, typ) => {
                        inner.insert(name.clone(), typ.clone());
                    }
                    Stmt::Let(name, typ, value) => {
                        let typ = typ
                            .clone()
                            .or_else(|| argument_type(checker, value, facts, &inner));
                        inner.remove(name);
                        if let Some(typ) = typ {
                            inner.insert(name.clone(), typ);
                        }
                    }
                    _ => {}
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
    if let Expr::StructInit { name, fields } = expr
        && facts.closures.contains_key(name)
    {
        let actual: Vec<_> = fields
            .iter()
            .map(|(field, value)| (field, argument_type(checker, value, facts, env)))
            .collect();
        if let Some(schema) = facts.structs.get_mut(name) {
            for (field, actual) in actual {
                let Some(actual) = actual else { continue };
                let Some((_, expected)) = schema.iter_mut().find(|(name, _)| name == field) else {
                    continue;
                };
                let unknown = matches!(expected, Typ::Generic(name) if name == "_")
                    || matches!(expected, Typ::Named(name) if name == "Any" || name == "_");
                let stronger_float = *expected == crate::core_ir::rust_floats::weak_type()
                    && crate::core_ir::rust_floats::kind(&actual).is_some();
                if (unknown || stronger_float) && *expected != actual {
                    *expected = actual;
                    *changed = true;
                }
            }
        }
    }
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
            Stmt::Declare(name, typ) => {
                env.insert(name.clone(), typ.clone());
            }
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

    #[test]
    fn generated_capture_fields_keep_source_float_width_without_mutating_user_schemas() {
        use crate::core_ir::rust_floats::{self, Kind};
        let function = |name: &str, params, ret, body| Decl::Function {
            name: name.into(),
            params,
            ret,
            body,
            type_params: vec![],
        };
        let mut module = UnifiedModule::new(vec![
            Decl::Struct {
                name: "captures".into(),
                fields: vec![("value".into(), Typ::Named("Any".into()))],
                type_params: vec![],
            },
            Decl::Struct {
                name: "User".into(),
                fields: vec![("value".into(), Typ::Named("Any".into()))],
                type_params: vec![],
            },
            function(
                "closure",
                vec![("self".into(), Typ::Named("captures".into()))],
                rust_floats::typ(Kind::F32),
                vec![Stmt::Return(Some(Expr::Field {
                    base: Box::new(Expr::Ident("self".into())),
                    name: "value".into(),
                }))],
            ),
            function(
                "main",
                vec![],
                Typ::Int,
                vec![
                    Stmt::Let(
                        "outer".into(),
                        Some(rust_floats::typ(Kind::F32)),
                        rust_floats::literal("1", Some(Kind::F32)),
                    ),
                    Stmt::Expr(Expr::StructInit {
                        name: "captures".into(),
                        fields: vec![("value".into(), Expr::Ident("outer".into()))],
                    }),
                    Stmt::Expr(Expr::StructInit {
                        name: "User".into(),
                        fields: vec![("value".into(), Expr::Ident("outer".into()))],
                    }),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ),
        ]);
        let closures = HashMap::from([("captures".into(), "closure".into())]);
        assert!(infer(&TypeChecker::new(), &mut module, &closures));
        let Decl::Struct { fields, .. } = &module.decls[0] else {
            panic!()
        };
        assert_eq!(fields[0].1, rust_floats::typ(Kind::F32));
        let Decl::Struct { fields, .. } = &module.decls[1] else {
            panic!()
        };
        assert_eq!(fields[0].1, Typ::Named("Any".into()));
        assert!(!infer(&TypeChecker::new(), &mut module, &closures));
    }

    #[test]
    fn capture_snapshot_proof_requires_readonly_unborrowed_scalar_sources() {
        use crate::core_ir::rust_floats::{self, Kind};
        let scalar = rust_floats::typ(Kind::F32);
        let constructor = || {
            Stmt::Expr(Expr::StructInit {
                name: "caps".into(),
                fields: vec![("value".into(), Expr::Ident("outer".into()))],
            })
        };
        let function = |name: &str, params, body| Decl::Function {
            name: name.into(),
            params,
            ret: scalar.clone(),
            body,
            type_params: vec![],
        };
        let base = UnifiedModule::new(vec![
            Decl::Struct {
                name: "caps".into(),
                fields: vec![("value".into(), scalar.clone())],
                type_params: vec![],
            },
            function(
                "closure",
                vec![("self".into(), Typ::Named("caps".into()))],
                vec![Stmt::Return(Some(Expr::Field {
                    base: Box::new(Expr::Ident("self".into())),
                    name: "value".into(),
                }))],
            ),
            function(
                "main",
                vec![],
                vec![
                    Stmt::Let(
                        "outer".into(),
                        Some(scalar.clone()),
                        Expr::FloatLit(crate::core_ir::FloatVal(1.0)),
                    ),
                    constructor(),
                ],
            ),
        ]);
        let closures = HashMap::from([("caps".into(), "closure".into())]);
        let check = |mut module: UnifiedModule| {
            let facts = TypeChecker::new().collect_facts(&module);
            proven_environments(&mut module, &facts, &closures).contains("caps")
        };
        assert!(check(base.clone()));
        for change in 0..8 {
            let mut module = base.clone();
            match change {
                0 => {
                    if let Decl::Function { body, .. } = &mut module.decls[2] {
                        body.push(Stmt::Assign(
                            "outer".into(),
                            Expr::FloatLit(crate::core_ir::FloatVal(2.0)),
                        ));
                    }
                }
                1 => {
                    if let Decl::Function { body, .. } = &mut module.decls[1] {
                        body.push(Stmt::FieldAssign {
                            base: Expr::Ident("self".into()),
                            name: "value".into(),
                            value: Expr::FloatLit(crate::core_ir::FloatVal(2.0)),
                        });
                    }
                }
                2 | 3 => {
                    if let Decl::Function { body, .. } = &mut module.decls[2] {
                        let value = Expr::Ident("outer".into());
                        let borrow = if change == 2 {
                            Expr::Call {
                                callee: Box::new(Expr::Ident(crate::core_ir::BORROW.into())),
                                args: vec![value],
                            }
                        } else {
                            Expr::Unary {
                                op: crate::core_ir::rust_references::SHARED_VALUE.into(),
                                expr: Box::new(value),
                            }
                        };
                        body.push(Stmt::Let("alias".into(), None, borrow));
                    }
                }
                4 => {
                    if let Decl::Struct { fields, .. } = &mut module.decls[0] {
                        fields[0].1 = Typ::Named("Any".into());
                    }
                }
                5 => {
                    if let Decl::Function { body, .. } = &mut module.decls[1] {
                        body.push(Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident(crate::core_ir::BORROW.into())),
                            args: vec![Expr::Field {
                                base: Box::new(Expr::Ident("self".into())),
                                name: "value".into(),
                            }],
                        }));
                    }
                }
                6 => {
                    if let Decl::Function { body, .. } = &mut module.decls[1] {
                        body.push(Stmt::FieldAssign {
                            base: Expr::Field {
                                base: Box::new(Expr::Ident("self".into())),
                                name: "value".into(),
                            },
                            name: "nested".into(),
                            value: Expr::FloatLit(crate::core_ir::FloatVal(2.0)),
                        });
                    }
                }
                7 => {
                    // A local constructor reading a global is not a stable local snapshot.
                    if let Decl::Function { body, .. } = &mut module.decls[2] {
                        body.remove(0);
                    }
                    module.decls.push(Decl::Global {
                        name: "outer".into(),
                        typ: scalar.clone(),
                        init: Some(Box::new(Expr::FloatLit(crate::core_ir::FloatVal(1.0)))),
                        mutable: true,
                        constant_value: false,
                    });
                }
                _ => unreachable!(),
            }
            assert!(!check(module), "unsafe capture variant {change}");
        }
    }
}
