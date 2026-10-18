//! Private frontend type failures survive substitution until supported execution is proven.
use super::Typ;
const PREFIX: &str = "#rust-associated-type-proof:";
const STEM: &str = "#rust-associated-type-proof";
const LEGACY: &str = "#unsupported-rust-associated-type";
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolutionFailure {
    Invalid(String),
    Unsupported(String),
}
impl ResolutionFailure {
    pub(crate) fn typ(self) -> Typ {
        Typ::Named(self.symbol())
    }
    pub(crate) fn symbol(&self) -> String {
        let (kind, message) = match self {
            Self::Invalid(message) => ("invalid:", message),
            Self::Unsupported(message) => ("unsupported:", message),
        };
        format!("{PREFIX}{kind}{message}")
    }
    pub(crate) fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid(_))
    }
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Invalid(message) | Self::Unsupported(message) => message,
        }
    }
}
pub(crate) fn decode(name: &str) -> Option<ResolutionFailure> {
    if name.starts_with(LEGACY) {
        return Some(ResolutionFailure::Unsupported(
            "unresolved Rust associated type".into(),
        ));
    }
    if !name.starts_with(STEM) {
        return None;
    }
    if let Some(message) = name
        .strip_prefix(PREFIX)
        .and_then(|name| name.strip_prefix("invalid:"))
    {
        return Some(ResolutionFailure::Invalid(message.into()));
    }
    if let Some(message) = name
        .strip_prefix(PREFIX)
        .and_then(|name| name.strip_prefix("unsupported:"))
    {
        return Some(ResolutionFailure::Unsupported(message.into()));
    }
    Some(ResolutionFailure::Unsupported(
        "malformed private Rust associated type proof".into(),
    ))
}
/// Pick a stable diagnostic while preserving definite invalidity over unsupported cases.
pub(crate) fn prefer_failure(
    left: Option<ResolutionFailure>,
    right: Option<ResolutionFailure>,
) -> Option<ResolutionFailure> {
    match (left, right) {
        (None, value) | (value, None) => value,
        (Some(left), Some(right)) => {
            if right.is_invalid() && !left.is_invalid()
                || right.is_invalid() == left.is_invalid() && right.message() < left.message()
            {
                Some(right)
            } else {
                Some(left)
            }
        }
    }
}

/// Inspect the same recursive private tuple/reference carriers as the presence scan.
pub(crate) fn failure_in_type(value: &Typ) -> Option<ResolutionFailure> {
    match value {
        Typ::Named(name) => {
            let mut failure = decode(name);
            if let Some(array) = super::rust_arrays::decode(name) {
                failure = prefer_failure(failure, failure_in_type(&array.element));
            }
            if let Some(reference) = super::rust_references::decode(name) {
                failure = prefer_failure(failure, failure_in_type(&reference.referent));
                failure = prefer_failure(failure, failure_in_type(&reference.runtime));
            }
            if let Some(fields) = super::tuples::fields(name) {
                for (_, typ) in fields {
                    failure = prefer_failure(failure, failure_in_type(&typ));
                }
            }
            failure
        }
        Typ::Array(item) | Typ::Vector(item) => failure_in_type(item),
        _ => None,
    }
}

/// Select the retained metadata family without allocating or cloning an AST.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetadataKind {
    TypeFailures,
    RustFixedArrays,
    RustFloats,
}

pub(crate) fn present(decls: &[super::Decl]) -> bool {
    metadata_present(decls, MetadataKind::TypeFailures)
}

/// Shared immutable presence traversal; encoded carriers decode only on their
/// existing prefixes, while ordinary modules never clone the AST.
pub(crate) fn metadata_present(decls: &[super::Decl], mode: MetadataKind) -> bool {
    use super::{Decl, Expr, Stmt};
    fn named(name: &str, mode: MetadataKind) -> bool {
        (mode == MetadataKind::TypeFailures && decode(name).is_some())
            || (mode == MetadataKind::RustFloats && super::rust_floats::is_proof(name))
            || super::rust_arrays::decode(name).is_some_and(|array| {
                mode == MetadataKind::RustFixedArrays || typ(&array.element, mode)
            })
            || super::rust_references::decode(name).is_some_and(|reference| {
                typ(&reference.referent, mode) || typ(&reference.runtime, mode)
            })
            || super::tuples::fields(name)
                .is_some_and(|fields| fields.iter().any(|(_, value)| typ(value, mode)))
    }
    fn typ(value: &Typ, mode: MetadataKind) -> bool {
        match value {
            Typ::Named(name) => named(name, mode),
            Typ::Array(item) | Typ::Vector(item) => typ(item, mode),
            _ => false,
        }
    }
    fn pattern(source: &str, mode: MetadataKind) -> bool {
        // Ordinary patterns have no type annotations. Decode only a possible
        // private nominal carrier; do not parse every scalar match in the fastpath.
        if ![
            STEM,
            LEGACY,
            "in-tuple-",
            "#rust-reference-type:",
            "#rust-fixed-array:",
            "#rust-float-",
        ]
        .iter()
        .any(|prefix| source.contains(prefix))
        {
            return false;
        }
        fn walk(value: &super::MatchPattern, mode: MetadataKind) -> bool {
            match value {
                super::MatchPattern::StructPat { name, fields } => {
                    named(name, mode) || fields.iter().any(|(_, value)| walk(value, mode))
                }
                super::MatchPattern::TuplePat(items) | super::MatchPattern::ArrayPat(items) => {
                    items.iter().any(|value| walk(value, mode))
                }
                _ => false,
            }
        }
        super::MatchPattern::parse(source).is_ok_and(|value| walk(&value, mode))
    }
    fn expr(value: &Expr, mode: MetadataKind) -> bool {
        match value {
            Expr::Ident(name)
                if mode == MetadataKind::RustFloats
                    && super::rust_floats::bit_method(name).is_some() =>
            {
                return true;
            }
            Expr::Unary { op, .. }
                if mode == MetadataKind::RustFloats
                    && super::rust_floats::inferred_type(op).is_some() =>
            {
                return true;
            }
            Expr::Unary { op, .. }
                if mode == MetadataKind::RustFixedArrays
                    && (super::rust_arrays::value_length(op).is_some()
                        || op == super::rust_arrays::PENDING_REPEAT) =>
            {
                return true;
            }
            Expr::Unary { op, .. }
                if super::rust_generics::decode_arguments(op)
                    .is_some_and(|types| types.iter().any(|value| typ(value, mode))) =>
            {
                return true;
            }
            Expr::Ident(name) if mode == MetadataKind::TypeFailures && decode(name).is_some() => {
                return true;
            }
            Expr::StructInit { name, .. } if named(name, mode) => return true,
            Expr::Closure {
                params, ret, body, ..
            } => {
                return params.iter().any(|(_, value)| typ(value, mode))
                    || typ(ret, mode)
                    || statements(body, mode);
            }
            Expr::Block { body, .. } if statements(body, mode) => return true,
            Expr::MatchValue { arms, .. } if arms.iter().any(|arm| pattern(&arm.pattern, mode)) => {
                return true;
            }
            _ => {}
        }
        let mut found = false;
        super::for_each_expr_child(value, &mut |value| {
            if !found {
                found = expr(value, mode);
            }
        });
        found
    }
    fn statements(body: &[Stmt], mode: MetadataKind) -> bool {
        body.iter().any(|stmt| match stmt {
            Stmt::Declare(_, annotation) => typ(annotation, mode),
            Stmt::Let(_, annotation, value) => {
                annotation.as_ref().is_some_and(|value| typ(value, mode)) || expr(value, mode)
            }
            Stmt::Assign(_, value)
            | Stmt::Expr(value)
            | Stmt::Throw(value)
            | Stmt::Return(Some(value)) => expr(value, mode),
            Stmt::IndexAssign { base, index, value } => {
                expr(base, mode) || expr(index, mode) || expr(value, mode)
            }
            Stmt::FieldAssign { base, value, .. } => expr(base, mode) || expr(value, mode),
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => expr(cond, mode) || statements(then_body, mode) || statements(else_body, mode),
            Stmt::Loop { cond, body, .. } => {
                cond.as_ref().is_some_and(|value| expr(value, mode)) || statements(body, mode)
            }
            Stmt::Match { scrutinee, arms } => {
                expr(scrutinee, mode)
                    || arms.iter().any(|arm| {
                        pattern(&arm.pattern, mode)
                            || arm.guard.as_ref().is_some_and(|value| expr(value, mode))
                            || statements(&arm.body, mode)
                    })
            }
            Stmt::Try { body, catches } => {
                statements(body, mode)
                    || catches
                        .iter()
                        .any(|arm| pattern(&arm.pattern, mode) || statements(&arm.body, mode))
            }
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => false,
        })
    }
    decls.iter().any(|decl| match decl {
        Decl::Function {
            params, ret, body, ..
        } => {
            params.iter().any(|(_, value)| typ(value, mode))
                || typ(ret, mode)
                || statements(body, mode)
        }
        Decl::Global {
            typ: value, init, ..
        } => typ(value, mode) || init.as_ref().is_some_and(|value| expr(value, mode)),
        Decl::Struct { fields, .. } => fields.iter().any(|(_, value)| typ(value, mode)),
        Decl::Class {
            fields, methods, ..
        } => fields.iter().any(|(_, value)| typ(value, mode)) || metadata_present(methods, mode),
        Decl::Interface { methods, .. } => methods.iter().any(|method| {
            typ(&method.ret, mode) || method.params.iter().any(|(_, value)| typ(value, mode))
        }),
        Decl::Component { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presence_scan_observes_value_only_failures_in_dead_code_and_globals() {
        use crate::core_ir::{Decl, Expr, Stmt};
        for proof in [
            ResolutionFailure::Invalid("value cycle".into()),
            ResolutionFailure::Unsupported("value projection".into()),
        ] {
            let value = Expr::Ident(proof.symbol());
            let function = Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![Stmt::If {
                    cond: Expr::BoolLit(false),
                    then_body: vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(value.clone()),
                        args: vec![],
                    })],
                    else_body: vec![],
                }],
            };
            assert!(present(&[function]));
            let global = |value| Decl::Global {
                name: "VALUE".into(),
                typ: Typ::Int,
                init: Some(Box::new(value)),
                mutable: false,
                constant_value: false,
            };
            assert!(present(&[global(value)]));
            assert!(!present(&[global(Expr::StringLit(proof.symbol()))]));
            assert!(!present(&[global(Expr::Ident("ordinary".into()))]));
        }
    }

    #[test]
    fn classifications_and_malformed_private_proofs_survive() {
        for proof in [
            ResolutionFailure::Invalid("cycle".into()),
            ResolutionFailure::Unsupported("GAT".into()),
        ] {
            let Typ::Named(name) = proof.clone().typ() else {
                panic!()
            };
            assert_eq!(name, proof.symbol());
            assert_eq!(decode(&name), Some(proof));
        }
        for name in [STEM, PREFIX, "#rust-associated-type-proof:bad", LEGACY] {
            assert!(matches!(
                decode(name),
                Some(ResolutionFailure::Unsupported(_))
            ));
        }
        assert!(decode("Row").is_none());
    }
}
