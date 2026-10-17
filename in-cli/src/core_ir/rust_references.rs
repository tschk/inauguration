//! Transient Rust reference provenance, erased after operator dispatch.
//! The proof identity and the preexisting owned representation are deliberately
//! distinct; this does not introduce a native reference ABI.
use super::{Expr, Typ};
use serde::{Deserialize, Serialize};

const TYPE_PREFIX: &str = "#rust-reference-type:";
pub(crate) const NEGATE_VALUE: &str = "#rust-negate";
pub(crate) const SHARED_VALUE: &str = "#rust-reference-shared";
pub(crate) const MUTABLE_VALUE: &str = "#rust-reference-mutable";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReferenceType {
    pub mutable: bool,
    pub referent: Typ,
    pub runtime: Typ,
}

pub(crate) fn decode(name: &str) -> Option<ReferenceType> {
    serde_json::from_str(name.strip_prefix(TYPE_PREFIX)?).ok()
}

pub(crate) fn typ(reference: ReferenceType) -> Typ {
    Typ::Named(format!(
        "{TYPE_PREFIX}{}",
        serde_json::to_string(&reference).expect("reference type serializes")
    ))
}

pub(crate) fn value_kind(op: &str) -> Option<bool> {
    match op {
        SHARED_VALUE => Some(false),
        MUTABLE_VALUE => Some(true),
        _ => None,
    }
}

pub(crate) fn value_type(op: &str, referent: Typ) -> Option<Typ> {
    let mutable = value_kind(op)?;
    let runtime = erase_type(referent.clone());
    Some(typ(ReferenceType {
        mutable,
        referent,
        runtime,
    }))
}

/// Exact proof identity. Generic referents stay deferred instead of guessing a
/// nominal receiver from an erased runtime representation.
pub(crate) fn receiver_identity(value: &Typ) -> Option<String> {
    match value {
        Typ::Named(name) => {
            if let Some(reference) = decode(name) {
                let inner = receiver_identity(&reference.referent)?;
                Some(format!(
                    "&{}{inner}",
                    if reference.mutable { "mut " } else { "" }
                ))
            } else if name.contains('<')
                || name.contains('>')
                || super::tuples::fields(name).is_some()
            {
                None
            } else {
                Some(name.clone())
            }
        }
        Typ::Generic(_) => None,
        _ => None,
    }
}

/// Source proof equality deliberately ignores the context-specific old runtime
/// representation. It must never equate an owned value and a borrow.
pub(crate) fn same_source_type(left: &Typ, right: &Typ) -> bool {
    match (left, right) {
        (Typ::Named(left), Typ::Named(right)) => match (decode(left), decode(right)) {
            (Some(left), Some(right)) => {
                left.mutable == right.mutable && same_source_type(&left.referent, &right.referent)
            }
            (None, None) => left == right,
            _ => false,
        },
        (Typ::Array(left), Typ::Array(right)) | (Typ::Vector(left), Typ::Vector(right)) => {
            same_source_type(left, right)
        }
        _ => left == right,
    }
}

pub(crate) fn has_reference_proof(typ: &Typ) -> bool {
    matches!(typ, Typ::Named(name) if decode(name).is_some())
}

pub(crate) fn erase_type(value: Typ) -> Typ {
    match value {
        Typ::Named(name) => {
            if let Some(reference) = decode(&name) {
                return erase_type(reference.runtime);
            }
            if let Some(fields) = super::tuples::fields(&name) {
                return super::tuples::typ(
                    &fields
                        .into_iter()
                        .map(|(_, field)| erase_type(field))
                        .collect::<Vec<_>>(),
                );
            }
            Typ::Named(name)
        }
        Typ::Array(item) => Typ::Array(Box::new(erase_type(*item))),
        Typ::Vector(item) => Typ::Vector(Box::new(erase_type(*item))),
        other => other,
    }
}

pub(crate) fn erase_value(value: &mut Expr) {
    if let Expr::Unary { op, .. } = value
        && op == NEGATE_VALUE
    {
        *op = "-".into();
        return;
    }
    if let Expr::Unary { op, expr } = value
        && value_kind(op).is_some()
    {
        *value = expr.as_ref().clone();
    }
}

/// The exact nominal leaf of a textual proof key, without changing its identity.
pub(crate) fn receiver_nominal_identity(key: &str) -> Option<&str> {
    if let Some(inner) = key.strip_prefix("&mut ") {
        return receiver_nominal_identity(inner);
    }
    if let Some(inner) = key.strip_prefix('&') {
        return receiver_nominal_identity(inner);
    }
    (!key.is_empty() && !key.contains('<') && !key.contains('>')).then_some(key)
}

pub(crate) fn qualify_receiver_identity(
    key: &str,
    rename: impl Fn(&str) -> String,
) -> Option<String> {
    fn qualify(key: &str, rename: &impl Fn(&str) -> String) -> Option<String> {
        if let Some(inner) = key.strip_prefix("&mut ") {
            return Some(format!("&mut {}", qualify(inner, rename)?));
        }
        if let Some(inner) = key.strip_prefix('&') {
            return Some(format!("&{}", qualify(inner, rename)?));
        }
        Some(rename(receiver_nominal_identity(key)?))
    }
    qualify(key, &rename)
}

pub(crate) fn erase_declarations(decls: &mut [super::Decl]) {
    crate::core_bindings::rewrite_named_types(decls, &|name| {
        decode(name).map(|reference| erase_type(reference.runtime))
    });
    crate::core_opt::map_decl_exprs_mut(decls, &mut erase_value);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proof_and_runtime_projection_remain_distinct() {
        let row = Typ::Named("module::Row".into());
        let ordinary = typ(ReferenceType {
            mutable: false,
            referent: row.clone(),
            runtime: row.clone(),
        });
        let receiver = typ(ReferenceType {
            mutable: false,
            referent: row,
            runtime: Typ::Named("&module::Row".into()),
        });
        assert_eq!(receiver_identity(&ordinary), Some("&module::Row".into()));
        assert_eq!(receiver_identity(&receiver), Some("&module::Row".into()));
        assert_eq!(ordinary.canonical(), Typ::Named("module::Row".into()));
        assert_eq!(receiver.canonical(), Typ::Named("&module::Row".into()));
        assert_ne!(erase_type(ordinary), erase_type(receiver));
        assert_eq!(
            receiver_identity(&value_type(SHARED_VALUE, Typ::Generic("T".into())).unwrap()),
            None
        );
    }
    #[test]
    fn shared_mutable_nested_and_generic_receiver_identities_remain_exact() {
        let row = Typ::Named("Row".into());
        let shared = value_type(SHARED_VALUE, row.clone()).unwrap();
        let mutable = value_type(MUTABLE_VALUE, row).unwrap();
        let nested = value_type(SHARED_VALUE, mutable.clone()).unwrap();
        assert_eq!(receiver_identity(&shared).as_deref(), Some("&Row"));
        assert_eq!(receiver_identity(&mutable).as_deref(), Some("&mut Row"));
        assert_eq!(receiver_identity(&nested).as_deref(), Some("&&mut Row"));
        assert_eq!(
            receiver_nominal_identity("&&mut module::Row"),
            Some("module::Row")
        );
        assert_eq!(
            qualify_receiver_identity("&&mut Row", |name| format!("module::{name}")),
            Some("&&mut module::Row".into())
        );
        assert!(receiver_identity(&Typ::Named("Row<T>".into())).is_none());
        assert!(receiver_nominal_identity("&Row<T>").is_none());
        assert!(qualify_receiver_identity("&", str::to_string).is_none());
    }

    #[test]
    fn tuple_and_container_erasure_recurses_into_reference_payloads() {
        let reference = value_type(SHARED_VALUE, Typ::Named("Row".into())).unwrap();
        let tuple =
            super::super::tuples::typ(&[reference.clone(), Typ::Array(Box::new(reference))]);
        let erased = erase_type(tuple);
        assert_eq!(
            erased,
            super::super::tuples::typ(&[
                Typ::Named("Row".into()),
                Typ::Array(Box::new(Typ::Named("Row".into())))
            ])
        );
    }
    #[test]
    fn declaration_erasure_preserves_effects_and_nested_closure_types() {
        use super::super::{Decl, Stmt};
        let row = Typ::Named("Row".into());
        let effect = || Expr::Call {
            callee: Box::new(Expr::Ident("effect".into())),
            args: vec![],
        };
        let original = vec![
            Decl::Global {
                name: "global".into(),
                typ: row.clone(),
                init: Some(Box::new(effect())),
                mutable: true,
                constant_value: false,
            },
            Decl::Function {
                name: "read".into(),
                params: vec![("value".into(), row.clone())],
                ret: row.clone(),
                type_params: vec![],
                body: vec![
                    Stmt::Expr(Expr::Closure {
                        params: vec![("inner".into(), row.clone())],
                        ret: row.clone(),
                        body: vec![
                            Stmt::Let("local".into(), Some(row.clone()), effect()),
                            Stmt::Return(Some(Expr::Ident("local".into()))),
                        ],
                        captures: vec![],
                    }),
                    Stmt::Return(Some(effect())),
                ],
            },
        ];
        let mut decorated = original.clone();
        crate::core_bindings::rewrite_named_types(&mut decorated, &|name| {
            (name == "Row").then(|| value_type(SHARED_VALUE, row.clone()).unwrap())
        });
        crate::core_opt::map_decl_exprs_mut(&mut decorated, &mut |value| {
            if matches!(value, Expr::Call { .. }) {
                *value = Expr::Unary {
                    op: SHARED_VALUE.into(),
                    expr: Box::new(Expr::Unary {
                        op: MUTABLE_VALUE.into(),
                        expr: Box::new(value.clone()),
                    }),
                };
            }
        });
        assert_ne!(decorated, original);
        erase_declarations(&mut decorated);
        assert_eq!(decorated, original);
    }
}
