//! Fresh scalar array storage with a source-proven element count.
const PREFIX: &str = "array-copy:";

pub(crate) fn valid_operand(operation: &str, operand: &super::Typ) -> bool {
    let Some(count) = count(operation) else {
        return false;
    };
    matches!(operand.canonical(), super::Typ::Array(element)
        if matches!(element.as_ref(), super::Typ::Int | super::Typ::Float | super::Typ::Bool)
            || count == 0 && *element == super::Typ::Void)
}

pub(crate) fn operation(count: usize) -> String {
    format!("{PREFIX}{count}")
}

pub(crate) fn count(operation: &str) -> Option<usize> {
    let count = operation.strip_prefix(PREFIX)?.parse().ok()?;
    (operation == self::operation(count)).then_some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_is_exact_and_canonical_including_empty_arrays() {
        for value in [0, 1, 42, usize::MAX] {
            assert_eq!(count(&operation(value)), Some(value));
        }
        for invalid in [
            "array-copy",
            "array-copy:",
            "array-copy:-1",
            "array-copy:+1",
            "array-copy:01",
            "array-copy:184467440737095516160",
        ] {
            assert_eq!(count(invalid), None);
        }
    }

    #[test]
    fn copy_requires_a_scalar_array_and_never_adopts_vector_alias_semantics() {
        use super::super::Typ;
        for element in [Typ::Int, Typ::Float, Typ::Bool] {
            assert!(valid_operand(
                &operation(2),
                &Typ::Array(Box::new(element.clone()))
            ));
            assert!(!valid_operand(
                &operation(2),
                &Typ::Vector(Box::new(element))
            ));
        }
        assert!(valid_operand(
            &operation(0),
            &Typ::Array(Box::new(Typ::Void))
        ));
        assert!(!valid_operand(
            &operation(1),
            &Typ::Array(Box::new(Typ::Void))
        ));
        assert!(!valid_operand(
            &operation(0),
            &Typ::Array(Box::new(Typ::String))
        ));
        assert!(!valid_operand(
            "array-copy:01",
            &Typ::Array(Box::new(Typ::Int))
        ));
    }

    #[test]
    fn empty_copy_uses_scalar_array_context_without_coercing_vectors_or_records() {
        use super::super::{Expr, Typ};
        let copy = Expr::Unary {
            op: operation(0),
            expr: Box::new(Expr::ArrayLit(vec![])),
        };
        let actual = Typ::Array(Box::new(Typ::Void));
        assert!(Typ::Array(Box::new(Typ::Bool)).accepts_sequence_literal(&copy, &actual));
        assert!(!Typ::Vector(Box::new(Typ::Bool)).accepts_sequence_literal(&copy, &actual));
        assert!(!Typ::Array(Box::new(Typ::String)).accepts_sequence_literal(&copy, &actual));
    }
}
