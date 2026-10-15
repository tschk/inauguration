//! Integer evaluation rules for the target machine word.
use crate::core_ir::Expr;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IntegerSemantics {
    Word32,
    #[default]
    Word64,
}

impl IntegerSemantics {
    /// Accept signed values and unsigned bit-pattern carriers of a 32-bit word.
    pub fn normalize_word(self, value: i64) -> Option<i64> {
        match self {
            Self::Word32 if (i64::from(i32::MIN)..=i64::from(u32::MAX)).contains(&value) => {
                Some(i64::from(value as i32))
            }
            Self::Word32 => None,
            Self::Word64 => Some(value),
        }
    }

    pub fn fold_binary(self, op: &str, a: i64, b: i64) -> Option<Expr> {
        let left = self.normalize_word(a)?;
        // Shift counts are mathematical counts, not signed word carriers.
        if matches!(op, "shl" | "<<" | "shr" | ">>" | "lshr") {
            let bits = if self == Self::Word32 { 32 } else { 64 };
            if !(0..bits).contains(&b) {
                return None;
            }
            let value = match (self, op) {
                (Self::Word32, "lshr") => i64::from(((left as u32) >> b) as i32),
                (Self::Word64, "lshr") => ((left as u64) >> b) as i64,
                (Self::Word32, "shl" | "<<") => i64::from((left as i32).wrapping_shl(b as u32)),
                (Self::Word32, _) => i64::from((left as i32).wrapping_shr(b as u32)),
                (Self::Word64, "shl" | "<<") => left.checked_shl(b as u32)?,
                (Self::Word64, _) => left.checked_shr(b as u32)?,
            };
            return Some(Expr::IntLit(value));
        }
        let right = self.normalize_word(b)?;
        if matches!(op, "udiv" | "urem") {
            let value = match self {
                Self::Word32 => {
                    let (a, b) = (left as u32, right as u32);
                    let value = if op == "udiv" {
                        a.checked_div(b)?
                    } else {
                        a.checked_rem(b)?
                    };
                    i64::from(value as i32)
                }
                Self::Word64 => {
                    let (a, b) = (left as u64, right as u64);
                    (if op == "udiv" {
                        a.checked_div(b)?
                    } else {
                        a.checked_rem(b)?
                    }) as i64
                }
            };
            return Some(Expr::IntLit(value));
        }
        let comparison = match op {
            "eq" | "==" => Some(left == right),
            "neq" | "!=" => Some(left != right),
            "lt" | "<" => Some(left < right),
            "gt" | ">" => Some(left > right),
            "le" | "<=" => Some(left <= right),
            "ge" | ">=" => Some(left >= right),
            "land" | "&&" => Some(left != 0 && right != 0),
            "lor" | "||" => Some(left != 0 || right != 0),
            _ => None,
        };
        if let Some(value) = comparison {
            return Some(Expr::BoolLit(value));
        }
        let value = if self == Self::Word32 {
            let (a, b) = (left as i32, right as i32);
            i64::from(match op {
                "add" | "+" => a.wrapping_add(b),
                "sub" | "-" => a.wrapping_sub(b),
                "mul" | "*" => a.wrapping_mul(b),
                "div" | "/" => a.checked_div(b)?,
                "mod" | "%" => a.checked_rem(b)?,
                "band" | "&" => a & b,
                "bor" | "|" => a | b,
                "xor" | "^" => a ^ b,
                _ => return None,
            })
        } else {
            match op {
                "add" | "+" => left.checked_add(right)?,
                "sub" | "-" => left.checked_sub(right)?,
                "mul" | "*" => left.checked_mul(right)?,
                "div" | "/" => left.checked_div(right)?,
                "mod" | "%" => left.checked_rem(right)?,
                "band" | "&" => left & right,
                "bor" | "|" => left | right,
                "xor" | "^" => left ^ right,
                _ => return None,
            }
        };
        Some(Expr::IntLit(value))
    }

    pub fn fold_unary(self, op: &str, value: i64) -> Option<Expr> {
        let value = self.normalize_word(value)?;
        Some(match op {
            "not" | "!" => Expr::BoolLit(value == 0),
            "neg" | "-" => Expr::IntLit(if self == Self::Word32 {
                i64::from((value as i32).wrapping_neg())
            } else {
                value.checked_neg()?
            }),
            "bitnot" | "~" => Expr::IntLit(!value),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsigned_operations_use_target_bits_and_preserve_invalid_operations() {
        for semantics in [IntegerSemantics::Word32, IntegerSemantics::Word64] {
            let half = if semantics == IntegerSemantics::Word32 {
                i64::from(i32::MAX)
            } else {
                i64::MAX
            };
            assert_eq!(
                semantics.fold_binary("udiv", -1, 2),
                Some(Expr::IntLit(half))
            );
            assert_eq!(semantics.fold_binary("urem", -1, 2), Some(Expr::IntLit(1)));
            assert_eq!(
                semantics.fold_binary("lshr", -1, 1),
                Some(Expr::IntLit(half))
            );
            assert_eq!(semantics.fold_binary("udiv", -1, 1), Some(Expr::IntLit(-1)));
            assert_eq!(semantics.fold_binary("urem", -1, 1), Some(Expr::IntLit(0)));
            assert_eq!(semantics.fold_binary("udiv", 42, 0), None);
            assert_eq!(semantics.fold_binary("urem", 42, 0), None);
            assert_eq!(semantics.fold_binary("lshr", 42, -1), None);
            let width = if semantics == IntegerSemantics::Word32 {
                32
            } else {
                64
            };
            assert_eq!(semantics.fold_binary("lshr", -1, width), None);
            assert_eq!(
                semantics.fold_binary("lshr", -1, width - 1),
                Some(Expr::IntLit(1))
            );
        }
        assert_eq!(
            IntegerSemantics::Word32.fold_binary("udiv", 4294967295, 2),
            Some(Expr::IntLit(2147483647))
        );
        assert_eq!(
            IntegerSemantics::Word32.fold_binary("udiv", 4294967296, 2),
            None
        );
    }

    #[test]
    fn machine_words_wrap_compare_and_shift_at_their_own_width() {
        let word = IntegerSemantics::Word32;
        assert_eq!(
            word.fold_binary("+", i32::MAX.into(), 1),
            Some(Expr::IntLit(i32::MIN.into()))
        );
        assert_eq!(
            IntegerSemantics::Word64.fold_binary("+", i32::MAX.into(), 1),
            Some(Expr::IntLit(2147483648))
        );
        assert_eq!(
            word.fold_binary("<", 0xffffffff, 0),
            Some(Expr::BoolLit(true))
        );
        assert_eq!(
            word.fold_binary("^", 0xffffffff, 0x80000000),
            Some(Expr::IntLit(0x7fffffff))
        );
        assert_eq!(
            word.fold_unary("-", i32::MIN.into()),
            Some(Expr::IntLit(i32::MIN.into()))
        );
        assert_eq!(word.fold_unary("~", 0), Some(Expr::IntLit(-1)));
        assert_eq!(
            word.fold_binary("<<", 1, 31),
            Some(Expr::IntLit(i32::MIN.into()))
        );
        assert_eq!(
            word.fold_binary(">>", 0x80000000, 31),
            Some(Expr::IntLit(-1))
        );
        for shift in [-1, 32, 64] {
            assert_eq!(word.fold_binary("<<", 1, shift), None);
        }
        for op in ["/", "%"] {
            assert_eq!(word.fold_binary(op, i32::MIN.into(), -1), None);
            assert_eq!(word.fold_binary(op, 1, 0), None);
        }
        assert_eq!(word.normalize_word(0x100000000), None);
        assert_eq!(word.normalize_word(i64::from(i32::MIN) - 1), None);
    }
}
