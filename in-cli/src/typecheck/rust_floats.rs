//! Rust float source constraints and result edges; the shared resolver owns scopes.
use super::{Facts, ResolvePolicy, TypeChecker};
use crate::core_ir::{
    Expr, FloatVal, Typ, rust_floats as floats,
    type_proofs::{self, ResolutionFailure},
};
use floats::Kind;
use std::collections::HashMap;

fn weak(typ: &Typ) -> bool {
    *typ == floats::weak_type()
}
fn source(typ: &Typ) -> bool {
    weak(typ) || floats::kind(typ).is_some()
}
fn tuple_fields(typ: &Typ) -> Option<Vec<(String, Typ)>> {
    let Typ::Named(name) = typ else {
        return None;
    };
    crate::core_ir::tuples::fields(name)
}
fn structural_source(typ: &Typ) -> bool {
    source(typ)
        || element_type(typ).is_some_and(|element| structural_source(&element))
        || tuple_fields(typ)
            .is_some_and(|fields| fields.iter().any(|(_, typ)| structural_source(typ)))
}
pub(super) fn has_weak_float(typ: &Typ) -> bool {
    weak(typ)
        || element_type(typ).is_some_and(|element| has_weak_float(&element))
        || tuple_fields(typ).is_some_and(|fields| fields.iter().any(|(_, typ)| has_weak_float(typ)))
}
fn definite(typ: &Typ) -> bool {
    matches!(
        typ,
        Typ::Int | Typ::Bool | Typ::String | Typ::Void | Typ::Array(_) | Typ::Vector(_)
    ) || matches!(typ, Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some())
}
fn mismatch() -> ResolutionFailure {
    ResolutionFailure::Invalid("Rust float source widths do not match".into())
}

/// Monotone constraints for normalized scoped binding names. Conflicts are sticky.
#[derive(Default)]
pub(super) struct Constraints {
    values: HashMap<String, Typ>,
    changed: bool,
    failure: Option<ResolutionFailure>,
}
impl Constraints {
    pub(super) fn bind(&mut self, name: &str, typ: &Typ) {
        if !structural_source(typ) {
            return;
        }
        let old = self.values.get(name).cloned();
        let merged = old.as_ref().map_or_else(
            || Some(typ.clone()),
            |old| join_elements(&[Some(old.clone()), Some(typ.clone())]),
        );
        if let Some(merged) = merged {
            if let Some(proof) = type_proofs::failure_in_type(&merged) {
                self.record(proof);
            } else if old.as_ref() != Some(&merged) {
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
        self.failure = type_proofs::prefer_failure(self.failure.take(), Some(proof));
    }
}
/// None delegates entirely non-Rust-float joins to the existing shared rules.
pub(super) fn join_types(types: &[Option<Typ>]) -> Option<Typ> {
    if !types.iter().flatten().any(source) {
        return None;
    }
    let mut kind = None;
    let mut failure = None;
    for typ in types {
        let next = match typ {
            Some(typ) if type_proofs::failure_in_type(typ).is_some() => {
                type_proofs::failure_in_type(typ)
            }
            Some(typ) if weak(typ) => None,
            Some(typ) if floats::kind(typ).is_some() => {
                let next = floats::kind(typ).unwrap();
                if kind.is_some_and(|kind| kind != next) {
                    Some(mismatch())
                } else {
                    kind = Some(next);
                    None
                }
            }
            Some(typ) if definite(typ) => Some(mismatch()),
            _ => Some(ResolutionFailure::Unsupported(
                "Rust float source identity is unresolved".into(),
            )),
        };
        failure = type_proofs::prefer_failure(failure, next);
    }
    Some(failure.map_or_else(
        || kind.map_or_else(floats::weak_type, floats::typ),
        ResolutionFailure::typ,
    ))
}
struct DirectSignature {
    declared_return: Typ,
    params: Vec<Typ>,
    slots: Vec<usize>,
}
fn direct_signature(
    expr: &Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) -> Option<DirectSignature> {
    let Expr::Call { callee, args } = expr else {
        return None;
    };
    let Expr::Ident(name) = callee.as_ref() else {
        return None;
    };
    if env.contains_key(name) {
        return None;
    }
    let (params, ret) = facts.function_signature(name)?;
    let Typ::Generic(parameter) = ret else {
        return None;
    };
    if parameter == "_" || params.len() != args.len() {
        return None;
    }
    let slots = params
        .iter()
        .enumerate()
        .filter_map(|(index, (_, typ))| (typ == ret).then_some(index))
        .collect::<Vec<_>>();
    if slots.is_empty() {
        return None;
    }
    Some(DirectSignature {
        declared_return: ret.clone(),
        params: params.iter().map(|(_, typ)| typ.clone()).collect(),
        slots,
    })
}
fn effective_direct_call(
    checker: &TypeChecker,
    expr: &Expr,
    expected: Option<&Typ>,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) -> Option<(DirectSignature, Typ)> {
    let signature = direct_signature(expr, facts, env)?;
    let Expr::Call { args, .. } = expr else {
        unreachable!()
    };
    let mut types = signature
        .slots
        .iter()
        .map(|index| source_type(checker, &args[*index], facts, env))
        .collect::<Vec<_>>();
    // An expectation can strengthen existing floating arguments, not manufacture
    // floating ownership for integer/unresolved generic calls.
    if !types.iter().flatten().any(source) {
        return None;
    }
    if let Some(expected) = expected.filter(|typ| source(typ)) {
        types.push(Some(expected.clone()));
    }
    join_types(&types).map(|typ| (signature, typ))
}
/// Optional expression-typing hook; no fallback recursion into expr_type.
pub(super) fn call_type(
    checker: &TypeChecker,
    expr: &Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) -> Option<Typ> {
    effective_direct_call(checker, expr, None, facts, env).map(|(_, typ)| typ)
}
fn finish_direct_call(
    checker: &TypeChecker,
    expr: &mut Expr,
    expected: Option<&Typ>,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    policy: ResolvePolicy,
) -> Option<Typ> {
    let (signature, result) = effective_direct_call(checker, expr, expected, facts, env)?;
    if let Some(proof) = type_proofs::failure_in_type(&result) {
        report(expr, proof.clone(), facts, policy);
        return Some(proof.typ());
    }
    let Expr::Call { args, .. } = expr else {
        unreachable!()
    };
    for (index, (value, declared)) in args.iter_mut().zip(&signature.params).enumerate() {
        if signature.slots.contains(&index) {
            // A parent result edge (or a strong peer) must select the kind. In
            // particular, a postorder Lower visit cannot default weak arguments.
            if floats::kind(&result).is_some() || policy != ResolvePolicy::RustFloatLower {
                finish_value(checker, value, Some(&result), facts, env, policy);
            }
        } else {
            finish_value(checker, value, Some(declared), facts, env, policy);
        }
    }
    Some(result)
}
/// Return true only when this helper finished the complete owned call signature.
pub(super) fn finish_call(
    checker: &TypeChecker,
    expr: &mut Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    policy: ResolvePolicy,
) -> bool {
    if let Expr::Call { callee, args } = expr
        && matches!(callee.as_ref(),Expr::Ident(name) if name=="vector-push" && !env.contains_key(name) && !facts.functions.contains_key(name))
        && let [receiver, value] = args.as_mut_slice()
        && let Some(Typ::Vector(element)) = source_type(checker, receiver, facts, env)
        && source(&element)
    {
        finish_value(checker, value, Some(&element), facts, env, policy);
        return true;
    }
    finish_direct_call(checker, expr, None, facts, env, policy).is_some()
}
/// Private value/cast/literal facts are read before ordinary expression typing.
/// Canonical Float/cast operators alone never establish Rust source ownership.
pub(super) fn source_type(
    checker: &TypeChecker,
    expr: &Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) -> Option<Typ> {
    match expr {
        Expr::Ident(name) if env.contains_key(name) => facts
            .float_constraints
            .borrow()
            .get(name)
            .or_else(|| checker.expr_type(expr, facts, env)),
        Expr::Call { .. } => call_type(checker, expr, facts, env)
            // expr_type also asks call_type. A known direct generic call with no
            // float inference must not re-enter the same argument tree there.
            .or_else(|| {
                direct_signature(expr, facts, env).map(|signature| signature.declared_return)
            })
            .or_else(|| checker.expr_type(expr, facts, env)),
        Expr::Unary { op, .. } if floats::inferred_type(op).is_some() => floats::inferred_type(op),
        Expr::Unary { op, expr: value }
            if matches!(
                op.as_str(),
                "-" | crate::core_ir::rust_references::NEGATE_VALUE
            ) =>
        {
            let operand = source_type(checker, value, facts, env);
            if operand.as_ref().is_some_and(source) {
                operand
            } else {
                checker.expr_type(expr, facts, env)
            }
        }
        Expr::Binary { op, lhs, rhs } if arithmetic(op) => join_types(&[
            source_type(checker, lhs, facts, env),
            source_type(checker, rhs, facts, env),
        ])
        .or_else(|| checker.expr_type(expr, facts, env)),
        _ => checker.expr_type(expr, facts, env),
    }
}
fn arithmetic(op: &str) -> bool {
    matches!(op, "+" | "-" | "*" | "/" | "%")
}
fn comparison(op: &str) -> bool {
    matches!(op, "==" | "!=" | "<" | "<=" | ">" | ">=")
}
fn report(expr: &mut Expr, proof: ResolutionFailure, facts: &Facts, policy: ResolvePolicy) {
    if policy == ResolvePolicy::RustFloatValidate && !proof.is_invalid() {
        return;
    }
    facts.float_constraints.borrow_mut().record(proof.clone());
    if policy != ResolvePolicy::RustFloatInference {
        *expr = Expr::Ident(proof.symbol());
    }
}
// Only value/result dependencies, never statement bodies or unrelated effects.
fn constrain_result(
    checker: &TypeChecker,
    expr: &Expr,
    expected: &Typ,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) {
    match expr {
        Expr::Ident(name) => {
            if env.get(name).is_some_and(structural_source) {
                facts.float_constraints.borrow_mut().bind(name, expected);
            }
        }
        Expr::Index { base, .. } => {
            if let Some(container) =
                source_type(checker, base, facts, env).filter(|typ| element_type(typ).is_some())
            {
                constrain_result(
                    checker,
                    base,
                    &with_element(&container, expected.clone()),
                    facts,
                    env,
                );
            }
        }
        Expr::Field { base, name } => {
            if let Some(mut fields) = source_type(checker, base, facts, env)
                .as_ref()
                .and_then(tuple_fields)
                && let Some((_, typ)) = fields.iter_mut().find(|(field, _)| field == name)
            {
                *typ = expected.clone();
                let refined = crate::core_ir::tuples::typ(
                    &fields.into_iter().map(|(_, typ)| typ).collect::<Vec<_>>(),
                );
                constrain_result(checker, base, &refined, facts, env);
            }
        }
        Expr::Call { args, .. } => {
            if let Some(signature) = direct_signature(expr, facts, env) {
                for index in signature.slots {
                    constrain_result(checker, &args[index], expected, facts, env);
                }
            }
        }
        Expr::Binary { op, lhs, rhs } if arithmetic(op) => {
            constrain_result(checker, lhs, expected, facts, env);
            constrain_result(checker, rhs, expected, facts, env);
        }
        Expr::Unary { op, expr }
            if matches!(
                op.as_str(),
                "-" | crate::core_ir::rust_references::NEGATE_VALUE
            ) || crate::core_ir::array_copy::count(op).is_some() =>
        {
            constrain_result(checker, expr, expected, facts, env)
        }
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => {
            constrain_result(checker, then_expr, expected, facts, env);
            constrain_result(checker, else_expr, expected, facts, env);
        }
        Expr::MatchValue { arms, .. } => {
            for arm in arms {
                constrain_result(checker, &arm.value, expected, facts, env);
            }
        }
        Expr::Block {
            result: Some(result),
            ..
        } => constrain_result(checker, result, expected, facts, env),
        _ => {}
    }
}
fn retain(value: Expr, kind: Kind, round: bool) -> Expr {
    let value = if round && kind == Kind::F32 {
        Expr::Unary {
            op: "float-cast-f32".into(),
            expr: Box::new(value),
        }
    } else {
        value
    };
    floats::value(value, kind)
}
fn element_type(typ: &Typ) -> Option<Typ> {
    match typ {
        Typ::Array(element) | Typ::Vector(element) => Some((**element).clone()),
        Typ::Named(name) => crate::core_ir::rust_arrays::decode(name).map(|array| array.element),
        _ => None,
    }
}
fn with_element(container: &Typ, element: Typ) -> Typ {
    match container {
        Typ::Array(_) => Typ::Array(Box::new(element)),
        Typ::Vector(_) => Typ::Vector(Box::new(element)),
        Typ::Named(name) => crate::core_ir::rust_arrays::decode(name).map_or_else(
            || container.clone(),
            |array| crate::core_ir::rust_arrays::typ(element, array.length),
        ),
        _ => container.clone(),
    }
}
// Delegate non-float tuple slots to existing scalar source-array rules:
// they distinguish flexible Int from strong Rust widths without erasing them.
fn join_tuple_field(types: &[Option<Typ>]) -> Option<Typ> {
    if let Some(joined) = join_elements(types) {
        return Some(joined);
    }
    let mut selected = types.first()?.clone()?;
    for typ in &types[1..] {
        let Some(typ) = typ else {
            return Some(
                ResolutionFailure::Unsupported("tuple field identity is unresolved".into()).typ(),
            );
        };
        match super::rust_arrays::merge_element(&selected, typ, 1) {
            Ok(typ) => selected = typ,
            Err(proof) => return Some(proof.typ()),
        }
    }
    Some(selected)
}
fn join_elements(types: &[Option<Typ>]) -> Option<Typ> {
    join_types(types)
        .or_else(|| super::rust_arrays::join_types(types))
        .or_else(|| {
            let first = types.first()?.as_ref()?;
            if let Some(fields) = tuple_fields(first) {
                if !types.iter().flatten().any(structural_source) {
                    return None;
                }
                let schemas = types
                    .iter()
                    .map(|typ| typ.as_ref().and_then(tuple_fields))
                    .collect::<Option<Vec<_>>>();
                let Some(schemas) = schemas else {
                    return Some(
                        ResolutionFailure::Unsupported(
                            "Rust float tuple identity is unresolved".into(),
                        )
                        .typ(),
                    );
                };
                if schemas.iter().any(|schema| schema.len() != fields.len()) {
                    return Some(
                        ResolutionFailure::Invalid("Rust tuple shapes do not match".into()).typ(),
                    );
                }
                let mut merged = Vec::new();
                for (index, (_, original)) in fields.iter().enumerate() {
                    let field_types = schemas
                        .iter()
                        .map(|schema| Some(schema[index].1.clone()))
                        .collect::<Vec<_>>();
                    let field = join_tuple_field(&field_types).unwrap_or_else(|| original.clone());
                    merged.push(field);
                }
                return Some(crate::core_ir::tuples::typ(&merged));
            }
            let vector = matches!(first, Typ::Vector(_));
            if !types.iter().all(|typ| {
                matches!(typ, Some(Typ::Vector(_))) && vector
                    || matches!(typ, Some(Typ::Array(_))) && !vector
            }) {
                return None;
            }
            let elements = types
                .iter()
                .map(|typ| typ.as_ref().and_then(element_type))
                .collect::<Vec<_>>();
            join_elements(&elements).map(|element| with_element(first, element))
        })
}
fn finish_tuple(
    checker: &TypeChecker,
    expr: &mut Expr,
    expected: Option<&Typ>,
    actual: Option<&Typ>,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    policy: ResolvePolicy,
) -> Option<Typ> {
    let fields = expected
        .and_then(tuple_fields)
        .or_else(|| actual.and_then(tuple_fields))?;
    let mut finished = fields
        .iter()
        .map(|(_, typ)| typ.clone())
        .collect::<Vec<_>>();
    match expr {
        Expr::Call { callee, args }
            if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::tuples::VALUE)
                && args.len() == fields.len() =>
        {
            for (index, (value, (_, expected))) in args.iter_mut().zip(&fields).enumerate() {
                if let Some(typ) =
                    checker.finish_typed_value(policy, value, Some(expected), facts, env)
                {
                    finished[index] =
                        join_tuple_field(&[Some(expected.clone()), Some(typ.clone())])
                            .unwrap_or(typ);
                }
            }
            let result = crate::core_ir::tuples::typ(&finished);
            if policy == ResolvePolicy::RustFloatLower
                && let Typ::Named(name) = &result
            {
                *expr = Expr::StructInit {
                    name: name.clone(),
                    fields: std::mem::take(args)
                        .into_iter()
                        .enumerate()
                        .map(|(index, value)| (format!("_{index}"), value))
                        .collect(),
                };
            }
            Some(result)
        }
        Expr::StructInit {
            name,
            fields: values,
        } if crate::core_ir::tuples::fields(name).is_some() && values.len() == fields.len() => {
            for (field, value) in values {
                if let Some((index, (_, expected))) = fields
                    .iter()
                    .enumerate()
                    .find(|(_, (name, _))| name == field)
                    && let Some(typ) =
                        checker.finish_typed_value(policy, value, Some(expected), facts, env)
                {
                    finished[index] =
                        join_tuple_field(&[Some(expected.clone()), Some(typ.clone())])
                            .unwrap_or(typ);
                }
            }
            let result = crate::core_ir::tuples::typ(&finished);
            if policy == ResolvePolicy::RustFloatLower
                && let Typ::Named(refined) = &result
            {
                *name = refined.clone();
            }
            Some(result)
        }
        _ => None,
    }
}
/// Finish only aggregate value elements, never repeat counts or statement bodies.
fn finish_aggregate(
    checker: &TypeChecker,
    expr: &mut Expr,
    expected: Option<&Typ>,
    actual: Option<&Typ>,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    policy: ResolvePolicy,
) -> Option<Typ> {
    let container = expected
        .filter(|typ| element_type(typ).is_some())
        .or_else(|| actual.filter(|typ| element_type(typ).is_some()))?;
    let element = element_type(container)?;
    if let Expr::Unary { op, expr: inner } = expr
        && crate::core_ir::rust_arrays::value_length(op).is_some()
    {
        // The outer source carrier already proves the fixed length. Its raw
        // ArrayLit/repeat payload has a different Core container type; do not
        // rejoin that payload against the retained fixed-array identity.
        let result = finish_aggregate(checker, inner, Some(container), actual, facts, env, policy);
        return Some(result.as_ref().and_then(element_type).map_or_else(
            || container.clone(),
            |element| with_element(container, element),
        ));
    }
    let items: &mut [Expr] = match expr {
        Expr::ArrayLit(items) => items,
        Expr::Call { callee, args } if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::ARRAY_REPEAT) =>
        {
            let length = args.len().min(1);
            &mut args[..length]
        }
        _ => return None,
    };
    let mut types = vec![Some(element.clone())];
    types.extend(
        items
            .iter()
            .map(|value| source_type(checker, value, facts, env)),
    );
    let selected = join_elements(&types).unwrap_or(element);
    if let Some(proof) = type_proofs::failure_in_type(&selected) {
        report(expr, proof.clone(), facts, policy);
        return Some(proof.typ());
    }
    let mut finished = selected.clone();
    for value in items {
        if let Some(actual) = checker.finish_typed_value(policy, value, Some(&selected), facts, env)
        {
            if let Some(joined) = join_elements(&[Some(finished.clone()), Some(actual)]) {
                finished = joined;
            }
        }
    }
    Some(with_element(container, finished))
}
fn contextual_empty_allocation(
    checker: &TypeChecker,
    expr: &Expr,
    expected: Option<&Typ>,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) -> Option<Typ> {
    let expected = expected?;
    let Typ::Vector(element) = expected else {
        return None;
    };
    if !source(element) {
        return None;
    }
    let Expr::Call { callee, args } = expr else {
        return None;
    };
    let Expr::Ident(name) = callee.as_ref() else {
        return None;
    };
    if env.contains_key(name)
        || facts.functions.contains_key(name)
        || Some(name.as_str()) != expected.vector_allocation_intrinsic()
    {
        return None;
    }
    let [capacity] = args.as_slice() else {
        return None;
    };
    if source_type(checker, capacity, facts, env)?.canonical() != Typ::Int {
        return None;
    }
    // The exact owned intrinsic allocates empty storage: there is no canonical
    // float element value here to reinterpret or convert. Keep capacity intact.
    Some(expected.clone())
}
/// Finish a parameter, store, assignment, initializer or return edge. A missing
/// expectation defaults a weak value only after inference has reached a fixed point.
pub(super) fn finish_value(
    checker: &TypeChecker,
    expr: &mut Expr,
    expected: Option<&Typ>,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    policy: ResolvePolicy,
) -> Option<Typ> {
    let expected = expected.filter(|typ| !matches!(typ,Typ::Generic(name) if name=="_"));
    if let Some(typ) = contextual_empty_allocation(checker, expr, expected, facts, env) {
        return Some(typ);
    }
    let actual = source_type(checker, expr, facts, env);
    let refined = match (actual.as_ref(), expected) {
        (Some(actual), Some(expected))
            if structural_source(actual) || structural_source(expected) =>
        {
            join_elements(&[Some(actual.clone()), Some(expected.clone())])
        }
        _ => None,
    };
    if let Some(proof) = refined.as_ref().and_then(type_proofs::failure_in_type) {
        report(expr, proof.clone(), facts, policy);
        return Some(proof.typ());
    }
    let expected = refined.as_ref().or(expected);
    if let Some(context) = expected
        .or(actual.as_ref())
        .filter(|typ| structural_source(typ))
    {
        constrain_result(checker, expr, context, facts, env);
    }
    if let Some(tuple) = finish_tuple(checker, expr, expected, actual.as_ref(), facts, env, policy)
    {
        return Some(tuple);
    }
    if let Some(aggregate) =
        finish_aggregate(checker, expr, expected, actual.as_ref(), facts, env, policy)
    {
        return Some(aggregate);
    }
    if let Some(proof) = actual.as_ref().and_then(type_proofs::failure_in_type) {
        report(expr, proof, facts, policy);
        return actual;
    }
    if !actual.as_ref().is_some_and(source) && !expected.is_some_and(source) {
        return refined.or(actual);
    }
    let destination = expected.and_then(floats::kind);
    let original = actual.as_ref().and_then(floats::kind);
    let failure = if destination.is_some() && original.is_some() && destination != original {
        Some(mismatch())
    } else if expected.is_some_and(|typ| !source(typ) && definite(typ))
        || actual
            .as_ref()
            .is_some_and(|typ| !source(typ) && definite(typ))
    {
        Some(mismatch())
    } else if !actual.as_ref().is_some_and(source) || expected.is_some_and(|typ| !source(typ)) {
        Some(ResolutionFailure::Unsupported(format!(
            "Rust float transfer source or destination identity is unresolved: actual={actual:?}, expected={expected:?}"
        )))
    } else {
        None
    };
    if let Some(proof) = failure {
        report(expr, proof.clone(), facts, policy);
        return Some(proof.typ());
    }
    let selected = destination
        .or(original)
        .or_else(|| (policy == ResolvePolicy::RustFloatLower).then_some(Kind::F64));
    let Some(kind) = selected else {
        return actual;
    };
    let typ = floats::typ(kind);
    constrain_result(checker, expr, &typ, facts, env);
    if floats::literal_info(expr).is_some() {
        if policy != ResolvePolicy::RustFloatInference {
            match floats::materialize_literal(expr, Some(kind)) {
                Ok(Some(value)) if policy == ResolvePolicy::RustFloatLower => {
                    *expr = retain(Expr::FloatLit(FloatVal(value)), kind, false)
                }
                Ok(_) => {}
                Err(proof) => {
                    report(expr, proof.clone(), facts, policy);
                    return Some(proof.typ());
                }
            }
        }
        return Some(typ);
    }
    if matches!(expr, Expr::Call { .. })
        && let Some(result) = finish_direct_call(checker, expr, Some(&typ), facts, env, policy)
    {
        if policy == ResolvePolicy::RustFloatLower
            && floats::kind(&result).is_some()
            && type_proofs::failure_in_type(&result).is_none()
        {
            *expr = floats::value(expr.clone(), kind);
        }
        return Some(result);
    }
    match expr {
        Expr::Binary { op, lhs, rhs } if arithmetic(op) => {
            finish_value(checker, lhs, Some(&typ), facts, env, policy);
            finish_value(checker, rhs, Some(&typ), facts, env, policy);
            if policy == ResolvePolicy::RustFloatLower {
                *expr = retain(expr.clone(), kind, true);
            }
        }
        Expr::Unary { op, expr: value }
            if matches!(
                op.as_str(),
                "-" | crate::core_ir::rust_references::NEGATE_VALUE
            ) =>
        {
            finish_value(checker, value, Some(&typ), facts, env, policy);
            if policy == ResolvePolicy::RustFloatLower {
                *expr = retain(expr.clone(), kind, false);
            }
        }
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => {
            finish_value(checker, then_expr, Some(&typ), facts, env, policy);
            finish_value(checker, else_expr, Some(&typ), facts, env, policy);
        }
        Expr::MatchValue { scrutinee, arms } => {
            let scrutinee_type = source_type(checker, scrutinee, facts, env);
            for arm in arms {
                let inner = scrutinee_type
                    .as_ref()
                    .and_then(|typ| checker.pattern_environment(&arm.pattern, typ, facts, env));
                finish_value(
                    checker,
                    &mut arm.value,
                    Some(&typ),
                    facts,
                    inner.as_ref().unwrap_or(env),
                    policy,
                );
            }
        }
        // Block-local result environments belong to finish_typed_value's shared
        // scoped traversal; the caller has already finished that result edge.
        _ => {}
    }
    Some(typ)
}
/// Shared traversal's expression hook. Weak operations remain untouched until a
/// result edge establishes their width; lazy branches and operands stay in place.
pub(super) fn finish_expr(
    checker: &TypeChecker,
    expr: &mut Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    policy: ResolvePolicy,
) -> Option<Typ> {
    if let Expr::Unary { op, expr: value } = expr
        && let Some(target) = floats::cast_kind(op)
    {
        let actual = source_type(checker, value, facts, env);
        if let Some(proof) = actual.as_ref().and_then(type_proofs::failure_in_type) {
            report(expr, proof.clone(), facts, policy);
            return Some(proof.typ());
        }
        if !actual.as_ref().is_some_and(source) {
            let proof = ResolutionFailure::Unsupported(
                "Rust float cast requires an owned float source; integer conversion is deferred"
                    .into(),
            );
            report(expr, proof.clone(), facts, policy);
            return Some(proof.typ());
        }
        finish_value(checker, value, None, facts, env, policy);
        if policy == ResolvePolicy::RustFloatLower {
            *expr = floats::value(
                Expr::Unary {
                    op: format!("float-cast-{}", target.name()),
                    expr: value.clone(),
                },
                target,
            );
        }
        return Some(floats::typ(target));
    }
    if let Some(info) = floats::literal_info(expr) {
        return if let Some(kind) = info.suffix {
            finish_value(checker, expr, Some(&floats::typ(kind)), facts, env, policy)
        } else {
            Some(floats::weak_type())
        };
    }
    if let Expr::Binary { op, lhs, rhs } = expr
        && (arithmetic(op) || comparison(op))
    {
        let is_comparison = comparison(op);
        let joined = join_types(&[
            source_type(checker, lhs, facts, env),
            source_type(checker, rhs, facts, env),
        ]);
        let Some(mut joined) = joined else {
            return source_type(checker, expr, facts, env);
        };
        if let Some(proof) = type_proofs::failure_in_type(&joined) {
            report(expr, proof.clone(), facts, policy);
            return Some(proof.typ());
        }
        if is_comparison && weak(&joined) && policy == ResolvePolicy::RustFloatLower {
            joined = floats::typ(Kind::F64);
        }
        if let Some(kind) = floats::kind(&joined) {
            finish_value(checker, lhs, Some(&joined), facts, env, policy);
            finish_value(checker, rhs, Some(&joined), facts, env, policy);
            if policy == ResolvePolicy::RustFloatLower && !is_comparison {
                *expr = retain(expr.clone(), kind, true);
            }
        }
        return Some(if is_comparison { Typ::Bool } else { joined });
    }
    let actual = source_type(checker, expr, facts, env);
    if let Some(proof) = actual.as_ref().and_then(type_proofs::failure_in_type) {
        report(expr, proof, facts, policy);
    }
    actual
}

#[cfg(test)]
mod tests {
    use super::*;
    fn new_facts() -> Facts {
        TypeChecker.collect_decl_facts(&[])
    }
    fn erase_values(expr: Expr) -> Expr {
        crate::core_opt::map_expr(expr, &mut |expr| match expr {
            Expr::Unary { op, expr } if floats::value_kind(&op).is_some() => *expr,
            expr => crate::core_opt::fold_expr(expr),
        })
    }
    #[test]
    fn scoped_constraints_strengthen_weak_bindings_and_keep_conflicts() {
        let mut values = Constraints::default();
        values.bind("x", &floats::weak_type());
        values.reset_changed();
        values.bind("x", &floats::typ(Kind::F32));
        assert!(values.changed());
        values.reset_changed();
        values.bind("x", &floats::weak_type());
        assert!(!values.changed());
        values.bind("x", &floats::typ(Kind::F64));
        assert!(values.failure().unwrap().is_invalid());
    }
    #[test]
    fn backward_constraint_materializes_original_decimal_once() {
        let facts = new_facts();
        let env = HashMap::from([("x".into(), floats::weak_type())]);
        let mut use_value = Expr::Ident("x".into());
        finish_value(
            &TypeChecker,
            &mut use_value,
            Some(&floats::typ(Kind::F32)),
            &facts,
            &env,
            ResolvePolicy::RustFloatInference,
        );
        let mut initializer = floats::literal("1.0000000596046447753906250000001", None);
        let expected = facts.float_constraints.borrow().get("x").unwrap();
        finish_value(
            &TypeChecker,
            &mut initializer,
            Some(&expected),
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        assert_eq!(
            erase_values(initializer),
            Expr::FloatLit(FloatVal(f64::from(f32::from_bits(1.0f32.to_bits() + 1))))
        );
    }
    #[test]
    fn f32_arithmetic_rounds_each_node_without_moving_effects() {
        let facts = new_facts();
        let env = HashMap::new();
        let add = Expr::Binary {
            op: "+".into(),
            lhs: Box::new(floats::literal("16777216", None)),
            rhs: Box::new(floats::literal("1", None)),
        };
        let mut value = Expr::Binary {
            op: "-".into(),
            lhs: Box::new(add),
            rhs: Box::new(floats::literal("16777216", None)),
        };
        finish_value(
            &TypeChecker,
            &mut value,
            Some(&floats::typ(Kind::F32)),
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        assert_eq!(erase_values(value), Expr::FloatLit(FloatVal(0.0)));
        let mut call = Expr::Call {
            callee: Box::new(Expr::Ident("effect".into())),
            args: vec![],
        };
        let facts = TypeChecker.collect_decl_facts(&[crate::core_ir::Decl::Function {
            name: "effect".into(),
            params: vec![],
            ret: floats::typ(Kind::F32),
            body: vec![],
            type_params: vec![],
        }]);
        finish_value(
            &TypeChecker,
            &mut call,
            Some(&floats::typ(Kind::F32)),
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        assert!(matches!(call, Expr::Call { .. }));
    }
    #[test]
    fn validation_defers_unknowns_and_weak_comparison_defaults() {
        let facts = new_facts();
        let env = HashMap::new();
        let mut cast = floats::cast(Expr::IntLit(1), Kind::F32);
        let before = cast.clone();
        finish_expr(
            &TypeChecker,
            &mut cast,
            &facts,
            &env,
            ResolvePolicy::RustFloatValidate,
        );
        assert_eq!(cast, before);
        assert!(facts.float_constraints.borrow().failure().is_none());
        let mut comparison = Expr::Binary {
            op: "==".into(),
            lhs: Box::new(floats::literal("1.0", None)),
            rhs: Box::new(floats::literal("1.0", None)),
        };
        let before = comparison.clone();
        finish_expr(
            &TypeChecker,
            &mut comparison,
            &facts,
            &env,
            ResolvePolicy::RustFloatValidate,
        );
        assert_eq!(comparison, before);
        finish_expr(
            &TypeChecker,
            &mut comparison,
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        assert_eq!(erase_values(comparison), Expr::BoolLit(true));
    }
    #[test]
    fn stale_constraints_cannot_resurrect_a_binding_outside_its_environment() {
        let facts = new_facts();
        facts
            .float_constraints
            .borrow_mut()
            .bind("local", &floats::typ(Kind::F32));
        assert_eq!(
            source_type(
                &TypeChecker,
                &Expr::Ident("local".into()),
                &facts,
                &HashMap::new()
            ),
            None
        );
        let before = facts.float_constraints.borrow().get("local");
        constrain_result(
            &TypeChecker,
            &Expr::Ident("local".into()),
            &floats::typ(Kind::F64),
            &facts,
            &HashMap::new(),
        );
        assert_eq!(facts.float_constraints.borrow().get("local"), before);
        assert!(facts.float_constraints.borrow().failure().is_none());
    }
    #[test]
    fn aggregate_elements_receive_strong_context_and_weak_defaults() {
        let env = HashMap::new();
        let expected = crate::core_ir::rust_arrays::typ(floats::typ(Kind::F32), 2);
        let mut value = crate::core_ir::rust_arrays::value(
            Expr::ArrayLit(vec![
                floats::literal("16777217", None),
                floats::literal("1", Some(Kind::F32)),
            ]),
            2,
        );
        let facts = new_facts();
        assert_eq!(
            finish_value(
                &TypeChecker,
                &mut value,
                Some(&expected),
                &facts,
                &env,
                ResolvePolicy::RustFloatLower
            ),
            Some(expected)
        );
        let Expr::Unary { expr, .. } = value else {
            panic!("array")
        };
        let Expr::ArrayLit(items) = *expr else {
            panic!("items")
        };
        assert_eq!(
            erase_values(items[0].clone()),
            Expr::FloatLit(FloatVal(16777216.0))
        );
        let mut default = Expr::ArrayLit(vec![floats::literal("16777217", None)]);
        assert_eq!(
            finish_value(
                &TypeChecker,
                &mut default,
                None,
                &facts,
                &env,
                ResolvePolicy::RustFloatLower
            ),
            Some(Typ::Array(Box::new(floats::typ(Kind::F64))))
        );
        let Expr::ArrayLit(items) = default else {
            panic!("default")
        };
        assert_eq!(
            erase_values(items[0].clone()),
            Expr::FloatLit(FloatVal(16777217.0))
        );
        let mut mixed = Expr::ArrayLit(vec![
            floats::literal("1", None),
            floats::literal("2", Some(Kind::F32)),
        ]);
        assert_eq!(
            finish_value(
                &TypeChecker,
                &mut mixed,
                None,
                &facts,
                &env,
                ResolvePolicy::RustFloatLower
            ),
            Some(Typ::Array(Box::new(floats::typ(Kind::F32))))
        );
    }
    #[test]
    fn repeat_counts_are_not_float_value_edges() {
        let facts = new_facts();
        let env = HashMap::new();
        let count = Expr::IntLit(2);
        let mut value = crate::core_ir::rust_arrays::value(
            Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::ARRAY_REPEAT.into())),
                args: vec![floats::literal("16777217", None), count.clone()],
            },
            2,
        );
        let expected = crate::core_ir::rust_arrays::typ(floats::typ(Kind::F32), 2);
        finish_value(
            &TypeChecker,
            &mut value,
            Some(&expected),
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        let Expr::Unary { expr, .. } = value else {
            panic!("repeat")
        };
        let Expr::Call { args, .. } = *expr else {
            panic!("call")
        };
        assert_eq!(args[1], count);
        assert_eq!(
            erase_values(args[0].clone()),
            Expr::FloatLit(FloatVal(16777216.0))
        );
    }
    fn generic_facts(slots: usize) -> Facts {
        TypeChecker.collect_decl_facts(&[crate::core_ir::Decl::Function {
            name: "id".into(),
            params: (0..slots)
                .map(|index| (format!("arg{index}"), Typ::Generic("T".into())))
                .collect(),
            ret: Typ::Generic("T".into()),
            body: vec![],
            type_params: vec!["T".into()],
        }])
    }
    fn generic_call(args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident("id".into())),
            args,
        }
    }
    #[test]
    fn direct_generic_call_defers_weak_defaults_to_contextual_result_edge() {
        let facts = generic_facts(1);
        let env = HashMap::new();
        let mut value = generic_call(vec![floats::literal("16777217.0", None)]);
        let before = value.clone();
        assert!(finish_call(
            &TypeChecker,
            &mut value,
            &facts,
            &env,
            ResolvePolicy::RustFloatLower
        ));
        assert_eq!(value, before);
        assert_eq!(
            source_type(&TypeChecker, &value, &facts, &env),
            Some(floats::weak_type())
        );
        assert_eq!(
            finish_value(
                &TypeChecker,
                &mut value,
                Some(&floats::typ(Kind::F32)),
                &facts,
                &env,
                ResolvePolicy::RustFloatLower
            ),
            Some(floats::typ(Kind::F32))
        );
        let Expr::Unary { expr, .. } = value else {
            panic!("typed result")
        };
        let Expr::Call { args, .. } = *expr else {
            panic!("call")
        };
        assert_eq!(
            erase_values(args[0].clone()),
            Expr::FloatLit(FloatVal(16777216.0))
        );
    }
    #[test]
    fn nested_nonfloat_generic_calls_keep_declared_fallback() {
        let facts = generic_facts(1);
        let env = HashMap::new();
        let mut value = Expr::IntLit(42);
        for _ in 0..48 {
            value = generic_call(vec![value]);
        }
        // No float fact was inferred. Preserve the declaration rather than
        // repeatedly asking expr_type to retry the same nested generic call.
        assert_eq!(call_type(&TypeChecker, &value, &facts, &env), None);
        assert_eq!(
            source_type(&TypeChecker, &value, &facts, &env),
            Some(Typ::Generic("T".into()))
        );
        assert_eq!(
            TypeChecker.expr_type(&value, &facts, &env),
            Some(Typ::Generic("T".into()))
        );
        assert!(facts.float_constraints.borrow().failure().is_none());
    }
    #[test]
    fn direct_generic_call_joins_all_matching_slots_without_shadow_capture() {
        let facts = generic_facts(2);
        let env = HashMap::new();
        let mut value = generic_call(vec![
            floats::literal("16777217.0", None),
            floats::literal("1", Some(Kind::F32)),
        ]);
        assert!(finish_call(
            &TypeChecker,
            &mut value,
            &facts,
            &env,
            ResolvePolicy::RustFloatLower
        ));
        let Expr::Call { args, .. } = &value else {
            panic!("call")
        };
        assert_eq!(
            erase_values(args[0].clone()),
            Expr::FloatLit(FloatVal(16777216.0))
        );
        let mut wrong = generic_call(vec![
            floats::literal("1", Some(Kind::F32)),
            floats::literal("1", Some(Kind::F64)),
        ]);
        finish_call(
            &TypeChecker,
            &mut wrong,
            &facts,
            &env,
            ResolvePolicy::RustFloatValidate,
        );
        assert!(
            matches!(wrong,Expr::Ident(name) if type_proofs::decode(&name).unwrap().is_invalid())
        );
        let env = HashMap::from([("id".into(), Typ::Int)]);
        let mut shadowed = generic_call(vec![
            floats::literal("1", Some(Kind::F32)),
            floats::literal("1", Some(Kind::F32)),
        ]);
        let before = shadowed.clone();
        assert!(!finish_call(
            &TypeChecker,
            &mut shadowed,
            &facts,
            &env,
            ResolvePolicy::RustFloatLower
        ));
        assert_eq!(shadowed, before);
    }
    #[test]
    fn index_context_refines_array_initializer_before_defaulting() {
        let facts = new_facts();
        let weak_array = crate::core_ir::rust_arrays::typ(floats::weak_type(), 1);
        let env = HashMap::from([("a".into(), weak_array)]);
        let mut index = Expr::Index {
            base: Box::new(Expr::Ident("a".into())),
            index: Box::new(Expr::IntLit(0)),
        };
        finish_value(
            &TypeChecker,
            &mut index,
            Some(&floats::typ(Kind::F32)),
            &facts,
            &env,
            ResolvePolicy::RustFloatInference,
        );
        let selected = facts.float_constraints.borrow().get("a").unwrap();
        assert_eq!(
            selected,
            crate::core_ir::rust_arrays::typ(floats::typ(Kind::F32), 1)
        );
        let mut initializer = crate::core_ir::rust_arrays::value(
            Expr::ArrayLit(vec![floats::literal(
                "1.0000000596046447753906250000001",
                None,
            )]),
            1,
        );
        finish_value(
            &TypeChecker,
            &mut initializer,
            Some(&selected),
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        let Expr::Unary { expr, .. } = initializer else {
            panic!("array")
        };
        let Expr::ArrayLit(items) = *expr else {
            panic!("items")
        };
        assert_eq!(
            erase_values(items[0].clone()),
            Expr::FloatLit(FloatVal(f64::from(f32::from_bits(1.0f32.to_bits() + 1))))
        );
    }
    #[test]
    fn equal_weak_tuple_shapes_refine_independently_without_schema_mutation() {
        let facts = new_facts();
        let shape = crate::core_ir::tuples::typ(&[floats::weak_type()]);
        let original = shape.clone();
        let env = HashMap::from([("p".into(), shape.clone()), ("q".into(), shape.clone())]);
        for (name, kind) in [("p", Kind::F32), ("q", Kind::F64)] {
            let mut field = Expr::Field {
                base: Box::new(Expr::Ident(name.into())),
                name: "_0".into(),
            };
            finish_value(
                &TypeChecker,
                &mut field,
                Some(&floats::typ(kind)),
                &facts,
                &env,
                ResolvePolicy::RustFloatInference,
            );
        }
        let narrow = facts.float_constraints.borrow().get("p").unwrap();
        let wide = facts.float_constraints.borrow().get("q").unwrap();
        assert_eq!(
            narrow,
            crate::core_ir::tuples::typ(&[floats::typ(Kind::F32)])
        );
        assert_eq!(wide, crate::core_ir::tuples::typ(&[floats::typ(Kind::F64)]));
        assert_eq!(shape, original);
        assert!(facts.structs.is_empty());
        let digits = "1.0000000596046447753906250000001";
        let mut values = Vec::new();
        for expected in [&narrow, &wide] {
            let Typ::Named(name) = &original else {
                panic!("tuple")
            };
            let mut value = Expr::StructInit {
                name: name.clone(),
                fields: vec![("_0".into(), floats::literal(digits, None))],
            };
            assert_eq!(
                finish_value(
                    &TypeChecker,
                    &mut value,
                    Some(expected),
                    &facts,
                    &env,
                    ResolvePolicy::RustFloatLower
                ),
                Some(expected.clone())
            );
            let Expr::StructInit { name, fields } = value else {
                panic!("tuple value")
            };
            assert_eq!(Typ::Named(name), expected.clone());
            values.push(erase_values(fields[0].1.clone()));
        }
        assert_eq!(
            values[0],
            Expr::FloatLit(FloatVal(f64::from(f32::from_bits(1.0f32.to_bits() + 1))))
        );
        assert_eq!(
            values[1],
            Expr::FloatLit(FloatVal(digits.parse::<f64>().unwrap()))
        );
        let mut conflicting = Expr::Field {
            base: Box::new(Expr::Ident("p".into())),
            name: "_0".into(),
        };
        finish_value(
            &TypeChecker,
            &mut conflicting,
            Some(&floats::typ(Kind::F64)),
            &facts,
            &env,
            ResolvePolicy::RustFloatInference,
        );
        assert!(
            facts
                .float_constraints
                .borrow()
                .failure()
                .unwrap()
                .is_invalid()
        );
    }
    #[test]
    fn lowered_array_copy_preserves_backward_element_constraints() {
        let facts = new_facts();
        let weak_array = crate::core_ir::rust_arrays::typ(floats::weak_type(), 1);
        let expected = crate::core_ir::rust_arrays::typ(floats::typ(Kind::F32), 1);
        let env = HashMap::from([("original".into(), weak_array)]);
        let mut value = Expr::Unary {
            op: crate::core_ir::array_copy::operation(1),
            expr: Box::new(Expr::Ident("original".into())),
        };
        let before = value.clone();
        finish_value(
            &TypeChecker,
            &mut value,
            Some(&expected),
            &facts,
            &env,
            ResolvePolicy::RustFloatInference,
        );
        assert_eq!(value, before);
        assert_eq!(
            facts.float_constraints.borrow().get("original"),
            Some(expected)
        );
        assert!(facts.float_constraints.borrow().failure().is_none());
    }
    #[test]
    fn tuple_block_element_uses_shared_scoped_context() {
        use crate::core_ir::Stmt;
        let facts = new_facts();
        let env = HashMap::new();
        let decimal = "1.0000000596046447753906250000001";
        let expected = crate::core_ir::tuples::typ(&[floats::typ(Kind::F32)]);
        let block = Expr::Block {
            body: vec![Stmt::Let("y".into(), None, floats::literal(decimal, None))],
            result: Some(Box::new(Expr::Ident("y".into()))),
        };
        let mut value = Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::tuples::VALUE.into())),
            args: vec![block],
        };
        finish_value(
            &TypeChecker,
            &mut value,
            Some(&expected),
            &facts,
            &env,
            ResolvePolicy::RustFloatInference,
        );
        assert_eq!(
            facts.float_constraints.borrow().get("y"),
            Some(floats::typ(Kind::F32))
        );
        finish_value(
            &TypeChecker,
            &mut value,
            Some(&expected),
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        let Expr::StructInit { fields, .. } = value else {
            panic!("tuple")
        };
        let Expr::Block { body, result } = fields[0].1.clone() else {
            panic!("block")
        };
        let Stmt::Let(_, _, initializer) = &body[0] else {
            panic!("local")
        };
        assert_eq!(
            erase_values(initializer.clone()),
            Expr::FloatLit(FloatVal(f64::from(f32::from_bits(1.0f32.to_bits() + 1))))
        );
        assert_eq!(result, Some(Box::new(Expr::Ident("y".into()))));
        assert_eq!(
            source_type(&TypeChecker, &Expr::Ident("y".into()), &facts, &env),
            None
        );
    }
    #[test]
    fn tuple_marker_and_copy_edges_use_contextual_field_types() {
        let facts = new_facts();
        let env = HashMap::new();
        let expected = crate::core_ir::tuples::typ(&[floats::typ(Kind::F32)]);
        let mut value = Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::tuples::VALUE.into())),
            args: vec![floats::literal("16777217", None)],
        };
        assert_eq!(
            finish_value(
                &TypeChecker,
                &mut value,
                Some(&expected),
                &facts,
                &env,
                ResolvePolicy::RustFloatLower
            ),
            Some(expected.clone())
        );
        assert!(matches!(value, Expr::StructInit { .. }));
        let weak_shape = crate::core_ir::tuples::typ(&[floats::weak_type()]);
        let env = HashMap::from([("original".into(), weak_shape)]);
        let mut copy = Expr::Ident("original".into());
        assert_eq!(
            finish_value(
                &TypeChecker,
                &mut copy,
                Some(&expected),
                &facts,
                &env,
                ResolvePolicy::RustFloatInference
            ),
            Some(expected.clone())
        );
        assert_eq!(copy, Expr::Ident("original".into()));
        assert_eq!(
            facts.float_constraints.borrow().get("original"),
            Some(expected)
        );
    }
    #[test]
    fn mixed_integer_float_tuple_keeps_source_integer_proofs() {
        let facts = new_facts();
        let env = HashMap::new();
        let integer = Typ::Named("#rust-integer-i64".into());
        let expected = crate::core_ir::tuples::typ(&[integer.clone(), floats::typ(Kind::F64)]);
        let mut value = Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::tuples::VALUE.into())),
            args: vec![Expr::IntLit(20), floats::literal("22.0", None)],
        };
        assert_eq!(
            finish_value(
                &TypeChecker,
                &mut value,
                Some(&expected),
                &facts,
                &env,
                ResolvePolicy::RustFloatLower
            ),
            Some(expected.clone())
        );
        let Expr::StructInit { name, fields } = value else {
            panic!("tuple")
        };
        assert_eq!(Typ::Named(name), expected);
        assert_eq!(fields[0].1, Expr::IntLit(20));
        assert_eq!(
            erase_values(fields[1].1.clone()),
            Expr::FloatLit(FloatVal(22.0))
        );
        let left = crate::core_ir::tuples::typ(&[
            Typ::Named("#rust-integer-i8".into()),
            floats::typ(Kind::F32),
        ]);
        let right = crate::core_ir::tuples::typ(&[
            Typ::Named("#rust-integer-i16".into()),
            floats::typ(Kind::F32),
        ]);
        let joined = join_elements(&[Some(left), Some(right)]).unwrap();
        assert!(type_proofs::failure_in_type(&joined).unwrap().is_invalid());
    }
    #[test]
    fn owned_empty_float_vector_allocation_accepts_context_without_value_coercion() {
        let facts = new_facts();
        let env = HashMap::new();
        let expected = Typ::Vector(Box::new(floats::typ(Kind::F64)));
        let intrinsic = expected.vector_allocation_intrinsic().unwrap();
        let value = Expr::Call {
            callee: Box::new(Expr::Ident(intrinsic.into())),
            args: vec![Expr::IntLit(3)],
        };
        for policy in [
            ResolvePolicy::RustFloatInference,
            ResolvePolicy::RustFloatValidate,
            ResolvePolicy::RustFloatLower,
        ] {
            let mut actual = value.clone();
            assert_eq!(
                finish_value(
                    &TypeChecker,
                    &mut actual,
                    Some(&expected),
                    &facts,
                    &env,
                    policy
                ),
                Some(expected.clone())
            );
            assert_eq!(actual, value);
        }
        let narrow = Typ::Vector(Box::new(floats::typ(Kind::F32)));
        let mut narrow_value = value.clone();
        assert_eq!(
            finish_value(
                &TypeChecker,
                &mut narrow_value,
                Some(&narrow),
                &facts,
                &env,
                ResolvePolicy::RustFloatLower
            ),
            Some(narrow)
        );
        assert_eq!(narrow_value, value);
        let shadowed = HashMap::from([(intrinsic.into(), Typ::Int)]);
        assert_eq!(
            contextual_empty_allocation(&TypeChecker, &value, Some(&expected), &facts, &shadowed),
            None
        );
        let wrong_arity = Expr::Call {
            callee: Box::new(Expr::Ident(intrinsic.into())),
            args: vec![],
        };
        assert_eq!(
            contextual_empty_allocation(&TypeChecker, &wrong_arity, Some(&expected), &facts, &env),
            None
        );
        let wrong_capacity = Expr::Call {
            callee: Box::new(Expr::Ident(intrinsic.into())),
            args: vec![Expr::BoolLit(true)],
        };
        assert_eq!(
            contextual_empty_allocation(
                &TypeChecker,
                &wrong_capacity,
                Some(&expected),
                &facts,
                &env
            ),
            None
        );
        let env = HashMap::from([("populated".into(), Typ::Vector(Box::new(Typ::Float)))]);
        let mut populated = Expr::Ident("populated".into());
        finish_value(
            &TypeChecker,
            &mut populated,
            Some(&expected),
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        assert!(
            matches!(populated,Expr::Ident(name) if matches!(type_proofs::decode(&name),Some(ResolutionFailure::Unsupported(_))))
        );
    }
    #[test]
    fn vector_push_finishes_only_a_proven_rust_float_element() {
        let facts = new_facts();
        let env = HashMap::from([("v".into(), Typ::Vector(Box::new(floats::typ(Kind::F32))))]);
        let mut value = Expr::Call {
            callee: Box::new(Expr::Ident("vector-push".into())),
            args: vec![Expr::Ident("v".into()), floats::literal("16777217.0", None)],
        };
        let before = value.clone();
        assert!(finish_call(
            &TypeChecker,
            &mut value,
            &facts,
            &env,
            ResolvePolicy::RustFloatInference
        ));
        assert_eq!(value, before);
        assert!(finish_call(
            &TypeChecker,
            &mut value,
            &facts,
            &env,
            ResolvePolicy::RustFloatLower
        ));
        let Expr::Call { args, .. } = &value else {
            panic!("push")
        };
        assert_eq!(args[0], Expr::Ident("v".into()));
        assert_eq!(
            erase_values(args[1].clone()),
            Expr::FloatLit(FloatVal(16777216.0))
        );
        let env = HashMap::from([("v".into(), Typ::Vector(Box::new(Typ::Float)))]);
        let mut generic = before.clone();
        assert!(!finish_call(
            &TypeChecker,
            &mut generic,
            &facts,
            &env,
            ResolvePolicy::RustFloatLower
        ));
        assert_eq!(generic, before);
    }
    #[test]
    fn mixed_widths_refuse_while_casts_and_generic_core_remain_distinct() {
        let facts = new_facts();
        let env = HashMap::new();
        let mut value = floats::literal("1", Some(Kind::F64));
        finish_value(
            &TypeChecker,
            &mut value,
            Some(&floats::typ(Kind::F32)),
            &facts,
            &env,
            ResolvePolicy::RustFloatValidate,
        );
        assert!(
            matches!(value,Expr::Ident(name) if type_proofs::decode(&name).unwrap().is_invalid())
        );
        let facts = new_facts();
        let mut value = floats::cast(floats::literal("1", Some(Kind::F64)), Kind::F32);
        finish_expr(
            &TypeChecker,
            &mut value,
            &facts,
            &env,
            ResolvePolicy::RustFloatLower,
        );
        assert_eq!(erase_values(value), Expr::FloatLit(FloatVal(1.0)));
        assert!(join_types(&[Some(Typ::Float), Some(Typ::Float)]).is_none());
    }
}
