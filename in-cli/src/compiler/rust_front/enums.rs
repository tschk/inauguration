//! Source-owned enum constructors and discriminant patterns.
use super::*;

fn zero(typ: &Typ) -> Option<Expr> {
    if let Typ::Named(name) = typ
        && let Some(kind) = crate::core_ir::rust_integer_type_name(name)
    {
        return Some(rust_integer_literal(0, kind));
    }
    if let Some(kind) = crate::core_ir::rust_floats::kind(typ) {
        return Some(crate::core_ir::rust_floats::literal("0.0", Some(kind)));
    }
    match typ {
        Typ::Int => Some(Expr::IntLit(0)),
        Typ::Bool => Some(Expr::BoolLit(false)),
        Typ::Float => Some(Expr::FloatLit(crate::core_ir::FloatVal(0.0))),
        Typ::String => Some(Expr::StringLit(String::new())),
        Typ::Void => Some(Expr::UnitLit),
        _ => None,
    }
}

pub(super) fn declarations(item: &syn::ItemEnum) -> Option<Vec<Decl>> {
    let name = item.ident.to_string();
    let mut fields = vec![("__discriminant".into(), Typ::Int)];
    let mut variants = Vec::new();
    for variant in &item.variants {
        match type_aliases::attrs_presence(&variant.attrs) {
            type_aliases::Presence::Inactive => continue,
            type_aliases::Presence::Conditional => return None,
            type_aliases::Presence::Active => {}
        }
        let enum_name = &item.ident;
        let member = &variant.ident;
        let path = syn::parse_quote!(#enum_name::#member);
        let (_, _, tag) = type_aliases::enum_variant(&path)?;
        let payload = variant
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let field_name = field
                    .ident
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| index.to_string());
                (format!("{member}_{field_name}"), map_type(&field.ty))
            })
            .collect::<Vec<_>>();
        fields.extend(payload.clone());
        variants.push((variant.ident.to_string(), tag, payload));
    }
    let mut declarations = vec![Decl::Struct {
        name: name.clone(),
        fields: fields.clone(),
        type_params: vec![],
    }];
    // Inactive variant slots are padding. Only materialize a constructor when
    // every slot has an owned representation; recursive/opaque payloads must
    // not be replaced by guessed integer storage.
    let Some(defaults) = fields
        .iter()
        .map(|(_, typ)| zero(typ))
        .collect::<Option<Vec<_>>>()
    else {
        return Some(declarations);
    };
    for (variant, tag, payload) in variants {
        let params = payload
            .iter()
            .enumerate()
            .map(|(index, (_, typ))| (format!("#enum-arg-{index}"), typ.clone()))
            .collect::<Vec<_>>();
        let mut values = fields
            .iter()
            .map(|(name, _)| name.clone())
            .zip(defaults.clone())
            .collect::<Vec<_>>();
        values[0].1 = Expr::IntLit(tag);
        for ((field, _), (parameter, _)) in payload.iter().zip(&params) {
            if let Some((_, value)) = values.iter_mut().find(|(name, _)| name == field) {
                *value = Expr::Ident(parameter.clone());
            }
        }
        declarations.push(Decl::Function {
            name: format!("{name}::{variant}"),
            params,
            ret: Typ::Named(name.clone()),
            body: vec![Stmt::Return(Some(Expr::StructInit {
                name: name.clone(),
                fields: values,
            }))],
            type_params: vec![],
        });
    }
    Some(declarations)
}

pub(super) fn pattern(pattern: &syn::Pat) -> String {
    let identifier_path;
    let (path, payload) = match pattern {
        syn::Pat::TupleStruct(value) => (
            &value.path,
            value
                .elems
                .iter()
                .enumerate()
                .map(|(index, pattern)| (index.to_string(), self::pattern(pattern)))
                .collect::<Vec<_>>(),
        ),
        syn::Pat::Struct(value) => (
            &value.path,
            value
                .fields
                .iter()
                .map(|field| {
                    (
                        compact_rust_tokens(&field.member),
                        self::pattern(&field.pat),
                    )
                })
                .collect(),
        ),
        syn::Pat::Path(value) => (&value.path, vec![]),
        syn::Pat::Ident(value) if value.by_ref.is_none() && value.subpat.is_none() => {
            identifier_path = syn::Path::from(value.ident.clone());
            (&identifier_path, vec![])
        }
        _ => return pattern.to_token_stream().to_string(),
    };
    let (owner, variant, tag) = match type_aliases::enum_pattern_variant(path) {
        Ok(Some(value)) => value,
        Ok(None) => return pattern.to_token_stream().to_string(),
        Err(failure) => return format!("#unowned-enum-pattern:{}", failure.symbol()),
    };
    let valid_shape = match (pattern, &variant.fields) {
        (syn::Pat::TupleStruct(pattern), syn::Fields::Unnamed(fields)) => {
            pattern.elems.len() == fields.unnamed.len()
                && !pattern
                    .elems
                    .iter()
                    .any(|pattern| matches!(pattern, syn::Pat::Rest(_)))
        }
        (syn::Pat::Struct(pattern), syn::Fields::Named(fields)) => {
            (pattern.rest.is_some() || pattern.fields.len() == fields.named.len())
                && pattern.fields.iter().all(|field| {
                    fields.named.iter().any(|declared| {
                        declared
                            .ident
                            .as_ref()
                            .is_some_and(|name| *name == compact_rust_tokens(&field.member))
                    })
                })
        }
        (syn::Pat::Path(_) | syn::Pat::Ident(_), syn::Fields::Unit) => true,
        _ => false,
    };
    if !valid_shape {
        return pattern.to_token_stream().to_string();
    }
    if owner == crate::core_ir::rust_options::OWNER {
        let marker = if variant.ident == "Some" {
            crate::core_ir::rust_options::SOME_PATTERN
        } else {
            crate::core_ir::rust_options::NONE_PATTERN
        };
        return format!(
            "{marker}{{{}}}",
            payload
                .into_iter()
                .map(|(field, value)| format!("_{field}:{value}"))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
    let mut fields = vec![format!("__discriminant:{tag}")];
    fields.extend(
        payload
            .into_iter()
            .map(|(field, value)| format!("{}_{field}:{value}", variant.ident)),
    );
    format!("{owner}{{{}}}", fields.join(","))
}

pub(super) fn path_value(path: &syn::Path, call_target: bool) -> Option<Expr> {
    let (owner, variant, _) = match type_aliases::enum_pattern_variant(path) {
        Ok(Some(variant)) => variant,
        Ok(None) => return None,
        Err(failure) => return Some(Expr::Ident(failure.symbol())),
    };
    if owner == crate::core_ir::rust_options::OWNER {
        if variant.ident == "Some" {
            return Some(Expr::Ident(crate::core_ir::rust_options::SOME.into()));
        }
        if !call_target {
            return Some(Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::rust_options::NONE.into())),
                args: vec![],
            });
        }
    }
    if matches!(variant.fields, syn::Fields::Unnamed(_)) {
        return Some(Expr::Ident(format!("{owner}::{}", variant.ident)));
    }
    if call_target || matches!(variant.fields, syn::Fields::Named(_)) {
        return Some(Expr::Ident(
            crate::core_ir::type_proofs::ResolutionFailure::Invalid(
                "enum variant requires its declared constructor syntax".into(),
            )
            .symbol(),
        ));
    }
    Some(Expr::Call {
        callee: Box::new(Expr::Ident(format!("{owner}::{}", variant.ident))),
        args: vec![],
    })
}

pub(super) fn record_value(
    value: &syn::ExprStruct,
    locals: &mut HashMap<String, String>,
) -> Option<Expr> {
    let (owner, variant, _) = type_aliases::enum_variant(&value.path)?;
    let syn::Fields::Named(fields) = &variant.fields else {
        return None;
    };
    let failure = || {
        Expr::Ident(
            crate::core_ir::type_proofs::ResolutionFailure::Invalid(
                "enum record constructor requires each declared field exactly once".into(),
            )
            .symbol(),
        )
    };
    if value.rest.is_some() || value.fields.len() != fields.named.len() {
        return Some(failure());
    }
    let mut body = Vec::new();
    let mut bindings = HashMap::new();
    for (index, field) in value.fields.iter().enumerate() {
        let name = compact_rust_tokens(&field.member);
        let temporary = format!("#enum-field-{index}");
        if bindings.insert(name, temporary.clone()).is_some() {
            return Some(failure());
        }
        body.push(Stmt::Let(
            temporary,
            None,
            lower_expr_with_types(&field.expr, locals),
        ));
    }
    let args = fields
        .named
        .iter()
        .map(|field| {
            let name = field.ident.as_ref()?.to_string();
            Some(Expr::Ident(bindings.get(&name)?.clone()))
        })
        .collect::<Option<Vec<_>>>();
    let Some(args) = args else {
        return Some(failure());
    };
    Some(Expr::Block {
        body,
        result: Some(Box::new(Expr::Call {
            callee: Box::new(Expr::Ident(format!("{owner}::{}", variant.ident))),
            args,
        })),
    })
}
