//! Fixed-array source facts retained until Rust value transfers are resolved.
use super::{Expr, Typ};

const TYPE_PREFIX: &str = "#rust-fixed-array:";
const VALUE_PREFIX: &str = "#rust-fixed-array-value:";
pub(crate) const PENDING_REPEAT: &str = "#rust-pending-array-repeat";

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FixedArray {
    pub element: Typ,
    pub length: usize,
}

pub(crate) fn typ(element: Typ, length: usize) -> Typ {
    Typ::Named(format!(
        "{TYPE_PREFIX}{}",
        serde_json::to_string(&FixedArray { element, length }).expect("array type serializes")
    ))
}

pub(crate) fn decode(name: &str) -> Option<FixedArray> {
    serde_json::from_str(name.strip_prefix(TYPE_PREFIX)?).ok()
}

pub(crate) fn value(expression: Expr, length: usize) -> Expr {
    Expr::Unary {
        op: format!("{VALUE_PREFIX}{length}"),
        expr: Box::new(expression),
    }
}

pub(crate) fn value_length(operation: &str) -> Option<usize> {
    let digits = operation.strip_prefix(VALUE_PREFIX)?;
    let length: usize = digits.parse().ok()?;
    (digits == length.to_string()).then_some(length)
}

/// Reuse the shared immutable metadata traversal without cloning ordinary modules.
pub(crate) fn has_metadata(module: &super::UnifiedModule) -> bool {
    super::type_proofs::metadata_present(
        &module.decls,
        super::type_proofs::MetadataKind::RustFixedArrays,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_array_facts_preserve_exact_element_identity_and_length() {
        let element = Typ::Named("#rust-integer-u8".into());
        let Typ::Named(name) = typ(element.clone(), 2) else {
            unreachable!()
        };
        assert_eq!(decode(&name), Some(FixedArray { element, length: 2 }));
        assert!(decode("Vec<u8>").is_none());
        let Expr::Unary { op, expr } = value(Expr::ArrayLit(vec![]), 0) else {
            unreachable!()
        };
        assert_eq!(value_length(&op), Some(0));
        assert_eq!(*expr, Expr::ArrayLit(vec![]));
        assert!(value_length("#rust-fixed-array-value:-1").is_none());
        assert!(value_length("#rust-fixed-array-value:01").is_none());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::core_ir::{
        Decl, Stmt, UnifiedModule,
        type_proofs::{self, ResolutionFailure},
    };
    #[test]
    fn array_elements_share_namespace_substitution_and_recursive_proofs() {
        let mut decls = vec![Decl::Function {
            name: "take".into(),
            params: vec![("value".into(), typ(Typ::Generic("T".into()), 3))],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![],
        }];
        let invalid = ResolutionFailure::Invalid("element cycle".into());
        crate::core_bindings::substitute_type_parameters(
            &mut decls[0],
            &std::collections::HashMap::from([("T".into(), invalid.clone().typ())]),
        );
        let Decl::Function { params, .. } = &decls[0] else {
            panic!()
        };
        assert_eq!(type_proofs::failure_in_type(&params[0].1), Some(invalid));
        assert!(type_proofs::present(&decls));
        assert!(has_metadata(&UnifiedModule::new(decls)));
        let plain = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
        }]);
        assert!(!has_metadata(&plain));
        let mut value_only = plain;
        if let Decl::Function { body, .. } = &mut value_only.decls[0] {
            body.push(Stmt::Expr(value(Expr::ArrayLit(vec![]), 0)));
        }
        assert!(has_metadata(&value_only));
        let source = typ(Typ::Named("#rust-integer-u8".into()), 3);
        assert_eq!(source.inference_type(), source);
        assert_eq!(source.canonical(), Typ::Array(Box::new(Typ::Int)));
    }
}

#[cfg(test)]
mod immutable_presence_tests {
    use super::*;
    use crate::core_ir::{Decl, MatchArm, MethodSig, Stmt, UnifiedModule, Visibility};

    fn function(body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body,
            type_params: vec![],
        }
    }
    fn expression(value: Expr) -> Decl {
        function(vec![Stmt::Expr(value)])
    }

    #[test]
    fn shared_scan_finds_array_facts_in_schemas_closures_patterns_and_generic_arguments() {
        let fixed = typ(Typ::Int, 2);
        let reference =
            crate::core_ir::rust_references::typ(crate::core_ir::rust_references::ReferenceType {
                mutable: false,
                referent: fixed.clone(),
                runtime: Typ::Int,
            });
        // Reference proof keeps its exact referent inside the encoded tuple shape.
        let tuple = crate::core_ir::tuples::typ(std::slice::from_ref(&reference));
        let Typ::Named(pattern_type) = &tuple else {
            panic!("tuple")
        };
        let pattern = format!("{pattern_type}{{}}");
        assert!(crate::core_ir::MatchPattern::parse(&pattern).is_ok());
        let cases = vec![
            Decl::Global {
                name: "initializer".into(),
                typ: Typ::Int,
                init: Some(Box::new(value(Expr::ArrayLit(vec![]), 0))),
                mutable: false,
                constant_value: false,
            },
            Decl::Global {
                name: "items".into(),
                typ: fixed.clone(),
                init: None,
                mutable: false,
                constant_value: false,
            },
            Decl::Struct {
                name: "Bag".into(),
                fields: vec![("items".into(), fixed.clone())],
                type_params: vec![],
            },
            Decl::Class {
                name: "Bag".into(),
                fields: vec![("items".into(), tuple.clone())],
                methods: vec![],
                visibility: Visibility::Pub,
                extends: None,
                implements: vec![],
                type_params: vec![],
            },
            Decl::Class {
                name: "Methods".into(),
                fields: vec![],
                methods: vec![function(vec![Stmt::Declare("items".into(), fixed.clone())])],
                visibility: Visibility::Pub,
                extends: None,
                implements: vec![],
                type_params: vec![],
            },
            Decl::Interface {
                name: "Factory".into(),
                methods: vec![MethodSig {
                    name: "make".into(),
                    params: vec![],
                    ret: fixed.clone(),
                }],
                visibility: Visibility::Pub,
                type_params: vec![],
            },
            expression(Expr::Closure {
                params: vec![],
                ret: fixed.clone(),
                body: vec![],
                captures: vec![],
            }),
            expression(Expr::Closure {
                params: vec![("items".into(), reference)],
                ret: Typ::Void,
                body: vec![],
                captures: vec![],
            }),
            expression(Expr::Closure {
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Declare("items".into(), fixed)],
                captures: vec![],
            }),
            function(vec![Stmt::Match {
                scrutinee: Expr::IntLit(0),
                arms: vec![MatchArm {
                    pattern,
                    guard: None,
                    body: vec![],
                }],
            }]),
            function(vec![Stmt::Match {
                scrutinee: Expr::BoolLit(true),
                arms: vec![MatchArm {
                    pattern: "true".into(),
                    guard: Some(value(Expr::ArrayLit(vec![]), 0)),
                    body: vec![],
                }],
            }]),
            expression(Expr::Unary {
                op: PENDING_REPEAT.into(),
                expr: Box::new(Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::ARRAY_REPEAT.into())),
                    args: vec![Expr::IntLit(42), Expr::Ident("external_count".into())],
                }),
            }),
            expression(Expr::Unary {
                op: crate::core_ir::rust_generics::arguments(&[Typ::Vector(Box::new(tuple))]),
                expr: Box::new(Expr::Ident("callable".into())),
            }),
        ];
        for (index, decl) in cases.into_iter().enumerate() {
            let module = UnifiedModule::new(vec![decl]);
            let before = module.clone();
            assert!(has_metadata(&module), "case {index}: {module:?}");
            assert!(
                !crate::core_ir::type_proofs::present(&module.decls),
                "ordinary array metadata is not a type failure: {index}"
            );
            assert_eq!(module, before);
        }
    }

    #[test]
    fn presence_modes_keep_failures_and_ordinary_source_values_distinct() {
        let failure = crate::core_ir::type_proofs::ResolutionFailure::Invalid("cycle".into()).typ();
        let failure_only = UnifiedModule::new(vec![expression(Expr::Unary {
            op: crate::core_ir::rust_generics::arguments(&[failure]),
            expr: Box::new(Expr::Ident("callable".into())),
        })]);
        assert!(crate::core_ir::type_proofs::present(&failure_only.decls));
        assert!(!has_metadata(&failure_only));
        let plain = UnifiedModule::new(vec![function(vec![
            Stmt::Declare("items".into(), Typ::Array(Box::new(Typ::Int))),
            Stmt::Expr(Expr::StringLit(format!("{TYPE_PREFIX}{VALUE_PREFIX}0"))),
            Stmt::Match {
                scrutinee: Expr::BoolLit(true),
                arms: vec![MatchArm {
                    pattern: "true".into(),
                    guard: None,
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                }],
            },
        ])]);
        assert!(!has_metadata(&plain));
        assert!(!crate::core_ir::type_proofs::present(&plain.decls));
    }
}
