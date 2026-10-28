//! Source-owned Box identity and finite, one-pointer heap storage.
use super::{Expr, Typ, rust_references as references, tuples};

pub(crate) const NEW: &str = "#rust-box-new";
pub(crate) const DEREF: &str = "#rust-box-deref";
pub(crate) const SHARED: &str = "#rust-box-borrow-shared";
pub(crate) const MUTABLE: &str = "#rust-box-borrow-mutable";
pub(crate) const DATA: &str = "#data";
const PREFIX: &str = "in-rust-box-";
const ADDRESS: &str = "#rust-box-address:";
const ALLOCATE: &str = "#rust-box-allocate:";
const TYPED_NEW: &str = "#rust-box-new:";

pub(crate) fn typ(payload: Typ) -> Typ {
    let Typ::Named(shape) = tuples::typ(&[payload]) else {
        unreachable!("a Box payload has a singleton shape")
    };
    Typ::Named(format!("{PREFIX}{shape}"))
}

pub(crate) fn payload(name: &str) -> Option<Typ> {
    let fields = tuples::fields(name.strip_prefix(PREFIX)?)?;
    (fields.len() == 1).then(|| fields[0].1.clone())
}

pub(crate) fn address_payload(name: &str) -> Option<Typ> {
    payload(name.strip_prefix(ADDRESS)?)
}

pub(crate) fn address(payload: Typ) -> Typ {
    let Typ::Named(owner) = typ(payload) else {
        unreachable!()
    };
    Typ::Named(format!("{ADDRESS}{owner}"))
}

pub(crate) fn fields(name: &str) -> Option<Vec<(String, Typ)>> {
    Some(vec![(DATA.into(), address(payload(name)?))])
}

pub(crate) fn allocation(payload: Typ) -> String {
    let Typ::Named(owner) = typ(payload) else {
        unreachable!()
    };
    format!("{ALLOCATE}{owner}")
}

pub(crate) fn allocation_payload(name: &str) -> Option<Typ> {
    payload(name.strip_prefix(ALLOCATE)?)
}

pub(crate) fn typed_constructor(payload: Typ) -> String {
    let Typ::Named(owner) = typ(payload) else {
        unreachable!()
    };
    format!("{TYPED_NEW}{owner}")
}

pub(crate) fn constructor_payload(name: &str) -> Option<Typ> {
    payload(name.strip_prefix(TYPED_NEW)?)
}

pub(crate) fn is_constructor(name: &str) -> bool {
    name == NEW || constructor_payload(name).is_some()
}

/// Deref accepts a Box or a borrow of one, but never an unrelated nominal owner.
pub(crate) fn pointee(value: &Typ) -> Option<Typ> {
    let Typ::Named(name) = value else { return None };
    if let Some(reference) = references::decode(name) {
        return pointee(&reference.referent);
    }
    payload(name)
}

pub(crate) fn value_type(op: &str, value: &Typ) -> Option<Typ> {
    if op == DEREF {
        let Typ::Named(name) = value else { return None };
        return address_payload(name);
    }
    let payload = pointee(value)?;
    match op {
        SHARED | MUTABLE => references::value_type(
            if op == MUTABLE {
                references::MUTABLE_VALUE
            } else {
                references::SHARED_VALUE
            },
            payload,
        ),
        _ => None,
    }
}

pub(crate) fn data(value: Expr) -> Expr {
    Expr::Field {
        base: Box::new(value),
        name: DATA.into(),
    }
}

pub(crate) fn dereference(value: Expr) -> Expr {
    Expr::Unary {
        op: DEREF.into(),
        expr: Box::new(data(value)),
    }
}

pub(crate) fn constructor(
    expr: &mut Expr,
    expected: Option<&Typ>,
    arguments: &[Option<Typ>],
) -> Option<Typ> {
    let Expr::Call { callee, args } = expr else {
        return None;
    };
    let Expr::Ident(name) = callee.as_ref() else {
        return None;
    };
    if !is_constructor(name) || args.len() != 1 {
        return None;
    }
    let destination = expected.and_then(|typ| match typ {
        Typ::Named(name) => payload(name),
        _ => None,
    });
    let payload = constructor_payload(name)
        .or_else(|| destination.clone())
        .or_else(|| arguments.first().cloned().flatten())?;
    if destination
        .as_ref()
        .is_some_and(|target| !references::same_source_type(&payload, target))
        || arguments.first().and_then(Option::as_ref).is_some_and(|actual| {
            !references::same_source_type(&payload, actual)
                && !(actual == &Typ::Int
                    && (payload == Typ::Int
                        || matches!(&payload, Typ::Named(name) if super::rust_integer_type_name(name).is_some())))
                && !(actual == &Typ::Float
                    && (payload == Typ::Float
                        || matches!(&payload, Typ::Named(name) if super::rust_floats::is_proof(name))))
        })
    {
        let failure = super::type_proofs::ResolutionFailure::Invalid(
            "Box payload disagrees with its source destination".into(),
        );
        *expr = Expr::Ident(failure.symbol());
        return Some(failure.typ());
    }
    let result = typ(payload.clone());
    let Typ::Named(name) = &result else {
        unreachable!()
    };
    let value = args.remove(0);
    *expr = Expr::StructInit {
        name: name.clone(),
        fields: vec![(
            DATA.into(),
            Expr::Call {
                callee: Box::new(Expr::Ident(allocation(payload))),
                args: vec![value],
            },
        )],
    };
    Some(result)
}

/// A borrow coercion projects the existing allocation, not a copied pointee.
pub(crate) fn coerce(expr: &mut Expr, expected: &Typ, actual: &Typ) -> bool {
    let (Typ::Named(expected), Typ::Named(actual)) = (expected, actual) else {
        return false;
    };
    let (Some(expected), Some(actual)) = (references::decode(expected), references::decode(actual))
    else {
        return false;
    };
    let Typ::Named(owner) = &actual.referent else {
        return false;
    };
    let Some(payload) = payload(owner) else {
        return false;
    };
    if !references::same_source_type(&payload, &expected.referent) {
        return false;
    }
    if expected.mutable && !actual.mutable {
        *expr = Expr::Ident(
            super::type_proofs::ResolutionFailure::Invalid(
                "Box dereference coercion cannot create a mutable borrow from a shared borrow"
                    .into(),
            )
            .symbol(),
        );
    } else if !matches!(payload.canonical(), Typ::Named(_)) {
        *expr = Expr::Ident(
            super::type_proofs::ResolutionFailure::Unsupported(
                "borrowed scalar Box payload requires the scalar reference address ABI".into(),
            )
            .symbol(),
        );
    } else {
        *expr = Expr::Unary {
            op: if expected.mutable { MUTABLE } else { SHARED }.into(),
            expr: Box::new(std::mem::replace(expr, Expr::UnitLit)),
        };
    }
    true
}
