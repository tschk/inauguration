//! Source-local initialization proof for explicit declarations without values.
//! This is separate from type inference and from MIR register definition checks.
use super::*;

#[derive(Clone)]
struct State {
    types: HashMap<String, Typ>,
    unreadable: HashSet<String>,
}

fn expression_has_declaration(value: &Expr) -> bool {
    if let Expr::Block { body, .. } | Expr::Closure { body, .. } = value
        && body_has_declaration(body)
    {
        return true;
    }
    let mut found = false;
    crate::core_ir::for_each_expr_child(value, &mut |value| {
        if !found {
            found = expression_has_declaration(value);
        }
    });
    found
}
fn body_has_declaration(body: &[Stmt]) -> bool {
    body.iter().any(|stmt| match stmt {
        Stmt::Declare(..) => true,
        Stmt::Let(_, _, value)
        | Stmt::Assign(_, value)
        | Stmt::Expr(value)
        | Stmt::Throw(value)
        | Stmt::Return(Some(value)) => expression_has_declaration(value),
        Stmt::FieldAssign { base, value, .. } => {
            expression_has_declaration(base) || expression_has_declaration(value)
        }
        Stmt::IndexAssign { base, index, value } => [base, index, value]
            .into_iter()
            .any(expression_has_declaration),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            expression_has_declaration(cond)
                || body_has_declaration(then_body)
                || body_has_declaration(else_body)
        }
        Stmt::Loop { cond, body, .. } => {
            cond.as_ref().is_some_and(expression_has_declaration) || body_has_declaration(body)
        }
        Stmt::Match { scrutinee, arms } => {
            expression_has_declaration(scrutinee)
                || arms.iter().any(|arm| {
                    arm.guard.as_ref().is_some_and(expression_has_declaration)
                        || body_has_declaration(&arm.body)
                })
        }
        Stmt::Try { body, catches } => {
            body_has_declaration(body) || catches.iter().any(|arm| body_has_declaration(&arm.body))
        }
        Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => false,
    })
}
fn has_declaration(decls: &[Decl]) -> bool {
    decls.iter().any(|decl| match decl {
        Decl::Function { body, .. } => body_has_declaration(body),
        Decl::Global {
            init: Some(value), ..
        } => expression_has_declaration(value),
        Decl::Class { methods, .. } => has_declaration(methods),
        _ => false,
    })
}

/// Preserve only bindings in the enclosing scope. A declaration in a child
/// branch/block cannot initialize or introduce a different enclosing binding.
fn join(state: &mut State, normal: &[State]) -> bool {
    if normal.is_empty() {
        return false;
    }
    state.unreadable = state
        .types
        .keys()
        .filter(|name| {
            normal
                .iter()
                .any(|branch| branch.unreadable.contains(*name))
        })
        .cloned()
        .collect();
    true
}
fn unsupported(owner: &str, feature: &str) -> String {
    format!(
        "definite-initialization-unsupported: `{owner}` contains {feature} with explicit declarations"
    )
}
fn read(name: &str, state: &State, owner: &str) -> Result<(), String> {
    if state.unreadable.contains(name) {
        Err(format!(
            "definite-initialization: read of `{name}` before assignment in `{owner}`"
        ))
    } else {
        Ok(())
    }
}
fn expression(
    value: &Expr,
    state: &mut State,
    context: &CompletionContext<'_, '_>,
    owner: &str,
) -> Result<bool, String> {
    match value {
        Expr::Ident(name) => {
            read(name, state, owner)?;
        }
        Expr::Closure { .. } => return Err(unsupported(owner, "an unlifted closure")),
        Expr::Block { body, result } => {
            let mut inner = state.clone();
            if !statements(body, &mut inner, context, owner)? {
                return Ok(false);
            }
            if let Some(value) = result
                && !expression(value, &mut inner, context, owner)?
            {
                return Ok(false);
            }
            return Ok(join(state, &[inner]));
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            if !expression(cond, state, context, owner)? {
                return Ok(false);
            }
            let mut left = state.clone();
            let mut right = state.clone();
            let left_normal = expression(then_expr, &mut left, context, owner)?;
            let right_normal = expression(else_expr, &mut right, context, owner)?;
            let mut normal = Vec::new();
            if left_normal && known_boolean(cond) != Some(false) {
                normal.push(left);
            }
            if right_normal && known_boolean(cond) != Some(true) {
                normal.push(right);
            }
            return Ok(join(state, &normal));
        }
        Expr::Binary { op, lhs, rhs } if matches!(op.trim(), "&&" | "||" | "and" | "or") => {
            if !expression(lhs, state, context, owner)? {
                return Ok(false);
            }
            let mut right = state.clone();
            let normal_right = expression(rhs, &mut right, context, owner)?;
            let is_and = matches!(op.trim(), "&&" | "and");
            let must_right = known_boolean(lhs) == Some(is_and);
            let skips_right = known_boolean(lhs) == Some(!is_and);
            let mut normal = Vec::new();
            if !must_right {
                normal.push(state.clone());
            }
            if !skips_right && normal_right {
                normal.push(right);
            }
            return Ok(join(state, &normal));
        }
        Expr::MatchValue { scrutinee, arms } => {
            if !expression(scrutinee, state, context, owner)? {
                return Ok(false);
            }
            let mut normal = Vec::new();
            for arm in arms {
                let mut branch = pattern_state(&arm.pattern, scrutinee, state, context, owner)?;
                if let Some(guard) = &arm.guard
                    && !expression(guard, &mut branch, context, owner)?
                {
                    continue;
                }
                if arm.guard.as_ref().and_then(known_boolean) != Some(false)
                    && expression(&arm.value, &mut branch, context, owner)?
                {
                    normal.push(branch);
                }
            }
            let (_, pending) = completion_match_with_fallthrough(
                scrutinee,
                arms.iter().map(|arm| {
                    (
                        arm.pattern.as_str(),
                        arm.guard.as_ref(),
                        CompletionArm::Value(&arm.value),
                    )
                }),
                &state.types,
                context,
            );
            if pending {
                normal.push(state.clone());
            }
            return Ok(join(state, &normal));
        }
        _ => {
            let mut result = Ok(true);
            crate::core_ir::for_each_expr_child(value, &mut |child| {
                if matches!(result, Ok(true)) {
                    result = expression(child, state, context, owner);
                }
            });
            if !result? {
                return Ok(false);
            }
        }
    }
    Ok(completion_expr(value, &state.types, context).has(Completion::NORMAL))
}
fn pattern_state(
    pattern: &str,
    scrutinee: &Expr,
    state: &State,
    context: &CompletionContext<'_, '_>,
    owner: &str,
) -> Result<State, String> {
    let typ = expr_type(scrutinee, context.facts, &state.types)
        .ok_or_else(|| unsupported(owner, "a match with unresolved scrutinee type"))?;
    let bindings = crate::core_ir::MatchPattern::parse(pattern)
        .and_then(|pattern| {
            pattern.typed_bindings(&typ, &|name| context.facts.structs.get(name).copied())
        })
        .map_err(|error| {
            format!("definite-initialization-unsupported: match pattern in `{owner}`: {error}")
        })?;
    let mut result = state.clone();
    for (name, typ) in bindings {
        result.unreadable.remove(&name);
        result.types.insert(name, typ);
    }
    Ok(result)
}
fn statements(
    body: &[Stmt],
    state: &mut State,
    context: &CompletionContext<'_, '_>,
    owner: &str,
) -> Result<bool, String> {
    for stmt in body {
        if !statement(stmt, state, context, owner)? {
            return Ok(false);
        }
    }
    Ok(true)
}
fn statement(
    stmt: &Stmt,
    state: &mut State,
    context: &CompletionContext<'_, '_>,
    owner: &str,
) -> Result<bool, String> {
    match stmt {
        Stmt::Declare(name, typ) => {
            state.types.insert(name.clone(), typ.clone());
            state.unreadable.insert(name.clone());
            Ok(true)
        }
        Stmt::Let(name, annotation, value) => {
            if !expression(value, state, context, owner)? {
                return Ok(false);
            }
            if let Some(typ) = annotation
                .clone()
                .or_else(|| expr_type(value, context.facts, &state.types))
            {
                state.types.insert(name.clone(), typ);
            }
            state.unreadable.remove(name);
            Ok(true)
        }
        Stmt::Assign(name, value) => {
            if !expression(value, state, context, owner)? {
                return Ok(false);
            }
            state.unreadable.remove(name);
            Ok(true)
        }
        Stmt::Expr(value) => expression(value, state, context, owner),
        Stmt::Return(value) => {
            if let Some(value) = value {
                expression(value, state, context, owner)?;
            }
            Ok(false)
        }
        Stmt::Break | Stmt::Continue => Ok(false),
        Stmt::Throw(_) | Stmt::Try { .. } | Stmt::Propagate => {
            Err(unsupported(owner, "exception control flow"))
        }
        Stmt::FieldAssign { base, value, .. } => {
            if !expression(base, state, context, owner)? {
                return Ok(false);
            }
            expression(value, state, context, owner)
        }
        Stmt::IndexAssign { base, index, value } => {
            for value in [base, index, value] {
                if !expression(value, state, context, owner)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            if !expression(cond, state, context, owner)? {
                return Ok(false);
            }
            let mut left = state.clone();
            let mut right = state.clone();
            let left_normal = statements(then_body, &mut left, context, owner)?;
            let right_normal = statements(else_body, &mut right, context, owner)?;
            let mut normal = Vec::new();
            if left_normal && known_boolean(cond) != Some(false) {
                normal.push(left);
            }
            if right_normal && known_boolean(cond) != Some(true) {
                normal.push(right);
            }
            Ok(join(state, &normal))
        }
        Stmt::Loop { cond, body, .. } => {
            let completion = completion_stmt(stmt, &mut state.types.clone(), context);
            if let Some(cond) = cond
                && !expression(cond, state, context, owner)?
            {
                return Ok(false);
            }
            // Validate the first possible iteration; no body write is assumed
            // to happen before an exit. In particular zero iterations exist.
            statements(body, &mut state.clone(), context, owner)?;
            Ok(completion.has(Completion::NORMAL))
        }
        Stmt::Match { scrutinee, arms } => {
            if !expression(scrutinee, state, context, owner)? {
                return Ok(false);
            }
            let mut normal = Vec::new();
            for arm in arms {
                let mut branch = pattern_state(&arm.pattern, scrutinee, state, context, owner)?;
                if let Some(guard) = &arm.guard
                    && !expression(guard, &mut branch, context, owner)?
                {
                    continue;
                }
                if arm.guard.as_ref().and_then(known_boolean) != Some(false)
                    && statements(&arm.body, &mut branch, context, owner)?
                {
                    normal.push(branch);
                }
            }
            let (_, pending) = completion_match_with_fallthrough(
                scrutinee,
                arms.iter().map(|arm| {
                    (
                        arm.pattern.as_str(),
                        arm.guard.as_ref(),
                        CompletionArm::Body(&arm.body),
                    )
                }),
                &state.types,
                context,
            );
            if pending {
                normal.push(state.clone());
            }
            Ok(join(state, &normal))
        }
    }
}

fn errors_in_declarations(decls: &[Decl]) -> (HashMap<String, String>, HashMap<String, String>) {
    if !has_declaration(decls) {
        return (HashMap::new(), HashMap::new());
    }
    let mut view = UnifiedModule::new(decls.to_vec());
    crate::core_bindings::normalize_decls(&mut view.decls);
    // Match production ordering while reusing only the structural class phase:
    // no folding, lowering or DCE may erase an obligation before this proof.
    if view
        .decls
        .iter()
        .any(|decl| matches!(decl, Decl::Class { .. }))
    {
        crate::lower_core::desugar_class_declarations(&mut view);
        crate::core_bindings::normalize_decls(&mut view.decls);
    }
    let functions: HashSet<_> = view
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let globals: HashSet<_> = view
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let nodes: HashSet<_> = functions.union(&globals).cloned().collect();
    let mut edges = HashMap::new();
    for decl in &mut view.decls {
        let name = match decl {
            Decl::Function { name, .. } | Decl::Global { name, .. } => name.clone(),
            _ => continue,
        };
        let references = std::cell::RefCell::new(HashSet::new());
        crate::core_bindings::rewrite_nominal_names(
            std::slice::from_mut(decl),
            &|_| None,
            &|name| {
                if nodes.contains(name) {
                    references.borrow_mut().insert(name.to_string());
                }
                None
            },
        );
        edges.insert(name, references.into_inner());
    }
    // Initialization consumes existing type facts, not a new whole-module
    // namespace policy. Normal verifier checks retain their own obligations.
    let facts = module_type_facts(&view);
    let context = CompletionContext::new(&view, &facts);
    let global_types: HashMap<_, _> = facts
        .globals
        .iter()
        .map(|(name, global)| (name.to_string(), global.typ.clone()))
        .collect();
    let mut errors = HashMap::new();
    for decl in &view.decls {
        let result = match decl {
            Decl::Function {
                name, params, body, ..
            } if body_has_declaration(body) => {
                let mut state = State {
                    types: global_types.clone(),
                    unreadable: HashSet::new(),
                };
                state.types.extend(params.iter().cloned());
                statements(body, &mut state, &context, name)
                    .err()
                    .map(|error| (name.clone(), error))
            }
            Decl::Global {
                name,
                init: Some(value),
                ..
            } if expression_has_declaration(value) => {
                let mut state = State {
                    types: global_types.clone(),
                    unreadable: HashSet::new(),
                };
                expression(value, &mut state, &context, &format!("global {name}"))
                    .err()
                    .map(|error| (name.clone(), error))
            }
            _ => None,
        };
        if let Some((name, error)) = result {
            errors.insert(name, error);
        }
    }
    // Use normalized lexical references, as the retained type-proof query does.
    // This is dependency propagation only: unrelated globals are not roots.
    loop {
        let mut changes = Vec::new();
        for (name, references) in &edges {
            let next = references
                .iter()
                .filter_map(|target| errors.get(target))
                .chain(errors.get(name))
                .min()
                .cloned();
            if next.as_ref() != errors.get(name)
                && let Some(error) = next
            {
                changes.push((name.clone(), error));
            }
        }
        if changes.is_empty() {
            break;
        }
        errors.extend(changes);
    }
    let global_errors = errors
        .iter()
        .filter(|(name, _)| globals.contains(*name))
        .map(|(name, error)| (name.clone(), error.clone()))
        .collect();
    errors.retain(|name, _| functions.contains(name));
    (errors, global_errors)
}
/// Errors keyed by real function names, including actual initializer/callable
/// dependencies. Callers choose executable/export roots; no dummy roots.
pub(crate) fn initialization_errors_by_function(decls: &[Decl]) -> HashMap<String, String> {
    errors_in_declarations(decls).0
}
pub(crate) fn initialization_errors_by_global(decls: &[Decl]) -> HashMap<String, String> {
    errors_in_declarations(decls).1
}
/// Direct whole-module emission treats all emitted functions/initializers as
/// roots. Owned executable selection instead uses the function error query.
pub(crate) fn check_definite_initialization(module: &UnifiedModule) -> Result<(), String> {
    let (functions, globals) = errors_in_declarations(&module.decls);
    let mut errors: Vec<_> = functions
        .into_iter()
        .map(|(name, error)| (format!("function {name}"), error))
        .chain(
            globals
                .into_iter()
                .map(|(name, error)| (format!("global {name}"), error)),
        )
        .collect();
    errors.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some((_, error)) = errors.into_iter().next() {
        Err(error)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn function(name: &str, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params: vec![("flag".into(), Typ::Bool)],
            ret: Typ::Int,
            body,
            type_params: vec![],
        }
    }
    fn declare() -> Stmt {
        Stmt::Declare("value".into(), Typ::Int)
    }
    fn assign() -> Stmt {
        Stmt::Assign("value".into(), Expr::IntLit(42))
    }
    fn result() -> Stmt {
        Stmt::Return(Some(Expr::Ident("value".into())))
    }
    fn condition(left: Vec<Stmt>, right: Vec<Stmt>) -> Stmt {
        Stmt::If {
            cond: Expr::Ident("flag".into()),
            then_body: left,
            else_body: right,
        }
    }
    fn module(body: Vec<Stmt>) -> UnifiedModule {
        UnifiedModule::new(vec![function("apply", body)])
    }
    fn check(body: Vec<Stmt>) -> Result<(), String> {
        check_definite_initialization(&module(body))
    }
    fn expect_uninitialized(body: Vec<Stmt>) {
        let error = check(body).unwrap_err();
        assert!(
            error.contains("definite-initialization: read of"),
            "{error}"
        );
    }
    #[test]
    fn literal_comparisons_preserve_initialization_without_width_assumptions() {
        let comparison = |op: &str, left, right| Expr::Binary {
            op: op.into(),
            lhs: Box::new(Expr::IntLit(left)),
            rhs: Box::new(Expr::IntLit(right)),
        };
        for (op, left, right, truth) in [("!=", 1, 0, true), ("==", 1, 0, false)] {
            let cond = comparison(op, left, right);
            assert_eq!(known_boolean(&cond), Some(truth));
            check(vec![
                declare(),
                Stmt::If {
                    cond: cond.clone(),
                    then_body: if truth { vec![assign()] } else { vec![] },
                    else_body: if truth { vec![] } else { vec![assign()] },
                },
                result(),
            ])
            .unwrap();
            expect_uninitialized(vec![
                declare(),
                Stmt::If {
                    cond,
                    then_body: if truth { vec![] } else { vec![assign()] },
                    else_body: if truth { vec![assign()] } else { vec![] },
                },
                result(),
            ]);
        }
        assert_eq!(known_boolean(&comparison("!=", 1_i64 << 32, 0)), None);
        assert_eq!(known_boolean(&Expr::Ident("flag".into())), None);
        let infinite = Stmt::Loop {
            kind: crate::core_ir::LoopKind::While,
            cond: Some(comparison("!=", 1, 0)),
            body: vec![],
        };
        check(vec![
            declare(),
            condition(vec![assign()], vec![infinite]),
            result(),
        ])
        .unwrap();
        check(vec![
            declare(),
            Stmt::Match {
                scrutinee: Expr::Ident("flag".into()),
                arms: vec![crate::core_ir::MatchArm {
                    pattern: "_".into(),
                    guard: Some(comparison("!=", 1, 0)),
                    body: vec![assign()],
                }],
            },
            result(),
        ])
        .unwrap();
    }
    #[test]
    fn declarations_accept_unused_linear_both_branch_and_returning_branch_paths() {
        for body in [
            vec![declare(), Stmt::Return(Some(Expr::IntLit(42)))],
            vec![declare(), assign(), result()],
            vec![
                declare(),
                condition(vec![assign()], vec![assign()]),
                result(),
            ],
            vec![
                declare(),
                condition(vec![Stmt::Return(Some(Expr::IntLit(42)))], vec![assign()]),
                result(),
            ],
        ] {
            let module = module(body);
            check_definite_initialization(&module).unwrap();
            let report = verify_module(
                &module,
                &VerifyOptions {
                    entry: None,
                    require_entry: false,
                },
            );
            assert!(report.ok, "{report:?}");
            crate::typecheck::TypeChecker::new()
                .check_module(&module)
                .unwrap();
        }
    }
    #[test]
    fn declarations_reject_read_partial_branch_zero_iteration_loop_and_self_read_rhs() {
        expect_uninitialized(vec![declare(), result()]);
        expect_uninitialized(vec![declare(), condition(vec![assign()], vec![]), result()]);
        expect_uninitialized(vec![
            declare(),
            Stmt::Loop {
                kind: crate::core_ir::LoopKind::While,
                cond: Some(Expr::Ident("flag".into())),
                body: vec![assign()],
            },
            result(),
        ]);
        expect_uninitialized(vec![
            declare(),
            Stmt::Assign(
                "value".into(),
                Expr::Binary {
                    op: "+".into(),
                    lhs: Box::new(Expr::Ident("value".into())),
                    rhs: Box::new(Expr::IntLit(1)),
                },
            ),
            result(),
        ]);
    }
    #[test]
    fn block_shadowing_cannot_initialize_the_enclosing_declaration() {
        let inner = Stmt::Expr(Expr::Block {
            body: vec![declare(), assign()],
            result: Some(Box::new(Expr::IntLit(42))),
        });
        expect_uninitialized(vec![declare(), inner, result()]);
        let unused_inner = Stmt::Expr(Expr::Block {
            body: vec![declare()],
            result: Some(Box::new(Expr::IntLit(42))),
        });
        check(vec![declare(), assign(), unused_inner, result()]).unwrap();
    }
    #[test]
    fn match_initialization_uses_existing_exhaustiveness_and_checks_guards() {
        let arm = |pattern: &str, guard: Option<Expr>, body: Vec<Stmt>| crate::core_ir::MatchArm {
            pattern: pattern.into(),
            guard,
            body,
        };
        let matched = |arms| Stmt::Match {
            scrutinee: Expr::Ident("flag".into()),
            arms,
        };
        check(vec![
            declare(),
            matched(vec![
                arm("true", None, vec![assign()]),
                arm("false", None, vec![assign()]),
            ]),
            result(),
        ])
        .unwrap();
        check(vec![
            declare(),
            matched(vec![
                arm("true", None, vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                arm("false", None, vec![assign()]),
            ]),
            result(),
        ])
        .unwrap();
        expect_uninitialized(vec![
            declare(),
            matched(vec![arm("true", None, vec![assign()])]),
            result(),
        ]);
        expect_uninitialized(vec![
            declare(),
            matched(vec![
                arm(
                    "true",
                    Some(Expr::Binary {
                        op: "==".into(),
                        lhs: Box::new(Expr::Ident("value".into())),
                        rhs: Box::new(Expr::IntLit(0)),
                    }),
                    vec![assign()],
                ),
                arm("_", None, vec![assign()]),
            ]),
            result(),
        ]);
    }
    #[test]
    fn conditional_and_short_circuit_assignments_do_not_initialize_skipped_paths() {
        let writes = || Expr::Block {
            body: vec![assign()],
            result: Some(Box::new(Expr::BoolLit(true))),
        };
        check(vec![
            declare(),
            Stmt::Expr(Expr::Conditional {
                cond: Box::new(Expr::Ident("flag".into())),
                then_expr: Box::new(writes()),
                else_expr: Box::new(writes()),
            }),
            result(),
        ])
        .unwrap();
        expect_uninitialized(vec![
            declare(),
            Stmt::Expr(Expr::Binary {
                op: "&&".into(),
                lhs: Box::new(Expr::Ident("flag".into())),
                rhs: Box::new(writes()),
            }),
            result(),
        ]);
    }
    #[test]
    fn declaration_bearing_exception_and_unlifted_closure_control_is_explicitly_unsupported() {
        for statement in [
            Stmt::Try {
                body: vec![],
                catches: vec![],
            },
            Stmt::Throw(Expr::IntLit(1)),
            Stmt::Propagate,
            Stmt::Expr(Expr::Closure {
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                captures: vec![],
            }),
        ] {
            let error = check(vec![
                declare(),
                statement,
                Stmt::Return(Some(Expr::IntLit(42))),
            ])
            .unwrap_err();
            assert!(
                error.starts_with("definite-initialization-unsupported:"),
                "{error}"
            );
        }
    }
    #[test]
    fn unused_invalid_global_is_not_a_function_root_but_real_initializer_dependencies_propagate() {
        let bad = Expr::Block {
            body: vec![declare()],
            result: Some(Box::new(Expr::Ident("value".into()))),
        };
        let declarations = vec![
            Decl::Global {
                name: "bad".into(),
                typ: Typ::Int,
                init: Some(Box::new(bad)),
                mutable: false,
                constant_value: false,
            },
            function("clean", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
            function(
                "reader",
                vec![Stmt::Return(Some(Expr::Ident("bad".into())))],
            ),
            function(
                "shadow",
                vec![
                    Stmt::Let("bad".into(), Some(Typ::Int), Expr::IntLit(42)),
                    Stmt::Return(Some(Expr::Ident("bad".into()))),
                ],
            ),
        ];
        let errors = initialization_errors_by_function(&declarations);
        assert!(errors.contains_key("reader"));
        assert!(!errors.contains_key("clean"));
        assert!(!errors.contains_key("shadow"));
        assert!(initialization_errors_by_global(&declarations).contains_key("bad"));
        assert!(check_definite_initialization(&UnifiedModule::new(declarations)).is_err());
    }
    #[test]
    fn declaration_annotations_preserve_generic_deferral_and_existing_metadata_type_facts() {
        let generic = Typ::Generic("T".into());
        check(vec![
            Stmt::Declare("unused".into(), generic),
            Stmt::Return(Some(Expr::IntLit(42))),
        ])
        .unwrap();
        let declarations = vec![
            function("linear", vec![declare(), assign(), result()]),
            function("bad", vec![declare(), result()]),
        ];
        let checked = crate::typecheck::TypeChecker::new().checked_inline_functions(&declarations);
        assert!(checked.contains("linear"));
        assert!(!checked.contains("bad"));
    }
    #[test]
    fn ordinary_programs_do_not_gain_new_type_or_control_flow_requirements() {
        let declarations = vec![function(
            "ordinary",
            vec![Stmt::Return(Some(Expr::Ident("missing".into())))],
        )];
        assert!(initialization_errors_by_function(&declarations).is_empty());
    }
}

#[cfg(test)]
mod class_tests {
    use super::*;
    fn class(body: Vec<Stmt>) -> Decl {
        Decl::Class {
            name: "Row".into(),
            fields: vec![],
            methods: vec![Decl::Function {
                name: "apply".into(),
                params: vec![],
                ret: Typ::Int,
                body,
                type_params: vec![],
            }],
            visibility: crate::core_ir::Visibility::Pub,
            extends: None,
            implements: vec![],
            type_params: vec![],
        }
    }
    #[test]
    fn class_declarations_use_the_existing_method_phase_without_losing_initialization_proof() {
        let bad = UnifiedModule::new(vec![class(vec![
            Stmt::Declare("value".into(), Typ::Int),
            Stmt::Return(Some(Expr::Ident("value".into()))),
        ])]);
        let errors = initialization_errors_by_function(&bad.decls);
        assert!(errors.contains_key("Row_apply"), "{errors:?}");
        assert!(
            check_definite_initialization(&bad)
                .unwrap_err()
                .contains("before assignment")
        );
        let good = UnifiedModule::new(vec![class(vec![
            Stmt::Declare("value".into(), Typ::Int),
            Stmt::Assign("value".into(), Expr::IntLit(42)),
            Stmt::Return(Some(Expr::Ident("value".into()))),
        ])]);
        check_definite_initialization(&good).unwrap();
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    #[test]
    fn a_nonreturning_callee_excludes_the_branch_from_the_normal_initialization_join() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "halt".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Loop {
                    kind: crate::core_ir::LoopKind::Infinite,
                    cond: None,
                    body: vec![],
                }],
                type_params: vec![],
            },
            Decl::Function {
                name: "apply".into(),
                params: vec![("flag".into(), Typ::Bool)],
                ret: Typ::Int,
                body: vec![
                    Stmt::Declare("value".into(), Typ::Int),
                    Stmt::If {
                        cond: Expr::Ident("flag".into()),
                        then_body: vec![Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("halt".into())),
                            args: vec![],
                        })],
                        else_body: vec![Stmt::Assign("value".into(), Expr::IntLit(42))],
                    },
                    Stmt::Return(Some(Expr::Ident("value".into()))),
                ],
                type_params: vec![],
            },
        ]);
        check_definite_initialization(&module).unwrap();
    }
}
