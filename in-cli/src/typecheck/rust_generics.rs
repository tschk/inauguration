//! Instantiate explicit source type arguments before source integer kinds are erased.
use crate::core_ir::{
    Decl, Expr, Typ, UnifiedModule, rust_generics, type_proofs::ResolutionFailure,
};
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) fn normalize(module: &mut UnifiedModule) {
    normalize_with_checks(module, true);
}

fn normalize_with_checks(module: &mut UnifiedModule, check_templates: bool) {
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
        for (name, failure) in failures {
            if let Some(Decl::Function { body, .. }) = templates.get_mut(&name) {
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
                target, &arguments, templates, duplicates, instances, pending, count,
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
        );
    }
    while let Some(mut declaration) = pending.pop_front() {
        rewrite(
            &mut declaration,
            &templates,
            &duplicates,
            &mut instances,
            &mut pending,
            &mut count,
        );
        module.decls.push(declaration);
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
    if has_constraints(template) {
        return Err(constraint_failure());
    }
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
    strip_template_marker(&mut instance);
    crate::core_bindings::substitute_type_parameters(&mut instance, &bindings);
    instances.insert(key, instance_name.clone());
    pending.push_back(instance);
    Ok(instance_name)
}

fn contains_generic(typ: &Typ) -> bool {
    match typ {
        Typ::Generic(_) => true,
        Typ::Array(item) | Typ::Vector(item) => contains_generic(item),
        Typ::Named(name) => {
            crate::core_ir::rust_arrays::decode(name)
                .is_some_and(|array| contains_generic(&array.element))
                || crate::core_ir::rust_references::decode(name)
                    .is_some_and(|reference| contains_generic(&reference.referent))
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
            if matches!(op.as_str(), rust_generics::TEMPLATE | rust_generics::CONSTRAINTS) && matches!(expr.as_ref(), Expr::UnitLit)
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
        return Some(constraint_failure());
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
    fn targets(declaration: &Decl) -> Vec<String> {
        let mut targets = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut [declaration.clone()], &mut |value| {
            if let Expr::Unary { op, expr } = value
                && rust_generics::decode_arguments(op).is_some()
                && let Expr::Ident(name) = expr.as_ref()
            {
                targets.push(name.clone());
            }
        });
        targets
    }
    let mut pending = VecDeque::new();
    for declaration in &module.decls {
        if !matches!(declaration, Decl::Function { type_params, .. } if !type_params.is_empty()) {
            pending.extend(targets(declaration));
        }
    }
    let mut requested = HashSet::new();
    while let Some(target) = pending.pop_front() {
        if requested.insert(target.clone())
            && let Some(template) = templates.get(&target)
        {
            pending.extend(targets(template));
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
                arguments |= rust_generics::decode_arguments(op).is_some();
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
            if op == rust_generics::CONSTRAINTS && matches!(expr.as_ref(), Expr::UnitLit))))
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
    crate::core_opt::map_decl_exprs_mut(std::slice::from_mut(declaration), &mut |expression| {
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
