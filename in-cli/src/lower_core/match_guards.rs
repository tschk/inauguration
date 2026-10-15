use crate::core_ir::{Decl, Expr, MatchArm, MatchPattern, Stmt, Typ, UnifiedModule};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

pub(crate) fn prepared_native_module(module: &UnifiedModule) -> Cow<'_, UnifiedModule> {
    if !module.decls.iter().any(decl_has_guard)
        && !super::value_spills::has_value_spills(&module.decls)
        && !super::constant_values::has_named_constants(&module.decls)
    {
        return Cow::Borrowed(module);
    }
    let mut lowered = module.clone();
    lower_guarded_decls(&mut lowered.decls);
    let mut names = crate::core_bindings::module_names(&lowered.decls);
    reserve_members(&lowered.decls, &mut names);
    if native_pattern_shapes_valid(&mut lowered.decls) {
        lower_members(&mut lowered.decls, &names, &mut 0, true);
    }
    super::lower_named_constants(&mut lowered.decls);
    super::lower_value_spills(&mut lowered.decls);
    crate::core_bindings::normalize_decls(&mut lowered.decls);
    crate::typecheck::TypeChecker::new().resolve_complements(&mut lowered);
    Cow::Owned(lowered)
}

pub(crate) fn lower_guarded_decls(decls: &mut [Decl]) {
    if !decls.iter().any(decl_has_guard) {
        return;
    }
    let mut names = crate::core_bindings::module_names(decls);
    reserve_members(decls, &mut names);
    lower_members(decls, &names, &mut 0, false);
}

fn reserve_members(decls: &[Decl], names: &mut HashSet<String>) {
    for decl in decls {
        match decl {
            Decl::Function {
                name, params, body, ..
            } => {
                names.insert(name.clone());
                names.extend(params.iter().map(|(name, _)| name.clone()));
                crate::core_bindings::reserve_body(body, names);
            }
            Decl::Class { methods, .. } => reserve_members(methods, names),
            Decl::Global {
                init: Some(expr), ..
            } => crate::core_bindings::reserve_expr(expr, names),
            _ => {}
        }
    }
}

fn decl_has_guard(decl: &Decl) -> bool {
    match decl {
        Decl::Function { body, .. } => body.iter().any(stmt_has_guard),
        Decl::Class { methods, .. } => methods.iter().any(decl_has_guard),
        Decl::Global {
            init: Some(expr), ..
        } => expr_has_guard(expr),
        _ => false,
    }
}

fn expr_has_guard(expr: &Expr) -> bool {
    if matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if crate::core_ir::is_compiler_callee(name)))
    {
        return true;
    }
    if matches!(expr, Expr::Unary { op, .. } if op.starts_with("numeric-cast-")) {
        return true;
    }

    if let Expr::Block { body, result } = expr {
        return body.iter().any(stmt_has_guard) || result.as_deref().is_some_and(expr_has_guard);
    } else if let Expr::Closure { body, .. } = expr {
        return body.iter().any(stmt_has_guard);
    }
    let mut found = false;
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        if !found {
            found = expr_has_guard(child);
        }
    });
    found
}

fn stmt_has_guard(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Expr(expr)
        | Stmt::Return(Some(expr))
        | Stmt::Throw(expr) => expr_has_guard(expr),
        Stmt::IndexAssign { base, index, value } => {
            [base, index, value].into_iter().any(expr_has_guard)
        }
        Stmt::FieldAssign { base, value, .. } => [base, value].into_iter().any(expr_has_guard),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            expr_has_guard(cond)
                || then_body.iter().any(stmt_has_guard)
                || else_body.iter().any(stmt_has_guard)
        }
        Stmt::Loop { cond, body, .. } => {
            cond.as_ref().is_some_and(expr_has_guard) || body.iter().any(stmt_has_guard)
        }
        Stmt::Match { scrutinee, arms } => {
            expr_has_guard(scrutinee)
                || arms.iter().any(|arm| {
                    arm.guard.is_some()
                        || complex_pattern(&arm.pattern)
                        || arm.body.iter().any(stmt_has_guard)
                })
        }
        Stmt::Try { body, catches } => {
            body.iter().any(stmt_has_guard)
                || catches
                    .iter()
                    .any(|catch| catch.body.iter().any(stmt_has_guard))
        }
        Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => false,
    }
}

fn lower_members(
    decls: &mut [Decl],
    names: &HashSet<String>,
    counter: &mut usize,
    native_patterns: bool,
) {
    for decl in decls {
        match decl {
            Decl::Function { body, .. } => lower_body(body, names, counter, native_patterns),
            Decl::Class { methods, .. } => lower_members(methods, names, counter, native_patterns),
            Decl::Global {
                init: Some(expr), ..
            } => lower_expr(expr, names, counter, native_patterns),
            _ => {}
        }
    }
}

fn lower_expr(
    expr: &mut Expr,
    names: &HashSet<String>,
    counter: &mut usize,
    native_patterns: bool,
) {
    if let Expr::Block { body, result } = expr {
        lower_body(body, names, counter, native_patterns);
        if let Some(result) = result {
            lower_expr(result, names, counter, native_patterns);
        }
    } else if let Expr::Closure { body, .. } = expr {
        lower_body(body, names, counter, native_patterns);
    } else {
        crate::core_ir::for_each_expr_child_mut(expr, &mut |child| {
            lower_expr(child, names, counter, native_patterns)
        });
    }
}

fn lower_body(
    body: &mut Vec<Stmt>,
    names: &HashSet<String>,
    counter: &mut usize,
    native_patterns: bool,
) {
    let mut out = Vec::with_capacity(body.len());
    for mut stmt in std::mem::take(body) {
        match &mut stmt {
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::Expr(expr)
            | Stmt::Return(Some(expr))
            | Stmt::Throw(expr) => lower_expr(expr, names, counter, native_patterns),
            Stmt::IndexAssign { base, index, value } => {
                for expr in [base, index, value] {
                    lower_expr(expr, names, counter, native_patterns);
                }
            }
            Stmt::FieldAssign { base, value, .. } => {
                for expr in [base, value] {
                    lower_expr(expr, names, counter, native_patterns);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                lower_expr(cond, names, counter, native_patterns);
                lower_body(then_body, names, counter, native_patterns);
                lower_body(else_body, names, counter, native_patterns);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    lower_expr(cond, names, counter, native_patterns);
                }
                lower_body(body, names, counter, native_patterns);
            }
            Stmt::Match { scrutinee, arms } => {
                lower_expr(scrutinee, names, counter, native_patterns);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        lower_expr(guard, names, counter, native_patterns);
                    }
                    lower_body(&mut arm.body, names, counter, native_patterns);
                }
            }
            Stmt::Try { body, catches } => {
                lower_body(body, names, counter, native_patterns);
                for catch in catches {
                    lower_body(&mut catch.body, names, counter, native_patterns);
                }
            }
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
        }
        if let Stmt::Match { scrutinee, arms } = stmt {
            if arms.iter().any(|arm| {
                arm.guard.is_some() || (native_patterns && complex_pattern(&arm.pattern))
            }) {
                out.extend(expand_match(
                    scrutinee,
                    arms,
                    names,
                    counter,
                    native_patterns,
                ));
            } else {
                out.push(Stmt::Match { scrutinee, arms });
            }
        } else {
            out.push(stmt);
        }
    }
    *body = out;
}

fn expand_match(
    scrutinee: Expr,
    arms: Vec<MatchArm>,
    names: &HashSet<String>,
    counter: &mut usize,
    native_patterns: bool,
) -> Vec<Stmt> {
    while ["__match_value_", "__match_done_"]
        .iter()
        .any(|prefix| names.contains(&format!("{prefix}{counter}")))
    {
        *counter += 1;
    }
    let value = format!("__match_value_{counter}");
    let done = format!("__match_done_{counter}");
    *counter += 1;
    let mut out = vec![
        Stmt::Let(value.clone(), None, scrutinee),
        Stmt::Let(done.clone(), Some(Typ::Bool), Expr::BoolLit(false)),
    ];
    // One gate per arm avoids duplicating the remaining arms on every guard failure.
    for arm in arms {
        let mut selected = vec![Stmt::Assign(done.clone(), Expr::BoolLit(true))];
        selected.extend(arm.body);
        let body = match arm.guard {
            Some(cond) => vec![Stmt::If {
                cond,
                then_body: selected,
                else_body: vec![],
            }],
            None => selected,
        };
        let test = (native_patterns && complex_pattern(&arm.pattern))
            .then(|| MatchPattern::parse(&arm.pattern).ok())
            .flatten()
            .and_then(|pattern| pattern_test(&pattern, Expr::Ident(value.clone())));
        let matched = if let Some((cond, mut bindings)) = test {
            bindings.extend(body);
            vec![Stmt::If {
                cond,
                then_body: bindings,
                else_body: vec![],
            }]
        } else {
            vec![Stmt::Match {
                scrutinee: Expr::Ident(value.clone()),
                arms: vec![
                    MatchArm {
                        pattern: arm.pattern,
                        guard: None,
                        body,
                    },
                    MatchArm {
                        pattern: "_".into(),
                        guard: None,
                        body: vec![],
                    },
                ],
            }]
        };
        out.push(Stmt::If {
            cond: Expr::Unary {
                op: "!".into(),
                expr: Box::new(Expr::Ident(done.clone())),
            },
            then_body: matched,
            else_body: vec![],
        });
    }
    out
}

fn complex_pattern(pattern: &str) -> bool {
    matches!(
        MatchPattern::parse(pattern),
        Ok(MatchPattern::ArrayPat(_) | MatchPattern::TuplePat(_) | MatchPattern::StructPat { .. })
    )
}

fn binary(op: &str, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op: op.into(),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn pattern_test(pattern: &MatchPattern, value: Expr) -> Option<(Expr, Vec<Stmt>)> {
    let mut bindings = Vec::new();
    let cond = match pattern {
        MatchPattern::WildPat | MatchPattern::RestPat => Expr::BoolLit(true),
        MatchPattern::IdentPat(name) => {
            bindings.push(Stmt::Let(name.clone(), None, value));
            Expr::BoolLit(true)
        }
        MatchPattern::IntPat(n) => binary("==", value, Expr::IntLit(*n)),
        MatchPattern::BoolPat(b) => binary("==", value, Expr::BoolLit(*b)),
        MatchPattern::StringPat(s) => Expr::Call {
            callee: Box::new(Expr::Ident("str-eq".into())),
            args: vec![value, Expr::StringLit(s.clone())],
        },
        MatchPattern::StructPat { fields, .. } => {
            let mut cond = Expr::BoolLit(true);
            for (name, pattern) in fields {
                let (test, vars) = pattern_test(
                    pattern,
                    Expr::Field {
                        base: Box::new(value.clone()),
                        name: name.clone(),
                    },
                )?;
                cond = binary("&&", cond, test);
                bindings.extend(vars);
            }
            cond
        }
        // shortcut: tuple values currently use array storage; named heterogeneous tuples need field-aware lowering.
        MatchPattern::ArrayPat(items) | MatchPattern::TuplePat(items) => {
            let rests: Vec<_> = items
                .iter()
                .enumerate()
                .filter(|(_, p)| matches!(p, MatchPattern::RestPat))
                .map(|(i, _)| i)
                .collect();
            if rests.len() > 1 {
                return None;
            }
            let len = Expr::Call {
                callee: Box::new(Expr::Ident("array-len".into())),
                args: vec![value.clone()],
            };
            let mut cond = binary(
                if rests.is_empty() { "==" } else { ">=" },
                len.clone(),
                Expr::IntLit((items.len() - rests.len()) as i64),
            );
            for (i, pattern) in items.iter().enumerate() {
                if matches!(pattern, MatchPattern::RestPat) {
                    continue;
                }
                let index = if rests.first().is_some_and(|rest| i > *rest) {
                    binary("-", len.clone(), Expr::IntLit((items.len() - i) as i64))
                } else {
                    Expr::IntLit(i as i64)
                };
                let (test, vars) = pattern_test(
                    pattern,
                    Expr::Index {
                        base: Box::new(value.clone()),
                        index: Box::new(index),
                    },
                )?;
                // Shape checks must short-circuit before any indexed read.
                cond = binary("&&", cond, test);
                bindings.extend(vars);
            }
            cond
        }
    };
    Some((cond, bindings))
}

// Direct backend callers also need shape proof before patterns become field/index reads.
fn native_pattern_shapes_valid(decls: &mut [Decl]) -> bool {
    let checker = crate::typecheck::TypeChecker::new();
    let facts = checker.collect_decl_facts(decls);
    fn body_valid(
        body: &mut [Stmt],
        env: &mut HashMap<String, Typ>,
        checker: &crate::typecheck::TypeChecker,
        facts: &crate::typecheck::Facts,
    ) -> bool {
        for stmt in body {
            match stmt {
                Stmt::Let(name, typ, value) => {
                    if let Some(typ) = typ.clone().or_else(|| checker.expr_type(value, facts, env))
                    {
                        env.insert(name.clone(), typ);
                    } else {
                        env.remove(name);
                    }
                }
                Stmt::Match { scrutinee, arms } => {
                    let typ = checker.expr_type(scrutinee, facts, env);
                    for arm in arms {
                        let mut scope = env.clone();
                        if let Ok(pattern) = MatchPattern::parse(&arm.pattern) {
                            if let Some(typ) = &typ {
                                match pattern
                                    .typed_bindings(typ, &|name| facts.pattern_fields(name))
                                {
                                    Ok(bindings) => {
                                        scope.extend(bindings);
                                        arm.pattern =
                                            pattern_source(&typed_pattern(&pattern, typ, facts));
                                    }
                                    Err(_) if complex_pattern(&arm.pattern) => return false,
                                    Err(_) => {}
                                }
                            } else if complex_pattern(&arm.pattern) {
                                return false;
                            }
                        }
                        if !body_valid(&mut arm.body, &mut scope, checker, facts) {
                            return false;
                        }
                    }
                }
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    if !body_valid(then_body, &mut env.clone(), checker, facts)
                        || !body_valid(else_body, &mut env.clone(), checker, facts)
                    {
                        return false;
                    }
                }
                Stmt::Loop { body, .. } => {
                    if !body_valid(body, &mut env.clone(), checker, facts) {
                        return false;
                    }
                }
                Stmt::Try { body, catches } => {
                    if !body_valid(body, &mut env.clone(), checker, facts) {
                        return false;
                    }
                    for catch in catches {
                        let mut scope = env.clone();
                        if let Ok(pattern) = MatchPattern::parse(&catch.pattern)
                            && let Ok(bindings) = pattern
                                .typed_bindings(&Typ::Named("Any".into()), &|name| {
                                    facts.pattern_fields(name)
                                })
                        {
                            scope.extend(bindings);
                        }
                        if !body_valid(&mut catch.body, &mut scope, checker, facts) {
                            return false;
                        }
                    }
                }
                _ => {}
            }
        }
        true
    }
    let globals: HashMap<_, _> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect();
    decls.iter_mut().all(|decl| match decl {
        Decl::Function { params, body, .. } => {
            let mut env = globals.clone();
            env.extend(params.iter().cloned());
            body_valid(body, &mut env, &checker, &facts)
        }
        // shortcut: require class/closure lifting first; validate their scopes when native emission supports them directly.
        Decl::Class { .. } => false,
        Decl::Global {
            init: Some(expr), ..
        } if matches!(expr.as_ref(), Expr::Closure { .. }) => false,
        _ => true,
    })
}

fn pattern_source(pattern: &MatchPattern) -> String {
    match pattern {
        MatchPattern::IntPat(value) => value.to_string(),
        MatchPattern::BoolPat(value) => value.to_string(),
        MatchPattern::StringPat(value) => format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('\"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t")
        ),
        MatchPattern::IdentPat(name) => name.clone(),
        MatchPattern::WildPat => "_".into(),
        MatchPattern::RestPat => "..".into(),
        MatchPattern::StructPat { name, fields } => format!(
            "{name} {{ {} }}",
            fields
                .iter()
                .map(|(name, pattern)| format!("{name}: {}", pattern_source(pattern)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MatchPattern::TuplePat(items) => format!(
            "({})",
            items
                .iter()
                .map(pattern_source)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MatchPattern::ArrayPat(items) => format!(
            "[{}]",
            items
                .iter()
                .map(pattern_source)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn value_pattern_test<'a>(
    source: &str,
    typ: &Typ,
    fields: &impl Fn(&str) -> Option<&'a [(String, Typ)]>,
    value: Expr,
) -> Result<(Expr, Vec<Stmt>), String> {
    let pattern = MatchPattern::parse(source)?;
    let bindings = pattern.typed_bindings(typ, fields)?;
    if matches!(&pattern,MatchPattern::TuplePat(items) if items.is_empty())
        && typ.canonical() == Typ::Void
    {
        return Ok((Expr::BoolLit(true), vec![]));
    }
    let pattern = typed_pattern_with_schema(&pattern, typ, fields);
    let (test, mut statements) = pattern_test(&pattern, value)
        .ok_or_else(|| "unsupported value match pattern".to_string())?;
    for stmt in &mut statements {
        if let Stmt::Let(name, typ, _) = stmt {
            *typ = bindings
                .iter()
                .find(|(binding, _)| binding == name)
                .map(|(_, typ)| typ.clone());
        }
    }
    Ok((test, statements))
}

fn typed_pattern(
    pattern: &MatchPattern,
    typ: &Typ,
    facts: &crate::typecheck::Facts,
) -> MatchPattern {
    typed_pattern_with_schema(pattern, typ, &|name| facts.pattern_fields(name))
}

fn typed_pattern_with_schema<'a>(
    pattern: &MatchPattern,
    typ: &Typ,
    schema_lookup: &impl Fn(&str) -> Option<&'a [(String, Typ)]>,
) -> MatchPattern {
    match (pattern, typ) {
        (MatchPattern::TuplePat(items), Typ::Void) if items.is_empty() => MatchPattern::WildPat,
        (MatchPattern::TuplePat(items), Typ::Named(name)) => {
            let Some(schema) = schema_lookup(name) else {
                return pattern.clone();
            };
            let rest = items
                .iter()
                .position(|item| matches!(item, MatchPattern::RestPat));
            let fields = items
                .iter()
                .enumerate()
                .filter(|(_, item)| !matches!(item, MatchPattern::RestPat))
                .map(|(index, item)| {
                    let index = if rest.is_some_and(|rest| index > rest) {
                        schema.len() - (items.len() - index)
                    } else {
                        index
                    };
                    let field = format!("_{index}");
                    let field_typ = &schema
                        .iter()
                        .find(|(name, _)| name == &field)
                        .expect("validated tuple field")
                        .1;
                    (
                        field,
                        typed_pattern_with_schema(item, field_typ, schema_lookup),
                    )
                })
                .collect();
            MatchPattern::StructPat {
                name: name.clone(),
                fields,
            }
        }
        (MatchPattern::StructPat { name, fields }, _) => {
            let Some(schema) = schema_lookup(name) else {
                return pattern.clone();
            };
            MatchPattern::StructPat {
                name: name.clone(),
                fields: fields
                    .iter()
                    .map(|(field, item)| {
                        let field_typ = &schema
                            .iter()
                            .find(|(name, _)| name == field)
                            .expect("validated struct field")
                            .1;
                        (
                            field.clone(),
                            typed_pattern_with_schema(item, field_typ, schema_lookup),
                        )
                    })
                    .collect(),
            }
        }
        (
            MatchPattern::ArrayPat(items) | MatchPattern::TuplePat(items),
            Typ::Array(item) | Typ::Vector(item),
        ) => {
            let items = items
                .iter()
                .map(|pattern| typed_pattern_with_schema(pattern, item, schema_lookup))
                .collect();
            if matches!(pattern, MatchPattern::ArrayPat(_)) {
                MatchPattern::ArrayPat(items)
            } else {
                MatchPattern::TuplePat(items)
            }
        }
        _ => pattern.clone(),
    }
}

#[cfg(test)]
mod value_pattern_tests {
    use super::*;

    #[test]
    fn unit_statement_pattern_forces_native_preparation_without_spills() {
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Match {
                scrutinee: Expr::UnitLit,
                arms: vec![MatchArm {
                    pattern: "()".into(),
                    guard: None,
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                }],
            }],
        }]);
        let prepared = prepared_native_module(&module);
        assert!(matches!(prepared, Cow::Owned(_)));
        let Decl::Function { body, .. } = &prepared.decls[0] else {
            panic!()
        };
        let Stmt::Match { arms, .. } = &body[0] else {
            panic!("{body:?}")
        };
        assert_eq!(arms[0].pattern, "_");
        assert_eq!(arms[0].body, vec![Stmt::Return(Some(Expr::IntLit(42)))]);
    }

    #[test]
    fn unit_pattern_never_uses_sequence_storage() {
        assert_eq!(
            value_pattern_test("()", &Typ::Void, &|_| None, Expr::UnitLit).unwrap(),
            (Expr::BoolLit(true), vec![])
        );
        assert!(value_pattern_test("()", &Typ::Int, &|_| None, Expr::IntLit(0)).is_err());
    }

    #[test]
    fn tuple_value_patterns_use_typed_field_bindings() {
        let schemas = HashMap::from([(
            "Pair".to_string(),
            vec![("_0".into(), Typ::Int), ("_1".into(), Typ::Bool)],
        )]);
        let fields = |name: &str| schemas.get(name).map(Vec::as_slice);
        let (_, bindings) = value_pattern_test(
            "(left, right)",
            &Typ::Named("Pair".into()),
            &fields,
            Expr::Ident("saved".into()),
        )
        .unwrap();
        assert!(
            matches!(&bindings[0], Stmt::Let(name, Some(Typ::Int), Expr::Field {name:field,..}) if name=="left" && field=="_0")
        );
        assert!(
            matches!(&bindings[1], Stmt::Let(name, Some(Typ::Bool), Expr::Field {name:field,..}) if name=="right" && field=="_1")
        );
        assert!(value_pattern_test("(left, right)", &Typ::Int, &fields, Expr::IntLit(42)).is_err());
    }

    #[test]
    fn array_value_pattern_checks_shape_before_element_reads() {
        let (test, bindings) = value_pattern_test(
            "[20, tail]",
            &Typ::Array(Box::new(Typ::Int)),
            &|_| None,
            Expr::Ident("saved".into()),
        )
        .unwrap();
        let Expr::Binary { op, lhs, .. } = test else {
            panic!()
        };
        assert_eq!(op, "&&");
        assert!(matches!(lhs.as_ref(),Expr::Binary {op,..} if op=="&&"));
        assert!(
            matches!(&bindings[0],Stmt::Let(name,Some(Typ::Int),Expr::Index {..}) if name=="tail")
        );
    }
}
