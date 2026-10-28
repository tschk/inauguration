//! Rust reference provenance and the owned aggregate address ABI.
//! Scalar views keep their existing representation; aggregate borrows retain
//! their reference identity through native lowering instead of copying storage.
use super::{Expr, Typ};
use serde::{Deserialize, Serialize};

const TYPE_PREFIX: &str = "#rust-reference-type:";
const ADDRESS_PREFIX: &str = "#rust-reference-address:";
pub(crate) const NEGATE_VALUE: &str = "#rust-negate";
pub(crate) const SHARED_VALUE: &str = "#rust-reference-shared";
pub(crate) const MUTABLE_VALUE: &str = "#rust-reference-mutable";
const SHARED_ADDRESS: &str = "#rust-address-shared";
const MUTABLE_ADDRESS: &str = "#rust-address-mutable";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReferenceType {
    pub mutable: bool,
    pub referent: Typ,
    pub runtime: Typ,
}

pub(crate) fn decode(name: &str) -> Option<ReferenceType> {
    serde_json::from_str(
        name.strip_prefix(TYPE_PREFIX)
            .or_else(|| name.strip_prefix(ADDRESS_PREFIX))?,
    )
    .ok()
}

pub(crate) fn typ(reference: ReferenceType) -> Typ {
    Typ::Named(format!(
        "{TYPE_PREFIX}{}",
        serde_json::to_string(&reference).expect("reference type serializes")
    ))
}

pub(crate) fn rebuild(name: &str, reference: ReferenceType) -> Typ {
    if name.starts_with(ADDRESS_PREFIX) {
        address_type(reference)
    } else {
        typ(reference)
    }
}

fn address_type(reference: ReferenceType) -> Typ {
    Typ::Named(format!(
        "{ADDRESS_PREFIX}{}",
        serde_json::to_string(&reference).expect("reference type serializes")
    ))
}

pub(crate) fn aggregate_referent(value: &Typ) -> Option<Typ> {
    let Typ::Named(name) = value else { return None };
    if let Some(payload) = super::rust_boxes::address_payload(name) {
        let referent = payload.canonical();
        return matches!(referent, Typ::Named(_)).then_some(referent);
    }
    let reference = decode(name)?;
    let runtime = reference.runtime.canonical();
    (matches!(runtime, Typ::Named(_)) && crate::core_atomic::Kind::from_type(&runtime).is_none())
        .then_some(runtime)
}

pub(crate) fn dereferenced(value: &Typ) -> Typ {
    aggregate_referent(value).unwrap_or_else(|| value.canonical())
}

pub(crate) fn value_kind(op: &str) -> Option<bool> {
    match op {
        SHARED_VALUE | SHARED_ADDRESS => Some(false),
        MUTABLE_VALUE | MUTABLE_ADDRESS => Some(true),
        _ => None,
    }
}

pub(crate) fn value_type(op: &str, referent: Typ) -> Option<Typ> {
    let mutable = value_kind(op)?;
    let runtime = erase_type(referent.clone());
    if matches!(op, SHARED_ADDRESS | MUTABLE_ADDRESS) {
        return Some(erase_type(typ(ReferenceType {
            mutable,
            referent,
            runtime,
        })));
    }
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
    if let (Typ::Named(left), Typ::Named(right)) = (left, right)
        && (super::rust_callables::is_encoded(left) || super::rust_callables::is_encoded(right))
    {
        let (left, right) = match (
            super::rust_callables::decode_item(left),
            super::rust_callables::decode_item(right),
        ) {
            (Some(left), Some(right)) if left.identity == right.identity => {
                (left.signature, right.signature)
            }
            (None, None) => {
                let (Some(left), Some(right)) = (
                    super::rust_callables::decode(left),
                    super::rust_callables::decode(right),
                ) else {
                    return false;
                };
                (left, right)
            }
            _ => return false,
        };
        return left.abi == right.abi
            && left.unsafe_call == right.unsafe_call
            && left.variadic == right.variadic
            && left.bound_lifetimes == right.bound_lifetimes
            && left.parameters.len() == right.parameters.len()
            && left
                .parameters
                .iter()
                .zip(&right.parameters)
                .all(|(left, right)| same_source_type(left, right))
            && same_source_type(&left.result, &right.result);
    }
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
                if aggregate_referent(&Typ::Named(name.clone())).is_some() {
                    return address_type(ReferenceType {
                        mutable: reference.mutable,
                        referent: erase_type(reference.referent),
                        runtime: erase_type(reference.runtime),
                    });
                }
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
    if let Expr::Unary { op, .. } = value {
        match op.as_str() {
            NEGATE_VALUE => *op = "-".into(),
            SHARED_VALUE => *op = SHARED_ADDRESS.into(),
            MUTABLE_VALUE => *op = MUTABLE_ADDRESS.into(),
            _ => {}
        }
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
    super::tuples::project_declarations(decls, |decls| {
        crate::core_bindings::rewrite_named_types(decls, &|name| {
            decode(name).map(|_| erase_type(Typ::Named(name.into())))
        });
        crate::core_opt::map_decl_exprs_mut(decls, &mut erase_value);
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
        let address = address_type(ReferenceType {
            mutable: false,
            referent: Typ::Named("Row".into()),
            runtime: Typ::Named("Row".into()),
        });
        assert_eq!(
            erased,
            super::super::tuples::typ(&[address.clone(), Typ::Array(Box::new(address))])
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
        let once = decorated.clone();
        erase_declarations(&mut decorated);
        assert_eq!(decorated, once);
        crate::core_bindings::rewrite_named_types(&mut decorated, &|name| {
            decode(name).map(|reference| reference.runtime)
        });
        crate::core_opt::map_decl_exprs_mut(&mut decorated, &mut |value| {
            if let Expr::Unary { op, expr } = value
                && matches!(op.as_str(), SHARED_ADDRESS | MUTABLE_ADDRESS)
            {
                *value = (**expr).clone();
            }
        });
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
    fn erased_references_preserve_integer_proof_until_integer_normalization() {
        let integer = Typ::Named("#rust-integer-i32".into());
        let proof = super::super::tuples::typ(&[
            value_type(SHARED_VALUE, integer.clone()).unwrap(),
            Typ::Int,
        ]);
        let integer_proof = super::super::tuples::typ(&[integer, Typ::Int]);
        let runtime = super::super::tuples::typ(&[Typ::Int, Typ::Int]);
        let mut declarations = vec![schema(&proof), schema(&runtime)];
        erase_declarations(&mut declarations);
        assert_eq!(declarations, vec![schema(&integer_proof), schema(&runtime)]);
        let mut module = UnifiedModule::new(declarations);
        crate::typecheck::rust_integers::normalize(
            &crate::typecheck::TypeChecker::new(),
            &mut module,
            crate::integer_semantics::IntegerSemantics::Word64,
        );
        assert_eq!(module.decls, vec![schema(&runtime)]);
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

#[cfg(test)]
mod declaration_reference_tests {
    use super::*;
    #[test]
    fn declaration_reference_annotation_is_erased_through_existing_type_walk() {
        let reference = value_type(SHARED_VALUE, Typ::String).unwrap();
        let mut decls = vec![crate::core_ir::Decl::Function {
            name: "work".into(),
            params: vec![],
            ret: Typ::Void,
            type_params: vec![],
            body: vec![crate::core_ir::Stmt::Declare("text".into(), reference)],
        }];
        erase_declarations(&mut decls);
        let crate::core_ir::Decl::Function { body, .. } = &decls[0] else {
            panic!()
        };
        assert!(matches!(
            &body[0],
            crate::core_ir::Stmt::Declare(_, Typ::String)
        ));
    }
}

#[cfg(test)]
mod callable_source_equality_tests {
    use super::*;
    #[test]
    fn callable_source_equality_preserves_children_and_ignores_only_reference_runtime() {
        let integer = Typ::Named("#rust-integer-i64".into());
        let callback = |runtime| {
            super::super::rust_callables::typ(super::super::rust_callables::Signature {
                abi: super::super::rust_callables::Abi::Rust,
                unsafe_call: false,
                variadic: false,
                bound_lifetimes: false,
                parameters: vec![typ(ReferenceType {
                    mutable: false,
                    referent: integer.clone(),
                    runtime,
                })],
                result: integer.clone(),
            })
        };
        assert!(same_source_type(
            &callback(Typ::Int),
            &callback(Typ::Named("old-pointer".into()))
        ));
        let Typ::Named(name) = callback(Typ::Int) else {
            unreachable!()
        };
        let mut different = super::super::rust_callables::decode(&name).unwrap();
        different.result = Typ::Named("#rust-integer-u64".into());
        assert!(!same_source_type(
            &Typ::Named(name),
            &super::super::rust_callables::typ(different)
        ));
    }
}

#[cfg(test)]
mod callable_item_source_equality_tests {
    use super::*;
    use crate::core_ir::rust_callables::{self, Abi, Item, Signature};
    #[test]
    fn items_compare_origin_and_exact_source_children_without_runtime_fields() {
        let signature = |runtime| Signature {
            abi: Abi::Rust,
            unsafe_call: false,
            variadic: false,
            bound_lifetimes: false,
            parameters: vec![typ(ReferenceType {
                mutable: false,
                referent: Typ::Named("#rust-integer-i64".into()),
                runtime,
            })],
            result: Typ::Bool,
        };
        let item = |identity: &str, runtime| {
            rust_callables::item_type(Item {
                identity: identity.into(),
                signature: signature(runtime),
            })
        };
        assert!(same_source_type(
            &item("owner::f", Typ::Int),
            &item("owner::f", Typ::Bool)
        ));
        assert!(!same_source_type(
            &item("owner::f", Typ::Int),
            &item("owner::g", Typ::Int)
        ));
        assert!(!same_source_type(
            &item("owner::f", Typ::Int),
            &rust_callables::typ(signature(Typ::Int))
        ));
    }
}
