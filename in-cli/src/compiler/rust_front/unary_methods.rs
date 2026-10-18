//! Rust-owned proof of unary trait implementations; never a basename lookup.
use super::imports::import_leaves;
use super::*;
use crate::boundary_ir::UnaryMethodBindings;
use crate::core_ir::rust_references;
use std::collections::BTreeMap;

fn standard_neg(path: &syn::Path, file: &syn::File) -> bool {
    let mut imports = HashMap::new();
    let mut shadowed = HashSet::new();
    let mut external_aliases = BTreeMap::new();
    let mut glob_import = false;
    for item in &file.items {
        if type_aliases::item_presence(item) == type_aliases::Presence::Inactive {
            continue;
        }
        match item {
            syn::Item::Use(value) => {
                glob_import |= import_leaves(
                    &value.tree,
                    &[],
                    value.leading_colon.is_some(),
                    &mut imports,
                );
            }
            syn::Item::Struct(value) => {
                shadowed.insert(value.ident.to_string());
            }
            syn::Item::Enum(value) => {
                shadowed.insert(value.ident.to_string());
            }
            syn::Item::Union(value) => {
                shadowed.insert(value.ident.to_string());
            }
            syn::Item::Trait(value) => {
                shadowed.insert(value.ident.to_string());
            }
            syn::Item::Type(value) => {
                shadowed.insert(value.ident.to_string());
            }
            syn::Item::Mod(value) => {
                shadowed.insert(value.ident.to_string());
            }
            syn::Item::ExternCrate(value) => {
                let alias = value
                    .rename
                    .as_ref()
                    .map(|(_, alias)| alias)
                    .unwrap_or(&value.ident)
                    .to_string();
                let external = (value.ident == "std" || value.ident == "core")
                    .then(|| value.ident.to_string());
                let import = external.as_ref().map(|name| (vec![name.clone()], true));
                // Duplicate imports are ambiguous even if a later extern crate
                // happens to have a standard-looking spelling.
                if external_aliases.insert(alias.clone(), external).is_some() {
                    external_aliases.insert(alias.clone(), None);
                }
                if imports.insert(alias.clone(), import).is_some() {
                    imports.insert(alias, None);
                }
            }
            _ => {}
        }
    }
    if path
        .segments
        .iter()
        .any(|segment| !matches!(segment.arguments, syn::PathArguments::None))
    {
        return false;
    }
    let mut parts: Vec<_> = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    let mut absolute = path.leading_colon.is_some();
    let mut visited = HashSet::new();
    loop {
        let Some(first) = parts.first() else {
            return false;
        };
        if absolute {
            if let Some(root) = external_aliases.get(first) {
                if imports.get(first).is_some_and(Option::is_none) {
                    return false;
                }
                let Some(root) = root else {
                    return false;
                };
                parts[0] = root.clone();
            }
        } else {
            if shadowed.contains(first) {
                return false;
            }
            if let Some(import) = imports.get(first) {
                if !visited.insert(first.clone()) {
                    return false;
                }
                let Some((prefix, rooted)) = import else {
                    return false;
                };
                let mut resolved = prefix.clone();
                resolved.extend(parts.into_iter().skip(1));
                parts = resolved;
                absolute = *rooted;
                continue;
            }
        }
        // A glob can introduce a competing root; without resolving that source
        // it cannot establish an unrooted external-crate identity.
        if !absolute && glob_import {
            return false;
        }
        return parts.len() == 3
            && matches!(parts[0].as_str(), "std" | "core")
            && parts[1] == "ops"
            && parts[2] == "Neg";
    }
}

// Bound provenance is stricter than legacy implementation discovery: unknown
// cfg imports and generic root shadows cannot establish an external identity.
fn strict_neg_scope(file: &syn::File) -> bool {
    type_aliases::current().is_some_and(|frame| frame.source_is_active())
        && type_aliases::attrs_presence(&file.attrs) == type_aliases::Presence::Active
        && file.items.iter().all(|item| {
            !matches!(item, syn::Item::Use(_) | syn::Item::ExternCrate(_))
                || type_aliases::item_presence(item) != type_aliases::Presence::Conditional
        })
}
// Only existing Catalog ownership establishes a source type identity. This
// proof helper never infers a layout or treats an unknown Named carrier as owned.
fn source_type_identity_owned(source: &syn::Type, allow_self: bool) -> bool {
    match source {
        syn::Type::Path(path) if path.qself.is_none() => {
            (allow_self && path.path.is_ident("Self"))
                || type_aliases::source_type_owned(&path.path).unwrap_or(false)
        }
        syn::Type::Reference(value) => source_type_identity_owned(&value.elem, allow_self),
        syn::Type::Paren(value) => source_type_identity_owned(&value.elem, allow_self),
        syn::Type::Group(value) => source_type_identity_owned(&value.elem, allow_self),
        _ => false,
    }
}
pub(super) fn generic_neg_obligations(
    file: &syn::File,
    function: &syn::ItemFn,
) -> Result<
    Option<Vec<crate::core_ir::rust_generics::NegObligation>>,
    crate::core_ir::type_proofs::ResolutionFailure,
> {
    use crate::core_ir::rust_generics::NegObligation;
    let _scope = type_aliases::generic_scope(&function.sig.generics);
    if type_aliases::attrs_presence(&function.attrs) == type_aliases::Presence::Active
        && type_aliases::current().is_some_and(|frame| frame.source_is_active())
    {
        let mut bounds: Vec<_> = function
            .sig
            .generics
            .type_params()
            .filter(|value| {
                type_aliases::attrs_presence(&value.attrs) == type_aliases::Presence::Active
            })
            .flat_map(|value| value.bounds.iter())
            .collect();
        if let Some(clause) = &function.sig.generics.where_clause {
            for predicate in &clause.predicates {
                if let syn::WherePredicate::Type(value) = predicate {
                    bounds.extend(value.bounds.iter());
                }
            }
        }
        let mut failure = None;
        for bound in bounds {
            if let syn::TypeParamBound::Trait(bound) = bound {
                for segment in &bound.path.segments {
                    if let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments {
                        for argument in &arguments.args {
                            if let syn::GenericArgument::AssocType(binding) = argument
                                && binding.ident == "Output"
                                && let Some(proof) = crate::core_ir::type_proofs::failure_in_type(
                                    &map_type(&binding.ty),
                                )
                                .filter(|proof| proof.is_invalid())
                            {
                                failure = crate::core_ir::type_proofs::prefer_failure(
                                    failure,
                                    Some(proof),
                                );
                            }
                        }
                    }
                }
            }
        }
        if let Some(failure) = failure {
            return Err(failure);
        }
    }
    let result = (|| {
        if !strict_neg_scope(file)
            || type_aliases::attrs_presence(&function.attrs) != type_aliases::Presence::Active
        {
            return None;
        }
        let parameters: HashSet<_> = function
            .sig
            .generics
            .type_params()
            .map(|value| value.ident.to_string())
            .collect();
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
                            .is_some_and(|segment| parameters.contains(&segment.ident.to_string()))
                    {
                        return None;
                    }
                    let last = path.segments.last_mut()?;
                    let output = match &last.arguments {
                        syn::PathArguments::None => None,
                        syn::PathArguments::AngleBracketed(arguments)
                            if arguments.args.len() == 1 && arguments.colon2_token.is_none() =>
                        {
                            let syn::GenericArgument::AssocType(binding) =
                                arguments.args.first()?
                            else {
                                return None;
                            };
                            if binding.ident != "Output" || binding.generics.is_some() {
                                return None;
                            }
                            if !source_type_identity_owned(&binding.ty, false) {
                                return None;
                            }
                            let output = map_type(&binding.ty);
                            if crate::core_ir::type_proofs::failure_in_type(&output).is_some() {
                                return None;
                            }
                            Some(output)
                        }
                        _ => return None,
                    };
                    last.arguments = syn::PathArguments::None;
                    if !standard_neg(&path, file) {
                        return None;
                    }
                    obligations.push(NegObligation {
                        receiver: receiver.clone(),
                        output,
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
        if let Some(where_clause) = &function.sig.generics.where_clause {
            for predicate in &where_clause.predicates {
                let syn::WherePredicate::Type(predicate) = predicate else {
                    return None;
                };
                if predicate.lifetimes.is_some() {
                    return None;
                }
                let receiver = map_type(&predicate.bounded_ty);
                if crate::core_ir::type_proofs::failure_in_type(&receiver).is_some() {
                    return None;
                }
                add(receiver, &predicate.bounds)?;
            }
        }
        (!obligations.is_empty()).then_some(obligations)
    })();
    Ok(result)
}
pub(super) fn neg_implementation_proof(
    file: &syn::File,
    implementation: &syn::ItemImpl,
    bindings: &UnaryMethodBindings,
) -> Option<(String, crate::core_ir::rust_generics::NegImplementation)> {
    if !strict_neg_scope(file)
        || type_aliases::attrs_presence(&implementation.attrs) != type_aliases::Presence::Active
    {
        return None;
    }
    let _scope = type_aliases::generic_scope(&implementation.generics);
    if !source_type_identity_owned(&implementation.self_ty, false) {
        return None;
    }
    let receiver = map_type(&implementation.self_ty);
    let identity = rust_references::receiver_identity(&receiver)?;
    let target = bindings.get(&(identity, "-".into()))?.as_ref().ok()?;
    let outputs: Vec<_> = implementation
        .items
        .iter()
        .filter_map(|item| match item {
            syn::ImplItem::Type(item) if item.ident == "Output" => Some(item),
            _ => None,
        })
        .collect();
    let [output] = outputs.as_slice() else {
        return None;
    };
    if type_aliases::attrs_presence(&output.attrs) != type_aliases::Presence::Active
        || implementation.items.iter().any(|item|matches!(item,syn::ImplItem::Fn(method) if method.sig.ident=="neg" && type_aliases::attrs_presence(&method.attrs)!=type_aliases::Presence::Active))
    { return None; }
    if !source_type_identity_owned(&output.ty, true) {
        return None;
    }
    let output = resolve_impl_type(map_type(&output.ty), &receiver, implementation).ok()?;
    Some((
        target.clone(),
        crate::core_ir::rust_generics::NegImplementation { receiver, output },
    ))
}
pub(super) fn collect(file: &syn::File) -> UnaryMethodBindings {
    let mut bindings = UnaryMethodBindings::new();
    for item in &file.items {
        if type_aliases::item_presence(item) == type_aliases::Presence::Inactive {
            continue;
        }
        let syn::Item::Impl(implementation) = item else {
            continue;
        };
        let _generic_scope = super::type_aliases::generic_scope(&implementation.generics);
        let Some((negative, trait_path, _)) = &implementation.trait_ else {
            continue;
        };
        if negative.is_some() || !standard_neg(trait_path, file) {
            continue;
        }
        let receiver_type = map_type(&implementation.self_ty);
        let Some(receiver) = rust_references::receiver_identity(&receiver_type) else {
            continue;
        };
        let target = format!(
            "{}::{}::neg",
            compact_rust_tokens(&implementation.self_ty),
            compact_rust_tokens(trait_path)
        );
        let methods: Vec<_> = implementation
            .items
            .iter()
            .filter_map(|item| match item {
                syn::ImplItem::Fn(value) if value.sig.ident == "neg" => Some(value),
                _ => None,
            })
            .collect();
        let outputs: Vec<_> = implementation
            .items
            .iter()
            .filter_map(|item| match item {
                syn::ImplItem::Type(value) if value.ident == "Output" => Some(value),
                _ => None,
            })
            .collect();
        let valid = methods.len() == 1
            && outputs.len() == 1
            && implementation.defaultness.is_none()
            && implementation.unsafety.is_none()
            && implementation.generics.type_params().next().is_none()
            && implementation.generics.const_params().next().is_none()
            && implementation.generics.where_clause.is_none()
            && outputs[0].generics.params.is_empty()
            && outputs[0].generics.where_clause.is_none()
            && methods[0].sig.generics.params.is_empty()
            && methods[0].sig.generics.where_clause.is_none()
            && methods[0].sig.asyncness.is_none()
            && methods[0].sig.constness.is_none()
            && methods[0].sig.unsafety.is_none()
            && methods[0].sig.abi.is_none()
            && methods[0].sig.variadic.is_none()
            && methods[0].defaultness.is_none()
            && methods[0].sig.inputs.len() == 1
            && matches!(methods[0].sig.inputs.first(), Some(syn::FnArg::Receiver(value)) if value.reference.is_none() && value.colon_token.is_none())
            && match &methods[0].sig.output {
                syn::ReturnType::Type(_, ret) => {
                    let expected =
                        resolve_impl_type(map_type(&outputs[0].ty), &receiver_type, implementation);
                    let _method_scope =
                        super::type_aliases::generic_scope(&methods[0].sig.generics);
                    match (
                        resolve_impl_type(map_type(ret), &receiver_type, implementation),
                        expected,
                    ) {
                        (Ok(actual), Ok(expected)) => actual == expected,
                        _ => false,
                    }
                }
                _ => false,
            };
        let value = if valid {
            Ok(target)
        } else {
            Err(format!(
                "unsupported exact Neg receiver/output contract for `{receiver}`"
            ))
        };
        if bindings
            .insert((receiver.clone(), "-".into()), value)
            .is_some()
        {
            bindings.insert(
                (receiver, "-".into()),
                Err("ambiguous Rust Neg implementations".into()),
            );
        }
    }
    bindings
}

pub(super) fn prefix(
    mut artifact: CompileArtifact,
    module: &str,
    file: &syn::File,
) -> Result<CompileArtifact, String> {
    let before: Vec<_> = artifact
        .semantic
        .decls
        .iter()
        .map(|decl| match decl {
            Decl::Struct { name, .. } => Some((false, name.clone())),
            Decl::Function { name, .. } => Some((true, name.clone())),
            _ => None,
        })
        .collect();
    artifact.semantic.decls = prefix_module_decls(artifact.semantic.decls, module, file);
    let mut types = BTreeMap::new();
    let mut values = BTreeMap::new();
    for (old, new) in before.iter().zip(&artifact.semantic.decls) {
        match (old, new) {
            (Some((false, old)), Decl::Struct { name: new, .. }) => {
                types.insert(old.clone(), new.clone());
            }
            (Some((true, old)), Decl::Function { name: new, .. }) => {
                values.insert(old.clone(), new.clone());
            }
            _ => {}
        }
    }
    let mut qualified_bindings = UnaryMethodBindings::new();
    for ((receiver, op), target) in artifact.unary_methods {
        let qualified = rust_references::qualify_receiver_identity(&receiver, |name| {
            types.get(name).cloned().unwrap_or_else(|| name.into())
        })
        .ok_or_else(|| format!("unresolved prefixed unary receiver `{receiver}`"))?;
        let target = target.and_then(|target| {
            values
                .get(&target)
                .cloned()
                .ok_or_else(|| format!("missing exact prefixed unary target `{target}`"))
        });
        crate::boundary_ir::merge_unary_method_bindings(
            &mut qualified_bindings,
            UnaryMethodBindings::from([((qualified, op), target)]),
        )?;
    }
    artifact.unary_methods = qualified_bindings;
    Ok(artifact)
}

#[cfg(test)]
mod collector_tests {
    use super::*;
    fn parsed(source: &str) -> syn::File {
        syn::parse_file(source).unwrap()
    }
    fn neg(path: &str, prelude: &str) -> bool {
        standard_neg(&syn::parse_str(path).unwrap(), &parsed(prelude))
    }
    #[test]
    fn known_inactive_items_cannot_supply_or_shadow_neg_bindings() {
        assert!(neg("std::ops::Neg", "#[cfg(any())] mod std {}"));
        assert!(collect(&parsed("struct Row;#[cfg(any())]impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}" )).is_empty());
        assert_eq!(collect(&parsed("struct Row;#[cfg(not(any()))]impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}" )).len(), 1);
    }

    #[test]
    fn standard_trait_resolution_requires_unambiguous_scoped_imports() {
        for (path, prelude) in [
            ("std::ops::Neg", ""),
            ("::std::ops::Neg", "mod std {}"),
            ("Minus", "use std::ops::Neg as Minus;"),
            ("Neg", "use core::ops::{self, Neg};"),
            ("ops::Neg", "use ::core::ops;"),
            (
                "Minus",
                "extern crate core as language; use language::ops::Neg as Minus;",
            ),
            ("Neg", "use ::std::ops::Neg; use unknown::*;"),
        ] {
            assert!(neg(path, prelude), "{path}: {prelude}");
        }
        for (path, prelude) in [
            ("std::ops::Neg", "mod std {}"),
            ("Neg", "trait Neg {}"),
            ("Neg", "use std::ops::Neg; trait Neg {}"),
            ("Neg", "use std::ops::Neg; use other::Neg;"),
            ("Neg", "use std::ops::*;"),
            ("std::ops::Neg", "use unknown::*;"),
            ("std::ops::Neg", "extern crate other as std;"),
            (
                "Neg",
                "use right::Neg as Left; use Left as Neg; use Neg as right;",
            ),
            ("Neg", "use other::Neg; extern crate std as Neg;"),
            (
                "::language::ops::Neg",
                "extern crate std as language; use other as language;",
            ),
        ] {
            assert!(!neg(path, prelude), "{path}: {prelude}");
        }
    }
    #[test]
    fn exact_receiver_proof_does_not_construct_method_target_from_key() {
        for (receiver, key, target) in [
            ("Row", "Row", "Row::std::ops::Neg::neg"),
            ("&mut Row", "&mut Row", "&mutRow::std::ops::Neg::neg"),
        ] {
            let file = parsed(&format!(
                "struct Row{{value:i64}} impl std::ops::Neg for {receiver}{{type Output=i64; fn neg(self)->i64{{77}}}}"
            ));
            let bindings = collect(&file);
            assert_eq!(
                bindings.get(&(key.into(), "-".into())),
                Some(&Ok(target.into()))
            );
        }
        let file = parsed(
            "use std::ops::Neg as Minus;struct Row;impl Minus for Row{type Output=Self;fn neg(self)->Row{self}}",
        );
        assert_eq!(
            collect(&file).get(&("Row".into(), "-".into())),
            Some(&Ok("Row::Minus::neg".into()))
        );
    }
    #[test]
    fn unsupported_exact_signatures_and_output_mismatches_retain_refusal() {
        for method in [
            "fn neg(&self)->i64{77}",
            "fn neg(self, other:i64)->i64{77}",
            "fn neg(self)->i32{77}",
            "fn neg<T>(self)->i64{77}",
            "async fn neg(self)->i64{77}",
            "unsafe fn neg(self)->i64{77}",
            "extern \"C\" fn neg(self)->i64{77}",
            "const fn neg(self)->i64{77}",
            "fn neg(self)->i64 where Self:Sized{77}",
        ] {
            let source =
                format!("struct Row;impl std::ops::Neg for Row{{type Output=i64;{method}}}");
            let bindings = collect(&parsed(&source));
            assert!(
                matches!(bindings.get(&("Row".into(), "-".into())), Some(Err(_))),
                "{method}: {bindings:?}"
            );
        }
        for source in [
            "struct Row;impl<T> std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}",
            "struct Row;impl std::ops::Neg for Row{fn neg(self)->i64{77}}",
            "struct Row;impl std::ops::Neg for Row{type Output=i64;type Output=i32;fn neg(self)->i64{77}}",
            "struct Row;impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{88}}",
        ] {
            assert!(
                matches!(
                    collect(&parsed(source)).get(&("Row".into(), "-".into())),
                    Some(Err(_))
                ),
                "{source}"
            );
        }
    }
    #[test]
    fn inherent_and_unrelated_traits_never_establish_operator_proof() {
        let source = "struct Row;trait Neg{fn neg(self)->i64;}impl Row{fn neg(self)->i64{77}}impl Neg for Row{fn neg(self)->i64{88}}";
        assert!(collect(&parsed(source)).is_empty());
    }
    #[test]
    fn generic_bound_metadata_requires_exact_active_complete_standard_trait_provenance() {
        use crate::core_ir::rust_generics;
        for source in [
            "use std::ops::Neg as Minus;fn negate<T:Minus<Output=i64>>(value:T)->i64{-value}",
            "fn negate<T>(value:T)->i64 where T: ::core::ops::Neg<Output=i64>{-value}",
        ] {
            let artifact =
                crate::compiler::rust_front::parse_rust_artifact_source(source, "bounds").unwrap();
            assert!(artifact.semantic.decls.iter().any(|decl|matches!(decl,Decl::Function{body,..}if body.iter().any(|statement|matches!(statement,Stmt::Expr(Expr::Unary{op,..})if rust_generics::decode_neg_bounds(op).is_some())))),"{source}");
        }
        for source in [
            "fn negate<T:std::ops::Neg<Output=i64>+MissingTrait>(value:T)->i64{-value}",
            "trait Neg{type Output;}fn negate<T:Neg<Output=i64>>(value:T)->i64{42}",
            "use std::ops::Neg as Minus;fn negate<Minus,T:Minus<Output=i64>>(value:T)->i64{42}",
            r#"#[cfg(feature="other")]use std::ops::Neg as Minus;fn negate<T:Minus<Output=i64>>(value:T)->i64{42}"#,
        ] {
            let artifact =
                crate::compiler::rust_front::parse_rust_artifact_source(source, "bounds").unwrap();
            assert!(!artifact.semantic.decls.iter().any(|decl|matches!(decl,Decl::Function{body,..}if body.iter().any(|statement|matches!(statement,Stmt::Expr(Expr::Unary{op,..})if rust_generics::decode_neg_bounds(op).is_some())))),"{source}");
        }
    }
    #[test]
    fn inactive_and_conditional_generic_bounds_cannot_create_definite_output_failures() {
        for source in [
            r#"fn unused<#[cfg(any())] T:std::ops::Neg<Output=bool<u8>>>(){}fn main()->i64{42}"#,
            r#"fn unused<#[cfg(feature="enabled")] T:std::ops::Neg<Output=bool<u8>>>(){}fn main()->i64{42}"#,
        ] {
            let artifact = crate::compiler::rust_front::parse_rust_artifact_source(
                source,
                "conditional-bounds",
            )
            .unwrap();
            assert!(
                !crate::typecheck::TypeChecker::new()
                    .typed_proof_nodes(&artifact.semantic.decls)
                    .values()
                    .any(crate::core_ir::type_proofs::ResolutionFailure::is_invalid),
                "{source}: {:#?}",
                artifact.semantic
            );
        }
    }
}
