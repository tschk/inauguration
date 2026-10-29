//! Bound satisfaction uses the same lexical trait/impl catalog as static dispatch.
use super::*;
use crate::core_ir::rust_generics::{TraitMethod, TraitObligation};

pub(super) fn obligations(
    index: &Index,
    function: &syn::ItemFn,
    frame: &Frame,
) -> Result<Option<Vec<TraitObligation>>, Failure> {
    if !frame.source_is_active()
        || attrs_presence(&function.attrs) != Presence::Active
        || index.incomplete
    {
        return Ok(None);
    }
    let frame = frame.generics(&function.sig.generics);
    let mut obligations = Vec::new();
    let mut seen = Vec::new();
    let mut add = |receiver: Typ,
                   bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::Token![+]>|
     -> Result<bool, Failure> {
        if let Some(failure) = crate::core_ir::type_proofs::failure_in_type(&receiver) {
            return Err(failure);
        }
        for bound in bounds {
            let syn::TypeParamBound::Trait(bound) = bound else {
                return Ok(false);
            };
            if bound.lifetimes.is_some() || !matches!(bound.modifier, syn::TraitBoundModifier::None)
            {
                return Ok(false);
            }
            let Some((identity, false)) = trait_nominal(&bound.path, &frame)? else {
                return Ok(false);
            };
            if seen.contains(&(receiver.clone(), identity.clone())) {
                continue;
            }
            let Some(implementations) = implementations(index, &identity, &frame)? else {
                return Ok(false);
            };
            let methods = methods(index, &identity, &receiver, &frame)?;
            seen.push((receiver.clone(), identity));
            obligations.push(TraitObligation {
                receiver: receiver.clone(),
                implementations,
                methods,
            });
        }
        Ok(true)
    };
    for parameter in &function.sig.generics.params {
        let syn::GenericParam::Type(parameter) = parameter else {
            return Ok(None);
        };
        if attrs_presence(&parameter.attrs) != Presence::Active
            || parameter.default.is_some()
            || !add(Typ::Generic(parameter.ident.to_string()), &parameter.bounds)?
        {
            return Ok(None);
        }
    }
    if let Some(clause) = &function.sig.generics.where_clause {
        for predicate in &clause.predicates {
            let syn::WherePredicate::Type(predicate) = predicate else {
                return Ok(None);
            };
            if predicate.lifetimes.is_some() {
                return Ok(None);
            }
            let _scope = frame.enter();
            if !add(
                crate::compiler::rust_front::map_type(&predicate.bounded_ty),
                &predicate.bounds,
            )? {
                return Ok(None);
            }
        }
    }
    Ok((!obligations.is_empty()).then_some(obligations))
}

fn methods(
    index: &Index,
    identity: &Identity,
    receiver: &Typ,
    frame: &Frame,
) -> Result<Vec<TraitMethod>, Failure> {
    let Some((module, declaration)) = index.trait_sources.get(identity) else {
        return Ok(vec![]);
    };
    let declaring = Frame {
        catalog: frame.catalog.clone(),
        module: module.clone(),
        locals: vec![],
    };
    let scalar = |typ: &Typ| {
        matches!(typ, Typ::Void | Typ::Bool)
            || matches!(typ, Typ::Named(name) if crate::core_ir::rust_integer_type_name(name).is_some())
            || crate::core_ir::rust_floats::kind(typ).is_some()
    };
    let mut methods = Vec::new();
    for member in &declaration.items {
        let syn::TraitItem::Fn(method) = member else {
            continue;
        };
        // Borrowed receivers and aggregate/Self results additionally need
        // parametric borrow/storage proofs; their bounds remain usable without
        // pretending their methods have a scalar invocation contract.
        let Some(argument) = method.sig.receiver() else {
            continue;
        };
        if argument.reference.is_some() || argument.colon_token.is_some() {
            continue;
        }
        let mut signature = method.sig.clone();
        signature.inputs = signature
            .inputs
            .into_iter()
            .filter(|input| !matches!(input, syn::FnArg::Receiver(_)))
            .collect();
        let signature = match source_signatures::declaration(&signature, &declaring) {
            Ok(mut signature)
                if signature.ordinary_rust_form()
                    && signature.parameters.iter().all(&scalar)
                    && scalar(&signature.result) =>
            {
                signature.parameters.insert(0, receiver.clone());
                signature
            }
            Err(failure) if failure.is_invalid() => return Err(failure),
            _ => continue,
        };
        let mut targets = Vec::new();
        for record in &index.implementations {
            let Some((_, path, _)) = &record.item.trait_ else {
                continue;
            };
            let implementation = Frame {
                catalog: frame.catalog.clone(),
                module: record.module.clone(),
                locals: vec![],
            };
            if !matches!(trait_nominal(path, &implementation)?, Some((owner, false)) if &owner == identity)
            {
                continue;
            }
            let syn::Type::Path(owner) = record.item.self_ty.as_ref() else {
                continue;
            };
            let Some((owner, false)) = nominal(&owner.path, &implementation)? else {
                continue;
            };
            let symbol = super::super::super::impl_method_symbol(&record.item, &method.sig.ident);
            let symbol = record
                .module
                .iter()
                .chain(std::iter::once(&symbol))
                .cloned()
                .collect::<Vec<_>>()
                .join("::");
            targets.push((
                Typ::Named(format!("crate::{}", owner.join("::"))),
                format!("crate::{symbol}"),
            ));
        }
        methods.push(TraitMethod {
            name: method.sig.ident.to_string(),
            signature,
            targets,
        });
    }
    Ok(methods)
}

fn implementations(
    index: &Index,
    identity: &Identity,
    frame: &Frame,
) -> Result<Option<Vec<Typ>>, Failure> {
    let Some((_, declaration)) = index.trait_sources.get(identity) else {
        return Ok(None);
    };
    // Auto/unsafe traits, supertraits, defaults and associated items need their
    // own obligations. An ordinary bound cannot erase any of those contracts.
    if declaration.unsafety.is_some()
        || declaration.auto_token.is_some()
        || constrained(&declaration.generics)
        || !declaration.supertraits.is_empty()
        || declaration.items.iter().any(|member| {
            !matches!(member, syn::TraitItem::Fn(method)
                if method.default.is_none()
                    && attrs_presence(&method.attrs) == Presence::Active)
        })
    {
        return Ok(None);
    }
    let mut implementations = Vec::new();
    for record in &index.implementations {
        let Some((negative, path, _)) = &record.item.trait_ else {
            continue;
        };
        let declaring = Frame {
            catalog: frame.catalog.clone(),
            module: record.module.clone(),
            locals: vec![],
        }
        .generics(&record.item.generics);
        let Some((trait_id, generic)) = trait_nominal(path, &declaring)? else {
            // Unowned source must not become evidence of a definite impl set.
            return Ok(None);
        };
        if &trait_id != identity {
            continue;
        }
        if generic
            || negative.is_some()
            || record.presence != Presence::Active
            || record.item.unsafety.is_some()
            || record.item.defaultness.is_some()
            || constrained(&record.item.generics)
        {
            return Ok(None);
        }
        let syn::Type::Path(receiver) = record.item.self_ty.as_ref() else {
            return Ok(None);
        };
        if receiver.qself.is_some() {
            return Ok(None);
        }
        let Some((owner, false)) = nominal(&receiver.path, &declaring)? else {
            return Ok(None);
        };
        if index.unmodeled_receivers.contains(&owner) {
            return Ok(None);
        }
        let mut methods = HashSet::new();
        for member in &record.item.items {
            let syn::ImplItem::Fn(method) = member else {
                return Ok(None);
            };
            if attrs_presence(&method.attrs) == Presence::Inactive {
                continue;
            }
            if attrs_presence(&method.attrs) != Presence::Active || method.defaultness.is_some() {
                return Ok(None);
            }
            let name = method.sig.ident.to_string();
            index.member_role(identity, &name)?;
            if !methods.insert(name) {
                return Err(Failure::Invalid(
                    "duplicate source trait implementation member".into(),
                ));
            }
            index.signature(record, method, frame)?;
        }
        if methods.len() != declaration.items.len() {
            return Err(Failure::Invalid(
                "source trait implementation is missing a required member".into(),
            ));
        }
        // Crate-root paths survive bottom-up inline-module prefixing, then use
        // the existing root canonicalization and dependency qualification walk.
        implementations.push(Typ::Named(format!("crate::{}", owner.join("::"))));
    }
    Ok(Some(implementations))
}
