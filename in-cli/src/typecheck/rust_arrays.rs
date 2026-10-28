//! Resolve fixed-array value transfers while source lengths and element kinds remain owned.
use super::{Facts, ResolvePolicy, TypeChecker};
use crate::core_ir::{
    Expr, Typ, UnifiedModule, array_copy, rust_arrays, type_proofs::ResolutionFailure,
};
use std::collections::HashMap;

fn fixed(typ: &Typ) -> Option<rust_arrays::FixedArray> {
    match typ {
        Typ::Named(name) => rust_arrays::decode(name),
        _ => None,
    }
}
fn scalar(typ: &Typ) -> bool {
    matches!(typ, Typ::Int | Typ::Float | Typ::Bool)
        || matches!(typ, Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some() || crate::core_ir::rust_floats::is_proof(name))
}
fn fresh(expr: &Expr) -> bool {
    matches!(expr, Expr::Unary { op, .. } if rust_arrays::value_length(op).is_some())
}
fn failure(expr: &mut Expr, reason: ResolutionFailure) {
    *expr = Expr::Ident(reason.symbol());
}

/// Constraints only strengthen flexible default integer elements; conflicts are sticky.
#[derive(Default)]
pub(super) struct Constraints {
    values: HashMap<String, Typ>,
    changed: bool,
    failure: Option<ResolutionFailure>,
}
impl Constraints {
    pub(super) fn bind(&mut self, name: &str, typ: &Typ) {
        if fixed(typ).is_none() {
            return;
        }
        let value = self.values.get(name).cloned();
        let merged = value.as_ref().map_or_else(
            || Some(typ.clone()),
            |old| join_types(&[Some(old.clone()), Some(typ.clone())]),
        );
        if let Some(merged) = merged {
            if let Some(proof) = crate::core_ir::type_proofs::failure_in_type(&merged) {
                self.record(proof);
            } else if value.as_ref() != Some(&merged) {
                self.values.insert(name.into(), merged);
                self.changed = true;
            }
        }
    }
    pub(super) fn get(&self, name: &str) -> Option<Typ> {
        self.values.get(name).cloned()
    }
    pub(super) fn reset_changed(&mut self) {
        self.changed = false;
    }
    pub(super) fn changed(&self) -> bool {
        self.changed
    }
    pub(super) fn failure(&self) -> Option<ResolutionFailure> {
        self.failure.clone()
    }
    fn record(&mut self, proof: ResolutionFailure) {
        self.failure =
            crate::core_ir::type_proofs::prefer_failure(self.failure.take(), Some(proof));
    }
}
pub(super) fn merge_element(
    left: &Typ,
    right: &Typ,
    length: usize,
) -> Result<Typ, ResolutionFailure> {
    if left == right {
        return Ok(left.clone());
    }
    if length == 0 && *left == Typ::Void {
        return Ok(right.clone());
    }
    if length == 0 && *right == Typ::Void {
        return Ok(left.clone());
    }
    if let (Some(left), Some(right)) = (fixed(left), fixed(right)) {
        if left.length != right.length {
            return Err(ResolutionFailure::Invalid(
                "nested fixed-array lengths do not match".into(),
            ));
        }
        return Ok(rust_arrays::typ(
            merge_element(&left.element, &right.element, left.length)?,
            left.length,
        ));
    }
    if let Some(joined) = super::rust_floats::join_types(&[Some(left.clone()), Some(right.clone())])
    {
        return match crate::core_ir::type_proofs::failure_in_type(&joined) {
            Some(proof) => Err(proof),
            None => Ok(joined),
        };
    }
    let integer = |typ: &Typ| {
        matches!(typ, Typ::Int)
            || matches!(typ,Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some())
    };
    if *left == Typ::Int && integer(right) {
        return Ok(right.clone());
    }
    if *right == Typ::Int && integer(left) {
        return Ok(left.clone());
    }
    Err(if scalar(left) && scalar(right) {
        ResolutionFailure::Invalid("fixed-array element types do not match".into())
    } else {
        ResolutionFailure::Unsupported(
            "fixed-array element compatibility requires source type resolution".into(),
        )
    })
}
/// None delegates non-array joins to the ordinary shared type rules.
pub(super) fn join_types(types: &[Option<Typ>]) -> Option<Typ> {
    if !types.iter().flatten().any(|typ| fixed(typ).is_some()) {
        return None;
    }
    let mut result: Option<rust_arrays::FixedArray> = None;
    for typ in types {
        let Some(typ) = typ else {
            return Some(
                ResolutionFailure::Unsupported("fixed-array branch identity is unresolved".into())
                    .typ(),
            );
        };
        if let Some(proof) = crate::core_ir::type_proofs::failure_in_type(typ) {
            return Some(proof.typ());
        }
        let Some(array) = fixed(typ) else {
            return Some(
                if scalar(typ) || matches!(typ, Typ::String | Typ::Vector(_) | Typ::Void) {
                    ResolutionFailure::Invalid("fixed-array branch types do not match".into())
                } else {
                    ResolutionFailure::Unsupported(
                        "fixed-array branch identity is unresolved".into(),
                    )
                }
                .typ(),
            );
        };
        result = Some(match result {
            None => array,
            Some(previous) => {
                if previous.length != array.length {
                    return Some(
                        ResolutionFailure::Invalid(
                            "fixed-array branch lengths do not match".into(),
                        )
                        .typ(),
                    );
                }
                let element = match merge_element(&previous.element, &array.element, array.length) {
                    Ok(element) => element,
                    Err(proof) => return Some(proof.typ()),
                };
                rust_arrays::FixedArray {
                    element,
                    length: array.length,
                }
            }
        });
    }
    result.map(|array| rust_arrays::typ(array.element, array.length))
}
fn report(expr: &mut Expr, proof: ResolutionFailure, facts: &Facts, policy: ResolvePolicy) {
    if policy == ResolvePolicy::RustArrayInference {
        facts.array_constraints.borrow_mut().record(proof);
    } else if policy == ResolvePolicy::RustFixedArrays || proof.is_invalid() {
        failure(expr, proof);
    }
}
/// Only result edges are traversed here; the shared resolver owns all scope/body walks.
fn constrain_result(expr: &Expr, expected: &Typ, facts: &Facts, env: &HashMap<String, Typ>) {
    match expr {
        Expr::Ident(name) => {
            // An expectation cannot manufacture source ownership for a scalar,
            // vector, unknown generic, or nominal carrier.
            if let Some(original) = env.get(name) {
                if fixed(original).is_none() {
                    return;
                }
                facts.array_constraints.borrow_mut().bind(name, original);
            } else {
                return;
            }
            facts.array_constraints.borrow_mut().bind(name, expected);
        }
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => {
            constrain_result(then_expr, expected, facts, env);
            constrain_result(else_expr, expected, facts, env);
        }
        Expr::MatchValue { arms, .. } => {
            for arm in arms {
                constrain_result(&arm.value, expected, facts, env);
            }
        }
        Expr::Block {
            result: Some(result),
            ..
        } => constrain_result(result, expected, facts, env),
        _ => {}
    }
}
/// A Rust string literal is a shared str reference, even though ordinary Core
/// expression typing retains its historical String runtime carrier.
pub(super) fn source_element_type(
    checker: &TypeChecker,
    expr: &Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) -> Option<Typ> {
    if matches!(expr, Expr::StringLit(_)) {
        Some(crate::core_ir::rust_references::typ(
            crate::core_ir::rust_references::ReferenceType {
                mutable: false,
                referent: Typ::String,
                runtime: Typ::String,
            },
        ))
    } else {
        checker.expr_type(expr, facts, env)
    }
}

pub(super) fn finish_value_with_actual(
    checker: &TypeChecker,
    expr: &mut Expr,
    expected: Option<&Typ>,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    policy: ResolvePolicy,
    actual: Option<Option<Typ>>,
) -> Option<Typ> {
    let expected = expected.filter(|typ| !matches!(typ,Typ::Generic(name) if name=="_"));
    if let Some(expected) = expected.filter(|typ| fixed(typ).is_some()) {
        constrain_result(expr, expected, facts, env);
    }
    let actual = actual.unwrap_or_else(|| checker.expr_type(expr, facts, env));
    if let Some(proof) = actual
        .as_ref()
        .and_then(crate::core_ir::type_proofs::failure_in_type)
    {
        report(expr, proof, facts, policy);
        return actual;
    }
    let source = actual.as_ref().and_then(fixed);
    let destination = expected.and_then(fixed);
    let Some(source) = source else {
        if destination.is_some() {
            let definite = actual.as_ref().is_some_and(|typ| {
                scalar(typ)
                    || matches!(
                        typ,
                        Typ::Array(_) | Typ::Vector(_) | Typ::String | Typ::Void
                    )
            });
            report(
                expr,
                if definite {
                    ResolutionFailure::Invalid(
                        "fixed-array value requires a Rust fixed-array source".into(),
                    )
                } else {
                    ResolutionFailure::Unsupported(
                        "fixed-array source identity is unresolved".into(),
                    )
                },
                facts,
                policy,
            );
        }
        return actual;
    };
    if matches!(expected,Some(Typ::Named(name)) if crate::core_ir::rust_references::decode(name).is_some())
    {
        return actual;
    }
    let mut result = actual.clone();
    let mut element = source.element.clone();
    if let Some(destination) = &destination {
        if source.length != destination.length {
            report(
                expr,
                ResolutionFailure::Invalid("fixed-array length mismatch".into()),
                facts,
                policy,
            );
            return expected.cloned();
        }
        match merge_element(&source.element, &destination.element, source.length) {
            Ok(merged) => element = merged,
            Err(proof) => {
                report(expr, proof, facts, policy);
                return expected.cloned();
            }
        }
        result = Some(rust_arrays::typ(element.clone(), source.length));
        if let Some(result) = &result {
            constrain_result(expr, result, facts, env);
        }
    } else if let Some(expected) = expected {
        report(
            expr,
            if scalar(expected) || matches!(expected, Typ::Vector(_) | Typ::String | Typ::Void) {
                ResolutionFailure::Invalid(
                    "fixed-array value does not match its destination type".into(),
                )
            } else {
                ResolutionFailure::Unsupported(
                    "fixed-array destination identity is unresolved".into(),
                )
            },
            facts,
            policy,
        );
        return Some(expected.clone());
    }
    // Every fresh element participates, rather than trusting ArrayLit's first item.
    if let Expr::Unary { op, expr: inner } = expr
        && rust_arrays::value_length(op).is_some()
    {
        let items: &[Expr] = match inner.as_ref() {
            Expr::ArrayLit(items) => items,
            Expr::Call { callee, args } if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::ARRAY_REPEAT) => {
                &args[..args.len().min(1)]
            }
            _ => &[],
        };
        let merged = items
            .iter()
            .filter_map(|item| source_element_type(checker, item, facts, env))
            .try_fold(element.clone(), |selected, actual| {
                merge_element(&actual, &selected, source.length)
            });
        match merged {
            Ok(selected) => {
                element = selected;
                result = Some(rust_arrays::typ(element.clone(), source.length));
            }
            Err(proof) => {
                report(expr, proof, facts, policy);
                return result;
            }
        }
    }
    if policy != ResolvePolicy::RustFixedArrays {
        return result;
    }
    if !fresh(expr) && !matches!(expr,Expr::Unary{op,..} if array_copy::count(op).is_some()) {
        if !scalar(&element) {
            report(
                expr,
                ResolutionFailure::Unsupported(
                    "fixed-array copies require proven scalar Copy elements".into(),
                ),
                facts,
                policy,
            );
        } else {
            *expr = Expr::Unary {
                op: array_copy::operation(source.length),
                expr: Box::new(std::mem::replace(expr, Expr::UnitLit)),
            };
        }
    }
    result
}
pub(super) fn check_join_with_actual(
    checker: &TypeChecker,
    expr: &mut Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    policy: ResolvePolicy,
    actual: Option<Option<Typ>>,
) {
    if matches!(expr, Expr::Conditional { .. } | Expr::MatchValue { .. })
        && let Some(proof) = actual
            .unwrap_or_else(|| checker.expr_type(expr, facts, env))
            .as_ref()
            .and_then(crate::core_ir::type_proofs::failure_in_type)
    {
        report(expr, proof, facts, policy);
    }
}
fn resolve_pending_lengths(checker: &TypeChecker, module: &mut UnifiedModule) {
    use crate::core_ir::{Decl, Stmt};
    let mut present = false;
    crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
        present |= matches!(expr, Expr::Unary {op,..} if op == rust_arrays::PENDING_REPEAT);
    });
    if !present {
        return;
    }
    let constants: Vec<_> = module
        .decls
        .iter()
        .filter(|decl| {
            matches!(
                decl,
                Decl::Global {
                    constant_value: true,
                    ..
                }
            )
        })
        .cloned()
        .collect();
    crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
        let Expr::Unary { op, expr: inner } = expr else {
            return;
        };
        if op != rust_arrays::PENDING_REPEAT {
            return;
        }
        let mut source_failure = None;
        let resolved = (|| {
            let Expr::Call { callee, args } = inner.as_ref() else {
                return None;
            };
            if !matches!(callee.as_ref(),Expr::Ident(name) if name == crate::core_ir::ARRAY_REPEAT)
                || args.len() != 2
            {
                return None;
            }
            // External modules have now supplied qualified constant definitions.
            // Evaluate only the count; the repeated value remains a runtime expression.
            let mut needed = std::collections::HashSet::new();
            crate::core_bindings::reserve_expr(&args[1], &mut needed);
            loop {
                let previous = needed.len();
                for declaration in &constants {
                    if let Decl::Global {
                        name,
                        init: Some(value),
                        ..
                    } = declaration
                        && needed.contains(name)
                    {
                        crate::core_bindings::reserve_expr(value, &mut needed);
                    }
                }
                if needed.len() == previous {
                    break;
                }
            }
            let mut decls: Vec<_> = constants
                .iter()
                .filter(|decl| matches!(decl, Decl::Global { name, .. } if needed.contains(name)))
                .cloned()
                .collect();
            decls.push(Decl::Function {
                name: "#array-length".into(),
                params: vec![],
                ret: Typ::Named("#rust-integer-usize".into()),
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::INLINE_CONST.into())),
                    args: vec![args[1].clone()],
                }))],
                type_params: vec![],
            });
            crate::core_bindings::rewrite_type_annotations(&mut decls, &|name| {
                rust_arrays::decode(name).map(|array| Typ::Array(Box::new(array.element)))
            });
            crate::core_opt::map_decl_exprs_mut(&mut decls, &mut |expr| {
                if let Expr::Unary { op, expr: inner } = expr
                    && rust_arrays::value_length(op).is_some()
                {
                    *expr = *std::mem::replace(inner, Box::new(Expr::UnitLit));
                }
            });
            let semantics = crate::integer_semantics::IntegerSemantics::Word64;
            let mut temporary = UnifiedModule::new(decls);
            let environment = constants
                .iter()
                .filter_map(|decl| match decl {
                    Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
                    _ => None,
                })
                .collect();
            let facts = checker.collect_facts(&temporary);
            if checker.expr_type(&args[1], &facts, &environment).is_some_and(|typ|
                matches!(typ.canonical(), Typ::Bool | Typ::Float | Typ::String | Typ::Array(_) | Typ::Vector(_) | Typ::Void)
                || matches!(typ,Typ::Named(name) if crate::core_ir::rust_references::decode(&name).is_some())) {
                source_failure = Some(ResolutionFailure::Invalid("fixed-array length must have usize type".into()));
                return None;
            }
            super::rust_integers::normalize(checker, &mut temporary, semantics);
            if let Some(proof) = checker
                .typed_proof_nodes(&temporary.decls)
                .into_values()
                .fold(None, |left, right| {
                    crate::core_ir::type_proofs::prefer_failure(left, Some(right))
                })
            {
                source_failure = Some(proof);
                return None;
            }
            for declaration in &temporary.decls {
                let value = match declaration {
                    Decl::Global {
                        init: Some(value), ..
                    } => value.as_ref(),
                    Decl::Function { body, .. } => {
                        let [Stmt::Return(Some(value))] = body.as_slice() else {
                            return None;
                        };
                        value
                    }
                    _ => continue,
                };
                let mut value = value.clone();
                crate::core_opt::map_expr_mut(&mut value, &mut |expr| {
                    if let Expr::Call { callee, args } = expr
                        && matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::INLINE_CONST)
                        && let [value] = args.as_slice()
                    {
                        *expr = value.clone();
                    }
                });
                if let Some(failure) =
                    checker.constant_value_type_failure(&value, &facts, &environment)
                {
                    source_failure = Some(failure);
                    return None;
                }
            }
            crate::lower_core::fold_static_initializers_with_semantics(
                &mut temporary.decls,
                semantics,
            );
            crate::lower_core::lower_named_constants_with_semantics(
                &mut temporary.decls,
                semantics,
            );
            let Decl::Function { body, .. } = temporary.decls.last()? else {
                return None;
            };
            let [Stmt::Return(Some(value))] = body.as_slice() else {
                return None;
            };
            let evaluated = match crate::lower_core::evaluate_pure_constant(value, semantics) {
                Ok(value) => value,
                Err(crate::lower_core::ConstantEvalError::Arithmetic) => {
                    source_failure = Some(ResolutionFailure::Invalid(
                        "fixed-array length constant arithmetic failed".into(),
                    ));
                    return None;
                }
            };
            let Expr::IntLit(length) = evaluated? else {
                return None;
            };
            usize::try_from(length as u64).ok()
        })();
        *expr = if let Some(proof) = source_failure {
            Expr::Ident(proof.symbol())
        } else if let Some(length) = resolved {
            rust_arrays::value(*std::mem::replace(inner, Box::new(Expr::UnitLit)), length)
        } else {
            Expr::Ident(
                ResolutionFailure::Unsupported(
                    "fixed-array repetition length is unresolved after module assembly".into(),
                )
                .symbol(),
            )
        };
    });
}

pub(super) fn validate(checker: &TypeChecker, module: &mut UnifiedModule) {
    resolve_pending_lengths(checker, module);
    if rust_arrays::has_metadata(module) {
        let _ =
            checker.resolve_typed_operators(module, ResolvePolicy::RustArrayValidate, None, None);
    }
}

fn erase_type(typ: Typ) -> Typ {
    match typ {
        Typ::Named(name) => {
            if let Some(array) = rust_arrays::decode(&name) {
                Typ::Array(Box::new(erase_type(array.element)))
            } else if let Some(mut reference) = crate::core_ir::rust_references::decode(&name) {
                reference.referent = erase_type(reference.referent);
                reference.runtime = erase_type(reference.runtime);
                crate::core_ir::rust_references::typ(reference)
            } else if let Some(payload) = crate::core_ir::rust_options::payload(&name) {
                crate::core_ir::rust_options::typ(erase_type(payload))
            } else if let Some(fields) = crate::core_ir::tuples::fields(&name) {
                crate::core_ir::tuples::typ(
                    &fields
                        .into_iter()
                        .map(|(_, typ)| erase_type(typ))
                        .collect::<Vec<_>>(),
                )
            } else {
                Typ::Named(name)
            }
        }
        Typ::Array(element) => Typ::Array(Box::new(erase_type(*element))),
        Typ::Vector(element) => Typ::Vector(Box::new(erase_type(*element))),
        other => other,
    }
}

pub(super) fn normalize(checker: &TypeChecker, module: &mut UnifiedModule) {
    if !rust_arrays::has_metadata(module) {
        return;
    }
    let _ = checker.resolve_typed_operators(module, ResolvePolicy::RustFixedArrays, None, None);
    crate::core_ir::tuples::project_declarations(&mut module.decls, |decls| {
        crate::core_bindings::rewrite_type_annotations(decls, &|name| {
            rust_arrays::decode(name).map(|_| erase_type(Typ::Named(name.into())))
        });
        crate::core_opt::map_decl_exprs_mut(decls, &mut |expr| {
            if let Expr::Unary { op, expr: inner } = expr
                && rust_arrays::value_length(op).is_some()
            {
                *expr = *std::mem::replace(inner, Box::new(Expr::UnitLit));
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn integer(name: &str) -> Typ {
        Typ::Named(format!("#rust-integer-{name}"))
    }
    fn array(element: Typ, n: usize) -> Typ {
        rust_arrays::typ(element, n)
    }
    #[test]
    fn constraints_strengthen_defaults_and_keep_strong_conflicts() {
        let mut constraints = Constraints::default();
        constraints.bind("a", &array(Typ::Int, 2));
        constraints.reset_changed();
        constraints.bind("a", &array(integer("u8"), 2));
        assert!(constraints.changed());
        assert_eq!(constraints.get("a"), Some(array(integer("u8"), 2)));
        constraints.reset_changed();
        constraints.bind("a", &array(Typ::Int, 2));
        assert!(!constraints.changed());
        constraints.bind("a", &array(integer("u16"), 2));
        assert!(
            constraints
                .failure()
                .is_some_and(|proof| proof.is_invalid())
        );
        constraints.bind("a", &array(integer("u8"), 2));
        assert!(
            constraints
                .failure()
                .is_some_and(|proof| proof.is_invalid())
        );
    }
    #[test]
    fn joins_keep_source_width_length_and_unowned_boundaries() {
        assert_eq!(
            join_types(&[Some(array(Typ::Int, 2)), Some(array(integer("u8"), 2))]),
            Some(array(integer("u8"), 2))
        );
        for types in [
            vec![
                Some(array(integer("u8"), 2)),
                Some(array(integer("u16"), 2)),
            ],
            vec![Some(array(Typ::Bool, 1)), Some(array(Typ::Bool, 2))],
            vec![Some(array(Typ::Bool, 1)), Some(Typ::Int)],
        ] {
            let joined = join_types(&types).unwrap();
            assert!(
                crate::core_ir::type_proofs::failure_in_type(&joined)
                    .unwrap()
                    .is_invalid()
            );
        }
        let joined = join_types(&[
            Some(array(Typ::Generic("T".into()), 1)),
            Some(array(integer("u8"), 1)),
        ])
        .unwrap();
        assert!(matches!(
            crate::core_ir::type_proofs::failure_in_type(&joined),
            Some(ResolutionFailure::Unsupported(_))
        ));
        assert!(join_types(&[Some(Typ::Bool), Some(Typ::Bool)]).is_none());
    }
    #[test]
    fn element_selection_accumulates_strong_evidence() {
        let selected = merge_element(&Typ::Int, &integer("u8"), 3).unwrap();
        assert!(
            merge_element(&selected, &integer("u16"), 3)
                .unwrap_err()
                .is_invalid()
        );
        assert_eq!(merge_element(&Typ::Void, &Typ::Bool, 0), Ok(Typ::Bool));
        assert!(merge_element(&Typ::Void, &Typ::Bool, 1).is_err());
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use crate::core_ir::{Decl, Stmt};
    fn source(text: &str) -> UnifiedModule {
        crate::compiler::rust_front::parse_rust_source(text).unwrap()
    }
    #[test]
    fn existing_scope_walker_backpropagates_array_constraints() {
        for text in [
            "fn main()->i64{let a=[0,0];let b:[u8;2]=a;42}",
            "fn main()->i64{let a=[42i64,0];let mut b=[0,0];b=a;42}",
            "fn main()->i64{let b:[u8;2]={let a=[0,0];a};42}",
        ] {
            let mut module = source(text);
            validate(&TypeChecker::new(), &mut module);
            assert!(
                !crate::core_ir::type_proofs::present(&module.decls),
                "{text}: {:?}",
                module.decls
            );
            let Decl::Function { body, .. } = module
                .decls
                .iter()
                .find(|decl| matches!(decl,Decl::Function{name,..}if name=="main"))
                .unwrap()
            else {
                panic!()
            };
            assert!(body.iter().any(|stmt|matches!(stmt,Stmt::Let(_,Some(typ),_)if fixed(typ).is_some_and(|array|matches!(array.element,Typ::Named(_))))),"{body:?}");
        }
    }
    #[test]
    fn source_join_conflicts_survive_cold_code_and_validation() {
        for text in [
            "fn main()->i64{if false{let a=if true{[1u8]}else{[1u16]};}42}",
            "fn main()->i64{let a=match true{true=>[1u8],false=>[1u8,2u8]};42}",
            "fn main()->i64{let a=[0,1u8,2u16];42}",
            "fn main(args:Vec<String>)->u8{[1u8]}",
        ] {
            let mut module = source(text);
            validate(&TypeChecker::new(), &mut module);
            let proofs = TypeChecker::new().typed_proof_nodes(&module.decls);
            assert!(
                proofs.values().any(ResolutionFailure::is_invalid),
                "{text}: {:?}",
                module.decls
            );
        }
    }
    #[test]
    fn assembled_pending_lengths_validate_cold_branch_types() {
        for condition in ["true", "1"] {
            let mut module = source("fn main()->i64{let a=[7;external::COUNT];42}");
            let value = Expr::Conditional {
                cond: Box::new(if condition == "true" {
                    Expr::BoolLit(true)
                } else {
                    Expr::IntLit(1)
                }),
                then_expr: Box::new(Expr::IntLit(3)),
                else_expr: Box::new(if condition == "true" {
                    Expr::BoolLit(false)
                } else {
                    Expr::IntLit(3)
                }),
            };
            module.decls.push(Decl::Global {
                name: "external::COUNT".into(),
                typ: Typ::Named("#rust-integer-usize".into()),
                init: Some(Box::new(value)),
                mutable: false,
                constant_value: true,
            });
            resolve_pending_lengths(&TypeChecker::new(), &mut module);
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .values()
                    .any(ResolutionFailure::is_invalid),
                "{module:?}"
            );
        }
    }
    #[test]
    fn assembled_pending_lengths_ignore_unrelated_unowned_initializers() {
        let mut module = source("fn main()->i64{let a=[7;external::COUNT];42}");
        module.decls.extend([
            Decl::Global {
                name: "external::COUNT".into(),
                typ: Typ::Named("#rust-integer-usize".into()),
                init: Some(Box::new(Expr::IntLit(3))),
                mutable: false,
                constant_value: true,
            },
            Decl::Global {
                name: "unrelated".into(),
                typ: Typ::Named("Unowned".into()),
                init: Some(Box::new(Expr::StructInit {
                    name: "Unowned".into(),
                    fields: vec![("x".into(), Expr::IntLit(9))],
                })),
                mutable: false,
                constant_value: true,
            },
        ]);
        resolve_pending_lengths(&TypeChecker::new(), &mut module);
        assert!(
            !crate::core_ir::type_proofs::present(&module.decls),
            "{module:?}"
        );
        let mut resolved = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            resolved |= matches!(expr, Expr::Unary { op, .. }
                if rust_arrays::value_length(op) == Some(3));
        });
        assert!(resolved, "{module:?}");
    }
    #[test]
    fn historical_constraints_cannot_resurrect_out_of_scope_bindings() {
        let checker = TypeChecker::new();
        let facts = checker.collect_facts(&UnifiedModule::new(vec![]));
        facts
            .array_constraints
            .borrow_mut()
            .bind("gone", &rust_arrays::typ(Typ::Int, 1));
        let env = HashMap::new();
        let ident = Expr::Ident("gone".into());
        assert_eq!(checker.expr_type(&ident, &facts, &env), None);
        facts.array_constraints.borrow_mut().reset_changed();
        constrain_result(
            &ident,
            &rust_arrays::typ(Typ::Named("#rust-integer-u8".into()), 1),
            &facts,
            &env,
        );
        assert!(!facts.array_constraints.borrow().changed());
    }
    #[test]
    fn flexible_integer_elements_do_not_adopt_reference_carriers() {
        let reference =
            crate::core_ir::rust_references::typ(crate::core_ir::rust_references::ReferenceType {
                mutable: false,
                referent: Typ::Int,
                runtime: Typ::Int,
            });
        assert!(matches!(
            merge_element(&Typ::Int, &reference, 1),
            Err(ResolutionFailure::Unsupported(_))
        ));
        assert!(matches!(
            merge_element(&Typ::Int, &Typ::Named("i64".into()), 1),
            Err(ResolutionFailure::Unsupported(_))
        ));
    }
}

#[cfg(test)]
mod nested_and_literal_tests {
    use super::*;
    #[test]
    fn nested_arrays_merge_element_constraints_without_losing_lengths() {
        let weak = rust_arrays::typ(Typ::Int, 2);
        let strong = rust_arrays::typ(Typ::Named("#rust-integer-i64".into()), 2);
        assert_eq!(merge_element(&weak, &strong, 3), Ok(strong.clone()));
        let wrong_length = rust_arrays::typ(Typ::Int, 1);
        assert!(
            merge_element(&wrong_length, &strong, 3)
                .unwrap_err()
                .is_invalid()
        );
        let wrong_kind = rust_arrays::typ(Typ::Named("#rust-integer-u8".into()), 2);
        assert!(
            merge_element(&wrong_kind, &strong, 3)
                .unwrap_err()
                .is_invalid()
        );
    }
    #[test]
    fn raw_string_elements_retain_shared_reference_identity() {
        let checker = TypeChecker::new();
        let facts = checker.collect_facts(&UnifiedModule::new(vec![]));
        let env = HashMap::new();
        let literal = Expr::StringLit("step".into());
        let expected =
            crate::core_ir::rust_references::typ(crate::core_ir::rust_references::ReferenceType {
                mutable: false,
                referent: Typ::String,
                runtime: Typ::String,
            });
        assert_eq!(
            source_element_type(&checker, &literal, &facts, &env),
            Some(expected.clone())
        );
        assert_eq!(merge_element(&expected, &expected, 1), Ok(expected.clone()));
        assert!(merge_element(&Typ::String, &expected, 1).is_err());
        assert_eq!(
            source_element_type(
                &checker,
                &Expr::Ident("owned".into()),
                &facts,
                &HashMap::from([("owned".into(), Typ::String)])
            ),
            Some(Typ::String)
        );
    }
}
