//! Transient explicit Rust type arguments, resolved before executable lowering.
use super::Typ;
const PREFIX: &str = "#rust-generic-arguments:";
const INFERRED_PREFIX: &str = "#rust-inferred-generic-arguments:";

pub(crate) fn arguments(types: &[Typ]) -> String {
    format!(
        "{PREFIX}{}",
        serde_json::to_string(types).expect("type metadata serializes")
    )
}

pub(crate) fn decode_arguments(op: &str) -> Option<Vec<Typ>> {
    serde_json::from_str(
        op.strip_prefix(PREFIX)
            .or_else(|| op.strip_prefix(INFERRED_PREFIX))?,
    )
    .ok()
}

pub(crate) fn inferred_arguments(types: &[Typ]) -> String {
    format!(
        "{INFERRED_PREFIX}{}",
        serde_json::to_string(types).expect("type metadata serializes")
    )
}

pub(crate) fn is_inferred(op: &str) -> bool {
    op.starts_with(INFERRED_PREFIX)
}

pub(crate) const TEMPLATE: &str = "#rust-generic-template";
pub(crate) const CONSTRAINTS: &str = "#rust-generic-constraints";
pub(crate) const ALIAS: &str = "#rust-generic-alias";
const EXPLICIT_ARITY: &str = "#rust-explicit-generic-arity:";
pub(crate) fn explicit_arity(count: usize) -> String {
    format!("{EXPLICIT_ARITY}{count}")
}
pub(crate) fn decode_explicit_arity(op: &str) -> Option<usize> {
    op.strip_prefix(EXPLICIT_ARITY)?.parse().ok()
}
// Source-owned, transient proofs. These retain exact Types until the merged
// generic phase; no runtime ABI or public Core IR schema changes are involved.
const NEG_BOUNDS: &str = "#rust-generic-neg-bounds:";
const NEG_IMPLEMENTATION: &str = "#rust-neg-implementation:";
const CALLABLE_BOUNDS: &str = "#rust-generic-callable-bounds:";
const TRAIT_BOUNDS: &str = "#rust-generic-source-trait-bounds:";
/// A closed source query proves these exact receivers implement the bound.
/// Missing receivers remain unproved, not negative evidence about other crates.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TraitObligation {
    pub receiver: Typ,
    pub implementations: Vec<Typ>,
}
pub(crate) fn trait_bounds(values: &[TraitObligation]) -> String {
    format!(
        "{TRAIT_BOUNDS}{}",
        serde_json::to_string(values).expect("type metadata serializes")
    )
}
pub(crate) fn decode_trait_bounds(op: &str) -> Option<Vec<TraitObligation>> {
    serde_json::from_str(op.strip_prefix(TRAIT_BOUNDS)?).ok()
}
pub(crate) fn is_trait_bounds(op: &str) -> bool {
    op.starts_with(TRAIT_BOUNDS)
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CallableObligation {
    pub receiver: Typ,
    pub signature: super::rust_callables::Signature,
}
pub(crate) fn callable_bounds(values: &[CallableObligation]) -> String {
    format!(
        "{CALLABLE_BOUNDS}{}",
        serde_json::to_string(values).expect("type metadata serializes")
    )
}
pub(crate) fn decode_callable_bounds(op: &str) -> Option<Vec<CallableObligation>> {
    serde_json::from_str(op.strip_prefix(CALLABLE_BOUNDS)?).ok()
}
pub(crate) fn is_callable_bounds(op: &str) -> bool {
    op.starts_with(CALLABLE_BOUNDS)
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct NegObligation {
    pub receiver: Typ,
    pub output: Option<Typ>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct NegImplementation {
    pub receiver: Typ,
    pub output: Typ,
}
pub(crate) fn neg_bounds(values: &[NegObligation]) -> String {
    format!(
        "{NEG_BOUNDS}{}",
        serde_json::to_string(values).expect("type metadata serializes")
    )
}
pub(crate) fn decode_neg_bounds(op: &str) -> Option<Vec<NegObligation>> {
    serde_json::from_str(op.strip_prefix(NEG_BOUNDS)?).ok()
}
pub(crate) fn is_neg_bounds(op: &str) -> bool {
    op.starts_with(NEG_BOUNDS)
}
pub(crate) fn neg_implementation(value: &NegImplementation) -> String {
    format!(
        "{NEG_IMPLEMENTATION}{}",
        serde_json::to_string(value).expect("type metadata serializes")
    )
}
pub(crate) fn decode_neg_implementation(op: &str) -> Option<NegImplementation> {
    serde_json::from_str(op.strip_prefix(NEG_IMPLEMENTATION)?).ok()
}
pub(crate) fn is_neg_implementation(op: &str) -> bool {
    op.starts_with(NEG_IMPLEMENTATION)
}
const RIGID_PREFIX: &str = "#rust-rigid-type:";
pub(crate) fn rigid_type(parameter: &str) -> Typ {
    Typ::Named(format!("{RIGID_PREFIX}{parameter}"))
}
pub(crate) fn is_rigid_type(value: &Typ) -> bool {
    matches!(value, Typ::Named(name) if name.starts_with(RIGID_PREFIX))
}

pub(crate) fn argument_failure(types: &[Typ]) -> Option<super::type_proofs::ResolutionFailure> {
    types.iter().fold(None, |failure, typ| {
        super::type_proofs::prefer_failure(failure, super::type_proofs::failure_in_type(typ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{
        Decl, Expr, Stmt,
        type_proofs::{self, ResolutionFailure},
    };
    #[test]
    fn neg_metadata_preserves_exact_source_types_and_rejects_malformed_payloads() {
        let receiver = Typ::Generic("T".into());
        let obligations = vec![NegObligation {
            receiver,
            output: Some(Typ::Named("#rust-integer-usize".into())),
        }];
        assert_eq!(
            decode_neg_bounds(&neg_bounds(&obligations)),
            Some(obligations)
        );
        let implementation = NegImplementation {
            receiver: Typ::Named("Row".into()),
            output: Typ::Named("#rust-integer-u64".into()),
        };
        assert_eq!(
            decode_neg_implementation(&neg_implementation(&implementation)),
            Some(implementation)
        );
        assert!(is_neg_bounds("#rust-generic-neg-bounds:malformed"));
        assert!(decode_neg_bounds("#rust-generic-neg-bounds:malformed").is_none());
        assert!(decode_neg_implementation("#rust-neg-implementation:malformed").is_none());
    }
    #[test]
    fn unused_neg_output_metadata_retains_nested_alias_cycle_proof() {
        let invalid = ResolutionFailure::Invalid("cyclic type alias Output".into());
        let nested =
            super::super::rust_references::typ(super::super::rust_references::ReferenceType {
                mutable: false,
                referent: Typ::Array(Box::new(invalid.clone().typ())),
                runtime: Typ::Int,
            });
        for op in [
            neg_bounds(&[NegObligation {
                receiver: Typ::Generic("T".into()),
                output: Some(nested.clone()),
            }]),
            neg_implementation(&NegImplementation {
                receiver: Typ::Named("Row".into()),
                output: nested,
            }),
        ] {
            let mut module = super::super::UnifiedModule::new(vec![Decl::Function {
                name: "unused".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![
                    Stmt::Expr(Expr::Unary {
                        op,
                        expr: Box::new(Expr::UnitLit),
                    }),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            }]);
            module.decls.push(Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            });
            assert!(type_proofs::metadata_present(
                &module.decls,
                type_proofs::MetadataKind::TypeFailures
            ));
            assert_eq!(
                crate::typecheck::TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .get("unused"),
                Some(&invalid)
            );
            let normalized =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
            let proofs = crate::typecheck::TypeChecker::new().typed_proof_nodes(&normalized.decls);
            assert_eq!(
                proofs.get("unused"),
                Some(&invalid),
                "cleanup must retain the preferred source failure: {normalized:#?}"
            );
            assert!(
                !proofs.contains_key("main"),
                "an unrelated entry must not inherit an unused helper's proof"
            );
        }
    }
    #[test]
    fn unused_explicit_arguments_retain_nested_invalid_proofs() {
        let invalid = ResolutionFailure::Invalid("cyclic alias A".into());
        let unsupported = ResolutionFailure::Unsupported("unknown projection".into());
        let nested =
            super::super::rust_references::typ(super::super::rust_references::ReferenceType {
                mutable: false,
                referent: Typ::Array(Box::new(invalid.clone().typ())),
                runtime: Typ::Int,
            });
        let tuple =
            super::super::tuples::typ(&[Typ::Vector(Box::new(nested)), unsupported.clone().typ()]);
        assert_eq!(
            argument_failure(&[unsupported.typ(), tuple.clone()]),
            Some(invalid.clone())
        );
        let function = Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Unary {
                    op: arguments(&[tuple]),
                    expr: Box::new(Expr::Ident("choose".into())),
                }),
                args: vec![],
            })],
        };
        assert!(type_proofs::present(&[function]));
        assert!(argument_failure(&[rigid_type("T"), Typ::Generic("T".into())]).is_none());
        assert!(is_rigid_type(&rigid_type("T")));
        assert!(!is_rigid_type(&Typ::Named("T".into())));
    }
    #[test]
    fn argument_failure_order_is_deterministic() {
        let a = ResolutionFailure::Invalid("a cycle".into()).typ();
        let z = ResolutionFailure::Invalid("z cycle".into()).typ();
        assert_eq!(
            argument_failure(&[a.clone(), z.clone()]),
            argument_failure(&[z, a])
        );
    }
}
