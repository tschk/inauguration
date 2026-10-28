//! Owned Option values retain a tag even when the payload itself is zero.
use super::{Expr, MatchPattern, Typ, tuples};

pub(crate) const SOME: &str = "#rust-option-some";
pub(crate) const NONE: &str = "#rust-option-none";
pub(crate) const OWNER: &str = "in-rust-option";
pub(crate) const SOME_PATTERN: &str = "in-rust-option-some";
pub(crate) const NONE_PATTERN: &str = "in-rust-option-none";
const PREFIX: &str = "in-rust-option-";

pub(crate) fn typ(payload: Typ) -> Typ {
    let Typ::Named(shape) = tuples::typ(&[payload]) else {
        unreachable!("a payload has a singleton shape")
    };
    Typ::Named(format!("{PREFIX}{shape}"))
}

pub(crate) fn payload(name: &str) -> Option<Typ> {
    let fields = tuples::fields(name.strip_prefix(PREFIX)?)?;
    (fields.len() == 1).then(|| fields[0].1.clone())
}

pub(crate) fn fields(name: &str) -> Option<Vec<(String, Typ)>> {
    Some(vec![
        ("tag".into(), Typ::Bool),
        ("value".into(), payload(name)?),
    ])
}

/// Inactive payload slots need owned initialization, not guessed pointer bits.
fn zero(
    typ: &Typ,
    schema: &impl Fn(&str) -> Option<Vec<(String, Typ)>>,
    active: &mut std::collections::HashSet<String>,
) -> Option<Expr> {
    if let Typ::Named(name) = typ {
        if let Some(kind) = super::rust_integer_type_name(name) {
            return Some(Expr::Unary {
                op: format!("rust-integer-literal-{kind}"),
                expr: Box::new(Expr::IntLit(0)),
            });
        }
        if let Some(payload) = payload(name) {
            return Some(value(name.clone(), false, zero(&payload, schema, active)?));
        }
        if let Some(reference) = super::rust_references::decode(name) {
            return zero(&reference.runtime, schema, active);
        }
        let fields = schema(name)?;
        if !active.insert(name.clone()) {
            return Some(Expr::Ident(
                super::type_proofs::ResolutionFailure::Invalid(
                    "recursive Option payload has no finite by-value storage".into(),
                )
                .symbol(),
            ));
        }
        let fields = fields
            .into_iter()
            .map(|(name, typ)| Some((name, zero(&typ, schema, active)?)))
            .collect::<Option<Vec<_>>>();
        active.remove(name);
        return Some(Expr::StructInit {
            name: name.clone(),
            fields: fields?,
        });
    }
    Some(match typ {
        Typ::Int => Expr::IntLit(0),
        Typ::Bool => Expr::BoolLit(false),
        Typ::Float => Expr::FloatLit(super::FloatVal(0.0)),
        Typ::String => Expr::StringLit(String::new()),
        Typ::Void => Expr::UnitLit,
        Typ::Vector(_) => Expr::Block {
            body: vec![super::Stmt::Let(
                "#option-inactive-vector".into(),
                Some(typ.clone()),
                Expr::ArrayLit(vec![]),
            )],
            result: Some(Box::new(Expr::Ident("#option-inactive-vector".into()))),
        },
        _ => return None,
    })
}

fn value(name: String, tag: bool, payload: Expr) -> Expr {
    Expr::StructInit {
        name,
        fields: vec![
            ("tag".into(), Expr::BoolLit(tag)),
            ("value".into(), payload),
        ],
    }
}

/// Use the destination's source payload before integer widths are erased.
pub(crate) fn constructor(
    expr: &mut Expr,
    expected: Option<&Typ>,
    arguments: &[Option<Typ>],
    schema: &impl Fn(&str) -> Option<Vec<(String, Typ)>>,
) -> Option<Typ> {
    let Expr::Call { callee, args } = expr else {
        return None;
    };
    let Expr::Ident(name) = callee.as_ref() else {
        return None;
    };
    let some = match (name.as_str(), args.len()) {
        (SOME, 1) => true,
        (NONE, 0) => false,
        _ => return None,
    };
    let destination = expected.and_then(|typ| match typ {
        Typ::Named(name) => payload(name),
        _ => None,
    });
    let payload_type =
        destination.or_else(|| some.then(|| arguments.first().cloned().flatten()).flatten())?;
    if some && let Some(actual) = arguments.first().and_then(Option::as_ref) {
        let reference = |typ: &Typ| match typ {
            Typ::Named(name) => super::rust_references::decode(name),
            _ => None,
        };
        let agrees = match (reference(&payload_type), reference(actual)) {
            (Some(expected), Some(actual)) => {
                (!expected.mutable || actual.mutable)
                    && super::rust_references::same_source_type(
                        &expected.referent,
                        &actual.referent,
                    )
            }
            (Some(_), None) => super::rust_references::aggregate_referent(&payload_type).is_none(),
            (None, Some(_)) => super::rust_references::aggregate_referent(actual).is_none(),
            _ => true,
        };
        if !agrees {
            let failure = super::type_proofs::ResolutionFailure::Invalid(
                "Option payload reference disagrees with its source destination".into(),
            );
            *expr = Expr::Ident(failure.symbol());
            return Some(failure.typ());
        }
    }
    let Typ::Named(name) = typ(payload_type.clone()) else {
        unreachable!()
    };
    let payload = if some {
        args[0].clone()
    } else {
        zero(&payload_type, schema, &mut std::collections::HashSet::new())?
    };
    *expr = value(name.clone(), some, payload);
    Some(Typ::Named(name))
}

/// Resolve private variant markers only against a proven Option scrutinee.
pub(crate) fn pattern(
    source: &mut String,
    typ: &Typ,
    schema: &impl Fn(&str) -> Option<Vec<(String, Typ)>>,
) {
    if !source.contains(SOME_PATTERN) && !source.contains(NONE_PATTERN) {
        return;
    }
    fn walk(
        pattern: &mut MatchPattern,
        typ: &Typ,
        schema: &impl Fn(&str) -> Option<Vec<(String, Typ)>>,
    ) {
        let MatchPattern::StructPat { name, fields } = pattern else {
            return;
        };
        if matches!(name.as_str(), SOME_PATTERN | NONE_PATTERN) {
            let Typ::Named(owner) = typ else { return };
            let Some(payload) = payload(owner) else {
                return;
            };
            let some = name == SOME_PATTERN;
            let mut projected = vec![("tag".into(), MatchPattern::BoolPat(some))];
            if some {
                if fields.len() != 1 {
                    return;
                }
                let (_, mut inner) = fields.remove(0);
                walk(&mut inner, &payload, schema);
                projected.push(("value".into(), inner));
            }
            *name = owner.clone();
            *fields = projected;
        } else if let Some(declared) = schema(name) {
            for (field, inner) in fields {
                if let Some((_, typ)) = declared.iter().find(|(name, _)| name == field) {
                    walk(inner, typ, schema);
                }
            }
        }
    }
    if let Ok(mut parsed) = MatchPattern::parse(source) {
        walk(&mut parsed, typ, schema);
        *source = crate::core_bindings::render_pattern(&parsed);
    }
}
