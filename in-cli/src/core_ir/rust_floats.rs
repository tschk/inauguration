//! Transient Rust float widths and exact decimal literals. Runtime storage remains f64.
use super::{Expr, Typ, type_proofs::ResolutionFailure};

const F32: &str = "#rust-float-f32";
const F64: &str = "#rust-float-f64";
const WEAK: &str = "#rust-float-weak";
const VALUE_F32: &str = "#rust-float-value-f32";
const VALUE_F64: &str = "#rust-float-value-f64";
const CAST_F32: &str = "#rust-float-cast-f32";
const CAST_F64: &str = "#rust-float-cast-f64";
const LITERAL: &str = "#rust-float-literal";
const LITERAL_F32: &str = "#rust-float-literal-f32";
const LITERAL_F64: &str = "#rust-float-literal-f64";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    F32,
    F64,
}
impl Kind {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "f32" => Some(Self::F32),
            "f64" => Some(Self::F64),
            _ => None,
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}
pub(crate) fn present(decls: &[super::Decl]) -> bool {
    super::type_proofs::metadata_present(decls, super::type_proofs::MetadataKind::RustFloats)
}
pub(crate) fn typ(kind: Kind) -> Typ {
    Typ::Named(
        match kind {
            Kind::F32 => F32,
            Kind::F64 => F64,
        }
        .into(),
    )
}
pub(crate) fn weak_type() -> Typ {
    Typ::Named(WEAK.into())
}
pub(crate) fn kind(typ: &Typ) -> Option<Kind> {
    match typ {
        Typ::Named(name) => match name.as_str() {
            F32 => Some(Kind::F32),
            F64 => Some(Kind::F64),
            _ => None,
        },
        _ => None,
    }
}
pub(crate) fn is_proof(name: &str) -> bool {
    matches!(name, F32 | F64 | WEAK)
}
fn literal_kind(op: &str) -> Option<Option<Kind>> {
    match op {
        LITERAL => Some(None),
        LITERAL_F32 => Some(Some(Kind::F32)),
        LITERAL_F64 => Some(Some(Kind::F64)),
        _ => None,
    }
}
pub(crate) fn is_literal(op: &str) -> bool {
    literal_kind(op).is_some()
}
pub(crate) fn cast_kind(op: &str) -> Option<Kind> {
    match op {
        CAST_F32 => Some(Kind::F32),
        CAST_F64 => Some(Kind::F64),
        _ => None,
    }
}
pub(crate) fn value_kind(op: &str) -> Option<Kind> {
    match op {
        VALUE_F32 => Some(Kind::F32),
        VALUE_F64 => Some(Kind::F64),
        _ => None,
    }
}
pub(crate) fn value(expr: Expr, kind: Kind) -> Expr {
    Expr::Unary {
        op: match kind {
            Kind::F32 => VALUE_F32,
            Kind::F64 => VALUE_F64,
        }
        .into(),
        expr: Box::new(expr),
    }
}
pub(crate) fn cast(value: Expr, kind: Kind) -> Expr {
    Expr::Unary {
        op: match kind {
            Kind::F32 => CAST_F32,
            Kind::F64 => CAST_F64,
        }
        .into(),
        expr: Box::new(value),
    }
}
/// Inspect private source result facts without parsing/rounding the decimal.
/// Generic Core IR cast operators do not establish Rust source provenance.
pub(crate) fn inferred_type(op: &str) -> Option<Typ> {
    cast_kind(op)
        .or_else(|| value_kind(op))
        .or_else(|| from_bits_kind(op))
        .map(typ)
        .or_else(|| literal_kind(op).map(|kind| kind.map_or_else(weak_type, typ)))
}
pub(crate) fn from_bits_kind(op: &str) -> Option<Kind> {
    match op {
        "#rust-float-from-bits-f32" => Some(Kind::F32),
        "#rust-float-from-bits-f64" => Some(Kind::F64),
        _ => None,
    }
}
pub(crate) fn bit_method(name: &str) -> Option<(Kind, bool)> {
    let (owner, method) = name.rsplit_once("::")?;
    let kind = match owner {
        F32 => Kind::F32,
        F64 => Kind::F64,
        _ => return None,
    };
    match method {
        "to_bits" => Some((kind, true)),
        "from_bits" => Some((kind, false)),
        _ => None,
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiteralInfo<'a> {
    pub digits: &'a str,
    pub suffix: Option<Kind>,
}
/// Decimal spelling remains exact until the consumer proves its contextual width.
pub(crate) fn literal(digits: &str, suffix: Option<Kind>) -> Expr {
    Expr::Unary {
        op: match suffix {
            None => LITERAL,
            Some(Kind::F32) => LITERAL_F32,
            Some(Kind::F64) => LITERAL_F64,
        }
        .into(),
        expr: Box::new(Expr::StringLit(digits.into())),
    }
}
pub(crate) fn literal_info(value: &Expr) -> Option<LiteralInfo<'_>> {
    let Expr::Unary { op, expr } = value else {
        return None;
    };
    let suffix = literal_kind(op)?;
    let Expr::StringLit(digits) = expr.as_ref() else {
        return None;
    };
    Some(LiteralInfo { digits, suffix })
}
/// Materialize only at a proven width/default boundary. Direct f32 parsing is
/// essential: decimal -> f64 -> f32 can round differently from decimal -> f32.
pub(crate) fn materialize_literal(
    value: &Expr,
    expected: Option<Kind>,
) -> Result<Option<f64>, ResolutionFailure> {
    let Expr::Unary { op, .. } = value else {
        return Ok(None);
    };
    if !is_literal(op) {
        return Ok(None);
    }
    let info = literal_info(value).ok_or_else(|| {
        ResolutionFailure::Unsupported("malformed private Rust float literal".into())
    })?;
    if let (Some(suffix), Some(expected)) = (info.suffix, expected)
        && suffix != expected
    {
        return Err(ResolutionFailure::Invalid(format!(
            "Rust float literal has type {}, expected {}",
            suffix.name(),
            expected.name()
        )));
    }
    let kind = info.suffix.or(expected).unwrap_or(Kind::F64);
    let digits = info.digits.replace('_', "");
    let parsed = match kind {
        Kind::F32 => digits.parse::<f32>().map(f64::from),
        Kind::F64 => digits.parse::<f64>(),
    }
    .map_err(|_| {
        ResolutionFailure::Invalid(format!(
            "invalid Rust {} literal `{}`",
            kind.name(),
            info.digits
        ))
    })?;
    if !parsed.is_finite() {
        return Err(ResolutionFailure::Invalid(format!(
            "literal `{}` is out of range for {}",
            info.digits,
            kind.name()
        )));
    }
    Ok(Some(parsed))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_is_strong_or_weak_without_changing_storage() {
        for width in [Kind::F32, Kind::F64] {
            let proof = typ(width);
            assert_eq!(kind(&proof), Some(width));
            assert_eq!(proof.canonical(), Typ::Float);
            assert_eq!(proof.inference_type(), proof);
            let Expr::Unary { op, .. } = cast(Expr::FloatLit(super::super::FloatVal(1.0)), width)
            else {
                panic!("cast")
            };
            assert_eq!(cast_kind(&op), Some(width));
            assert_eq!(inferred_type(&op), Some(proof));
            assert_eq!(
                inferred_type(&format!("numeric-cast-{}", width.name())),
                None
            );
            assert_eq!(inferred_type(&format!("float-cast-{}", width.name())), None);
        }
        assert_eq!(weak_type().canonical(), Typ::Float);
        assert_eq!(weak_type().inference_type(), weak_type());
        assert_eq!(kind(&weak_type()), None);
        assert!(!is_proof("f32"));
        assert!(!is_proof("#rust-float-unknown"));
    }
    #[test]
    fn contextual_decimal_parsing_avoids_double_rounding() {
        let digits = "1.0000000596046447753906250000001";
        let value = literal(digits, None);
        assert_eq!(
            literal_info(&value),
            Some(LiteralInfo {
                digits,
                suffix: None
            })
        );
        assert_eq!(inferred_type(LITERAL), Some(weak_type()));
        let direct = materialize_literal(&value, Some(Kind::F32))
            .unwrap()
            .unwrap();
        let wide = materialize_literal(&value, None).unwrap().unwrap();
        assert_eq!(direct, f64::from(f32::from_bits(1.0f32.to_bits() + 1)));
        assert_eq!(f64::from(wide as f32), 1.0);
        assert_eq!(literal_info(&value).unwrap().digits, digits);
        assert!(
            materialize_literal(&literal(digits, Some(Kind::F64)), Some(Kind::F32))
                .unwrap_err()
                .is_invalid()
        );
    }
    #[test]
    fn literal_overflow_and_signed_zero_keep_source_semantics() {
        for (digits, width) in [("1e39", Kind::F32), ("1e309", Kind::F64)] {
            assert!(
                materialize_literal(&literal(digits, Some(width)), None)
                    .unwrap_err()
                    .is_invalid()
            );
        }
        assert_eq!(
            materialize_literal(&literal("-0.0", Some(Kind::F32)), None)
                .unwrap()
                .unwrap()
                .to_bits(),
            (-0.0f64).to_bits()
        );
        assert_eq!(
            materialize_literal(&literal("1_6_7_7_7_2_1_7", None), Some(Kind::F32)).unwrap(),
            Some(16777216.0)
        );
        assert_eq!(
            materialize_literal(&literal("1e-999", None), Some(Kind::F32)).unwrap(),
            Some(0.0)
        );
        assert_eq!(
            materialize_literal(&Expr::FloatLit(super::super::FloatVal(1.0)), None),
            Ok(None)
        );
    }
    #[test]
    fn tuple_and_annotation_walks_retain_then_project_float_metadata() {
        use super::super::{
            Decl, Stmt, tuples,
            type_proofs::{MetadataKind, metadata_present},
        };
        let tuple = tuples::typ(&[typ(Kind::F32), weak_type(), typ(Kind::F64)]);
        let Typ::Named(name) = &tuple else {
            panic!("tuple")
        };
        assert_eq!(
            tuples::fields(name)
                .unwrap()
                .into_iter()
                .map(|(_, typ)| typ)
                .collect::<Vec<_>>(),
            vec![typ(Kind::F32), weak_type(), typ(Kind::F64)]
        );
        let mut decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: tuple,
            type_params: vec![],
            body: vec![Stmt::Declare(
                "value".into(),
                Typ::Array(Box::new(typ(Kind::F32))),
            )],
        }];
        assert!(metadata_present(&decls, MetadataKind::RustFloats));
        crate::core_bindings::rewrite_type_annotations(&mut decls, &|name| {
            is_proof(name).then_some(Typ::Float)
        });
        assert!(!metadata_present(&decls, MetadataKind::RustFloats));
        let literal_only = vec![Decl::Global {
            name: "VALUE".into(),
            typ: Typ::Float,
            init: Some(Box::new(literal("1.0", None))),
            mutable: false,
            constant_value: true,
        }];
        assert!(metadata_present(&literal_only, MetadataKind::RustFloats));
        assert!(!metadata_present(&literal_only, MetadataKind::TypeFailures));
        let canonical_only = vec![Decl::Global {
            name: "CORE".into(),
            typ: Typ::Float,
            init: Some(Box::new(Expr::Unary {
                op: "float-cast-f32".into(),
                expr: Box::new(Expr::FloatLit(super::super::FloatVal(1.0))),
            })),
            mutable: false,
            constant_value: true,
        }];
        assert!(!metadata_present(&canonical_only, MetadataKind::RustFloats));
    }
}
