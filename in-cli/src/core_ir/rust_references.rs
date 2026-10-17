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

pub(crate) fn erase_declarations(decls: &mut Vec<super::Decl>) {
    // Distinct source proof tuples can share one old runtime storage shape.
    // Remember only canonical generated schemas; malformed/user-generic tuple
    // declarations must retain their conflicts for the verifier to reject.
    let originals: Vec<_> = decls
        .iter()
        .map(|decl| match decl {
            super::Decl::Struct {
                name,
                fields,
                type_params,
            } => super::tuples::fields(name)
                .map(|expected| (name.clone(), type_params.is_empty() && &expected == fields)),
            _ => None,
        })
        .collect();
    crate::core_bindings::rewrite_named_types(decls, &|name| {
        decode(name).map(|reference| erase_type(reference.runtime))
    });
    crate::core_opt::map_decl_exprs_mut(decls, &mut erase_value);
    for (decl, original) in decls.iter_mut().zip(&originals) {
        if let Some((original_name, canonical)) = original
            && let super::Decl::Struct { name, fields, .. } = decl
            && let Typ::Named(runtime_name) = erase_type(Typ::Named(original_name.clone()))
        {
            // All uses rewrite from the original encoded identity. A malformed
            // schema must not acquire a different identity from its bad fields.
            *name = runtime_name;
            if *canonical && let Some(runtime_fields) = super::tuples::fields(name) {
                // Restore the old canonical tuple storage under erased leaves.
                *fields = runtime_fields;
            }
        }
    }
    let mut seen: std::collections::HashMap<super::Decl, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    let mut index = 0;
    decls.retain(|decl| {
        let original = &originals[index];
        index += 1;
        let Some((original, true)) = original else {
            return true;
        };
        let origins = seen.entry(decl.clone()).or_default();
        let first = origins.is_empty();
        let repeated_original = !origins.insert(original.clone());
        // Do not hide preexisting duplicate declarations, even when an earlier
        // occurrence of that same original was merged into another shape.
        first || repeated_original
    });
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

#[cfg(test)]
mod tuple_erasure_tests {
    use super::*;
    use crate::core_ir::{Decl, Stmt, UnifiedModule};
    fn named(value: &Typ) -> String {
        let Typ::Named(name) = value else {
            panic!("expected tuple")
        };
        name.clone()
    }
    fn schema(value: &Typ) -> Decl {
        let name = named(value);
        Decl::Struct {
            name: name.clone(),
            fields: super::super::tuples::fields(&name).unwrap(),
            type_params: vec![],
        }
    }
    fn tuple_value(value: &Typ, first: Expr) -> Expr {
        Expr::StructInit {
            name: named(value),
            fields: vec![("_0".into(), first), ("_1".into(), Expr::IntLit(0))],
        }
    }
    #[test]
    fn erasure_keeps_nested_tuple_schemas_constructors_aliases_params_and_returns_consistent() {
        let row = Typ::Named("Row".into());
        let borrowed = value_type(SHARED_VALUE, row.clone()).unwrap();
        let proof = super::super::tuples::typ(&[borrowed, Typ::Int]);
        let runtime = erase_type(proof.clone());
        assert_ne!(proof, runtime);
        let nested = super::super::tuples::typ(&[
            Typ::Array(Box::new(proof.clone())),
            Typ::Vector(Box::new(proof.clone())),
        ]);
        let nested_runtime = erase_type(nested.clone());
        let mut declarations = vec![
            Decl::Struct {
                name: "Row".into(),
                fields: vec![("x".into(), Typ::Int)],
                type_params: vec![],
            },
            schema(&proof),
            schema(&runtime),
            schema(&nested),
            schema(&nested_runtime),
            Decl::Struct {
                name: "Alias".into(),
                fields: vec![
                    ("tuple".into(), proof.clone()),
                    ("nested".into(), nested.clone()),
                ],
                type_params: vec![],
            },
            Decl::Global {
                name: "pair".into(),
                typ: proof.clone(),
                init: Some(Box::new(tuple_value(
                    &proof,
                    Expr::StructInit {
                        name: "Row".into(),
                        fields: vec![("x".into(), Expr::IntLit(42))],
                    },
                ))),
                mutable: false,
                constant_value: false,
            },
            Decl::Function {
                name: "roundtrip".into(),
                params: vec![("value".into(), proof.clone())],
                ret: proof.clone(),
                body: vec![
                    Stmt::Let("local".into(), Some(proof), Expr::Ident("value".into())),
                    Stmt::Return(Some(Expr::Ident("local".into()))),
                ],
                type_params: vec![],
            },
        ];
        erase_declarations(&mut declarations);
        assert_eq!(
            declarations
                .iter()
                .filter(|decl| matches!(decl,Decl::Struct{name,..}if name==&named(&runtime)))
                .count(),
            1
        );
        assert_eq!(
            declarations
                .iter()
                .filter(|decl| matches!(decl,Decl::Struct{name,..}if name==&named(&nested_runtime)))
                .count(),
            1
        );
        let Decl::Struct { fields, .. } = declarations
            .iter()
            .find(|decl| matches!(decl,Decl::Struct{name,..}if name=="Alias"))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(
            fields,
            &vec![
                ("tuple".into(), runtime.clone()),
                ("nested".into(), nested_runtime)
            ]
        );
        let Decl::Global {
            typ,
            init: Some(init),
            ..
        } = declarations
            .iter()
            .find(|decl| matches!(decl, Decl::Global { .. }))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(typ, &runtime);
        assert!(matches!(init.as_ref(),Expr::StructInit{name,..}if name==&named(&runtime)));
        let Decl::Function {
            params, ret, body, ..
        } = declarations
            .iter()
            .find(|decl| matches!(decl, Decl::Function { .. }))
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(params[0].1, runtime);
        assert_eq!(ret, typ);
        assert!(matches!(&body[0],Stmt::Let(_,Some(typ),_)if typ==ret));
        let once = declarations.clone();
        erase_declarations(&mut declarations);
        assert_eq!(declarations, once);
        let module = UnifiedModule::new(declarations);
        let report = crate::core_ir_verifier::verify_module(
            &module,
            &crate::core_ir_verifier::VerifyOptions {
                entry: None,
                require_entry: false,
            },
        );
        assert!(report.ok, "{report:?}");
    }
    #[test]
    fn erasure_preserves_repeated_original_and_conflicting_tuple_declarations() {
        let row = Typ::Named("Row".into());
        let proof = super::super::tuples::typ(&[value_type(SHARED_VALUE, row).unwrap(), Typ::Int]);
        let runtime = erase_type(proof.clone());
        // B,A,A must not hide the duplicate A merely because its first occurrence merged.
        let mut declarations = vec![schema(&runtime), schema(&proof), schema(&proof)];
        erase_declarations(&mut declarations);
        assert_eq!(declarations.len(), 2);
        let mut malformed = schema(&proof);
        let Decl::Struct { fields, .. } = &mut malformed else {
            panic!()
        };
        fields[0].1 = Typ::String;
        let mut declarations = vec![schema(&proof), malformed];
        erase_declarations(&mut declarations);
        assert_eq!(declarations.len(), 2);
        let module = UnifiedModule::new(declarations);
        let report = crate::core_ir_verifier::verify_module(
            &module,
            &crate::core_ir_verifier::VerifyOptions {
                entry: None,
                require_entry: false,
            },
        );
        assert!(!report.ok);
        assert_eq!(
            report.reason_code.as_deref(),
            Some("duplicate-top-level-name")
        );
        let mut generic = schema(&proof);
        let Decl::Struct { type_params, .. } = &mut generic else {
            panic!()
        };
        type_params.push("T".into());
        let mut declarations = vec![schema(&proof), generic];
        erase_declarations(&mut declarations);
        assert_eq!(declarations.len(), 2);
    }
    #[test]
    fn erased_reference_integer_leaves_return_to_canonical_tuple_storage() {
        let integer = Typ::Named("#rust-integer-i32".into());
        let proof =
            super::super::tuples::typ(&[value_type(SHARED_VALUE, integer).unwrap(), Typ::Int]);
        let runtime = super::super::tuples::typ(&[Typ::Int, Typ::Int]);
        let mut declarations = vec![schema(&proof), schema(&runtime)];
        erase_declarations(&mut declarations);
        assert_eq!(declarations, vec![schema(&runtime)]);
        let module = UnifiedModule::new(declarations);
        let report = crate::core_ir_verifier::verify_module(
            &module,
            &crate::core_ir_verifier::VerifyOptions {
                entry: None,
                require_entry: false,
            },
        );
        assert!(report.ok, "{report:?}");
    }
}
