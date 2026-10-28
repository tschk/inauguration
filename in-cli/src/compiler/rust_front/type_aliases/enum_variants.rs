//! Source enum values through explicit imports and visible module/enum globs.
use super::*;

type Variant = (String, syn::Variant, i64);

fn option_variant(name: &str) -> Option<Variant> {
    Some(match name {
        "Some" => (
            crate::core_ir::rust_options::OWNER.into(),
            syn::parse_quote!(Some(T)),
            1,
        ),
        "None" => (
            crate::core_ir::rust_options::OWNER.into(),
            syn::parse_quote!(None),
            0,
        ),
        _ => return None,
    })
}

fn qualified_option_variant(path: &syn::Path) -> Option<Variant> {
    let mut owner = path.clone();
    let member = owner.segments.pop()?.into_value();
    let variant = option_variant(&member.ident.to_string())?;
    owner.segments.pop_punct();
    let mut source = syn::Type::Path(syn::TypePath {
        qself: None,
        path: owner,
    });
    let resolved = source_signature_type(&source)
        .or_else(|failure| {
            let syn::Type::Path(owner) = &mut source else {
                unreachable!()
            };
            let owner = owner
                .path
                .segments
                .last_mut()
                .ok_or_else(|| unsupported("Option owner"))?;
            if !matches!(owner.arguments, syn::PathArguments::None) {
                return Err(failure);
            }
            owner.arguments = syn::PathArguments::AngleBracketed(syn::parse_quote!(<()>));
            source_signature_type(&source)
        })
        .ok()?;
    let Typ::Named(name) = resolved else {
        return None;
    };
    let fields = crate::core_ir::tuples::fields(&name)?;
    matches!(fields.first(),Some((_,Typ::Named(name))) if name=="#rust-source-signature:std:option:Option").then_some(variant)
}

pub(super) fn identity(value: &Resolved, frame: &Frame) -> Option<Vec<String>> {
    let path = match value.clone().visibility().0 {
        Resolved::Type(Typ::Named(name)) => crate::core_ir::source_types::nominal_identity(&name)?
            .split("::")
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        Resolved::Nominal(obligations) => obligations.into_iter().find_map(|obligation| {
            if let NominalObligation::Application(application) = obligation {
                application.identity
            } else {
                None
            }
        })?,
        _ => return None,
    };
    let (name, module) = path.split_last()?;
    frame
        .catalog
        .modules
        .get(module)?
        .enums
        .contains_key(name)
        .then_some(path)
}

pub(super) fn resolve(path: &syn::Path) -> Result<Option<Variant>, Failure> {
    let Some(frame) = current() else {
        return Ok(None);
    };
    lookup(path, &frame, &frame.module, &mut HashSet::new(), true)
}

fn lookup(
    path: &syn::Path,
    frame: &Frame,
    access: &[String],
    active: &mut HashSet<Key>,
    prelude: bool,
) -> Result<Option<Variant>, Failure> {
    let _guard = frame.enter();
    if path.segments.len() > 1 {
        if let Some(variant) = qualified_enum_variant(path) {
            return Ok(Some(variant));
        }
        if let Some(variant) = qualified_option_variant(path) {
            return Ok(Some(variant));
        }
        let mut owner = path.clone();
        let Some(member) = owner.segments.pop() else {
            return Ok(None);
        };
        let name = member.into_value().ident;
        owner.segments.pop_punct();
        if let Some(module) = source_module_path(&owner)? {
            let declaring = Frame {
                catalog: frame.catalog.clone(),
                module,
                locals: vec![],
            };
            return lookup(&syn::parse_quote!(#name), &declaring, access, active, false);
        }
        return Ok(None);
    }
    let Some(name) = path.get_ident().map(ToString::to_string) else {
        return Ok(None);
    };
    let key = (frame.module.clone(), frame.locals.len(), name.clone());
    if !active.insert(key.clone()) {
        return Ok(None);
    }
    let result = (|| {
        for index in (0..=frame.locals.len()).rev() {
            let scope = if index == 0 {
                frame.catalog.modules.get(&frame.module).map(Rc::as_ref)
            } else {
                Some(frame.locals[index - 1].as_ref())
            };
            let Some(scope) = scope else { continue };
            let declaring = Frame {
                catalog: frame.catalog.clone(),
                module: frame.module.clone(),
                locals: frame.locals[..index].to_vec(),
            };
            let _scope = declaring.enter();
            // Ordinary declarations and explicit imports outrank glob imports.
            if scope.values.contains_key(&name) {
                return Ok(None);
            }
            if let Some(imports) = scope.imports.get(&name) {
                let mut variants = Vec::new();
                for import in imports {
                    let Binding::Visible(binding, visibility) = import else {
                        return Err(unsupported("conditional enum variant import"));
                    };
                    if !visibility_extent(visibility, &frame.module)?.permits(access) {
                        continue;
                    }
                    let Binding::Import(parts, absolute) = binding.as_ref() else {
                        continue;
                    };
                    let source =
                        format!("{}{}", if *absolute { "::" } else { "" }, parts.join("::"));
                    let target = syn::parse_str(&source)
                        .map_err(|_| unsupported("enum variant import path"))?;
                    if let Some(variant) =
                        lookup(&target, &declaring, &frame.module, active, false)?
                    {
                        variants.push(variant)
                    }
                }
                return unique(variants);
            }
            let mut variants = Vec::new();
            for (parts, absolute, presence, visibility) in &scope.glob_sources {
                if !visibility_extent(visibility, &frame.module)?.permits(access) {
                    continue;
                }
                if *presence != Presence::Active {
                    return Err(unsupported("conditional enum variant glob"));
                }
                let source = format!("{}{}", if *absolute { "::" } else { "" }, parts.join("::"));
                let target: syn::Path = syn::parse_str(&format!("{source}::{name}"))
                    .map_err(|_| unsupported("enum glob variant path"))?;
                if let Some(variant) = lookup(&target, &declaring, &frame.module, active, false)? {
                    variants.push(variant)
                }
            }
            if !variants.is_empty() {
                return unique(variants);
            }
        }
        Ok(if prelude { option_variant(&name) } else { None })
    })();
    active.remove(&key);
    result
}

fn unique(variants: Vec<Variant>) -> Result<Option<Variant>, Failure> {
    let mut selected: Option<Variant> = None;
    for variant in variants {
        if let Some(previous) = &selected {
            if previous.0 != variant.0 || previous.1.ident != variant.1.ident {
                return Err(Failure::Invalid("ambiguous enum variant import".into()));
            }
        } else {
            selected = Some(variant)
        }
    }
    Ok(selected)
}
