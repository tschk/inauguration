//! Bind generic method calls before substitution can expose inherent methods.
use super::{Facts, ResolvePolicy, TypeChecker};
use crate::core_ir::type_proofs::ResolutionFailure;
use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule, rust_generics, rust_references};
use std::collections::HashMap;

pub(super) fn bind_templates(module: &mut UnifiedModule) {
    if !module.decls.iter().any(|decl| {
        matches!(decl, Decl::Function { type_params, body, .. }
            if !type_params.is_empty() && body.iter().any(|statement|
                matches!(statement, Stmt::Expr(Expr::Unary { op, .. })
                    if rust_generics::is_trait_bounds(op))))
    }) {
        return;
    }
    let checker = TypeChecker::new();
    let mut facts = checker.collect_facts(module);
    let globals: HashMap<_, _> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect();
    for declaration in &mut module.decls {
        let Decl::Function {
            params,
            ret,
            body,
            type_params,
            ..
        } = declaration
        else {
            continue;
        };
        if type_params.is_empty() {
            continue;
        }
        facts.bound_trait_methods = body
            .iter()
            .filter_map(|statement| match statement {
                Stmt::Expr(Expr::Unary { op, expr }) if matches!(expr.as_ref(), Expr::UnitLit) => {
                    rust_generics::decode_trait_bounds(op)
                }
                _ => None,
            })
            .flatten()
            .collect();
        if facts.bound_trait_methods.is_empty() {
            continue;
        }
        let mut environment = globals.clone();
        environment.extend(params.iter().cloned());
        checker.resolve_typed_body(
            body,
            &facts,
            &mut environment,
            Some(ret),
            ResolvePolicy::RustTraitMethods,
        );
    }
}

pub(super) fn bind_call(
    expression: &mut Expr,
    arguments: Option<&[Option<Typ>]>,
    expected: Option<&Typ>,
    facts: &Facts,
) -> Option<Typ> {
    let Expr::Call { callee, .. } = expression else {
        return None;
    };
    let Expr::Ident(name) = callee.as_ref() else {
        return None;
    };
    let name = name.strip_prefix(crate::core_ir::METHOD_PREFIX)?;
    let arguments = arguments?;
    let receiver = arguments.first()?.as_ref()?;
    let mut candidates = facts
        .bound_trait_methods
        .iter()
        .filter(|bound| rust_references::same_source_type(&bound.receiver, receiver))
        .flat_map(|bound| bound.methods.iter().filter(|method| method.name == name));
    let method = candidates.next()?.clone();
    if candidates.next().is_some() {
        let failure =
            ResolutionFailure::Invalid("multiple trait bounds provide the requested method".into());
        **callee = Expr::Ident(failure.symbol());
        return Some(failure.typ());
    }
    let mut failure = if arguments.len() != method.signature.parameters.len() {
        Some(ResolutionFailure::Invalid(
            "trait method argument count differs".into(),
        ))
    } else {
        None
    };
    for (expected, actual) in method
        .signature
        .parameters
        .iter()
        .skip(1)
        .zip(arguments.iter().skip(1))
    {
        failure = crate::core_ir::type_proofs::prefer_failure(
            failure,
            match actual {
                // Only unsuffixed numeric inference may wait for context.
                // An opaque generic argument is not evidence for a scalar,
                // even if a later specialization happens to match it.
                Some(actual)
                    if matches!(actual, Typ::Int | Typ::Float)
                        || actual == &crate::core_ir::rust_floats::weak_type()
                        || rust_references::same_source_type(actual, expected) =>
                {
                    None
                }
                Some(_) => Some(ResolutionFailure::Invalid(
                    "trait method argument source type differs".into(),
                )),
                None => Some(ResolutionFailure::Unsupported(
                    "trait method argument source type is unresolved".into(),
                )),
            },
        );
    }
    failure = crate::core_ir::type_proofs::prefer_failure(
        failure,
        super::rust_generics::proven_operator_result_failure(&method.signature.result, expected),
    );
    if let Some(failure) = failure {
        **callee = Expr::Ident(failure.symbol());
        return Some(failure.typ());
    }
    let result = method.signature.result.clone();
    **callee = Expr::Unary {
        op: rust_generics::bound_method(&rust_generics::BoundMethod {
            receiver: receiver.clone(),
            method,
        }),
        expr: Box::new(Expr::UnitLit),
    };
    Some(result)
}

pub(super) fn callee_type(expression: &Expr) -> Option<Typ> {
    let Expr::Unary { op, .. } = expression else {
        return None;
    };
    rust_generics::decode_bound_method(op)
        .map(|call| crate::core_ir::rust_callables::typ(call.method.signature))
}

pub(super) fn resolve_instance(instance: &mut Decl, templates: &HashMap<String, Decl>) {
    crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(instance), &mut |value| {
        let Expr::Unary { op, .. } = value else {
            return;
        };
        if !rust_generics::is_bound_method(op) {
            return;
        }
        let target = rust_generics::decode_bound_method(op).and_then(|call| {
            let mut targets = call.method.targets.iter().filter(|(receiver, _)| {
                let receiver = local_receiver(receiver);
                rust_references::same_source_type(&receiver, &call.receiver)
            });
            let (_, target) = targets.next()?;
            if targets.next().is_some() { return None }
            let target = target.strip_prefix("crate::").unwrap_or(target);
            matches!(templates.get(target), Some(Decl::Function { type_params, .. }) if type_params.is_empty())
                .then(|| target.to_owned())
        });
        *value = Expr::Ident(target.unwrap_or_else(|| {
            ResolutionFailure::Unsupported(
                "bound trait method requires one owned source implementation".into(),
            )
            .symbol()
        }));
    });
}

fn local_receiver(receiver: &Typ) -> Typ {
    match receiver {
        Typ::Named(name) if name.starts_with("crate::") => Typ::Named(name[7..].into()),
        value => value.clone(),
    }
}
