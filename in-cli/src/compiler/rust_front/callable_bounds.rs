//! Source-owned callable obligations and argument-position impl Trait parameters.
use super::*;
use crate::core_ir::{rust_callables, rust_generics::CallableObligation};

/// Anonymous parameters participate in inference, but are never turbofish slots.
pub(super) fn desugar_arguments(function: &mut syn::ItemFn) -> Option<usize> {
    if !function.sig.inputs.iter().any(|input| {
        let syn::FnArg::Typed(input) = input else {
            return false;
        };
        let mut typ = input.ty.as_ref();
        loop {
            typ = match typ {
                syn::Type::Reference(value) => &value.elem,
                syn::Type::Paren(value) => &value.elem,
                syn::Type::Group(value) => &value.elem,
                syn::Type::ImplTrait(_) => return true,
                _ => return false,
            };
        }
    }) {
        return None;
    }
    fn parameter(
        typ: &mut syn::Type,
        generics: &mut syn::Generics,
        reserved: &HashSet<&str>,
    ) -> bool {
        match typ {
            syn::Type::Reference(value) => parameter(&mut value.elem, generics, reserved),
            syn::Type::Paren(value) => parameter(&mut value.elem, generics, reserved),
            syn::Type::Group(value) => parameter(&mut value.elem, generics, reserved),
            syn::Type::ImplTrait(value) => {
                let mut index = generics.params.len();
                let name = loop {
                    let name = format!("__in_impl_trait_{index}");
                    if !reserved.contains(name.as_str())
                        && !generics.type_params().any(|value| value.ident == name)
                    {
                        break syn::Ident::new(&name, value.impl_token.span);
                    }
                    index += 1;
                };
                let mut parameter = syn::TypeParam::from(name.clone());
                parameter.bounds = value.bounds.clone();
                generics.params.push(syn::GenericParam::Type(parameter));
                *typ = syn::parse_quote!(#name);
                true
            }
            _ => false,
        }
    }
    let source = function.to_token_stream().to_string();
    let reserved = source
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .collect();
    let explicit = function.sig.generics.type_params().count();
    let mut changed = false;
    for input in &mut function.sig.inputs {
        if let syn::FnArg::Typed(input) = input {
            changed |= parameter(&mut input.ty, &mut function.sig.generics, &reserved);
        }
    }
    changed.then_some(explicit)
}

pub(super) fn obligations(
    file: &syn::File,
    function: &syn::ItemFn,
) -> Result<Option<Vec<CallableObligation>>, crate::core_ir::type_proofs::ResolutionFailure> {
    let mut failure = None;
    let result = (|| {
        let _scope = type_aliases::generic_scope(&function.sig.generics);
        if type_aliases::attrs_presence(&function.attrs) != type_aliases::Presence::Active
            || type_aliases::attrs_presence(&file.attrs) != type_aliases::Presence::Active
            || !type_aliases::current().is_some_and(|frame| frame.source_is_active())
            || file.items.iter().any(|item| {
                matches!(item, syn::Item::Use(_) | syn::Item::ExternCrate(_))
                    && type_aliases::item_presence(item) == type_aliases::Presence::Conditional
            })
        {
            return None;
        }
        let parameters = declared_type_parameters(&function.sig.generics);
        let mut obligations = Vec::new();
        let mut add =
            |receiver: Typ,
             bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::Token![+]>|
             -> Option<()> {
                for bound in bounds {
                    let syn::TypeParamBound::Trait(bound) = bound else {
                        return None;
                    };
                    if bound.lifetimes.is_some()
                        || !matches!(bound.modifier, syn::TraitBoundModifier::None)
                    {
                        return None;
                    }
                    let mut path = bound.path.clone();
                    if path.leading_colon.is_none()
                        && path
                            .segments
                            .first()
                            .is_some_and(|part| parameters.contains(&part.ident.to_string()))
                    {
                        return None;
                    }
                    let last = path.segments.last_mut()?;
                    let syn::PathArguments::Parenthesized(arguments) = &last.arguments else {
                        return None;
                    };
                    // Reuse exact function-pointer source signatures, not runtime words.
                    let inputs = &arguments.inputs;
                    let output = &arguments.output;
                    let callable: syn::Type = syn::parse_quote!(fn(#inputs) #output);
                    let source = match type_aliases::source_signature_type(&callable) {
                        Ok(source) => source,
                        Err(proof) => {
                            failure = crate::core_ir::type_proofs::prefer_failure(
                                failure.take(),
                                Some(proof),
                            );
                            return None;
                        }
                    };
                    let signature = rust_callables::signature_of(&source)?;
                    last.arguments = syn::PathArguments::None;
                    let prelude = path.is_ident("FnMut")
                        && type_aliases::current().is_some_and(|frame| frame.implicit_prelude())
                        && type_aliases::source_type_owned(&path).ok() == Some(false);
                    if !prelude && !unary_methods::standard_ops_trait(&path, file, "FnMut") {
                        return None;
                    }
                    // Value receivers need a mutable binding for arbitrary
                    // FnMut implementations. Keep other patterns constrained
                    // until their source place mutability is represented.
                    if function.sig.inputs.iter().any(|input| {
                        matches!(input, syn::FnArg::Typed(input)
                            if map_type(&input.ty) == receiver
                                && !matches!(input.pat.as_ref(), syn::Pat::Ident(binding)
                                    if binding.mutability.is_some()
                                        && binding.by_ref.is_none()
                                        && binding.subpat.is_none()))
                    }) {
                        return None;
                    }
                    obligations.push(CallableObligation {
                        receiver: receiver.clone(),
                        signature,
                    });
                }
                Some(())
            };
        for parameter in &function.sig.generics.params {
            let syn::GenericParam::Type(parameter) = parameter else {
                return None;
            };
            if type_aliases::attrs_presence(&parameter.attrs) != type_aliases::Presence::Active
                || parameter.default.is_some()
            {
                return None;
            }
            add(Typ::Generic(parameter.ident.to_string()), &parameter.bounds)?;
        }
        if let Some(clause) = &function.sig.generics.where_clause {
            for predicate in &clause.predicates {
                let syn::WherePredicate::Type(predicate) = predicate else {
                    return None;
                };
                if predicate.lifetimes.is_some() {
                    return None;
                }
                add(map_type(&predicate.bounded_ty), &predicate.bounds)?;
            }
        }
        (!obligations.is_empty()).then_some(obligations)
    })();
    failure.map_or(Ok(result), Err)
}
