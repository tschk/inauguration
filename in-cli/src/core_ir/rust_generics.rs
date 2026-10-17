//! Transient explicit Rust type arguments, resolved before executable lowering.
use super::Typ;
const PREFIX: &str = "#rust-generic-arguments:";

pub(crate) fn arguments(types: &[Typ]) -> String {
    format!(
        "{PREFIX}{}",
        serde_json::to_string(types).expect("type metadata serializes")
    )
}

pub(crate) fn decode_arguments(op: &str) -> Option<Vec<Typ>> {
    serde_json::from_str(op.strip_prefix(PREFIX)?).ok()
}

pub(crate) const TEMPLATE: &str = "#rust-generic-template";
pub(crate) const CONSTRAINTS: &str = "#rust-generic-constraints";
pub(crate) const ALIAS: &str = "#rust-generic-alias";
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
