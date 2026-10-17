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
        let (kind, message) = match self {
            Self::Invalid(message) => ("invalid:", message),
            Self::Unsupported(message) => ("unsupported:", message),
        };
        Typ::Named(format!("{PREFIX}{kind}{message}"))
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
/// Allocation-free for ordinary scalar types; only existing encoded tuple/ref
/// leaves need decoding. This presence scan is not type inference or validation.
pub(crate) fn present(decls: &[super::Decl]) -> bool {
    use super::{Decl, Expr, Stmt};
    fn named(name: &str) -> bool {
        decode(name).is_some()
            || super::rust_references::decode(name)
                .is_some_and(|reference| typ(&reference.referent) || typ(&reference.runtime))
            || super::tuples::fields(name)
                .is_some_and(|fields| fields.iter().any(|(_, value)| typ(value)))
    }
    fn typ(value: &Typ) -> bool {
        match value {
            Typ::Named(name) => named(name),
            Typ::Array(item) | Typ::Vector(item) => typ(item),
            _ => false,
        }
    }
    fn pattern(source: &str) -> bool {
        // Ordinary patterns have no type annotations. Decode only a possible
        // private nominal carrier; do not parse every scalar match in the fastpath.
        if ![STEM, LEGACY, "in-tuple-", "#rust-reference-type:"]
            .iter()
            .any(|prefix| source.contains(prefix))
        {
            return false;
        }
        fn walk(value: &super::MatchPattern) -> bool {
            match value {
                super::MatchPattern::StructPat { name, fields } => {
                    named(name) || fields.iter().any(|(_, value)| walk(value))
                }
                super::MatchPattern::TuplePat(items) | super::MatchPattern::ArrayPat(items) => {
                    items.iter().any(walk)
                }
                _ => false,
            }
        }
        super::MatchPattern::parse(source).is_ok_and(|value| walk(&value))
    }
    fn expr(value: &Expr) -> bool {
        match value {
            Expr::StructInit { name, .. } if named(name) => return true,
            Expr::Closure {
                params, ret, body, ..
            } => return params.iter().any(|(_, value)| typ(value)) || typ(ret) || statements(body),
            Expr::Block { body, .. } if statements(body) => return true,
            Expr::MatchValue { arms, .. } if arms.iter().any(|arm| pattern(&arm.pattern)) => {
                return true;
            }
            _ => {}
        }
        let mut found = false;
        super::for_each_expr_child(value, &mut |value| {
            if !found {
                found = expr(value);
            }
        });
        found
    }
    fn statements(body: &[Stmt]) -> bool {
        body.iter().any(|stmt| match stmt {
            Stmt::Let(_, annotation, value) => annotation.as_ref().is_some_and(typ) || expr(value),
            Stmt::Assign(_, value)
            | Stmt::Expr(value)
            | Stmt::Throw(value)
            | Stmt::Return(Some(value)) => expr(value),
            Stmt::IndexAssign { base, index, value } => expr(base) || expr(index) || expr(value),
            Stmt::FieldAssign { base, value, .. } => expr(base) || expr(value),
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => expr(cond) || statements(then_body) || statements(else_body),
            Stmt::Loop { cond, body, .. } => cond.as_ref().is_some_and(expr) || statements(body),
            Stmt::Match { scrutinee, arms } => {
                expr(scrutinee)
                    || arms.iter().any(|arm| {
                        pattern(&arm.pattern)
                            || arm.guard.as_ref().is_some_and(expr)
                            || statements(&arm.body)
                    })
            }
            Stmt::Try { body, catches } => {
                statements(body)
                    || catches
                        .iter()
                        .any(|arm| pattern(&arm.pattern) || statements(&arm.body))
            }
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => false,
        })
    }
    decls.iter().any(|decl| match decl {
        Decl::Function {
            params, ret, body, ..
        } => params.iter().any(|(_, value)| typ(value)) || typ(ret) || statements(body),
        Decl::Global {
            typ: value, init, ..
        } => typ(value) || init.as_ref().is_some_and(|value| expr(value)),
        Decl::Struct { fields, .. } => fields.iter().any(|(_, value)| typ(value)),
        Decl::Class {
            fields, methods, ..
        } => fields.iter().any(|(_, value)| typ(value)) || present(methods),
        Decl::Interface { methods, .. } => methods
            .iter()
            .any(|method| typ(&method.ret) || method.params.iter().any(|(_, value)| typ(value))),
        Decl::Component { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifications_and_malformed_private_proofs_survive() {
        for proof in [
            ResolutionFailure::Invalid("cycle".into()),
            ResolutionFailure::Unsupported("GAT".into()),
        ] {
            let Typ::Named(name) = proof.clone().typ() else {
                panic!()
            };
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
