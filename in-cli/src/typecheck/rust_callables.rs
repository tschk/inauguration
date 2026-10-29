//! Scoped source function-pointer validation. The parent owns all scope traversal.
use super::{Facts, ResolvedInputs, TypeChecker};
use crate::core_ir::{
    Expr, Typ,
    rust_callables::{self, Signature},
    type_proofs::{self, ResolutionFailure},
};
use std::collections::HashMap;

pub(super) fn signature(typ: &Typ) -> Option<Signature> {
    rust_callables::signature_of(typ)
}
// A borrowed function item keeps its immutable, zero-sized source identity.
// Do not extend this to mutable fn-pointer slots without a shared-place ABI,
// or use invocation evidence as an item-to-pointer argument coercion.
fn invocation_signature(typ: &Typ) -> Option<Signature> {
    if let Typ::Named(name) = typ
        && let Some(reference) = crate::core_ir::rust_references::decode(name)
    {
        let Typ::Named(referent) = reference.referent else {
            return None;
        };
        return rust_callables::decode_item(&referent).map(|item| item.signature);
    }
    signature(typ)
}
fn encoded(typ: &Typ) -> bool {
    matches!(typ, Typ::Named(name) if rust_callables::is_encoded(name))
}
pub(super) fn callee_signature(
    callee: &Expr,
    facts: &Facts,
    env: &HashMap<String, Typ>,
) -> Option<Signature> {
    let Expr::Ident(name) = callee else {
        return None;
    };
    if let Some(typ) = env.get(name) {
        return invocation_signature(typ);
    }
    if facts
        .generic_parameters
        .get(name)
        .is_some_and(|parameters| !parameters.is_empty())
    {
        return None;
    }
    facts.callable_signatures.get(name).cloned()
}
fn item_origin(name: &str, facts: &Facts) -> Option<String> {
    let mut current = name.to_owned();
    let mut visiting = std::collections::HashSet::new();
    loop {
        if !visiting.insert(current.clone())
            || !facts.functions.contains_key(&current)
            || !facts.callable_signatures.contains_key(&current)
            || facts
                .generic_parameters
                .get(&current)
                .is_some_and(|parameters| !parameters.is_empty())
        {
            return None;
        }
        match facts.callable_alias_origins.get(&current) {
            Some(Some(target)) => current = target.clone(),
            Some(None) => return None,
            None => return Some(current),
        }
    }
}
pub(super) fn function_value_type(name: &str, facts: &Facts) -> Option<Typ> {
    let identity = item_origin(name, facts)?;
    let signature = facts.callable_signatures.get(&identity)?.clone();
    Some(rust_callables::item_type(rust_callables::Item {
        identity,
        signature,
    }))
}
fn generic_item_origin(name: &str, arity: usize, facts: &Facts) -> Option<String> {
    let mut current = name.to_owned();
    let mut visiting = std::collections::HashSet::new();
    loop {
        if !visiting.insert(current.clone())
            || !facts.functions.contains_key(&current)
            || !facts.callable_signatures.contains_key(&current)
            || facts
                .generic_parameters
                .get(&current)
                .is_none_or(|parameters| parameters.is_empty() || parameters.len() != arity)
        {
            return None;
        }
        match facts.callable_alias_origins.get(&current) {
            Some(Some(target)) => current = target.clone(),
            Some(None) => return None,
            None => return Some(current),
        }
    }
}
pub(super) fn generic_function_value_type(
    name: &str,
    arguments: &[Typ],
    facts: &Facts,
) -> Option<Typ> {
    if arguments.is_empty() || crate::core_ir::rust_generics::argument_failure(arguments).is_some()
    {
        return None;
    }
    let origin = generic_item_origin(name, arguments.len(), facts)?;
    let parameters = facts.generic_parameters.get(&origin)?;
    let bindings = parameters
        .iter()
        .cloned()
        .zip(arguments.iter().cloned())
        .collect();
    let mut projected = crate::core_ir::Decl::Function {
        name: "#generic-callable-item".into(),
        params: vec![],
        ret: rust_callables::typ(facts.callable_signatures.get(&origin)?.clone()),
        body: vec![],
        type_params: vec![],
    };
    crate::core_bindings::substitute_type_parameters(&mut projected, &bindings);
    let crate::core_ir::Decl::Function { ret, .. } = projected else {
        unreachable!()
    };
    let signature = signature(&ret)?;
    Some(rust_callables::item_type(rust_callables::Item {
        identity: format!(
            "#rust-specialization:{origin}:{}",
            crate::core_ir::rust_generics::arguments(arguments)
        ),
        signature,
    }))
}
fn unsupported(reason: &str) -> ResolutionFailure {
    ResolutionFailure::Unsupported(reason.into())
}
fn agreement(expected: &Typ, actual: &Typ) -> Option<ResolutionFailure> {
    if let Some(failure) = type_proofs::prefer_failure(
        type_proofs::failure_in_type(expected),
        type_proofs::failure_in_type(actual),
    ) {
        return Some(failure);
    }
    if let Typ::Named(expected_name) = expected
        && let Some(expected_item) = rust_callables::decode_item(expected_name)
    {
        match actual {
            Typ::Named(actual_name) => match rust_callables::decode_item(actual_name) {
                Some(actual_item) if expected_item.identity == actual_item.identity => {}
                Some(_) => {
                    return Some(ResolutionFailure::Invalid(
                        "distinct Rust function item identities".into(),
                    ));
                }
                None => {
                    return Some(unsupported(
                        "a function pointer cannot become a particular function item",
                    ));
                }
            },
            _ => {
                return Some(ResolutionFailure::Invalid(
                    "function item source identity mismatch".into(),
                ));
            }
        }
    }
    match (signature(expected), signature(actual)) {
        (Some(expected), Some(actual)) => {
            if expected.abi != actual.abi
                || expected.unsafe_call != actual.unsafe_call
                || expected.variadic != actual.variadic
                || expected.bound_lifetimes != actual.bound_lifetimes
            {
                return Some(unsupported(
                    "callable ABI, safety or lifetime coercion is not owned",
                ));
            }
            if expected.parameters.len() != actual.parameters.len() {
                return Some(ResolutionFailure::Invalid(
                    "incompatible Rust callable argument count".into(),
                ));
            }
            expected
                .parameters
                .iter()
                .zip(&actual.parameters)
                .chain(std::iter::once((&expected.result, &actual.result)))
                .fold(None, |failure, (expected, actual)| {
                    type_proofs::prefer_failure(failure, agreement(expected, actual))
                })
        }
        (Some(_), None)
            if matches!(
                actual,
                Typ::Bool | Typ::Float | Typ::String | Typ::Void | Typ::Array(_) | Typ::Vector(_)
            ) || matches!(actual, Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some() || crate::core_ir::rust_floats::is_proof(name)) =>
        {
            Some(ResolutionFailure::Invalid(
                "non-callable Rust value requires a function pointer".into(),
            ))
        }
        (Some(_), None) if !encoded(actual) => Some(unsupported(
            "callable value lacks an exact source function signature",
        )),
        (None, Some(_)) if !encoded(expected) => {
            super::rust_generics::proven_operator_result_failure(actual, Some(expected))
        }
        _ if encoded(expected) || encoded(actual) => {
            Some(unsupported("malformed Rust callable signature"))
        }
        _ => super::rust_generics::proven_operator_result_failure(actual, Some(expected)),
    }
}
fn definite_noncallable(typ: &Typ, expression: &Expr, facts: &Facts) -> bool {
    match typ {
        Typ::Bool | Typ::Float | Typ::String | Typ::Void | Typ::Array(_) | Typ::Vector(_) => true,
        Typ::Named(name) => {
            crate::core_ir::rust_integer_type_name(name).is_some()
                || crate::core_ir::rust_floats::is_proof(name)
                || crate::core_ir::rust_references::decode(name).is_some()
                || crate::core_ir::rust_arrays::decode(name).is_some()
                || crate::core_ir::tuples::fields(name).is_some()
                || facts.structs.contains_key(name)
        }
        Typ::Int => {
            matches!(expression, Expr::IntLit(_))
                || matches!(expression, Expr::Unary { op, .. } if op.starts_with("rust-integer-literal"))
        }
        _ => false,
    }
}
fn cast_value(
    expr: &mut Expr,
    facts: &Facts,
    inputs: &ResolvedInputs<'_>,
    lower: bool,
    word64: bool,
) -> Option<Option<Typ>> {
    if let Expr::Call { callee, .. } = expr
        && matches!(callee.as_ref(), Expr::Ident(name) if name == "unsupported-rust-cast" && !facts.functions.contains_key(name))
        && let Some(source) = inputs
            .arguments
            .and_then(|values| values.first())
            .and_then(Option::as_ref)
            .filter(|typ| encoded(typ))
    {
        let failure = type_proofs::failure_in_type(source).unwrap_or_else(|| {
            if signature(source).is_some() {
                ResolutionFailure::Invalid(
                    "Rust callables cannot be cast to this target type".into(),
                )
            } else {
                unsupported("malformed Rust callable cast operand")
            }
        });
        *expr = Expr::Ident(failure.symbol());
        return Some(Some(failure.typ()));
    }
    let Expr::Unary { op, expr: operand } = expr else {
        return None;
    };
    let source = inputs
        .operands
        .and_then(|values| values.first())
        .and_then(Option::as_ref);
    if rust_callables::is_cast(op) {
        let mut failure = source.and_then(type_proofs::failure_in_type);
        let target = rust_callables::decode_cast(op);
        let result = target
            .as_ref()
            .map(|signature| rust_callables::typ(signature.clone()));
        if let Some(target) = &target {
            failure = type_proofs::prefer_failure(failure, signature_failure(target));
            let proof = match source {
                Some(source) if signature(source).is_some() => {
                    agreement(&rust_callables::typ(target.clone()), source)
                }
                Some(source) if definite_noncallable(source, operand, facts) => {
                    Some(ResolutionFailure::Invalid(
                        "non-callable Rust value cannot be cast to a function pointer".into(),
                    ))
                }
                _ => Some(unsupported(
                    "function pointer cast source identity is unresolved",
                )),
            };
            failure = type_proofs::prefer_failure(failure, proof);
            if !target.ordinary_rust_form() {
                failure = type_proofs::prefer_failure(
                    failure,
                    Some(unsupported(
                        "function pointer cast ABI, safety or lifetime coercion is not owned",
                    )),
                );
            }
        } else {
            failure = type_proofs::prefer_failure(
                failure,
                Some(unsupported("malformed Rust callable cast")),
            );
        }
        if let Some(failure) = failure {
            *expr = Expr::Ident(failure.symbol());
            return Some(Some(failure.typ()));
        }
        if lower {
            *expr = *std::mem::replace(operand, Box::new(Expr::UnitLit));
        }
        return Some(result);
    }
    if op == "numeric-cast-usize" && source.is_some_and(encoded) {
        let failure = source.and_then(type_proofs::failure_in_type).or_else(|| {
            (!word64).then(|| {
                unsupported("Rust callable addresses require the owned Word64 representation")
            })
        });
        if let Some(failure) = failure {
            *expr = Expr::Ident(failure.symbol());
            return Some(Some(failure.typ()));
        }
        if source.and_then(signature).is_none() {
            let failure = unsupported("malformed Rust callable address cast");
            *expr = Expr::Ident(failure.symbol());
            return Some(Some(failure.typ()));
        }
        if lower {
            *expr = *std::mem::replace(operand, Box::new(Expr::UnitLit));
        }
        return Some(Some(if lower {
            Typ::Int
        } else {
            Typ::Named("#rust-integer-usize".into())
        }));
    }
    None
}

/// Conditional/match result coercion is local to this value join. It must not
/// merge function item identities in unconstrained generic argument slots.
pub(super) fn join(types: &[Option<Typ>]) -> Option<Typ> {
    let first = types.first()?.as_ref()?;
    let first_signature = signature(first)?;
    let pointer = rust_callables::typ(first_signature);
    let mut identical = true;
    for typ in types {
        let typ = typ.as_ref()?;
        signature(typ)?;
        if agreement(&pointer, typ).is_some() {
            return None;
        }
        identical &= crate::core_ir::rust_references::same_source_type(first, typ);
    }
    Some(if identical { first.clone() } else { pointer })
}

fn supported(signature: &Signature, lower: bool) -> bool {
    signature.ordinary_rust_form()
        && signature.parameters.len() <= 2
        && signature.parameters.iter().chain(std::iter::once(&signature.result)).all(|typ| {
            matches!(typ, Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some())
                || (lower && typ == &Typ::Int)
        })
}
fn signature_failure(signature: &Signature) -> Option<ResolutionFailure> {
    let mut failure = None;
    signature.visit_types(&mut |typ| {
        failure = type_proofs::prefer_failure(failure.take(), type_proofs::failure_in_type(typ));
    });
    failure
}
// Source-domain validation consumes the parent walker's already resolved
// children. It must run before runtime Int erasure and before dead-code pruning.
fn operator_value(
    expr: &mut Expr,
    facts: &Facts,
    inputs: &ResolvedInputs<'_>,
    lower: bool,
    word64: bool,
) -> Option<Option<Typ>> {
    let operands = inputs.operands?;
    if !operands.iter().flatten().any(encoded) {
        return None;
    }
    let mut failure = operands.iter().flatten().fold(None, |failure, typ| {
        type_proofs::prefer_failure(failure, type_proofs::failure_in_type(typ))
    });
    if operands
        .iter()
        .flatten()
        .any(|typ| encoded(typ) && signature(typ).is_none())
    {
        let failure = type_proofs::prefer_failure(
            failure,
            Some(unsupported("malformed Rust callable operator source type")),
        )
        .expect("malformed callable supplies a failure");
        *expr = Expr::Ident(failure.symbol());
        return Some(Some(failure.typ()));
    }
    let mut result = None;
    match expr {
        Expr::Binary { op, lhs, rhs } => {
            let comparison = matches!(op.as_str(), "==" | "!=" | "<" | "<=" | ">" | ">=");
            let left = operands.first().and_then(Option::as_ref);
            let right = operands.get(1).and_then(Option::as_ref);
            let item = operands.iter().flatten().any(|typ| matches!(typ, Typ::Named(name) if rust_callables::decode_item(name).is_some()));
            let check = if item {
                Some(ResolutionFailure::Invalid(
                    "Rust function items do not support scalar operators".into(),
                ))
            } else if comparison {
                result = Some(Typ::Bool);
                match (left, right) {
                    (Some(left), Some(right))
                        if signature(left).is_some() && signature(right).is_some() =>
                    {
                        let mut check = agreement(left, right);
                        if !word64 {
                            check = type_proofs::prefer_failure(
                                check,
                                Some(unsupported(
                                    "Rust function pointer comparisons require Word64",
                                )),
                            );
                        }
                        if lower && check.is_none() && !matches!(op.as_str(), "==" | "!=") {
                            // Existing integer lowering uses this same sign-bit bias.
                            // Move children once: no repeated source evaluation.
                            let operator = op.clone();
                            let left = *std::mem::replace(lhs, Box::new(Expr::UnitLit));
                            let right = *std::mem::replace(rhs, Box::new(Expr::UnitLit));
                            if let Some(unsigned) = crate::core_ir::unsigned_compare_expr(
                                &operator,
                                left,
                                right,
                                64,
                                crate::integer_semantics::IntegerSemantics::Word64,
                            ) {
                                *expr = unsigned;
                            } else {
                                check = Some(unsupported(
                                    "Rust function pointer ordering lacks unsigned lowering",
                                ));
                            }
                        }
                        check
                    }
                    (Some(left), Some(right))
                        if (encoded(left) && definite_noncallable(right, rhs, facts))
                            || (encoded(right) && definite_noncallable(left, lhs, facts)) =>
                    {
                        Some(ResolutionFailure::Invalid(
                            "Rust function pointer comparison requires compatible pointers".into(),
                        ))
                    }
                    _ => Some(unsupported(
                        "Rust function pointer comparison source types are unresolved",
                    )),
                }
            } else if matches!(
                op.as_str(),
                "+" | "-"
                    | "*"
                    | "/"
                    | "%"
                    | "&"
                    | "|"
                    | "^"
                    | "<<"
                    | ">>"
                    | "&&"
                    | "||"
                    | "udiv"
                    | "urem"
                    | "lshr"
            ) {
                Some(ResolutionFailure::Invalid(
                    "Rust function pointers do not support arithmetic or bitwise operators".into(),
                ))
            } else {
                Some(unsupported(
                    "Rust callable operator source semantics are unresolved",
                ))
            };
            failure = type_proofs::prefer_failure(failure, check);
        }
        Expr::Unary { op, .. }
            if matches!(
                op.as_str(),
                "!" | "-"
                    | "+"
                    | "complement"
                    | "~"
                    | "bitnot"
                    | crate::core_ir::rust_references::NEGATE_VALUE
            ) =>
        {
            failure = type_proofs::prefer_failure(
                failure,
                Some(ResolutionFailure::Invalid(
                    "Rust callables do not support unary scalar operators".into(),
                )),
            );
        }
        // Declaration, source-cast and transfer wrappers have separate owners.
        _ => return None,
    }
    if let Some(failure) = failure {
        *expr = Expr::Ident(failure.symbol());
        return Some(Some(failure.typ()));
    }
    Some(result)
}

/// Return Some(None) for a handled but unresolved edge, distinct from an edge
/// not belonging to this policy. No child expression is evaluated or revisited.
#[allow(clippy::too_many_arguments)]
pub(super) fn finish(
    checker: &TypeChecker,
    expr: &mut Expr,
    expected: Option<&Typ>,
    facts: &Facts,
    env: &HashMap<String, Typ>,
    inputs: &ResolvedInputs<'_>,
    lower: bool,
    word64: bool,
    callee_type: Option<&Option<Typ>>,
) -> Option<Option<Typ>> {
    let cast_result = cast_value(expr, facts, inputs, lower, word64)
        .or_else(|| operator_value(expr, facts, inputs, lower, word64));
    let mut failure = None;
    if let Expr::Ident(name) = expr
        && !env.contains_key(name)
        && facts.callable_alias_origins.contains_key(name)
    {
        if let Some(origin) = item_origin(name, facts) {
            *name = origin;
        } else {
            failure = Some(unsupported(
                "callable alias has no unique owned source item origin",
            ));
        }
    }
    let mut actual = cast_result.clone().unwrap_or_else(|| {
        inputs
            .actual
            .clone()
            .unwrap_or_else(|| checker.expr_type(expr, facts, env))
    });
    let mut callable_result = false;
    if cast_result.is_none()
        && let Expr::Call { callee, args } = expr
    {
        let direct = matches!(callee.as_ref(), Expr::Ident(name)
            if !env.contains_key(name) && facts.callable_signatures.contains_key(name));
        let resolved_signature = match callee_type {
            Some(value) => value.as_ref().and_then(invocation_signature),
            None => callee_signature(callee, facts, env),
        };
        let local = !direct
            && (resolved_signature.is_some()
                || matches!(callee.as_ref(), Expr::Ident(name) if env.get(name).is_some_and(encoded)));
        if let Some(callable) = resolved_signature {
            callable_result = local;
            failure = type_proofs::prefer_failure(failure, signature_failure(&callable));
            if callable.parameters.len() != args.len() && local {
                failure = Some(ResolutionFailure::Invalid(
                    "incompatible Rust callable argument count".into(),
                ));
            }
            for (index, expected) in callable.parameters.iter().enumerate() {
                let argument = inputs
                    .arguments
                    .and_then(|values| values.get(index))
                    .and_then(Option::as_ref);
                if !local && !encoded(expected) && !argument.is_some_and(encoded) {
                    continue;
                }
                failure = type_proofs::prefer_failure(
                    failure,
                    match argument {
                        Some(actual)
                            if encoded(expected)
                                && signature(actual).is_none()
                                && args.get(index).is_some_and(|expression| {
                                    definite_noncallable(actual, expression, facts)
                                }) =>
                        {
                            Some(ResolutionFailure::Invalid(
                                "non-callable Rust argument requires a function pointer".into(),
                            ))
                        }
                        Some(actual) => agreement(expected, actual),
                        None => Some(unsupported("callable argument source type is unresolved")),
                    },
                );
            }
            if local || encoded(&callable.result) {
                actual = Some(callable.result.clone());
            }
            if local && (!word64 || !supported(&callable, lower)) {
                failure = type_proofs::prefer_failure(
                    failure,
                    Some(unsupported(
                        "Rust callable requires an owned Word64 Int signature with zero to two parameters",
                    )),
                );
            }
            if local && lower && failure.is_none() {
                let mut operands = Vec::with_capacity(args.len() + 1);
                operands.push(*std::mem::replace(callee, Box::new(Expr::UnitLit)));
                operands.append(args);
                let name = ["invoke", "invoke1", "invoke2"][callable.parameters.len()];
                *expr = Expr::Call {
                    callee: Box::new(Expr::Ident(name.into())),
                    args: operands,
                };
            }
        } else if local {
            failure = Some(unsupported("malformed Rust callable signature"));
        }
    }
    if let Some(expected) = expected {
        // Only callable source edges are new validation obligations here.
        if callable_result || encoded(expected) || actual.as_ref().is_some_and(encoded) {
            failure = type_proofs::prefer_failure(
                failure,
                match &actual {
                    Some(actual)
                        if encoded(expected)
                            && signature(actual).is_none()
                            && definite_noncallable(actual, expr, facts) =>
                    {
                        Some(ResolutionFailure::Invalid(
                            "non-callable Rust value requires a function pointer".into(),
                        ))
                    }
                    Some(actual) => agreement(expected, actual),
                    None => Some(unsupported("callable value source type is unresolved")),
                },
            );
        }
    }
    if let Some(failure) = failure {
        *expr = Expr::Ident(failure.symbol());
        return Some(Some(failure.typ()));
    }
    if let Some(expected @ Typ::Named(expected_name)) = expected
        && rust_callables::decode(expected_name).is_some()
        && matches!(&actual, Some(Typ::Named(name)) if rust_callables::decode_item(name).is_some())
    {
        actual = Some(expected.clone());
    }
    Some(actual)
}

#[cfg(test)]
mod tests {
    #[test]
    fn callable_operator_source_domains_match_rust_oracles_including_cold_code() {
        use super::*;
        for (name, source, invalid) in [
            (
                "item_equality",
                r#"fn a()->i64{42}fn main()->i64{if a==a{42}else{7}}"#,
                true,
            ),
            (
                "pointer_equality",
                r#"fn a()->i64{42}fn main()->i64{let p=a as fn()->i64;if p==p{42}else{7}}"#,
                false,
            ),
            (
                "pointer_order",
                r#"fn a()->i64{42}fn main()->i64{let p=a as fn()->i64;if p<=p{42}else{7}}"#,
                false,
            ),
            (
                "item_add",
                r#"fn a()->i64{42}fn main()->i64{let v=a+1;42}"#,
                true,
            ),
            (
                "pointer_add",
                r#"fn a()->i64{42}fn main()->i64{let p=a as fn()->i64;let v=p+1i64;42}"#,
                true,
            ),
            (
                "item_usize",
                r#"fn a()->i64{42}fn main()->i64{let address=a as usize;if address!=0{42}else{7}}"#,
                false,
            ),
            (
                "pointer_usize",
                r#"fn a()->i64{42}fn main()->i64{let p=a as fn()->i64;let address=p as usize;if address!=0{42}else{7}}"#,
                false,
            ),
            (
                "item_not",
                r#"fn a()->i64{42}fn main()->i64{let v=!a;42}"#,
                true,
            ),
            (
                "pointer_neg",
                r#"fn a()->i64{42}fn main()->i64{let p=a as fn()->i64;let v=-p;42}"#,
                true,
            ),
            (
                "item_bool_cast",
                r#"fn a()->i64{42}fn main()->i64{let v=a as bool;42}"#,
                true,
            ),
            (
                "pointer_integer_equality",
                r#"fn a()->i64{42}fn main()->i64{let p=a as fn()->i64;if p==42i64{42}else{7}}"#,
                true,
            ),
            (
                "item_equality_cold",
                r#"fn a()->i64{42}fn main()->i64{if false{let v=a==a;}42}"#,
                true,
            ),
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let normalized =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            let proofs = TypeChecker::new().typed_proof_nodes(&normalized.decls);
            assert_eq!(
                proofs.values().any(ResolutionFailure::is_invalid),
                invalid,
                "{name}: {proofs:?}"
            );
        }
    }
    #[test]
    fn callable_operator_ordering_is_unsigned_and_evaluates_operands_once() {
        use super::*;
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        let pointer = rust_callables::typ(source_signature(vec![], integer("i64")));
        let operands = [Some(pointer.clone()), Some(pointer)];
        let lhs = Expr::IntLit(i64::MIN);
        let rhs = Expr::IntLit(1);
        let mut expression = Expr::Binary {
            op: "<".into(),
            lhs: Box::new(lhs.clone()),
            rhs: Box::new(rhs.clone()),
        };
        assert_eq!(
            operator_value(
                &mut expression,
                &facts,
                &ResolvedInputs {
                    operands: Some(&operands),
                    ..Default::default()
                },
                true,
                true
            ),
            Some(Some(Typ::Bool))
        );
        let expected = crate::core_ir::unsigned_compare_expr(
            "<",
            lhs,
            rhs,
            64,
            crate::integer_semantics::IntegerSemantics::Word64,
        )
        .unwrap();
        assert_eq!(expression, expected);
        assert_eq!(
            crate::core_opt::map_expr(expression.clone(), &mut crate::core_opt::fold_expr),
            Expr::BoolLit(false)
        );
        let mut unknownword = Expr::Binary {
            op: "<".into(),
            lhs: Box::new(Expr::Ident("left".into())),
            rhs: Box::new(Expr::Ident("right".into())),
        };
        let result = operator_value(
            &mut unknownword,
            &facts,
            &ResolvedInputs {
                operands: Some(&operands),
                ..Default::default()
            },
            true,
            false,
        )
        .flatten()
        .unwrap();
        assert!(!type_proofs::failure_in_type(&result).unwrap().is_invalid());
    }

    #[test]
    fn callable_operator_inherited_invalid_dominates_unknown_other_operand() {
        use super::*;
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        let invalid = ResolutionFailure::Invalid("source argument cycle".into());
        let operands = [
            Some(rust_callables::typ(source_signature(
                vec![],
                integer("i64"),
            ))),
            Some(invalid.clone().typ()),
        ];
        let mut expression = Expr::Binary {
            op: "==".into(),
            lhs: Box::new(Expr::Ident("pointer".into())),
            rhs: Box::new(Expr::Ident("bad".into())),
        };
        let result = operator_value(
            &mut expression,
            &facts,
            &ResolvedInputs {
                operands: Some(&operands),
                ..Default::default()
            },
            false,
            true,
        )
        .flatten()
        .unwrap();
        assert_eq!(type_proofs::failure_in_type(&result), Some(invalid));
    }
    use super::*;
    fn integer(name: &str) -> Typ {
        Typ::Named(format!("#rust-integer-{name}"))
    }
    fn source_signature(parameters: Vec<Typ>, result: Typ) -> Signature {
        Signature {
            abi: rust_callables::Abi::Rust,
            unsafe_call: false,
            variadic: false,
            bound_lifetimes: false,
            parameters,
            result,
        }
    }
    #[test]
    fn local_callable_lowering_moves_target_and_arguments_once() {
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        let env = HashMap::from([(
            "callback".into(),
            rust_callables::typ(source_signature(vec![integer("i64")], integer("i64"))),
        )]);
        let argument = Expr::Call {
            callee: Box::new(Expr::Ident("effect".into())),
            args: vec![],
        };
        let mut expression = Expr::Call {
            callee: Box::new(Expr::Ident("callback".into())),
            args: vec![argument.clone()],
        };
        let actuals = [Some(integer("i64"))];
        let result = finish(
            &checker,
            &mut expression,
            Some(&integer("i64")),
            &facts,
            &env,
            &ResolvedInputs {
                arguments: Some(&actuals),
                ..Default::default()
            },
            true,
            true,
            None,
        );
        assert_eq!(result, Some(Some(integer("i64"))));
        assert_eq!(
            expression,
            Expr::Call {
                callee: Box::new(Expr::Ident("invoke1".into())),
                args: vec![Expr::Ident("callback".into()), argument]
            }
        );
    }
    #[test]
    fn callable_mismatch_and_nested_invalid_beat_unsupported_abi() {
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        let mut unsupported_signature = source_signature(vec![integer("i64")], Typ::Float);
        unsupported_signature.unsafe_call = true;
        let env = HashMap::from([(
            "callback".into(),
            rust_callables::typ(unsupported_signature),
        )]);
        let invalid = ResolutionFailure::Invalid("cyclic callable argument alias".into());
        let actuals = [Some(invalid.clone().typ())];
        let mut expression = Expr::Call {
            callee: Box::new(Expr::Ident("callback".into())),
            args: vec![Expr::IntLit(0)],
        };
        let result = finish(
            &checker,
            &mut expression,
            None,
            &facts,
            &env,
            &ResolvedInputs {
                arguments: Some(&actuals),
                ..Default::default()
            },
            true,
            true,
            None,
        );
        assert_eq!(result, Some(Some(invalid.clone().typ())));
        assert_eq!(expression, Expr::Ident(invalid.symbol()));
        assert!(
            agreement(&integer("usize"), &integer("u64"))
                .is_some_and(|failure| failure.is_invalid())
        );
    }
    #[test]
    fn function_item_identity_survives_until_an_owned_pointer_coercion() {
        let signature = source_signature(vec![], integer("i64"));
        let item = |identity: &str| {
            rust_callables::item_type(rust_callables::Item {
                identity: identity.into(),
                signature: signature.clone(),
            })
        };
        let first = item("left::answer");
        let second = item("right::answer");
        assert!(agreement(&first, &second).is_some_and(|failure| failure.is_invalid()));
        let pointer = rust_callables::typ(signature);
        assert!(agreement(&pointer, &first).is_none());
        assert_eq!(
            join(&[Some(first.clone()), Some(first.clone())]),
            Some(first.clone())
        );
        assert_eq!(
            join(&[Some(first.clone()), Some(second)]),
            Some(pointer.clone())
        );
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        let env = HashMap::new();
        let mut value = Expr::Ident("left::answer".into());
        assert_eq!(
            finish(
                &checker,
                &mut value,
                Some(&pointer),
                &facts,
                &env,
                &ResolvedInputs {
                    actual: Some(Some(first.clone())),
                    ..Default::default()
                },
                false,
                true,
                None
            ),
            Some(Some(pointer))
        );
        assert_eq!(
            finish(
                &checker,
                &mut value,
                None,
                &facts,
                &env,
                &ResolvedInputs {
                    actual: Some(Some(first.clone())),
                    ..Default::default()
                },
                false,
                true,
                None
            ),
            Some(Some(first))
        );
    }
    #[test]
    fn compiler_alias_origins_preserve_item_identity_and_physical_function_value() {
        let checker = TypeChecker::new();
        let mut facts = checker.collect_decl_facts(&[]);
        let signature = source_signature(vec![], integer("i64"));
        for name in ["source::answer", "relay", "outer::alias"] {
            facts
                .functions
                .insert(name.into(), (vec![], integer("i64")));
            facts
                .callable_signatures
                .insert(name.into(), signature.clone());
        }
        facts
            .callable_alias_origins
            .insert("relay".into(), Some("source::answer".into()));
        facts
            .callable_alias_origins
            .insert("outer::alias".into(), Some("relay".into()));
        assert_eq!(
            function_value_type("source::answer", &facts),
            function_value_type("outer::alias", &facts)
        );
        let mut expression = Expr::Ident("outer::alias".into());
        finish(
            &checker,
            &mut expression,
            None,
            &facts,
            &HashMap::new(),
            &ResolvedInputs::default(),
            false,
            true,
            None,
        );
        assert_eq!(expression, Expr::Ident("source::answer".into()));
        facts
            .callable_alias_origins
            .insert("source::answer".into(), Some("relay".into()));
        assert_eq!(function_value_type("outer::alias", &facts), None);
    }
    #[test]
    fn explicit_pointer_cast_validates_operand_instead_of_trusting_target() {
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        let env = HashMap::new();
        let target = source_signature(vec![], integer("i64"));
        let item = rust_callables::item_type(rust_callables::Item {
            identity: "answer".into(),
            signature: target.clone(),
        });
        for (actual, invalid) in [(Some(item.clone()), false), (Some(integer("i64")), true)] {
            let mut expression =
                rust_callables::cast(Expr::Ident("operand".into()), target.clone());
            let operands = [actual];
            let result = finish(
                &checker,
                &mut expression,
                None,
                &facts,
                &env,
                &ResolvedInputs {
                    operands: Some(&operands),
                    ..Default::default()
                },
                false,
                true,
                None,
            )
            .flatten();
            assert_eq!(
                result
                    .as_ref()
                    .and_then(type_proofs::failure_in_type)
                    .is_some_and(|failure| failure.is_invalid()),
                invalid
            );
            if !invalid {
                assert_eq!(result, Some(rust_callables::typ(target.clone())));
            }
        }
        let mut expression = rust_callables::cast(Expr::Ident("unknown".into()), target.clone());
        let operands = [None];
        let result = finish(
            &checker,
            &mut expression,
            None,
            &facts,
            &env,
            &ResolvedInputs {
                operands: Some(&operands),
                ..Default::default()
            },
            false,
            true,
            None,
        )
        .flatten();
        assert!(
            result
                .as_ref()
                .and_then(type_proofs::failure_in_type)
                .is_some()
        );
        let mut expression = Expr::Unary {
            op: "numeric-cast-usize".into(),
            expr: Box::new(Expr::Ident("answer".into())),
        };
        let operands = [Some(item)];
        assert_eq!(
            finish(
                &checker,
                &mut expression,
                None,
                &facts,
                &env,
                &ResolvedInputs {
                    operands: Some(&operands),
                    ..Default::default()
                },
                true,
                true,
                None
            ),
            Some(Some(Typ::Int))
        );
        assert_eq!(expression, Expr::Ident("answer".into()));
    }
    #[test]
    fn callable_erasure_preserves_nested_invalid_proofs() {
        use crate::core_ir::{Decl, Stmt, UnifiedModule};
        let failure = ResolutionFailure::Invalid("invalid callable result".into());
        let signature = source_signature(vec![integer("i64")], failure.typ());
        let mut module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![("callback".into(), rust_callables::typ(signature.clone()))],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![
                Stmt::Expr(Expr::Unary {
                    op: rust_callables::declaration(&signature),
                    expr: Box::new(Expr::UnitLit),
                }),
                Stmt::Return(Some(Expr::IntLit(42))),
            ],
        }]);
        super::super::erase_rust_callable_metadata(&mut module);
        assert!(
            TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .values()
                .any(ResolutionFailure::is_invalid)
        );
        let Decl::Function { params, body, .. } = &module.decls[0] else {
            panic!()
        };
        assert!(
            type_proofs::failure_in_type(&params[0].1)
                .unwrap()
                .is_invalid()
        );
        assert!(
            matches!(&body[0], Stmt::Expr(Expr::Ident(name)) if type_proofs::decode(name).is_some_and(|proof|proof.is_invalid()))
        );
    }

    #[test]
    fn raw_mir_and_source_sinks_refuse_alias_origin_metadata() {
        use crate::core_ir::{Decl, Stmt, UnifiedModule};
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Bool,
            type_params: vec![],
            body: vec![
                Stmt::Expr(Expr::Unary {
                    op: rust_callables::alias_origin("original"),
                    expr: Box::new(Expr::UnitLit),
                }),
                Stmt::Return(Some(Expr::BoolLit(true))),
            ],
        }]);
        assert!(
            crate::compiler::mir_lower::lower_to_mir(&module)
                .unwrap_err()
                .contains("requires normalization")
        );
        for language in [
            crate::native_emit::source_emit::SourceLang::Go,
            crate::native_emit::source_emit::SourceLang::Rust,
        ] {
            assert!(
                crate::native_emit::source_emit::emit_source_module(&module, language)
                    .unwrap_err()
                    .contains("requires normalization")
            );
        }
    }

    #[test]
    fn generic_alias_origins_preserve_item_identity_across_turbofish_and_inference() {
        for (name, source, valid) in [
            (
                "alias_value",
                "fn id<T>(x:T)->T{x}use id as alias;fn main()->i64{let pick=alias::<i64>;pick(42i64)}",
                true,
            ),
            (
                "same_origin",
                "fn id<T>(x:T)->T{x}use id as alias;fn same<T>(a:T,_b:T)->T{a}fn main()->i64{same(id::<i64>,alias::<i64>)(42i64)}",
                true,
            ),
            (
                "distinct_arguments",
                "fn id<T>(x:T)->T{x}use id as alias;fn same<T>(a:T,_b:T)->T{a}fn main()->i64{same(id::<i64>,alias::<u64>)(42i64)}",
                false,
            ),
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let normalized =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            let proofs = TypeChecker::new().typed_proof_nodes(&normalized.decls);
            if valid {
                assert!(proofs.is_empty(), "{name}: {proofs:?}");
            } else {
                assert!(
                    proofs.values().any(ResolutionFailure::is_invalid),
                    "{name}: {proofs:?}"
                );
            }
        }
    }
}
