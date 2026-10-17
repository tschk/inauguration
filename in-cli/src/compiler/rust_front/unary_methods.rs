//! Rust-owned proof of unary trait implementations; never a basename lookup.
use super::*;
use crate::boundary_ir::UnaryMethodBindings;
use crate::core_ir::rust_references;
use std::collections::BTreeMap;

type Import = Option<(Vec<String>, bool)>;

fn import_leaves(
    tree: &syn::UseTree,
    prefix: &[String],
    absolute: bool,
    out: &mut BTreeMap<String, Import>,
) {
    let mut insert = |alias: String, path: Vec<String>| {
        out.entry(alias)
            .and_modify(|value| *value = None)
            .or_insert(Some((path, absolute)));
    };
    match tree {
        syn::UseTree::Path(value) => {
            let mut path = prefix.to_vec();
            path.push(value.ident.to_string());
            import_leaves(&value.tree, &path, absolute, out);
        }
        syn::UseTree::Name(value) if value.ident == "self" => {
            if let Some(alias) = prefix.last() {
                insert(alias.clone(), prefix.to_vec());
            }
        }
        syn::UseTree::Name(value) => {
            let mut path = prefix.to_vec();
            path.push(value.ident.to_string());
            insert(value.ident.to_string(), path);
        }
        syn::UseTree::Rename(value) => {
            let mut path = prefix.to_vec();
            if value.ident != "self" {
                path.push(value.ident.to_string());
            }
            insert(value.rename.to_string(), path);
        }
        syn::UseTree::Group(value) => {
            for item in &value.items {
                import_leaves(item, prefix, absolute, out);
            }
        }
        syn::UseTree::Glob(_) => {} // A glob is not authoritative evidence for a standard trait.
    }
}

fn standard_neg(path: &syn::Path, file: &syn::File) -> bool {
    let mut imports = BTreeMap::new();
    let mut shadowed = HashSet::new();
    let mut external_aliases = BTreeMap::new();
    fn has_glob(tree: &syn::UseTree) -> bool {
        match tree {
            syn::UseTree::Glob(_) => true,
            syn::UseTree::Path(path) => has_glob(&path.tree),
            syn::UseTree::Group(group) => group.items.iter().any(has_glob),
            _ => false,
        }
    }
    let glob_import = file
        .items
        .iter()
        .any(|item| matches!(item, syn::Item::Use(value) if has_glob(&value.tree)));
    for item in &file.items {
        match item {
            syn::Item::Use(value) => import_leaves(
                &value.tree,
                &[],
                value.leading_colon.is_some(),
                &mut imports,
            ),
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

pub(super) fn collect(file: &syn::File) -> UnaryMethodBindings {
    let mut bindings = UnaryMethodBindings::new();
    for item in &file.items {
        let syn::Item::Impl(implementation) = item else {
            continue;
        };
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
                    match (
                        resolve_impl_type(map_type(ret), &receiver_type, implementation),
                        resolve_impl_type(map_type(&outputs[0].ty), &receiver_type, implementation),
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
}
