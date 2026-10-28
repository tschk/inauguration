//! Instantiate source type arguments before source integer kinds are erased.
use crate::core_ir::{
    Decl, Expr, Typ, UnifiedModule, rust_generics, type_proofs::ResolutionFailure,
};
use std::collections::{HashMap, HashSet, VecDeque};

/// Infer only complete source-owned substitutions. An absent result means the
/// existing unresolved call remains unchanged; no runtime carrier invents a
/// Rust primitive width. The normal instantiator still validates templates.
pub(super) fn infer_call(
    expr: &Expr,
    arguments: &[Option<Typ>],
    expected: Option<&Typ>,
    facts: &super::Facts,
    env: &HashMap<String, Typ>,
) -> Option<Result<(Vec<Typ>, Typ), ResolutionFailure>> {
    let Expr::Call { callee, .. } = expr else {
        return None;
    };
    if let Some(failure) = explicit_result_failure(callee, expected, facts, env) {
        return Some(Err(failure));
    }
    let Expr::Ident(target) = callee.as_ref() else {
        return None;
    };
    if env.contains_key(target) {
        return None;
    }
    let parameters = facts
        .generic_parameters
        .get(target)
        .filter(|p| !p.is_empty())?;
    let (signature, result) = facts.function_signature(target)?;
    if signature.len() != arguments.len() {
        return Some(Err(ResolutionFailure::Invalid(format!(
            "generic call `{target}` has incompatible argument count"
        ))));
    }
    let names = parameters.iter().cloned().collect::<HashSet<_>>();
    let mut bindings = HashMap::new();
    let mut failures = None;
    if let Some(expected) = expected {
        failures = crate::core_ir::type_proofs::prefer_failure(
            failures,
            unify_source(result, expected, &names, &mut bindings, 0).err(),
        );
    }
    for ((_, declared), actual) in signature.iter().zip(arguments) {
        if let Some(actual) = actual {
            failures = crate::core_ir::type_proofs::prefer_failure(
                failures,
                crate::core_ir::type_proofs::failure_in_type(actual),
            );
            failures = crate::core_ir::type_proofs::prefer_failure(
                failures,
                unify_source(declared, actual, &names, &mut bindings, 0).err(),
            );
        }
    }
    if let Some(failure) = failures {
        return Some(Err(failure));
    }
    let types = parameters
        .iter()
        .map(|name| bindings.get(name).cloned())
        .collect::<Option<Vec<_>>>()?;
    // Every actual must agree with the inferred substitution. Unknown evidence
    // cannot be silently coerced just because another argument owns a width.
    if arguments
        .iter()
        .any(|typ| typ.as_ref().is_none_or(|typ| !owned_type(typ, 0)))
    {
        return None;
    }
    let mut projected = Decl::Function {
        name: "#inferred-result".into(),
        params: signature.to_vec(),
        ret: result.clone(),
        body: vec![],
        // This projection has no generic binder; substitute its source signature.
        type_params: vec![],
    };
    crate::core_bindings::substitute_type_parameters(&mut projected, &bindings);
    let Decl::Function { params, ret, .. } = projected else {
        unreachable!()
    };
    for ((_, declared), actual) in params.iter().zip(arguments) {
        if let Some(actual) = actual {
            if !source_argument_agrees(declared, actual) {
                return Some(Err(source_conflict(
                    declared,
                    actual,
                    "incompatible inferred Rust generic argument types".into(),
                )));
            }
        }
    }
    Some(Ok((types, ret)))
}

// Explicit arguments remain authoritative. Check the result destination before
// specialization/erasure, without replacing them with inferred arguments.
fn explicit_result_failure(
    callee: &Expr,
    expected: Option<&Typ>,
    facts: &super::Facts,
    env: &HashMap<String, Typ>,
) -> Option<ResolutionFailure> {
    let expected = expected.filter(|typ| owned_type(typ, 0))?;
    let Expr::Unary { op, expr } = callee else {
        return None;
    };
    let arguments = rust_generics::decode_arguments(op)?;
    let Expr::Ident(target) = expr.as_ref() else {
        return None;
    };
    if env.contains_key(target) {
        return None;
    }
    let parameters = facts.generic_parameters.get(target)?;
    if parameters.len() != arguments.len() || arguments.iter().any(|typ| !owned_type(typ, 0)) {
        return None;
    }
    let (_, result) = facts.function_signature(target)?;
    let bindings = parameters.iter().cloned().zip(arguments).collect();
    let mut projected = Decl::Function {
        name: "#explicit-result".into(),
        params: vec![],
        ret: result.clone(),
        body: vec![],
        // Substitute this projected signature outside its former generic binder.
        type_params: vec![],
    };
    crate::core_bindings::substitute_type_parameters(&mut projected, &bindings);
    let Decl::Function { ret, .. } = projected else {
        unreachable!()
    };
    if !owned_type(&ret, 0) || source_argument_agrees(expected, &ret) {
        return None;
    }
    Some(source_conflict(
        &ret,
        expected,
        "explicit Rust generic call result conflicts with its destination type".into(),
    ))
}

// Only authoritative source operator edges use this check. Ordinary calls keep
// their existing inference/coercion policy, and cast operands have no destination.
pub(super) fn proven_operator_result_failure(
    actual: &Typ,
    expected: Option<&Typ>,
) -> Option<ResolutionFailure> {
    let expected = expected.filter(|typ| owned_type(typ, 0))?;
    if !owned_type(actual, 0) || crate::core_ir::rust_references::same_source_type(actual, expected)
    {
        return None;
    }
    Some(source_conflict(
        actual,
        expected,
        "proven Rust unary operator result conflicts with its destination type".into(),
    ))
}

// A reference mutability coercion can occur inside an aggregate. Only defer
// when every differing leaf is that same-referent coercion: a different length,
// primitive identity, or reference/scalar shape is still a contradiction.
fn source_conflict(left: &Typ, right: &Typ, message: String) -> ResolutionFailure {
    if reference_coercion_uncertain(left, right, 0) {
        ResolutionFailure::Unsupported(message)
    } else {
        ResolutionFailure::Invalid(message)
    }
}

fn reference_coercion_uncertain(left: &Typ, right: &Typ, depth: usize) -> bool {
    if depth >= 128 {
        return false;
    }
    match (left, right) {
        (Typ::Array(left), Typ::Array(right)) | (Typ::Vector(left), Typ::Vector(right)) => {
            reference_coercion_uncertain(left, right, depth + 1)
        }
        (Typ::Named(left), Typ::Named(right)) => {
            if let (Some(left), Some(right)) = (
                crate::core_ir::rust_references::decode(left),
                crate::core_ir::rust_references::decode(right),
            ) {
                return (left.mutable != right.mutable
                    && crate::core_ir::rust_references::same_source_type(
                        &left.referent,
                        &right.referent,
                    ))
                    || reference_coercion_uncertain(&left.referent, &right.referent, depth + 1);
            }
            if let (Some(left), Some(right)) = (
                crate::core_ir::rust_arrays::decode(left),
                crate::core_ir::rust_arrays::decode(right),
            ) {
                return left.length == right.length
                    && reference_coercion_uncertain(&left.element, &right.element, depth + 1);
            }
            if let (Some(left), Some(right)) = (
                crate::core_ir::rust_options::payload(left),
                crate::core_ir::rust_options::payload(right),
            ) {
                return reference_coercion_uncertain(&left, &right, depth + 1);
            }
            if let (Some(left), Some(right)) = (
                crate::core_ir::tuples::fields(left),
                crate::core_ir::tuples::fields(right),
            ) {
                return left.len() == right.len()
                    && left.iter().zip(&right).all(|((_, left), (_, right))| {
                        crate::core_ir::rust_references::same_source_type(left, right)
                            || reference_coercion_uncertain(left, right, depth + 1)
                    })
                    && left.iter().zip(&right).any(|((_, left), (_, right))| {
                        reference_coercion_uncertain(left, right, depth + 1)
                    });
            }
            false
        }
        _ => false,
    }
}

fn owned_type(typ: &Typ, depth: usize) -> bool {
    if depth >= 128 {
        return false;
    }
    match typ {
        Typ::Int | Typ::Float | Typ::Generic(_) => false,
        Typ::Array(item) | Typ::Vector(item) => owned_type(item, depth + 1),
        Typ::Named(name) => {
            if crate::core_ir::rust_callables::is_encoded(name) {
                let Some(signature) = crate::core_ir::rust_callables::signature_of(typ) else {
                    return false;
                };
                let mut owned = signature.ordinary_rust_form();
                signature.visit_types(&mut |child| owned &= owned_type(child, depth + 1));
                return owned;
            }
            if typ == &crate::core_ir::rust_floats::weak_type() {
                return false;
            }
            if let Some(array) = crate::core_ir::rust_arrays::decode(name) {
                return owned_type(&array.element, depth + 1);
            }
            if let Some(reference) = crate::core_ir::rust_references::decode(name) {
                return owned_type(&reference.referent, depth + 1);
            }
            if let Some(payload) = crate::core_ir::rust_boxes::payload(name)
                .or_else(|| crate::core_ir::rust_boxes::address_payload(name))
                .or_else(|| crate::core_ir::rust_boxes::constructor_payload(name))
                .or_else(|| crate::core_ir::rust_boxes::allocation_payload(name))
            {
                return owned_type(&payload, depth + 1);
            }
            if let Some(payload) = crate::core_ir::rust_options::payload(name) {
                return owned_type(&payload, depth + 1);
            }
            if let Some(fields) = crate::core_ir::tuples::fields(name) {
                return fields.iter().all(|(_, typ)| owned_type(typ, depth + 1));
            }
            crate::core_ir::type_proofs::failure_in_type(typ).is_none()
        }
        _ => true,
    }
}

// Directional destination agreement. Function items retain identity everywhere
// except a proven ordinary fn-pointer destination; generic T itself is not one.
fn source_argument_agrees(expected: &Typ, actual: &Typ) -> bool {
    if crate::core_ir::rust_references::same_source_type(expected, actual) {
        return true;
    }
    let (Typ::Named(expected_name), Typ::Named(actual_name)) = (expected, actual) else {
        return false;
    };
    let (Some(expected), Some(actual)) = (
        crate::core_ir::rust_callables::decode(expected_name),
        crate::core_ir::rust_callables::decode_item(actual_name),
    ) else {
        return false;
    };
    expected.ordinary_rust_form()
        && actual.signature.ordinary_rust_form()
        && crate::core_ir::rust_references::same_source_type(
            &crate::core_ir::rust_callables::typ(expected),
            &crate::core_ir::rust_callables::typ(actual.signature),
        )
}

fn unify_source(
    declared: &Typ,
    actual: &Typ,
    names: &HashSet<String>,
    bindings: &mut HashMap<String, Typ>,
    depth: usize,
) -> Result<(), ResolutionFailure> {
    if depth >= 128 {
        return Ok(());
    }
    if let Typ::Generic(name) = declared {
        if names.contains(name) && owned_type(actual, depth) {
            if let Some(previous) = bindings.get(name) {
                if !source_argument_agrees(previous, actual) {
                    return Err(source_conflict(
                        previous,
                        actual,
                        format!("conflicting Rust generic source types for `{name}`"),
                    ));
                }
            } else {
                bindings.insert(name.clone(), actual.clone());
            }
        }
        return Ok(());
    }
    match (declared, actual) {
        (Typ::Array(left), Typ::Array(right)) | (Typ::Vector(left), Typ::Vector(right)) => {
            unify_source(left, right, names, bindings, depth + 1)
        }
        (Typ::Named(left), Typ::Named(right)) => {
            if crate::core_ir::rust_callables::is_encoded(left)
                || crate::core_ir::rust_callables::is_encoded(right)
            {
                let (Some(left_signature), Some(right_signature)) = (
                    crate::core_ir::rust_callables::signature_of(declared),
                    crate::core_ir::rust_callables::signature_of(actual),
                ) else {
                    return Err(ResolutionFailure::Unsupported(
                        "generic callable source form is not owned".into(),
                    ));
                };
                let mut failure = (!left_signature.ordinary_rust_form()
                    || !right_signature.ordinary_rust_form())
                .then(|| {
                    ResolutionFailure::Unsupported(
                        "generic callable ABI/safety/lifetime obligations are not owned".into(),
                    )
                });
                // Items are nominal source identities, not structural pointers.
                if let Some(left_item) = crate::core_ir::rust_callables::decode_item(left) {
                    if crate::core_ir::rust_callables::decode_item(right)
                        .is_none_or(|right_item| left_item.identity != right_item.identity)
                    {
                        failure = crate::core_ir::type_proofs::prefer_failure(
                            failure,
                            Some(ResolutionFailure::Invalid(
                                "distinct Rust function item types".into(),
                            )),
                        );
                    }
                }
                if left_signature.parameters.len() != right_signature.parameters.len() {
                    failure = crate::core_ir::type_proofs::prefer_failure(
                        failure,
                        Some(ResolutionFailure::Invalid(
                            "generic callable parameter count differs".into(),
                        )),
                    );
                }
                for (declared, actual) in left_signature
                    .parameters
                    .iter()
                    .zip(&right_signature.parameters)
                    .chain(std::iter::once((
                        &left_signature.result,
                        &right_signature.result,
                    )))
                {
                    failure = crate::core_ir::type_proofs::prefer_failure(
                        failure,
                        unify_source(declared, actual, names, bindings, depth + 1).err(),
                    );
                }
                return failure.map_or(Ok(()), Err);
            }

            if let (Some(left), Some(right)) = (
                crate::core_ir::rust_arrays::decode(left),
                crate::core_ir::rust_arrays::decode(right),
            ) {
                if left.length != right.length {
                    return Err(ResolutionFailure::Invalid(
                        "incompatible generic fixed-array lengths".into(),
                    ));
                }
                return unify_source(&left.element, &right.element, names, bindings, depth + 1);
            }
            if let (Some(left), Some(right)) = (
                crate::core_ir::rust_references::decode(left),
                crate::core_ir::rust_references::decode(right),
            ) {
                // A possible borrow coercion cannot hide a definite referent
                // contradiction in another owned generic occurrence.
                unify_source(&left.referent, &right.referent, names, bindings, depth + 1)?;
                if left.mutable != right.mutable {
                    return Err(ResolutionFailure::Unsupported(
                        "generic reference coercion requires source borrow resolution".into(),
                    ));
                }
                return Ok(());
            }
            if let (Some(left), Some(right)) = (
                crate::core_ir::rust_options::payload(left),
                crate::core_ir::rust_options::payload(right),
            ) {
                return unify_source(&left, &right, names, bindings, depth + 1);
            }
            if let (Some(left), Some(right)) = (
                crate::core_ir::rust_boxes::payload(left),
                crate::core_ir::rust_boxes::payload(right),
            ) {
                return unify_source(&left, &right, names, bindings, depth + 1);
            }
            if let (Some(left), Some(right)) = (
                crate::core_ir::tuples::fields(left),
                crate::core_ir::tuples::fields(right),
            ) {
                if left.len() != right.len() {
                    return Err(ResolutionFailure::Invalid(
                        "incompatible generic tuple arity".into(),
                    ));
                }
                let mut failures = None;
                for ((_, left), (_, right)) in left.iter().zip(&right) {
                    failures = crate::core_ir::type_proofs::prefer_failure(
                        failures,
                        unify_source(left, right, names, bindings, depth + 1).err(),
                    );
                }
                if let Some(failure) = failures {
                    return Err(failure);
                }
            }
            concrete_source_agreement(declared, actual, depth)
        }
        _ => concrete_source_agreement(declared, actual, depth),
    }
}

fn concrete_source_agreement(
    declared: &Typ,
    actual: &Typ,
    depth: usize,
) -> Result<(), ResolutionFailure> {
    // Only owned source identities prove a contradiction. Unknown carriers and
    // unresolved generic leaves retain the existing deferred path.
    if owned_type(declared, depth)
        && owned_type(actual, depth)
        && !crate::core_ir::rust_references::same_source_type(declared, actual)
    {
        Err(source_conflict(
            declared,
            actual,
            format!(
                "incompatible concrete Rust generic call types: expected {declared:?}, found {actual:?}"
            ),
        ))
    } else {
        Ok(())
    }
}

pub(super) fn normalize(module: &mut UnifiedModule) {
    normalize_with_checks(module, true);
}

fn normalize_with_checks(module: &mut UnifiedModule, check_templates: bool) {
    // This also runs on rigid audit modules, after their source substitutions.
    // It never infers directly inside unconstrained generic template bodies.
    let checker = super::TypeChecker::new();
    // Raw source proof is captured before canonical ABI erasure. Dependencies
    // carry these private function markers through their existing artifact path.
    let neg_implementations = collect_neg_implementations(module);
    let _ = checker.resolve_typed_operators(
        module,
        super::ResolvePolicy::RustGenericArguments,
        None,
        None,
    );
    preserve_argument_proofs(module);
    let mut templates = HashMap::new();
    let mut duplicates = HashSet::new();
    for declaration in &module.decls {
        if let Decl::Function { name, .. } = declaration
            && templates
                .insert(name.clone(), declaration.clone())
                .is_some()
        {
            duplicates.insert(name.clone());
        }
    }
    if check_templates {
        let requested = requested_templates(module, &templates);
        let mut facts = super::TypeChecker::new().collect_facts(module);
        let globals = module
            .decls
            .iter()
            .filter_map(|declaration| match declaration {
                Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let failures = templates
            .iter()
            .filter(|(name, _)| requested.contains(*name))
            .filter_map(|(name, template)| {
                validate_template(template, &templates, &mut facts, &globals)
                    .map(|failure| (name.clone(), failure))
            })
            .collect::<Vec<_>>();
        let failures = failures.into_iter().collect::<HashMap<_, _>>();
        for (name, failure) in &failures {
            if let Some(Decl::Function { body, .. }) = templates.get_mut(name) {
                body.insert(0, crate::core_ir::Stmt::Expr(Expr::Ident(failure.symbol())));
            }
        }
        // Preserve audited obligations on unresolved direct source calls too:
        // unused pure generic results must not erase them before backend checks.
        // Initial binding normalization disambiguates lexical callable shadows.
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Call { callee, .. } = expr
                && let Expr::Ident(name) = callee.as_ref()
                && let Some(failure) = failures.get(name)
            {
                **callee = Expr::Ident(failure.symbol());
            }
        });
        // Incomplete implicit inference may never instantiate a template. Its
        // already-validated obligation still belongs to the original callable
        // declaration, so retain the same proof through ordinary call edges.
        // Entry-root DCE continues to remove unreachable helper declarations.
        for declaration in &mut module.decls {
            if let Decl::Function { name, body, .. } = declaration
                && let Some(failure) = failures.get(name)
            {
                body.insert(0, crate::core_ir::Stmt::Expr(Expr::Ident(failure.symbol())));
            }
        }
    }
    for declaration in &mut module.decls {
        strip_template_marker(declaration);
    }
    let mut instances = HashMap::<(String, String), String>::new();
    let mut pending = VecDeque::new();
    let mut count = 0usize;
    fn rewrite(
        declaration: &mut Decl,
        templates: &HashMap<String, Decl>,
        duplicates: &HashSet<String>,
        instances: &mut HashMap<(String, String), String>,
        pending: &mut VecDeque<Decl>,
        count: &mut usize,
        neg_implementations: &[rust_generics::NegImplementation],
    ) {
        crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(declaration), &mut |value| {
            let Expr::Unary { op, expr } = value else {
                return;
            };
            let Some(arguments) = rust_generics::decode_arguments(op) else {
                return;
            };
            let Expr::Ident(target) = expr.as_ref() else {
                *value = Expr::Ident(
                    ResolutionFailure::Unsupported(
                        "explicit generic target requires a source function identity".into(),
                    )
                    .symbol(),
                );
                return;
            };
            let result = instantiate(
                target,
                &arguments,
                templates,
                duplicates,
                instances,
                pending,
                count,
                neg_implementations,
            );
            *value = Expr::Ident(match result {
                Ok(name) => name,
                Err(failure) => failure.symbol(),
            });
        });
    }
    for declaration in &mut module.decls {
        if matches!(declaration, Decl::Function { type_params, .. } if !type_params.is_empty()) {
            continue;
        }
        rewrite(
            declaration,
            &templates,
            &duplicates,
            &mut instances,
            &mut pending,
            &mut count,
            &neg_implementations,
        );
    }
    let mut inference_facts = checker.collect_facts(module);
    let globals = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    while let Some(mut declaration) = pending.pop_front() {
        if let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = &mut declaration
        {
            // Only instantiated bodies are revisited. Recursive specialization
            // still uses the existing shared instance table and 1024 cap.
            inference_facts
                .functions
                .insert(name.clone(), (params.clone(), ret.clone()));
            let mut env = globals.clone();
            env.extend(params.iter().cloned());
            checker.resolve_typed_body(
                body,
                &inference_facts,
                &mut env,
                Some(ret),
                super::ResolvePolicy::RustGenericArguments,
            );
        }
        rewrite(
            &mut declaration,
            &templates,
            &duplicates,
            &mut instances,
            &mut pending,
            &mut count,
            &neg_implementations,
        );
        module.decls.push(declaration);
    }
    // Incomplete inference leaves the original callable reachable. Add its
    // obligation only AFTER all specialization, so concrete bodies were not
    // discarded by instantiate's early proof validation.
    for declaration in &mut module.decls {
        if let Decl::Function {
            name, type_params, ..
        } = declaration
            && !type_params.is_empty()
            && templates.get(name).is_some_and(has_constraints)
        {
            retain_constraint_obligation(declaration);
        }
        if let Decl::Function { body, .. } = declaration {
            body.retain(|statement| !matches!(statement, crate::core_ir::Stmt::Expr(Expr::Unary { op, expr }) if rust_generics::is_neg_implementation(op) && matches!(expr.as_ref(),Expr::UnitLit)));
        }
    }
    crate::core_opt::propagate_function_values(&mut module.decls);
    if count > 0 {
        remove_unused_templates(module);
    }
}

fn remove_unused_templates(module: &mut UnifiedModule) {
    fn references(declaration: &Decl, names: &mut HashSet<String>) {
        match declaration {
            Decl::Function { body, .. } => crate::core_bindings::reserve_body(body, names),
            Decl::Global {
                init: Some(init), ..
            } => crate::core_bindings::reserve_expr(init, names),
            _ => {}
        }
    }
    let mut used = HashSet::new();
    for declaration in &module.decls {
        if !matches!(declaration, Decl::Function { type_params, .. } if !type_params.is_empty()) {
            references(declaration, &mut used);
        }
    }
    let mut scanned = HashSet::new();
    loop {
        let mut changed = false;
        for declaration in &module.decls {
            if let Decl::Function {
                name, type_params, ..
            } = declaration
                && !type_params.is_empty()
                && used.contains(name)
                && scanned.insert(name.clone())
            {
                references(declaration, &mut used);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    module.decls.retain(|declaration| {
        !matches!(declaration, Decl::Function { name, type_params, .. }
            if !type_params.is_empty() && !used.contains(name)
                && !super::TypeChecker::new().typed_proof_nodes(std::slice::from_ref(declaration))
                    .values().any(|failure| failure.is_invalid()))
    });
}

fn instantiate(
    target: &str,
    arguments: &[Typ],
    templates: &HashMap<String, Decl>,
    duplicates: &HashSet<String>,
    instances: &mut HashMap<(String, String), String>,
    pending: &mut VecDeque<Decl>,
    count: &mut usize,
    neg_implementations: &[rust_generics::NegImplementation],
) -> Result<String, ResolutionFailure> {
    if let Some(failure) = rust_generics::argument_failure(arguments) {
        return Err(failure);
    }
    if let Some(failure) = crate::core_ir::type_proofs::decode(target) {
        return Err(failure);
    }
    if duplicates.contains(target) {
        return Err(ResolutionFailure::Invalid(format!(
            "duplicate generic function `{target}`"
        )));
    }
    let Some(template) = templates.get(target) else {
        return Err(ResolutionFailure::Unsupported(format!(
            "generic source function `{target}` is not owned"
        )));
    };
    let Decl::Function { type_params, .. } = template else {
        unreachable!()
    };
    if !type_params.is_empty() && arguments.is_empty() {
        return Err(ResolutionFailure::Unsupported(
            "empty explicit arguments require generic argument inference".into(),
        ));
    }
    if type_params.len() != arguments.len() {
        return Err(ResolutionFailure::Invalid(format!(
            "generic function `{target}` expects {} type arguments, found {}",
            type_params.len(),
            arguments.len(),
        )));
    }
    if !is_source_template(template) {
        return Err(ResolutionFailure::Unsupported(format!(
            "generic source constraints are not owned for `{target}`"
        )));
    }
    let proofs = super::TypeChecker::new().typed_proof_nodes(std::slice::from_ref(template));
    if let Some(failure) = proofs
        .values()
        .find(|proof| proof.is_invalid())
        .or_else(|| proofs.values().next())
    {
        return Err((*failure).clone());
    }
    let constrained = has_constraints(template);
    if arguments.iter().any(contains_generic) {
        return Err(ResolutionFailure::Unsupported(format!(
            "generic function `{target}` has unresolved type arguments"
        )));
    }
    let encoded = rust_generics::arguments(arguments);
    let key = (target.to_owned(), encoded.clone());
    if let Some(name) = instances.get(&key) {
        return Ok(name.clone());
    }
    // Bound polymorphic recursion instead of exhausting compiler memory.
    if *count >= 1024 {
        return Err(ResolutionFailure::Unsupported(
            "generic specialization limit exceeded".into(),
        ));
    }
    *count += 1;
    let instance_name = format!("#rust-specialization:{target}:{encoded}");
    let bindings = type_params
        .iter()
        .cloned()
        .zip(arguments.iter().cloned())
        .collect();
    let mut instance = template.clone();
    let Decl::Function {
        name, type_params, ..
    } = &mut instance
    else {
        unreachable!()
    };
    *name = instance_name.clone();
    type_params.clear();
    apply_alias_arguments(&mut instance, arguments);
    crate::core_bindings::substitute_type_parameters(&mut instance, &bindings);
    let discharged = !constrained || discharge_neg_obligations(&instance, neg_implementations);
    strip_template_marker(&mut instance);
    if !discharged {
        // Keep the concrete body and origin-tagged operators for the existing
        // authoritative unary resolver. This proof still forbids execution.
        retain_constraint_obligation(&mut instance);
    }
    instances.insert(key, instance_name.clone());
    pending.push_back(instance);
    Ok(instance_name)
}

fn contains_generic(typ: &Typ) -> bool {
    match typ {
        Typ::Generic(_) => true,
        Typ::Array(item) | Typ::Vector(item) => contains_generic(item),
        Typ::Named(name) => {
            if crate::core_ir::rust_callables::is_encoded(name) {
                let Some(signature) = crate::core_ir::rust_callables::signature_of(typ) else {
                    return true;
                };
                let mut unresolved = false;
                signature.visit_types(&mut |child| unresolved |= contains_generic(child));
                return unresolved;
            }
            crate::core_ir::rust_arrays::decode(name)
                .is_some_and(|array| contains_generic(&array.element))
                || crate::core_ir::rust_references::decode(name)
                    .is_some_and(|reference| contains_generic(&reference.referent))
                || crate::core_ir::rust_boxes::payload(name)
                    .or_else(|| crate::core_ir::rust_boxes::address_payload(name))
                    .or_else(|| crate::core_ir::rust_boxes::constructor_payload(name))
                    .or_else(|| crate::core_ir::rust_boxes::allocation_payload(name))
                    .is_some_and(|payload| contains_generic(&payload))
                || crate::core_ir::tuples::fields(name)
                    .is_some_and(|fields| fields.iter().any(|(_, typ)| contains_generic(typ)))
        }
        _ => false,
    }
}

fn is_source_template(declaration: &Decl) -> bool {
    matches!(declaration, Decl::Function { body, .. } if body.iter().any(|stmt|
        matches!(stmt, crate::core_ir::Stmt::Expr(Expr::Unary { op, expr })
            if op == rust_generics::TEMPLATE && matches!(expr.as_ref(), Expr::UnitLit))))
}

fn strip_template_marker(declaration: &mut Decl) {
    if let Decl::Function { body, .. } = declaration {
        body.retain(|stmt| !matches!(stmt, crate::core_ir::Stmt::Expr(Expr::Unary { op, expr })
            if (matches!(op.as_str(), rust_generics::TEMPLATE | rust_generics::CONSTRAINTS) || rust_generics::is_neg_bounds(op)) && matches!(expr.as_ref(), Expr::UnitLit)
                || op == rust_generics::ALIAS && matches!(expr.as_ref(), Expr::Ident(_))));
    }
}

fn validate_template(
    template: &Decl,
    templates: &HashMap<String, Decl>,
    facts: &mut super::Facts,
    globals: &HashMap<String, Typ>,
) -> Option<ResolutionFailure> {
    if !is_source_template(template) {
        return None;
    }
    let proofs = super::TypeChecker::new().typed_proof_nodes(std::slice::from_ref(template));
    if let Some(failure) = proofs
        .values()
        .find(|proof| proof.is_invalid())
        .or_else(|| proofs.values().next())
    {
        return Some((*failure).clone());
    }
    let Decl::Function {
        name, type_params, ..
    } = template
    else {
        return None;
    };
    if has_constraints(template) {
        // Known source failures were handled above. Unproved obligations are
        // retained on concrete instances (or the still-reachable template), not
        // substituted for the original call before operator resolution.
        return None;
    }
    let mut rigid = template.clone();
    let bindings: HashMap<_, _> = type_params
        .iter()
        .map(|parameter| {
            (
                parameter.clone(),
                rust_generics::rigid_type(&format!("{name}:{parameter}")),
            )
        })
        .collect();
    let arguments = type_params
        .iter()
        .map(|parameter| bindings[parameter].clone())
        .collect::<Vec<_>>();
    apply_alias_arguments(&mut rigid, &arguments);
    let Decl::Function {
        name: audit_name,
        type_params,
        ..
    } = &mut rigid
    else {
        unreachable!()
    };
    *audit_name = format!("#rust-generic-audit:{name}");
    type_params.clear();
    strip_template_marker(&mut rigid);
    crate::core_bindings::substitute_type_parameters(&mut rigid, &bindings);
    let mut audit = UnifiedModule::new(vec![rigid]);
    let needed = requested_templates(&audit, templates);
    audit.decls.extend(
        needed
            .iter()
            .filter_map(|name| templates.get(name).cloned()),
    );
    normalize_with_checks(&mut audit, false);
    // Check the same ordered stores as emission, not unresolved field/index
    // modification markers which are not source function calls.
    crate::lower_core::lower_value_spills(&mut audit.decls);
    // Original generic bodies are checked through their rigid instances.
    for declaration in &mut audit.decls {
        if let Decl::Function {
            type_params, body, ..
        } = declaration
            && !type_params.is_empty()
        {
            body.clear();
        }
    }
    if let Err(reason) = crate::core_ir_verifier::check_definite_initialization(&audit) {
        return Some(ResolutionFailure::Unsupported(reason));
    }
    let additions = super::TypeChecker::new().collect_facts(&audit);
    let mut previous = Vec::new();
    for (name, signature) in additions.functions {
        previous.push((name.clone(), facts.functions.insert(name, signature)));
    }
    let mut previous_structs = Vec::new();
    for (name, fields) in additions.structs {
        previous_structs.push((name.clone(), facts.structs.insert(name, fields)));
    }
    let mut errors = Vec::new();
    let mut ownership = None;
    for declaration in &audit.decls {
        ownership = crate::core_ir::type_proofs::prefer_failure(
            ownership,
            super::rigid_values::check(declaration, facts, globals),
        );
        errors.extend(super::TypeChecker::new().check_function_with_facts(
            declaration,
            facts,
            globals,
        ));
    }
    for (name, signature) in previous {
        if let Some(signature) = signature {
            facts.functions.insert(name, signature);
        } else {
            facts.functions.remove(&name);
        }
    }
    for (name, fields) in previous_structs {
        if let Some(fields) = fields {
            facts.structs.insert(name, fields);
        } else {
            facts.structs.remove(&name);
        }
    }
    if errors.is_empty() {
        return ownership;
    }

    let message = format!("generic definition `{name}` is not valid for every type: {errors:?}");
    Some(
        if errors.iter().any(|error| {
            matches!(
                error,
                super::TypeError::ReturnTypeMismatch { .. }
                    | super::TypeError::TypeMismatch { .. }
                    | super::TypeError::ArityMismatch { .. }
            )
        }) {
            ResolutionFailure::Invalid(message)
        } else {
            ResolutionFailure::Unsupported(message)
        },
    )
}

fn requested_templates(
    module: &UnifiedModule,
    templates: &HashMap<String, Decl>,
) -> HashSet<String> {
    fn targets(declaration: &Decl, templates: &HashMap<String, Decl>) -> Vec<String> {
        let mut targets = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut [declaration.clone()], &mut |value| {
            if let Expr::Unary { op, expr } = value
                && rust_generics::decode_arguments(op).is_some()
                && let Expr::Ident(name) = expr.as_ref()
            {
                targets.push(name.clone());
            }
            // Binding normalization has already disambiguated local callable
            // shadows. Retain source templates reachable by inferred calls too,
            // so rigid audits can infer only after their substitutions.
            if let Expr::Call { callee, .. } = value
                && let Expr::Ident(name) = callee.as_ref()
                && matches!(templates.get(name), Some(Decl::Function { type_params, .. }) if !type_params.is_empty())
            {
                targets.push(name.clone());
            }
        });
        targets
    }
    let mut pending = VecDeque::new();
    for declaration in &module.decls {
        if !matches!(declaration, Decl::Function { type_params, .. } if !type_params.is_empty()) {
            pending.extend(targets(declaration, templates));
        }
    }
    let mut requested = HashSet::new();
    while let Some(target) = pending.pop_front() {
        if requested.insert(target.clone())
            && let Some(template) = templates.get(&target)
        {
            pending.extend(targets(template, templates));
        }
    }
    requested
}

/// Specialization may erase unused parameters, so retain argument-schema failures first.
fn preserve_argument_proofs(module: &mut UnifiedModule) {
    let proofs = super::TypeChecker::new().typed_proof_nodes(&module.decls);
    if proofs.is_empty() {
        return;
    }
    for declaration in &mut module.decls {
        let name = match declaration {
            Decl::Function { name, .. } | Decl::Global { name, .. } => name,
            _ => continue,
        };
        let Some(failure) = proofs.get(name) else {
            continue;
        };
        let mut arguments = false;
        crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(declaration), &mut |value| {
            if let Expr::Unary { op, .. } = value {
                arguments |= rust_generics::decode_arguments(op).is_some()
                    || rust_generics::is_neg_bounds(op)
                    || rust_generics::is_neg_implementation(op);
            }
        });
        if !arguments {
            continue;
        }
        let marker = crate::core_ir::Stmt::Expr(Expr::Ident(failure.symbol()));
        match declaration {
            Decl::Function { body, .. } if !body.contains(&marker) => body.insert(0, marker),
            Decl::Global {
                init: Some(init), ..
            } => {
                let init = init.as_mut();
                if !matches!(init, Expr::Block { body, .. } if body.first() == Some(&marker)) {
                    *init = Expr::Block {
                        body: vec![marker],
                        result: Some(Box::new(std::mem::replace(init, Expr::UnitLit))),
                    };
                }
            }
            _ => {}
        }
    }
}

fn has_constraints(declaration: &Decl) -> bool {
    matches!(declaration, Decl::Function { body, .. } if body.iter().any(|statement|
        matches!(statement, crate::core_ir::Stmt::Expr(Expr::Unary { op, expr })
            if (op == rust_generics::CONSTRAINTS || rust_generics::is_neg_bounds(op)) && matches!(expr.as_ref(), Expr::UnitLit))))
}
fn collect_neg_implementations(module: &UnifiedModule) -> Vec<rust_generics::NegImplementation> {
    module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { body, .. } => Some(body),
            _ => None,
        })
        .flatten()
        .filter_map(|statement| match statement {
            crate::core_ir::Stmt::Expr(Expr::Unary { op, expr })
                if matches!(expr.as_ref(), Expr::UnitLit) =>
            {
                rust_generics::decode_neg_implementation(op)
            }
            _ => None,
        })
        .collect()
}
fn discharge_neg_obligations(
    instance: &Decl,
    implementations: &[rust_generics::NegImplementation],
) -> bool {
    let Decl::Function { body, .. } = instance else {
        return false;
    };
    let mut seen = false;
    for statement in body {
        let crate::core_ir::Stmt::Expr(Expr::Unary { op, expr }) = statement else {
            continue;
        };
        if !matches!(expr.as_ref(), Expr::UnitLit) {
            continue;
        }
        if op == rust_generics::CONSTRAINTS {
            return false;
        }
        if !rust_generics::is_neg_bounds(op) {
            continue;
        }
        let Some(obligations) = rust_generics::decode_neg_bounds(op) else {
            return false;
        };
        if obligations.is_empty() {
            return false;
        }
        seen = true;
        for obligation in obligations {
            let primitive = matches!(&obligation.receiver, Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some_and(|kind|matches!(kind,"i8"|"i16"|"i32"|"i64"|"isize")))
                || crate::core_ir::rust_floats::kind(&obligation.receiver).is_some();
            let output = if primitive {
                Some(obligation.receiver.clone())
            } else {
                let matches: Vec<_> = implementations
                    .iter()
                    .filter(|proof| {
                        crate::core_ir::rust_references::same_source_type(
                            &proof.receiver,
                            &obligation.receiver,
                        )
                    })
                    .collect();
                if matches.len() != 1 {
                    return false;
                }
                Some(matches[0].output.clone())
            };
            if obligation.output.as_ref().is_some_and(|expected| {
                output.as_ref().is_none_or(|actual| {
                    !crate::core_ir::rust_references::same_source_type(expected, actual)
                })
            }) {
                return false;
            }
        }
    }
    seen
}
fn retain_constraint_obligation(declaration: &mut Decl) {
    if let Decl::Function { body, .. } = declaration {
        let marker = crate::core_ir::Stmt::Expr(Expr::Ident(constraint_failure().symbol()));
        if !body.contains(&marker) {
            body.insert(0, marker);
        }
    }
}
fn constraint_failure() -> ResolutionFailure {
    ResolutionFailure::Unsupported(
        "explicit generic bounds, lifetimes and const parameters require constraint resolution"
            .into(),
    )
}
fn apply_alias_arguments(declaration: &mut Decl, arguments: &[Typ]) {
    let target = match declaration {
        Decl::Function { body, .. } => body.iter().find_map(|statement| match statement {
            crate::core_ir::Stmt::Expr(Expr::Unary { op, expr }) if op == rust_generics::ALIAS => {
                if let Expr::Ident(target) = expr.as_ref() {
                    Some(target.clone())
                } else {
                    None
                }
            }
            _ => None,
        }),
        _ => None,
    };
    let Some(target) = target else { return };
    let specialized_origin = format!(
        "#rust-specialization:{target}:{}",
        rust_generics::arguments(arguments)
    );
    crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(declaration), &mut |expression| {
        if let Expr::Unary { op, expr } = expression
            && crate::core_ir::rust_callables::decode_alias_origin(op).as_deref()
                == Some(target.as_str())
            && matches!(expr.as_ref(), Expr::UnitLit)
        {
            *op = crate::core_ir::rust_callables::alias_origin(&specialized_origin);
            return;
        }
        if let Expr::Call { callee, .. } = expression
            && matches!(callee.as_ref(), Expr::Ident(name) if name == &target)
        {
            **callee = Expr::Unary {
                op: rust_generics::arguments(arguments),
                expr: Box::new(std::mem::replace(callee.as_mut(), Expr::UnitLit)),
            };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unused_nominal_arguments_keep_field_failures_before_erasure() {
        let source =
            "type A=A;struct Row{value:A}fn choose<T>()->i64{42}fn main()->i64{choose::<Row>()}";
        let raw = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        let checker = super::super::TypeChecker::new();
        assert!(
            checker
                .typed_proof_nodes(&raw.decls)
                .get("main")
                .is_some_and(ResolutionFailure::is_invalid)
        );
        let mut module = raw;
        normalize(&mut module);
        assert!(
            checker
                .typed_proof_nodes(&module.decls)
                .get("main")
                .is_some_and(ResolutionFailure::is_invalid)
        );
    }
}

#[cfg(test)]
mod implicit_argument_tests {
    use super::*;
    fn source_module(source: &str) -> Result<UnifiedModule, String> {
        // Source-identity checks must precede the semantic-only API's erasure.
        crate::compiler::rust_front::parse_rust_artifact_source(source, "implicit-generics")
            .map(|artifact| artifact.semantic)
    }
    fn infer(
        source: &str,
        arguments: &[Typ],
    ) -> Option<Result<(Vec<Typ>, Typ), ResolutionFailure>> {
        let module = source_module(source).unwrap();
        let facts = super::super::TypeChecker::new().collect_facts(&module);
        infer_call(
            &Expr::Call {
                callee: Box::new(Expr::Ident("choose".into())),
                args: arguments.iter().map(|_| Expr::UnitLit).collect(),
            },
            &arguments.iter().cloned().map(Some).collect::<Vec<_>>(),
            None,
            &facts,
            &HashMap::new(),
        )
    }
    #[test]
    fn implicit_substitutions_keep_declaration_order_and_repeated_agreement() {
        let source = "fn choose<A,B>(x:B,y:A,z:B)->A{y}";
        let f32_type = crate::core_ir::rust_floats::typ(crate::core_ir::rust_floats::Kind::F32);
        let inferred = infer(source, &[f32_type.clone(), Typ::Bool, f32_type.clone()])
            .unwrap()
            .unwrap();
        assert_eq!(inferred, (vec![Typ::Bool, f32_type.clone()], Typ::Bool));
        assert!(
            infer(
                source,
                &[f32_type, Typ::Bool, Typ::Named("#rust-integer-u32".into())]
            )
            .unwrap()
            .unwrap_err()
            .is_invalid()
        );
    }
    #[test]
    fn implicit_substitutions_preserve_reference_and_pointer_identity() {
        let source = "fn choose<T>(a:T,b:T)->T{b}";
        let integer = Typ::Named("#rust-integer-i64".into());
        let reference =
            crate::core_ir::rust_references::typ(crate::core_ir::rust_references::ReferenceType {
                mutable: false,
                referent: integer.clone(),
                runtime: Typ::Int,
            });
        assert!(
            infer(source, &[reference, integer])
                .unwrap()
                .unwrap_err()
                .is_invalid()
        );
        assert!(
            infer(
                source,
                &[
                    Typ::Named("#rust-integer-usize".into()),
                    Typ::Named("#rust-integer-u64".into())
                ]
            )
            .unwrap()
            .unwrap_err()
            .is_invalid()
        );
        assert!(
            infer(source, &[Typ::Int, Typ::Int]).is_none(),
            "canonical carrier is not Rust source width"
        );
    }
    #[test]
    fn discarded_generic_arguments_still_require_exact_source_agreement() {
        let source = "fn choose<T>(a:T,b:T)->i64{42}";
        let i64_type = Typ::Named("#rust-integer-i64".into());
        for (left, right) in [
            (i64_type.clone(), Typ::Named("#rust-integer-u64".into())),
            (
                crate::core_ir::rust_floats::typ(crate::core_ir::rust_floats::Kind::F32),
                crate::core_ir::rust_floats::typ(crate::core_ir::rust_floats::Kind::F64),
            ),
            (
                crate::core_ir::rust_arrays::typ(i64_type.clone(), 1),
                crate::core_ir::rust_arrays::typ(i64_type, 2),
            ),
        ] {
            assert!(
                infer(source, &[left, right])
                    .unwrap()
                    .unwrap_err()
                    .is_invalid()
            );
        }
        let module = source_module("fn choose<T>(x:T)->T{x}fn main()->i64{choose(true)}").unwrap();
        let module =
            super::super::normalize_module(crate::parser_registry::ParserId::Rust, &module);
        assert!(
            super::super::TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .values()
                .any(ResolutionFailure::is_invalid),
            "return destination is a constraint, not a cast: {module:#?}"
        );
    }
    #[test]
    fn aggregate_reference_coercion_uncertainty_is_not_a_source_contradiction() {
        let source = "fn choose<T>(a:T,b:T)->T{b}";
        let i64_type = Typ::Named("#rust-integer-i64".into());
        let reference = |mutable, referent: Typ| {
            crate::core_ir::rust_references::typ(crate::core_ir::rust_references::ReferenceType {
                mutable,
                referent,
                runtime: Typ::Int,
            })
        };
        let shared = reference(false, i64_type.clone());
        let mutable = reference(true, i64_type);
        for (left, right) in [
            (
                crate::core_ir::tuples::typ(std::slice::from_ref(&shared)),
                crate::core_ir::tuples::typ(std::slice::from_ref(&mutable)),
            ),
            (
                crate::core_ir::rust_arrays::typ(shared.clone(), 1),
                crate::core_ir::rust_arrays::typ(mutable.clone(), 1),
            ),
        ] {
            assert!(
                !infer(source, &[left, right])
                    .unwrap()
                    .unwrap_err()
                    .is_invalid()
            );
        }
        for (left, right) in [
            (
                crate::core_ir::tuples::typ(&[
                    shared.clone(),
                    Typ::Named("#rust-integer-i64".into()),
                ]),
                crate::core_ir::tuples::typ(&[
                    mutable.clone(),
                    Typ::Named("#rust-integer-u64".into()),
                ]),
            ),
            (
                crate::core_ir::rust_arrays::typ(shared.clone(), 1),
                crate::core_ir::rust_arrays::typ(mutable.clone(), 2),
            ),
            (
                crate::core_ir::tuples::typ(std::slice::from_ref(&shared)),
                crate::core_ir::tuples::typ(&[Typ::Named("#rust-integer-i64".into())]),
            ),
            (
                crate::core_ir::tuples::typ(&[shared]),
                crate::core_ir::tuples::typ(&[reference(
                    true,
                    Typ::Named("#rust-integer-u64".into()),
                )]),
            ),
        ] {
            assert!(
                infer(source, &[left, right])
                    .unwrap()
                    .unwrap_err()
                    .is_invalid()
            );
        }
    }
    #[test]
    fn source_aggregate_reference_coercions_remain_honest_refusals() {
        for source in [
            "fn second<T>(a:T,b:T)->T{b}fn main()->i64{let x=7i64;let mut y=42i64;let r=second((&x,),(&mut y,));*r.0}",
            "fn second<T>(a:T,b:T)->T{b}fn main()->i64{let x=7i64;let mut y=42i64;let r=second([&x],[&mut y]);*r[0]}",
        ] {
            let mut module = source_module(source).unwrap();
            let mut references = 0;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if matches!(expr, Expr::Unary {op, ..} if crate::core_ir::rust_references::value_kind(op).is_some())
                {
                    references += 1;
                }
            });
            assert_eq!(
                references, 2,
                "both source borrow identities must reach inference"
            );
            let module =
                super::super::normalize_module(crate::parser_registry::ParserId::Rust, &module);
            let proofs = super::super::TypeChecker::new().typed_proof_nodes(&module.decls);
            assert!(
                proofs.values().any(|failure| !failure.is_invalid()),
                "unmodeled legal coercion must retain Unsupported: {module:#?}"
            );
            assert!(
                !super::super::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .values()
                    .any(ResolutionFailure::is_invalid),
                "legal coercion cannot become Invalid: {source}: {module:#?}"
            );
        }
    }
    #[test]
    fn implicit_calls_reuse_source_specialization_and_explicit_contract_checks() {
        for source in [
            "fn first<T>(a:T,b:T)->T{a}fn left()->f32{42.0f32}fn right()->f32{7.0f32}fn main()->i64{if first(left(),right())==42.0f32{42}else{7}}",
            "fn second<T>(a:T,b:T)->T{b}fn main()->i64{if second(1.00000011920928955078125f32,{let a=1.0000000596046447753906250000001;a})==1.00000011920928955078125f32{42}else{7}}",
            "fn choose<A,B>(a:A,b:B)->B{b}fn main()->i64{if choose(true,42.0f32)==42.0f32{42}else{7}}",
            "fn id<T>(v:T)->T{v}fn outer<T>(v:T)->T{id(v)}fn main()->i64{if outer(42.0f32)==42.0f32{42}else{7}}",
            "fn choose<T>(a:T,b:T)->T{b}fn main()->i64{if choose::<f32>(7.0f32,42.0f32)==42.0f32{42}else{7}}",
        ] {
            let raw = source_module(source).unwrap();
            let module =
                super::super::normalize_module(crate::parser_registry::ParserId::Rust, &raw);
            assert!(
                super::super::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{source}: {module:#?}"
            );
            assert!(module.decls.iter().any(|decl| matches!(decl, Decl::Function{name,type_params,..}if name.starts_with("#rust-specialization:") && type_params.is_empty())), "must specialize before generic erasure: {module:#?}");
        }
        for source in [
            "fn choose<T>(a:T,b:T)->T{b}fn main()->i64{let x=7i64;choose(&x,42i64)}",
            "fn choose<T>(a:T,b:T)->T{b}fn main()->i64{choose(7u8,42u16) as i64}",
        ] {
            let raw = source_module(source).unwrap();
            let module =
                super::super::normalize_module(crate::parser_registry::ParserId::Rust, &raw);
            assert!(
                super::super::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .values()
                    .any(ResolutionFailure::is_invalid),
                "{source}: {module:#?}"
            );
        }
        let raw =
            source_module("fn choose<T:Copy>(a:T,b:T)->T{b}fn main()->i64{choose(7i64,42i64)}")
                .unwrap();
        let module = super::super::normalize_module(crate::parser_registry::ParserId::Rust, &raw);
        assert!(
            !super::super::TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty(),
            "generic obligations must not disappear"
        );
    }
    #[test]
    fn nested_instances_keep_concrete_callee_targets_before_optimization() {
        let source =
            "fn inner<U>(x:U)->U{x}fn outer<T>(x:T)->T{inner(x)}fn main()->i64{outer(42i64)}";
        let raw = source_module(source).unwrap();
        let mut module =
            super::super::normalize_module(crate::parser_registry::ParserId::Rust, &raw);
        assert!(
            super::super::TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty(),
            "{module:#?}"
        );
        let inner = module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function {
                    name, type_params, ..
                } if name.starts_with("#rust-specialization:inner:") && type_params.is_empty() => {
                    Some(name.clone())
                }
                _ => None,
            })
            .expect("inner must be instantiated, not left to optimizer elimination");
        let outer = module.decls.iter_mut().find(|decl| matches!(decl,Decl::Function{name,..}if name.starts_with("#rust-specialization:outer:"))).unwrap();
        let mut calls_inner = false;
        crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(outer), &mut |expr| {
            if matches!(expr,Expr::Call{callee,..}if matches!(callee.as_ref(),Expr::Ident(name)if name==&inner))
            {
                calls_inner = true;
            }
        });
        assert!(
            calls_inner,
            "outer instance must target concrete inner before optimization"
        );
    }
    #[test]
    fn independent_concrete_mismatch_dominates_reference_coercion_uncertainty() {
        for source in [
            "fn choose<T>(a:T,b:&i64,c:bool)->T{a}fn main()->i64{let mut x=7i64;choose(42i64,&mut x,9i64)}",
            "fn choose<T>(pair:(&T,bool))->i64{42}fn main()->i64{let mut x=7i64;choose((&mut x,9i64))}",
        ] {
            let raw = source_module(source).unwrap();
            let module =
                super::super::normalize_module(crate::parser_registry::ParserId::Rust, &raw);
            assert!(
                super::super::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .values()
                    .any(ResolutionFailure::is_invalid),
                "independent concrete contradiction must dominate: {source}: {module:#?}"
            );
        }
        let names = HashSet::from(["T".to_string()]);
        assert!(
            unify_source(&Typ::Bool, &Typ::Int, &names, &mut HashMap::new(), 0).is_ok(),
            "unowned integer carrier is not a source contradiction"
        );
    }
    #[test]
    fn explicit_result_destination_is_checked_without_reinferring_arguments() {
        for source in [
            "fn id<T>(x:T)->T{x}fn main()->i64{id::<bool>(true)}",
            "fn id<T>(x:T)->T{x}fn main()->i64{let x:i64=id::<bool>(true);x}",
            "fn id<T>(x:T)->T{x}fn consume(x:i64)->i64{42}fn main()->i64{consume(id::<bool>(true))}",
            "fn id<T>(x:T)->T{x}fn main()->i64{let mut x=0i64;x=id::<bool>(true);x}",
        ] {
            let raw = source_module(source).unwrap();
            let module =
                super::super::normalize_module(crate::parser_registry::ParserId::Rust, &raw);
            assert!(
                super::super::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .values()
                    .any(ResolutionFailure::is_invalid),
                "{source}: {module:#?}"
            );
        }
        let raw =
            source_module("fn id<T>(x:T)->T{x}fn main()->i64{if id::<bool>(true){42}else{7}}")
                .unwrap();
        let module = super::super::normalize_module(crate::parser_registry::ParserId::Rust, &raw);
        assert!(
            super::super::TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty(),
            "{module:#?}"
        );
    }
    #[test]
    fn local_callable_shadow_does_not_claim_source_template() {
        let raw = source_module("fn choose<T>(x:T)->T{x}fn main()->i64{42}").unwrap();
        let facts = super::super::TypeChecker::new().collect_facts(&raw);
        let expr = Expr::Call {
            callee: Box::new(Expr::Ident("choose".into())),
            args: vec![Expr::BoolLit(true)],
        };
        assert!(
            infer_call(
                &expr,
                &[Some(Typ::Bool)],
                None,
                &facts,
                &HashMap::from([("choose".into(), Typ::Int)])
            )
            .is_none()
        );
    }
}

#[cfg(test)]
mod reachable_template_proof_tests {
    use super::*;
    fn parsed(source: &str) -> UnifiedModule {
        crate::compiler::rust_front::parse_rust_artifact_source(source, "template-proof")
            .unwrap()
            .semantic
    }
    #[test]
    fn exact_neg_bounds_discharge_without_runtime_carrier_equality() {
        let obligation = |output: &str| Decl::Function {
            name: "instance".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![crate::core_ir::Stmt::Expr(Expr::Unary {
                op: rust_generics::neg_bounds(&[rust_generics::NegObligation {
                    receiver: Typ::Named("Row".into()),
                    output: Some(Typ::Named(output.into())),
                }]),
                expr: Box::new(Expr::UnitLit),
            })],
        };
        let proof = rust_generics::NegImplementation {
            receiver: Typ::Named("Row".into()),
            output: Typ::Named("#rust-integer-usize".into()),
        };
        assert!(discharge_neg_obligations(
            &obligation("#rust-integer-usize"),
            std::slice::from_ref(&proof)
        ));
        assert!(!discharge_neg_obligations(
            &obligation("#rust-integer-u64"),
            std::slice::from_ref(&proof)
        ));
        assert!(!discharge_neg_obligations(
            &obligation("#rust-integer-usize"),
            &[proof.clone(), proof]
        ));
    }
    #[test]
    fn deferred_concrete_neg_body_preserves_existing_ambiguity_authority() {
        let artifact = crate::compiler::rust_front::parse_rust_artifact_source("struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{99}}fn negate<T:std::ops::Neg<Output=i64>>(value:T)->i64{-value}fn main()->i64{negate(Row{x:42})}","ambiguous-bounds").unwrap();
        let mut module = artifact.semantic;
        normalize(&mut module);
        let error = super::super::TypeChecker::new()
            .resolve_unary_methods(&mut module, &artifact.unary_methods)
            .unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
    }
    #[test]
    fn deferred_bounds_stay_nonexecuting_after_dce_and_keep_invalid_priority() {
        for source in [
            "fn id<T:MissingTrait>(value:T)->T{value}fn main()->i64{id(42i64)}",
            "fn id<T:MissingTrait>(value:T)->T{value}fn main()->i64{if id(true){42}else{7}}",
            "fn id<T:Copy>(value:T)->T{value}fn main()->i64{id::<i64>(42i64)}",
            "fn id<T:Copy>(value:T)->T{value}fn main()->i64{let unused=id::<bool>(true);42}",
        ] {
            let mut module = parsed(source);
            normalize(&mut module);
            assert!(
                super::super::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .get("main")
                    .is_some_and(|proof| !proof.is_invalid()),
                "{source}: {module:#?}"
            );
            crate::core_opt::optimize_with_entry(&mut module.decls, Some("main"));
            assert!(
                crate::core_ir_verifier::check_execution_contracts(&module).is_err(),
                "{source}"
            );
        }
        let raw = parsed(
            "fn id<T:MissingTrait>(value:T)->T{value}fn main()->i64{let bad:i8=128;id(42i64)}",
        );
        let normalized =
            super::super::normalize_module(crate::parser_registry::ParserId::Rust, &raw);
        assert!(
            super::super::TypeChecker::new()
                .typed_proof_nodes(&normalized.decls)
                .get("main")
                .is_some_and(ResolutionFailure::is_invalid)
        );
    }
    #[test]
    fn unresolved_implicit_arguments_preserve_reachable_source_obligations() {
        let mut module = parsed(
            "extern crate external;fn bound<T:Copy>(value:T)->T{value}fn main()->i64{let value=bound(external::value());42}",
        );
        normalize(&mut module);
        let proofs = super::super::TypeChecker::new().typed_proof_nodes(&module.decls);
        for name in ["bound", "main"] {
            let proof = proofs
                .get(name)
                .expect("reachable source constraint proof must survive incomplete inference");
            assert!(
                !proof.is_invalid(),
                "unowned constructor/Copy satisfaction is not a proven source error"
            );
            assert!(
                proof.message().contains("constraint resolution"),
                "{proof:?}"
            );
        }
        crate::core_opt::optimize_with_entry(&mut module.decls, Some("main"));
        let proofs = super::super::TypeChecker::new().typed_proof_nodes(&module.decls);
        let proof = proofs
            .get("main")
            .expect("discarded result must retain call-site source obligation after DCE");
        assert!(!proof.is_invalid());
        assert!(proof.message().contains("constraint resolution"));
    }
    #[test]
    fn independent_invalid_source_dominates_deferred_template_obligation() {
        let module = parsed(
            "extern crate external;fn bound<T:Copy>(value:T)->T{value}fn main()->i64{let invalid:i8=128;let value=bound(external::value());42}",
        );
        let module =
            super::super::normalize_module(crate::parser_registry::ParserId::Rust, &module);
        assert!(
            super::super::TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .get("main")
                .is_some_and(ResolutionFailure::is_invalid),
            "{module:#?}"
        );
    }
    #[test]
    fn unused_constrained_helper_keeps_entry_root_elimination() {
        let mut module = parsed(
            "extern crate external;fn bound<T:Copy>(value:T)->T{value}fn unused()->i64{let value=bound(external::value());7}fn main()->i64{42}",
        );
        normalize(&mut module);
        let checker = super::super::TypeChecker::new();
        assert!(
            checker
                .typed_proof_nodes(&module.decls)
                .contains_key("unused")
        );
        assert!(
            !checker
                .typed_proof_nodes(&module.decls)
                .contains_key("main")
        );
        crate::core_opt::optimize_with_entry(&mut module.decls, Some("main"));
        assert!(
            checker.typed_proof_nodes(&module.decls).is_empty(),
            "unused constraints are not entry roots: {module:#?}"
        );
    }
    #[test]
    fn proven_neg_output_is_checked_at_its_value_destination() {
        for (output, implementation, returned, invalid) in [
            ("i32", "42", "-value", true),
            ("i32", "42", "(-value) as i64", false),
            ("bool", "true", "-value", true),
            ("i64", "42", "-value", false),
        ] {
            let source = format!(
                "struct Row{{x:i64}}impl std::ops::Neg for Row{{type Output={output};fn neg(self)->{output}{{{implementation}}}}}fn negate<T:std::ops::Neg<Output={output}>>(value:T)->i64{{{returned}}}fn main()->i64{{negate(Row{{x:42}})}}"
            );
            let mut module = parsed(&source);
            normalize(&mut module);
            let proofs = super::super::TypeChecker::new().typed_proof_nodes(&module.decls);
            assert_eq!(
                proofs.values().any(ResolutionFailure::is_invalid),
                invalid,
                "{source}: {module:#?}"
            );
        }
    }
    #[test]
    fn proven_operator_result_retains_reference_coercion_boundary() {
        let integer = Typ::Named("#rust-integer-i64".into());
        assert!(
            proven_operator_result_failure(&Typ::Bool, Some(&integer))
                .is_some_and(|failure| failure.is_invalid())
        );
        assert!(proven_operator_result_failure(&integer, None).is_none());
        let reference = |mutable| {
            crate::core_ir::rust_references::typ(crate::core_ir::rust_references::ReferenceType {
                mutable,
                referent: integer.clone(),
                runtime: Typ::Int,
            })
        };
        assert!(
            proven_operator_result_failure(&reference(true), Some(&reference(false)))
                .is_some_and(|failure| !failure.is_invalid())
        );
        assert!(
            proven_operator_result_failure(&reference(false), Some(&integer))
                .is_some_and(|failure| failure.is_invalid())
        );
    }
}

#[cfg(test)]
mod callable_item_generic_tests {
    use super::*;
    use crate::core_ir::rust_callables::{self, Abi, Item, Signature};
    fn signature(child: Typ) -> Signature {
        Signature {
            abi: Abi::Rust,
            unsafe_call: false,
            variadic: false,
            bound_lifetimes: false,
            parameters: vec![child.clone()],
            result: child,
        }
    }
    fn integer() -> Typ {
        Typ::Named("#rust-integer-i64".into())
    }
    fn item(name: &str) -> Typ {
        rust_callables::item_type(Item {
            identity: name.into(),
            signature: signature(integer()),
        })
    }
    #[test]
    fn pointer_formals_unify_item_children_and_substitute_nested_signature() {
        let declared = rust_callables::typ(signature(Typ::Generic("T".into())));
        let actual = item("a");
        let mut bindings = HashMap::new();
        unify_source(
            &declared,
            &actual,
            &HashSet::from(["T".into()]),
            &mut bindings,
            0,
        )
        .unwrap();
        assert_eq!(bindings.get("T"), Some(&integer()));
        assert!(contains_generic(&declared));
        assert!(owned_type(&actual, 0));
        let mut projection = Decl::Function {
            name: "test".into(),
            params: vec![("f".into(), declared)],
            ret: Typ::Void,
            body: vec![],
            type_params: vec![],
        };
        crate::core_bindings::substitute_type_parameters(&mut projection, &bindings);
        let Decl::Function { params, .. } = projection else {
            panic!()
        };
        assert!(source_argument_agrees(&params[0].1, &actual));
    }
    #[test]
    fn generic_item_binding_keeps_identity_unless_destination_already_proves_pointer() {
        let generic = Typ::Generic("T".into());
        let names = HashSet::from(["T".into()]);
        let mut bindings = HashMap::new();
        unify_source(&generic, &item("a"), &names, &mut bindings, 0).unwrap();
        assert!(
            unify_source(&generic, &item("b"), &names, &mut bindings, 0)
                .unwrap_err()
                .is_invalid()
        );
        let pointer = rust_callables::typ(signature(integer()));
        let mut contextual = HashMap::new();
        unify_source(&generic, &pointer, &names, &mut contextual, 0).unwrap();
        unify_source(&generic, &item("a"), &names, &mut contextual, 0).unwrap();
        unify_source(&generic, &item("b"), &names, &mut contextual, 0).unwrap();
        assert_eq!(contextual.get("T"), Some(&pointer));
        assert!(!source_argument_agrees(&item("a"), &pointer));
    }
    #[test]
    fn callable_child_contradiction_dominates_unowned_form() {
        let left = rust_callables::typ(signature(Typ::Bool));
        let mut right = signature(integer());
        right.unsafe_call = true;
        let right = rust_callables::typ(right);
        assert!(
            unify_source(&left, &right, &HashSet::new(), &mut HashMap::new(), 0)
                .unwrap_err()
                .is_invalid()
        );
        assert!(!owned_type(&right, 0));
    }
}
