//! Resolve Rust integer widths before canonical native carriers erase them.
use super::{Facts, TypeChecker};
use crate::core_ir::{self, Decl, Expr, Stmt, Typ, UnifiedModule};
use crate::integer_semantics::IntegerSemantics;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Kind {
    bits: u8,
    signed: bool,
    source: bool,
}
impl Kind {
    fn same_width(self, other: Self) -> bool {
        self.bits == other.bits && self.signed == other.signed
    }
    fn prefer_source(self, other: Self) -> Self {
        if !self.source && other.source {
            other
        } else {
            self
        }
    }

    fn parse(name: &str, semantics: IntegerSemantics) -> Option<Self> {
        let (bits, signed) = match name {
            "isize" => (
                if semantics == IntegerSemantics::Word32 {
                    32
                } else {
                    64
                },
                true,
            ),
            "usize" => (
                if semantics == IntegerSemantics::Word32 {
                    32
                } else {
                    64
                },
                false,
            ),
            name => core_ir::integer_spec(name)?,
        };
        Some(Self {
            bits,
            signed,
            source: true,
        })
    }
    fn typ(self) -> Typ {
        if !self.source {
            return Typ::Int;
        }
        Typ::Named(format!(
            "#rust-integer-{}{}",
            if self.signed { "i" } else { "u" },
            self.bits
        ))
    }
    fn cast(self, value: Expr) -> Expr {
        core_ir::integer_cast_expr(
            value,
            &format!(
                "int-cast-{}{}",
                if self.signed { "i" } else { "u" },
                self.bits
            ),
        )
        .unwrap_or_else(invalid)
    }
    fn supported(self, semantics: IntegerSemantics) -> bool {
        self.bits
            <= if semantics == IntegerSemantics::Word32 {
                32
            } else {
                64
            }
    }
    fn contains(self, value: i64) -> bool {
        if self.bits == 64 {
            return self.signed || value >= 0;
        }
        if self.signed {
            let bound = 1i64 << (self.bits - 1);
            (-bound..bound).contains(&value)
        } else {
            (0..(1i64 << self.bits)).contains(&value)
        }
    }
}
fn same_kinds(left: Option<Kind>, right: Option<Kind>) -> bool {
    left.zip(right)
        .is_some_and(|(left, right)| left.same_width(right))
}
fn kind_mismatch(actual: Kind, expected: Kind) -> core_ir::type_proofs::ResolutionFailure {
    let message = format!(
        "incompatible Rust integer kinds: expected {}{}, found {}{}",
        if expected.signed { "i" } else { "u" },
        expected.bits,
        if actual.signed { "i" } else { "u" },
        actual.bits,
    );
    if actual.source && expected.source {
        core_ir::type_proofs::ResolutionFailure::Invalid(message)
    } else {
        core_ir::type_proofs::ResolutionFailure::Unsupported(message)
    }
}

pub(crate) fn wrapping_operator(member: &str) -> Option<&'static str> {
    match member {
        "wrapping_add" => Some("+"),
        "wrapping_sub" => Some("-"),
        "wrapping_mul" => Some("*"),
        _ => None,
    }
}
fn primitive_member(name: &str, semantics: IntegerSemantics) -> Option<(Kind, &str)> {
    let (owner, member) = name.rsplit_once("::")?;
    Some((
        Kind::parse(core_ir::rust_integer_type_name(owner)?, semantics)?,
        member,
    ))
}
fn wrapping_target(
    name: &str,
    semantics: IntegerSemantics,
) -> Option<(Option<Kind>, &'static str)> {
    if let Some(member) = name.strip_prefix(core_ir::METHOD_PREFIX) {
        return Some((None, wrapping_operator(member)?));
    }
    let (kind, member) = primitive_member(name, semantics)?;
    Some((Some(kind), wrapping_operator(member)?))
}

/// Only private, already-resolved primitive owners carry integer constants.
fn primitive_constant(name: &str, semantics: IntegerSemantics) -> Option<(Kind, i64)> {
    let (kind, member) = primitive_member(name, semantics)?;
    let value = match member {
        "BITS" => {
            return Some((
                Kind {
                    bits: 32,
                    signed: false,
                    source: true,
                },
                i64::from(kind.bits),
            ));
        }
        "MIN" if !kind.signed => 0,
        "MIN" => -(i64::MAX >> (64 - kind.bits)) - 1,
        "MAX" if kind.signed => i64::MAX >> (64 - kind.bits),
        "MAX" => (u64::MAX >> (64 - kind.bits)) as i64,
        _ => return None,
    };
    Some((kind, value))
}

fn invalid() -> Expr {
    Expr::Ident("#unsupported-rust-integer-semantics".into())
}
// Collapsing an unsupported integer operation must retain a child source error.
// Prefer Invalid over Unsupported; within each category retain traversal order.
fn invalid_preserving_proof(expr: &mut Expr) -> Expr {
    let mut selected: Option<(bool, String)> = None;
    crate::core_opt::map_expr_mut(expr, &mut |child| {
        if let Expr::Ident(name) = child
            && let Some(proof) = core_ir::type_proofs::decode(name)
            && selected
                .as_ref()
                .is_none_or(|(invalid, _)| !invalid && proof.is_invalid())
        {
            selected = Some((proof.is_invalid(), name.clone()));
        }
    });
    selected.map_or_else(invalid, |(_, name)| Expr::Ident(name))
}
fn typ_kind(typ: &Typ, semantics: IntegerSemantics) -> Option<Kind> {
    let Typ::Named(name) = typ else { return None };
    Kind::parse(core_ir::rust_integer_type_name(name)?, semantics)
}

fn unsupported_type(
    typ: &Typ,
    facts: &Facts,
    semantics: IntegerSemantics,
    seen: &mut HashSet<String>,
) -> bool {
    match typ {
        Typ::Array(item) | Typ::Vector(item) => unsupported_type(item, facts, semantics, seen),
        Typ::Named(name) if name.starts_with("#rust-integer-") => {
            core_ir::rust_integer_type_name(name)
                .and_then(|name| Kind::parse(name, semantics))
                .is_none_or(|kind| !kind.supported(semantics))
        }
        Typ::Named(name) if seen.insert(name.clone()) => {
            if let Some(fields) = facts.structs.get(name) {
                fields
                    .iter()
                    .any(|(_, typ)| unsupported_type(typ, facts, semantics, seen))
            } else if let Some(fields) = core_ir::tuples::fields(name) {
                fields
                    .iter()
                    .any(|(_, typ)| unsupported_type(typ, facts, semantics, seen))
            } else {
                false
            }
        }
        _ => false,
    }
}

fn constant_integer(value: &Expr) -> Option<i64> {
    let folded = crate::core_opt::map_expr(value.clone(), &mut crate::core_opt::fold_expr);
    if let Expr::IntLit(value) = folded {
        Some(value)
    } else {
        None
    }
}
fn constant_overflow(kind: Kind, op: &str, left: &Expr, right: &Expr) -> bool {
    let (Some(left), Some(right)) = (constant_integer(left), constant_integer(right)) else {
        return false;
    };
    if kind.signed {
        let (left, right) = (i128::from(left), i128::from(right));
        let result = match op {
            "+" => Some(left + right),
            "-" => Some(left - right),
            "*" => Some(left * right),
            "/" if right != 0 => Some(left / right),
            "%" if right == -1 && left == -(1i128 << (kind.bits - 1)) => return true,
            _ => None,
        };
        let bound = 1i128 << (kind.bits - 1);
        result.is_some_and(|value| !(-bound..bound).contains(&value))
    } else {
        let (left, right) = (u128::from(left as u64), u128::from(right as u64));
        let result = match op {
            "+" => left.checked_add(right),
            "-" => left.checked_sub(right),
            "*" => left.checked_mul(right),
            _ => return false,
        };
        result.is_none_or(|value| value > ((1u128 << kind.bits) - 1))
    }
}

#[derive(Clone, Copy)]
enum OverflowPolicy {
    Checked,
    Wrapping,
}

/// Evaluate an exact-width checked operation after bounded operand evaluation.
pub(crate) fn fold_checked_integer(
    name: &str,
    op: &str,
    lhs: i64,
    rhs: Option<i64>,
    semantics: IntegerSemantics,
) -> Option<Expr> {
    fold_integer(name, op, lhs, rhs, semantics, OverflowPolicy::Checked)
}

pub(crate) fn fold_wrapping_integer(
    name: &str,
    op: &str,
    lhs: i64,
    rhs: Option<i64>,
    semantics: IntegerSemantics,
) -> Option<Expr> {
    fold_integer(name, op, lhs, rhs, semantics, OverflowPolicy::Wrapping)
}

fn fold_integer(
    name: &str,
    op: &str,
    lhs: i64,
    rhs: Option<i64>,
    semantics: IntegerSemantics,
    overflow: OverflowPolicy,
) -> Option<Expr> {
    let kind = Kind::parse(name, semantics)?;
    if !kind.supported(semantics) {
        return None;
    }
    let mask = (1u128 << kind.bits) - 1;
    let unsigned = |value: i64| u128::from(value as u64) & mask;
    let signed = |value: i64| {
        let value = unsigned(value) as i128;
        let sign = 1i128 << (kind.bits - 1);
        if value & sign != 0 {
            value - (1i128 << kind.bits)
        } else {
            value
        }
    };
    let carrier = |value: u128| {
        let value = value & mask;
        let sign = 1u128 << (kind.bits - 1);
        let value = if kind.signed && value & sign != 0 {
            (value as i128 - (1i128 << kind.bits)) as i64
        } else {
            value as u64 as i64
        };
        semantics.normalize_word(value).map(Expr::IntLit)
    };
    if matches!(overflow, OverflowPolicy::Wrapping) {
        let (left, right) = (unsigned(lhs), unsigned(rhs?));
        let value = match op {
            "+" => left.wrapping_add(right),
            "-" => left.wrapping_sub(right),
            "*" => left.wrapping_mul(right),
            _ => return None,
        };
        return carrier(value);
    }
    if matches!(op, "<<" | ">>" | "lshr") {
        let count = rhs?;
        if !(0..i64::from(kind.bits)).contains(&count) {
            return None;
        }
        let value = if op == "<<" {
            unsigned(lhs) << count
        } else if kind.signed && op != "lshr" {
            (signed(lhs) >> count) as u128
        } else {
            unsigned(lhs) >> count
        };
        return carrier(value);
    }
    if matches!(op, "~" | "bitnot" | "complement") {
        return carrier(!unsigned(lhs));
    }
    if kind.signed {
        let left = signed(lhs);
        let value = if let Some(rhs) = rhs {
            let right = signed(rhs);
            match op {
                "+" => left.checked_add(right)?,
                "-" => left.checked_sub(right)?,
                "*" => left.checked_mul(right)?,
                "/" => left.checked_div(right)?,
                "%" => {
                    if left == -(1i128 << (kind.bits - 1)) && right == -1 {
                        return None;
                    }
                    left.checked_rem(right)?
                }
                "&" => left & right,
                "|" => left | right,
                "^" | "xor" => left ^ right,
                _ => return None,
            }
        } else if op == "-" {
            left.checked_neg()?
        } else {
            return None;
        };
        let bound = 1i128 << (kind.bits - 1);
        if !(-bound..bound).contains(&value) {
            return None;
        }
        // Sign-extend narrow signed results into the canonical carrier.
        return semantics.normalize_word(value as i64).map(Expr::IntLit);
    }
    let left = unsigned(lhs);
    let right = unsigned(rhs?);
    let value = match op {
        "+" => left.checked_add(right)?,
        "-" => left.checked_sub(right)?,
        "*" => left.checked_mul(right)?,
        "/" | "udiv" => left.checked_div(right)?,
        "%" | "urem" => left.checked_rem(right)?,
        "&" => left & right,
        "|" => left | right,
        "^" | "xor" => left ^ right,
        _ => return None,
    };
    if value > mask {
        return None;
    }
    carrier(value)
}

struct Pass<'a> {
    checker: &'a TypeChecker,
    facts: &'a Facts,
    semantics: IntegerSemantics,
    inferred: HashMap<String, Kind>,
    flexible: HashSet<String>,
    opaque: HashSet<String>,
    rewrite: bool,
    return_type: Option<Typ>,
    const_env: Option<HashMap<String, i64>>,
}
impl Pass<'_> {
    fn validate_constant(&self, value: &mut Expr) {
        if self.rewrite
            && matches!(
                crate::lower_core::evaluate_pure_constant(value, self.semantics),
                Err(crate::lower_core::ConstantEvalError::Arithmetic)
            )
        {
            let previous = invalid_preserving_proof(value);
            let previous = match previous {
                Expr::Ident(name) => core_ir::type_proofs::decode(&name),
                _ => None,
            };
            if let Some(failure) = core_ir::type_proofs::prefer_failure(
                previous,
                Some(core_ir::type_proofs::ResolutionFailure::Invalid(
                    "constant integer arithmetic failed".into(),
                )),
            ) {
                *value = Expr::Ident(failure.symbol());
            }
        }
    }
    fn expected(&self, typ: Option<&Typ>) -> Option<Kind> {
        typ.and_then(|typ| typ_kind(typ, self.semantics))
    }
    fn folded_constant(&self, expr: &Expr) -> Expr {
        let values = self.const_env.as_ref();
        crate::core_opt::map_expr(expr.clone(), &mut |expr| {
            let expr = if let Expr::Ident(name) = &expr {
                primitive_constant(name, self.semantics)
                    .filter(|(kind, _)| kind.supported(self.semantics))
                    .map(|(_, value)| Expr::IntLit(value))
                    .or_else(|| {
                        values
                            .and_then(|values| values.get(name))
                            .copied()
                            .map(Expr::IntLit)
                    })
                    .unwrap_or(expr)
            } else {
                expr
            };
            let expr = if values.is_some() {
                if let Expr::Block {
                    result: Some(result),
                    ..
                } = expr
                {
                    *result
                } else {
                    expr
                }
            } else {
                expr
            };
            crate::core_opt::fold_expr(expr)
        })
    }
    fn constant(&self, expr: &Expr) -> Option<i64> {
        if let Expr::IntLit(value) = self.folded_constant(expr) {
            Some(value)
        } else {
            None
        }
    }
    fn pattern_hint(&self, pattern: &str) -> Option<Kind> {
        if let Ok(core_ir::MatchPattern::IdentPat(name)) = core_ir::MatchPattern::parse(pattern) {
            self.inferred.get(&name).copied()
        } else {
            None
        }
    }
    fn flexible_pattern(
        &mut self,
        inner: &HashMap<String, Typ>,
        outer: &HashMap<String, Typ>,
        unknown: bool,
    ) {
        if unknown {
            for name in inner.keys() {
                if !outer.contains_key(name) {
                    self.flexible.insert(name.clone());
                }
            }
        }
    }
    fn opaque_int(&self, expr: &Expr, env: &HashMap<String, Typ>) -> bool {
        let candidate = match expr {
            Expr::Ident(name) => self.opaque.contains(name) || !self.flexible.contains(name),
            Expr::Call { .. } | Expr::Field { .. } | Expr::Index { .. } => true,
            Expr::Binary { .. }
            | Expr::Block { .. }
            | Expr::Conditional { .. }
            | Expr::MatchValue { .. } => !self.contextual_integer(expr, env),
            _ => false,
        };
        candidate
            && self.hint(expr, env).is_none()
            && self.checker.expr_type(expr, self.facts, env) == Some(Typ::Int)
    }
    fn contextual_integer(&self, expr: &Expr, env: &HashMap<String, Typ>) -> bool {
        if core_ir::expr_diverges(expr) {
            return true;
        }
        match expr {
            Expr::Ident(name) => {
                self.hint(expr, env).is_some()
                    || (self.flexible.contains(name) && !self.opaque.contains(name))
            }
            Expr::Unary { op, .. } if op == "rust-integer-literal" => true,
            Expr::Unary { op, expr }
                if matches!(
                    op.as_str(),
                    "-" | "~" | "complement" | "bitnot" | core_ir::rust_references::NEGATE_VALUE
                ) =>
            {
                self.contextual_integer(expr, env)
            }
            Expr::Binary { op, lhs, rhs }
                if matches!(
                    op.as_str(),
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "xor" | "<<" | ">>"
                ) =>
            {
                self.contextual_integer(lhs, env)
                    && (matches!(op.as_str(), "<<" | ">>") || self.contextual_integer(rhs, env))
            }
            Expr::Block {
                result: Some(result),
                ..
            } => self.contextual_integer(result, env),
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => self.contextual_integer(then_expr, env) && self.contextual_integer(else_expr, env),
            Expr::MatchValue { arms, .. } => {
                !arms.is_empty()
                    && arms
                        .iter()
                        .all(|arm| self.contextual_integer(&arm.value, env))
            }
            _ => self.hint(expr, env).is_some(),
        }
    }
    fn hint(&self, expr: &Expr, env: &HashMap<String, Typ>) -> Option<Kind> {
        match expr {
            Expr::Call { callee, .. } => {
                if let Expr::Ident(name) = callee.as_ref()
                    && let Some((Some(kind), _)) = wrapping_target(name, self.semantics)
                {
                    return Some(kind);
                }
                self.checker
                    .expr_type(expr, self.facts, env)
                    .and_then(|typ| typ_kind(&typ, self.semantics))
            }
            Expr::Unary { op, expr } if op == "rust-integer-literal" => {
                let _ = expr;
                None
            }
            Expr::Unary { op, .. } if op.starts_with("rust-integer-value-") => {
                Kind::parse(op.strip_prefix("rust-integer-value-")?, self.semantics)
            }
            Expr::Unary { op, .. } if op.starts_with("rust-integer-literal-") => {
                Kind::parse(op.strip_prefix("rust-integer-literal-")?, self.semantics)
            }
            Expr::Unary { op, expr }
                if matches!(
                    op.as_str(),
                    "-" | "~" | "complement" | "bitnot" | core_ir::rust_references::NEGATE_VALUE
                ) =>
            {
                self.hint(expr, env)
            }
            Expr::Ident(name) => primitive_constant(name, self.semantics)
                .map(|(kind, _)| kind)
                .or_else(|| self.inferred.get(name).copied())
                .or_else(|| env.get(name).and_then(|typ| typ_kind(typ, self.semantics))),
            _ => self
                .checker
                .expr_type(expr, self.facts, env)
                .and_then(|typ| typ_kind(&typ, self.semantics)),
        }
    }
    fn value(
        &mut self,
        expr: &mut Expr,
        expected: Option<&Typ>,
        env: &mut HashMap<String, Typ>,
        ret: Option<Kind>,
    ) -> Option<Kind> {
        if expected == Some(&Typ::Int) && self.hint(expr, env).is_none() {
            return self.expression(
                expr,
                Some(Kind {
                    bits: if self.semantics == IntegerSemantics::Word32 {
                        32
                    } else {
                        64
                    },
                    signed: true,
                    source: false,
                }),
                env,
                ret,
            );
        }
        if self.rewrite
            && expected.is_some_and(|typ| {
                unsupported_type(typ, self.facts, self.semantics, &mut HashSet::new())
            })
        {
            *expr = invalid();
            return None;
        }
        if let Some(typ @ (Typ::Array(item) | Typ::Vector(item))) = expected {
            match expr {
                Expr::Unary { op, expr } if crate::core_ir::array_copy::count(op).is_some() => {
                    self.value(expr, Some(typ), env, ret);
                    return None;
                }
                Expr::Call { callee, args } if matches!(callee.as_ref(),Expr::Ident(name) if name == core_ir::INLINE_CONST) =>
                {
                    if let [value] = args.as_mut_slice() {
                        let outer = self.const_env.replace(HashMap::new());
                        self.value(value, Some(typ), env, ret);
                        self.validate_constant(value);
                        self.const_env = outer;
                    }
                    return None;
                }
                Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Closure { .. }) => {
                    if let Expr::Closure {
                        ret: closure_ret, ..
                    } = callee.as_mut()
                        && matches!(closure_ret,Typ::Generic(name) if name == "_")
                    {
                        *closure_ret = typ.clone();
                    }
                    return self.expression(expr, None, env, ret);
                }
                Expr::ArrayLit(items) => {
                    for value in items {
                        self.value(value, Some(item), env, ret);
                    }
                    return None;
                }
                Expr::Block { body, result } => {
                    let mut inner = env.clone();
                    self.body(body, &mut inner, ret);
                    if let Some(result) = result {
                        self.value(result, Some(typ), &mut inner, ret);
                    }
                    return None;
                }
                Expr::Conditional {
                    cond,
                    then_expr,
                    else_expr,
                } => {
                    self.expression(cond, None, env, ret);
                    self.value(then_expr, Some(typ), &mut env.clone(), ret);
                    self.value(else_expr, Some(typ), &mut env.clone(), ret);
                    return None;
                }
                Expr::MatchValue { scrutinee, arms } => {
                    let hint = arms.iter().find_map(|arm| self.pattern_hint(&arm.pattern));
                    let kind = self.expression(scrutinee, hint, env, ret);
                    let scrutinee_type = kind
                        .map(Kind::typ)
                        .or_else(|| self.checker.expr_type(scrutinee, self.facts, env));
                    for arm in arms {
                        let mut inner = scrutinee_type
                            .as_ref()
                            .and_then(|typ| {
                                self.checker
                                    .pattern_environment(&arm.pattern, typ, self.facts, env)
                            })
                            .unwrap_or_else(|| env.clone());
                        self.flexible_pattern(&inner, env, kind.is_none());
                        if let Some(guard) = &mut arm.guard {
                            self.expression(guard, None, &mut inner, ret);
                        }
                        self.value(&mut arm.value, Some(typ), &mut inner, ret);
                    }
                    return None;
                }
                _ => {}
            }
        }
        self.expression(expr, self.expected(expected), env, ret)
    }
    fn known_noninteger(&self, value: &Expr, env: &HashMap<String, Typ>) -> bool {
        if core_ir::expr_diverges(value) {
            return false;
        }
        match self.checker.expr_type(value, self.facts, env) {
            Some(
                Typ::Bool | Typ::Float | Typ::String | Typ::Array(_) | Typ::Vector(_) | Typ::Void,
            ) => true,
            Some(Typ::Named(name)) => {
                core_ir::rust_floats::is_proof(&name) || self.facts.structs.contains_key(&name)
            }
            _ => false,
        }
    }

    fn opaque_callee(&self, value: &Expr) -> bool {
        match value {
            Expr::Ident(name) => self.opaque.contains(name),
            Expr::Block {
                result: Some(result),
                ..
            } => self.opaque_callee(result),
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => self.opaque_callee(then_expr) || self.opaque_callee(else_expr),
            _ => false,
        }
    }

    fn expression(
        &mut self,
        expr: &mut Expr,
        expected: Option<Kind>,
        env: &mut HashMap<String, Typ>,
        ret: Option<Kind>,
    ) -> Option<Kind> {
        let mut bad = false;
        let mut mismatch = None;
        let kind = match expr {
            Expr::Ident(name) => {
                if let Some((kind, value)) = primitive_constant(name, self.semantics) {
                    if self.rewrite {
                        *expr = kind.cast(Expr::IntLit(value));
                    }
                    Some(kind)
                } else if primitive_member(name, self.semantics).is_some() {
                    if self.rewrite {
                        *expr = Expr::Ident(
                            core_ir::type_proofs::ResolutionFailure::Unsupported(
                                "primitive method values require callable-value typing".into(),
                            )
                            .symbol(),
                        );
                    }
                    None
                } else {
                    let known =
                        self.inferred.get(name).copied().or_else(|| {
                            env.get(name).and_then(|typ| typ_kind(typ, self.semantics))
                        });
                    if self.flexible.contains(name)
                        && !self.opaque.contains(name)
                        && let Some(expected) = expected
                    {
                        let selected =
                            known.map_or(expected, |known| known.prefer_source(expected));
                        self.inferred.insert(name.clone(), selected);
                    }
                    self.inferred.get(name).copied().or(known)
                }
            }
            Expr::Unary { op, expr: value } if op.starts_with("rust-integer-value-") => {
                let kind = op
                    .strip_prefix("rust-integer-value-")
                    .and_then(|name| Kind::parse(name, self.semantics));
                bad |= kind.is_none();
                self.expression(value, None, env, ret);
                if self.rewrite {
                    *expr = (**value).clone();
                }
                kind
            }
            Expr::Unary { op, expr: value }
                if op == "rust-integer-literal" || op.starts_with("rust-integer-literal-") =>
            {
                let suffix = op.strip_prefix("rust-integer-literal-");
                let explicit = suffix.and_then(|name| Kind::parse(name, self.semantics));
                bad |= suffix.is_some() && explicit.is_none();
                let kind = explicit.or(expected).or_else(|| {
                    self.rewrite.then_some(Kind {
                        bits: 32,
                        signed: true,
                        source: false,
                    })
                });
                if self.rewrite
                    && let Some(kind) = kind
                {
                    match value.as_ref() {
                        Expr::IntLit(integer) => {
                            // A u64 literal carries its full unsigned bit pattern.
                            bad |= !(kind.contains(*integer)
                                || (!kind.signed && kind.bits == 64 && suffix == Some("u64")));
                            if !bad {
                                let semantics = self.semantics;
                                *expr = crate::core_opt::map_expr(
                                    kind.cast(Expr::IntLit(*integer)),
                                    &mut |expr| {
                                        crate::core_opt::fold_expr_with_semantics(expr, semantics)
                                    },
                                );
                            }
                        }
                        _ => bad = true,
                    }
                }
                kind
            }
            Expr::Unary { op, expr: value } => {
                let name = op.clone();
                let cast = name
                    .strip_prefix("numeric-cast-")
                    .or_else(|| name.strip_prefix("int-cast-"))
                    .or_else(|| name.strip_prefix("float-saturate-"));
                let target = cast.and_then(|name| Kind::parse(name, self.semantics));
                let inner = self.expression(
                    value,
                    if cast.is_some() { None } else { expected },
                    env,
                    ret,
                );
                if let Some(target) = target {
                    if self.rewrite && inner.is_some() && !name.starts_with("float-saturate-") {
                        *expr = target.cast((**value).clone());
                    }
                    Some(target)
                } else if matches!(
                    name.as_str(),
                    "-" | "~" | "complement" | "bitnot" | core_ir::rust_references::NEGATE_VALUE
                ) {
                    if self.rewrite
                        && let Some(kind) = inner
                    {
                        let negate =
                            matches!(name.as_str(), "-" | core_ir::rust_references::NEGATE_VALUE);
                        bad |= negate && !kind.signed;
                        if negate && kind.signed && self.const_env.is_none() {
                            bad |= self.constant(value).is_some_and(|value| {
                                fold_checked_integer(
                                    &format!("i{}", kind.bits),
                                    "-",
                                    value,
                                    None,
                                    self.semantics,
                                )
                                .is_none()
                            });
                        }
                        if name == core_ir::rust_references::NEGATE_VALUE {
                            *op = "-".into();
                        }
                        // Constant control flow selects whether signed negation is evaluated.
                        if self.const_env.is_some() {
                            *expr = Expr::Unary {
                                op: format!(
                                    "rust-checked-integer-{}{}",
                                    if kind.signed { "i" } else { "u" },
                                    kind.bits
                                ),
                                expr: Box::new(expr.clone()),
                            };
                        } else {
                            *expr = kind.cast(expr.clone());
                        }
                    }
                    inner
                } else {
                    None
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                let operator = op.clone();
                let comparison = matches!(operator.as_str(), "==" | "!=" | "<" | "<=" | ">" | ">=");
                let shift = matches!(operator.as_str(), "<<" | ">>" | "lshr");
                let mut peer = self.hint(lhs, env);
                if !shift && let Some(right) = self.hint(rhs, env) {
                    peer = Some(peer.map_or(right, |left| left.prefer_source(right)));
                }
                if !comparison && let Some(expected) = expected {
                    peer = Some(peer.map_or(expected, |peer| peer.prefer_source(expected)));
                }
                let peer = peer.or_else(|| {
                    if self.opaque_int(lhs, env) || (!shift && self.opaque_int(rhs, env)) {
                        Some(Kind {
                            bits: if self.semantics == IntegerSemantics::Word32 {
                                32
                            } else {
                                64
                            },
                            signed: true,
                            source: false,
                        })
                    } else {
                        None
                    }
                });
                let left = self.expression(lhs, peer, env, ret);
                let right =
                    self.expression(rhs, if shift { None } else { peer.or(left) }, env, ret);
                // Legacy builtins and not-yet-inferred closure parameters use
                // canonical Int. Preserve that existing operation rather than
                // inventing a Rust width from its other operand.
                let kind = if !shift && (left.is_none() || right.is_none()) {
                    None
                } else {
                    left.or(if shift { None } else { right })
                };
                if shift
                    && self.rewrite
                    && let Some(kind) = left
                {
                    if let Some(count) = self.constant(rhs) {
                        bad |= !(0..i64::from(kind.bits)).contains(&count);
                    }
                }
                if !shift && left.is_some() && right.is_some() && !same_kinds(left, right) {
                    bad = true;
                    mismatch = left
                        .zip(right)
                        .map(|(left, right)| kind_mismatch(left, right));
                }
                if self.rewrite
                    && let Some(kind) = kind
                {
                    let constants = (self.constant(lhs), self.constant(rhs));
                    if self.const_env.is_none()
                        && let (Some(left), Some(right)) = constants
                    {
                        bad |= constant_overflow(
                            kind,
                            &operator,
                            &Expr::IntLit(left),
                            &Expr::IntLit(right),
                        );
                    }
                    // Constant control flow selects which arithmetic is evaluated.
                    let checked_constant = self.const_env.is_some()
                        && matches!(operator.as_str(), "+" | "-" | "*" | "/" | "%" | "<<" | ">>");
                    if !kind.supported(self.semantics) {
                        bad = true;
                    } else if checked_constant {
                        *expr = Expr::Unary {
                            op: format!(
                                "rust-checked-integer-{}{}",
                                if kind.signed { "i" } else { "u" },
                                kind.bits
                            ),
                            expr: Box::new(expr.clone()),
                        };
                    } else if !kind.signed && matches!(operator.as_str(), "<" | "<=" | ">" | ">=") {
                        *expr = core_ir::unsigned_compare_expr(
                            &operator,
                            (**lhs).clone(),
                            (**rhs).clone(),
                            u32::from(kind.bits),
                            self.semantics,
                        )
                        .unwrap_or_else(invalid);
                    } else {
                        if !kind.signed {
                            *op = match operator.as_str() {
                                "/" => "udiv",
                                "%" => "urem",
                                ">>" => "lshr",
                                _ => &operator,
                            }
                            .into();
                        }
                        if !comparison && !matches!(operator.as_str(), "&&" | "||") {
                            *expr = kind.cast(expr.clone());
                        }
                    }
                }
                if comparison || matches!(operator.as_str(), "&&" | "||") {
                    None
                } else {
                    kind
                }
            }
            Expr::Call { callee, args } if matches!(callee.as_ref(),Expr::Ident(name) if name == core_ir::INLINE_CONST) => {
                if let [value] = args.as_mut_slice() {
                    let outer = self.const_env.replace(HashMap::new());
                    let kind = self.expression(value, expected, env, ret);
                    self.validate_constant(value);
                    self.const_env = outer;
                    kind
                } else {
                    None
                }
            }
            Expr::Call { callee, args } if matches!(callee.as_ref(), Expr::Ident(name) if wrapping_target(name, self.semantics).is_some()) =>
            {
                let target = if let Expr::Ident(name) = callee.as_ref() {
                    wrapping_target(name, self.semantics)
                } else {
                    None
                };
                let Some((owner, operator)) = target else {
                    if self.rewrite {
                        *expr = Expr::Ident(
                            core_ir::type_proofs::ResolutionFailure::Unsupported(
                                "unresolved wrapping call target".into(),
                            )
                            .symbol(),
                        );
                    }
                    return None;
                };
                if let [receiver, argument] = args.as_mut_slice() {
                    let noninteger = owner.is_some()
                        && (self.known_noninteger(receiver, env)
                            || self.known_noninteger(argument, env));
                    let contextual = matches!(receiver,Expr::Unary{op,..} if op=="rust-integer-literal")
                        || matches!(receiver,Expr::Ident(name) if self.flexible.contains(name)&&!self.opaque.contains(name)&&env.get(name)==Some(&Typ::Int));
                    let target = owner.or_else(|| self.hint(receiver, env)).or_else(|| {
                        if contextual {
                            expected.or_else(|| self.hint(argument, env))
                        } else {
                            None
                        }
                    });
                    let receiver_kind = self.expression(receiver, target, env, ret);
                    let kind = owner.or(receiver_kind).or(target);
                    let argument_kind = self.expression(argument, kind, env, ret);
                    for actual in [receiver_kind, argument_kind] {
                        if !same_kinds(actual, kind) {
                            bad = true;
                            if let Some(proof) = actual
                                .zip(kind)
                                .map(|(actual, expected)| kind_mismatch(actual, expected))
                                && (!mismatch.as_ref().is_some_and(
                                    core_ir::type_proofs::ResolutionFailure::is_invalid,
                                ) || proof.is_invalid())
                            {
                                mismatch = Some(proof);
                            }
                        }
                    }
                    if owner.is_some()
                        && [receiver_kind, argument_kind]
                            .into_iter()
                            .flatten()
                            .any(|kind| !kind.source)
                    {
                        bad = true;
                        if !mismatch
                            .as_ref()
                            .is_some_and(core_ir::type_proofs::ResolutionFailure::is_invalid)
                        {
                            mismatch = Some(core_ir::type_proofs::ResolutionFailure::Unsupported(
                                "opaque canonical wrapping operands require source integer types"
                                    .into(),
                            ));
                        }
                    }
                    if kind.is_some_and(|kind| !kind.supported(self.semantics)) {
                        bad = true;
                        if !mismatch
                            .as_ref()
                            .is_some_and(core_ir::type_proofs::ResolutionFailure::is_invalid)
                        {
                            mismatch = Some(core_ir::type_proofs::ResolutionFailure::Unsupported(
                                "wrapping integer owner exceeds target word width".into(),
                            ));
                        }
                    } else if bad && mismatch.is_none() {
                        mismatch = Some(core_ir::type_proofs::ResolutionFailure::Unsupported(
                            "wrapping operands have no proven compatible Rust integer kind".into(),
                        ));
                    }
                    if noninteger {
                        bad = true;
                        mismatch = Some(core_ir::type_proofs::ResolutionFailure::Invalid(
                            "primitive wrapping operand is not an integer".into(),
                        ));
                    }
                    if self.rewrite
                        && !bad
                        && let Some(kind) = kind
                    {
                        let name = format!("{}{}", if kind.signed { "i" } else { "u" }, kind.bits);
                        let operation = Expr::Binary {
                            op: operator.into(),
                            lhs: Box::new(receiver.clone()),
                            rhs: Box::new(argument.clone()),
                        };
                        if let (Some(left), Some(right)) =
                            (constant_integer(receiver), constant_integer(argument))
                        {
                            *expr = fold_wrapping_integer(
                                &name,
                                operator,
                                left,
                                Some(right),
                                self.semantics,
                            )
                            .unwrap_or_else(invalid);
                        } else if self.const_env.is_some() {
                            *expr = Expr::Unary {
                                op: format!("rust-wrapping-integer-{name}"),
                                expr: Box::new(operation),
                            };
                        } else {
                            *expr = kind.cast(operation);
                        }
                    }
                    kind
                } else {
                    let proven = owner.is_some()
                        || args
                            .first()
                            .and_then(|value| self.hint(value, env))
                            .is_some_and(|kind| kind.source);
                    for value in args.iter_mut() {
                        self.expression(value, None, env, ret);
                    }
                    bad = true;
                    mismatch = Some(if proven {
                        core_ir::type_proofs::ResolutionFailure::Invalid(
                            "wrapping operation requires exactly two operands".into(),
                        )
                    } else {
                        core_ir::type_proofs::ResolutionFailure::Unsupported(
                            "unresolved wrapping receiver and arity".into(),
                        )
                    });
                    owner
                }
            }
            Expr::Call { callee, args } => {
                let signature = match callee.as_ref() {
                    Expr::Ident(name) => self.facts.functions.get(name).cloned().or_else(|| {
                        crate::core_atomic::signature(name)
                            .map(|signature| {
                                (
                                    signature
                                        .params
                                        .iter()
                                        .enumerate()
                                        .map(|(index, typ)| (format!("arg{index}"), typ.clone()))
                                        .collect(),
                                    signature.ret.clone(),
                                )
                            })
                            .or_else(|| {
                                let params = if name == core_ir::VECTOR_ALLOCATION
                                    || name == "range-from"
                                {
                                    vec![Typ::Int]
                                } else if matches!(
                                    name.as_str(),
                                    "range-exclusive" | "range-inclusive"
                                ) {
                                    vec![Typ::Int, Typ::Int]
                                } else {
                                    crate::core_ir_verifier::intrinsic_param_types(name)?.to_vec()
                                };
                                Some((
                                    params
                                        .into_iter()
                                        .enumerate()
                                        .map(|(index, typ)| (format!("arg{index}"), typ))
                                        .collect(),
                                    Typ::Int,
                                ))
                            })
                    }),
                    Expr::Closure { params, ret, .. } => Some((params.clone(), ret.clone())),
                    _ => None,
                };
                let vector_expected = if let Expr::Ident(name) = callee.as_ref()
                    && !self.facts.functions.contains_key(name)
                    && matches!(name.as_str(), "vector-push" | "vector-extend")
                    && let Some(receiver) = args.first()
                    && let Some(Typ::Vector(item)) =
                        self.checker.expr_type(receiver, self.facts, env)
                {
                    Some(if name == "vector-push" {
                        *item
                    } else {
                        Typ::Vector(item)
                    })
                } else {
                    None
                };
                // The callee may itself contain executable blocks or closures.
                // Traverse it once before arguments, matching source evaluation.
                let callee_kind = self.expression(callee, None, env, ret);
                let atomic = if let Expr::Ident(name) = callee.as_ref() {
                    crate::core_atomic::operation(name)
                        .and_then(|(kind, _)| Kind::parse(kind.suffix(), self.semantics))
                } else {
                    None
                };
                for (index, arg) in args.iter_mut().enumerate() {
                    if index == 1 && vector_expected.is_some() {
                        self.value(arg, vector_expected.as_ref(), env, ret);
                        continue;
                    }
                    if let Some(atomic_kind) = atomic
                        && signature
                            .as_ref()
                            .and_then(|(params, _)| params.get(index))
                            .is_some_and(|(_, typ)| *typ == Typ::Int)
                    {
                        self.expression(arg, Some(atomic_kind), env, ret);
                        continue;
                    }
                    self.value(
                        arg,
                        signature
                            .as_ref()
                            .and_then(|(params, _)| params.get(index))
                            .map(|(_, typ)| typ),
                        env,
                        ret,
                    );
                }
                if self.rewrite
                    && let Some(kind) = callee_kind
                {
                    let proof = if !kind.source && self.opaque_callee(callee) {
                        core_ir::type_proofs::ResolutionFailure::Unsupported(
                            "opaque canonical callee has no proven callable type".into(),
                        )
                    } else {
                        core_ir::type_proofs::ResolutionFailure::Invalid(
                            "integer value is not callable".into(),
                        )
                    };
                    **callee = Expr::Ident(proof.symbol());
                }
                signature
                    .as_ref()
                    .and_then(|(_, typ)| typ_kind(typ, self.semantics))
            }
            Expr::Block { body, result } => {
                let mut inner = env.clone();
                self.body(body, &mut inner, ret);
                result
                    .as_mut()
                    .and_then(|result| self.expression(result, expected, &mut inner, ret))
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => {
                self.expression(cond, None, env, ret);
                let left = self.expression(then_expr, expected, &mut env.clone(), ret);
                let right = self.expression(else_expr, expected.or(left), &mut env.clone(), ret);
                if left.is_some() && right.is_some() && !same_kinds(left, right) {
                    bad = true;
                    mismatch = left
                        .zip(right)
                        .map(|(left, right)| kind_mismatch(left, right));
                }
                left.or(right)
            }
            Expr::Closure {
                params,
                ret: closure_ret,
                body,
                ..
            } => {
                let mut inner = env.clone();
                inner.extend(params.iter().cloned());
                let outer = self.return_type.replace(closure_ret.clone());
                self.body(body, &mut inner, typ_kind(closure_ret, self.semantics));
                self.return_type = outer;
                if self.rewrite
                    && (unsupported_type(
                        closure_ret,
                        self.facts,
                        self.semantics,
                        &mut HashSet::new(),
                    ) || params.iter().any(|(_, typ)| {
                        unsupported_type(typ, self.facts, self.semantics, &mut HashSet::new())
                    }))
                {
                    body.insert(0, Stmt::Expr(invalid()));
                }
                None
            }
            Expr::StructInit { name, fields } => {
                bad |= unsupported_type(
                    &Typ::Named(name.clone()),
                    self.facts,
                    self.semantics,
                    &mut HashSet::new(),
                );
                let schema = self.facts.structs.get(name).cloned().unwrap_or_default();
                for (field, value) in fields {
                    let expected = schema
                        .iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, typ)| typ);
                    self.value(value, expected, env, ret);
                }
                None
            }
            Expr::Field { .. } | Expr::Index { .. } => {
                let kind = self
                    .checker
                    .expr_type(expr, self.facts, env)
                    .and_then(|typ| typ_kind(&typ, self.semantics));
                core_ir::for_each_expr_child_mut(expr, &mut |child| {
                    self.expression(child, None, env, ret);
                });
                kind
            }
            Expr::MatchValue { scrutinee, arms } => {
                let hint = arms.iter().find_map(|arm| self.pattern_hint(&arm.pattern));
                let scrutinee_kind = self.expression(scrutinee, hint, env, ret);
                let typ = scrutinee_kind
                    .map(Kind::typ)
                    .or_else(|| self.checker.expr_type(scrutinee, self.facts, env));
                let mut kind = expected;
                for arm in arms {
                    let mut inner = typ
                        .as_ref()
                        .and_then(|typ| {
                            self.checker
                                .pattern_environment(&arm.pattern, typ, self.facts, env)
                        })
                        .unwrap_or_else(|| env.clone());
                    self.flexible_pattern(&inner, env, scrutinee_kind.is_none());
                    if let Some(guard) = &mut arm.guard {
                        self.expression(guard, None, &mut inner, ret);
                    }
                    let value = self.expression(&mut arm.value, kind, &mut inner, ret);
                    if kind.is_some() && value.is_some() && !same_kinds(kind, value) {
                        bad = true;
                        mismatch = kind
                            .zip(value)
                            .map(|(expected, actual)| kind_mismatch(actual, expected));
                    }
                    kind = kind.or(value);
                }
                kind
            }
            _ => {
                core_ir::for_each_expr_child_mut(expr, &mut |child| {
                    self.expression(child, None, env, ret);
                });
                None
            }
        };
        bad |= kind.is_some_and(|kind| !kind.supported(self.semantics));
        if let Some((actual, expected)) = kind
            .zip(expected)
            .filter(|(actual, expected)| !actual.same_width(*expected))
        {
            let proof = kind_mismatch(actual, expected);
            if !mismatch
                .as_ref()
                .is_some_and(core_ir::type_proofs::ResolutionFailure::is_invalid)
                || proof.is_invalid()
            {
                mismatch = Some(proof);
            }
        }
        if self.rewrite
            && let Some(proof) = mismatch
        {
            let preserved = invalid_preserving_proof(expr);
            *expr = if !proof.is_invalid()
                && matches!(&preserved, Expr::Ident(name) if core_ir::type_proofs::decode(name).is_some_and(|proof| proof.is_invalid()))
            {
                preserved
            } else {
                Expr::Ident(proof.symbol())
            };
        } else if bad && self.rewrite {
            *expr = invalid_preserving_proof(expr);
        }
        kind
    }
    fn body(&mut self, body: &mut [Stmt], env: &mut HashMap<String, Typ>, ret: Option<Kind>) {
        for stmt in body {
            match stmt {
                Stmt::Declare(name, typ) => {
                    self.flexible.remove(name);
                    self.opaque.remove(name);
                    self.inferred.remove(name);
                    if *typ == Typ::Int {
                        self.opaque.insert(name.clone());
                    }
                    if let Some(kind) = self.expected(Some(typ)) {
                        self.inferred.insert(name.clone(), kind);
                    }
                    if let Some(constants) = &mut self.const_env {
                        constants.remove(name);
                    }
                    env.insert(name.clone(), typ.clone());
                }
                Stmt::Let(name, annotation, value) => {
                    // Canonical Int is a lowering carrier, not an explicit Rust width.
                    let carrier_kind = if *annotation == Some(Typ::Int) {
                        let kind =
                            self.expression(value, self.inferred.get(name).copied(), env, ret);
                        if self.contextual_integer(value, env) {
                            *annotation = None;
                        } else {
                            self.opaque.insert(name.clone());
                        }
                        Some(kind)
                    } else {
                        None
                    };
                    if annotation.is_none() {
                        self.flexible.insert(name.clone());
                    }
                    let expected = self
                        .expected(annotation.as_ref())
                        .or_else(|| self.inferred.get(name).copied());
                    let kind = if let Some(kind) = carrier_kind {
                        kind
                    } else if annotation.is_some() {
                        self.value(value, annotation.as_ref(), env, ret)
                    } else {
                        self.expression(value, expected, env, ret)
                    };
                    if kind.is_none() && self.opaque_int(value, env) {
                        self.opaque.insert(name.clone());
                    }
                    if self.rewrite && self.const_env.is_some() {
                        let value = self.constant(value);
                        if let Some(constants) = &mut self.const_env {
                            if let Some(value) = value {
                                constants.insert(name.clone(), value);
                            } else {
                                constants.remove(name);
                            }
                        }
                    }
                    let selected = if self.flexible.contains(name) && !self.opaque.contains(name) {
                        match (expected, kind) {
                            (Some(expected), Some(actual)) => Some(expected.prefer_source(actual)),
                            _ => expected.or(kind),
                        }
                    } else {
                        expected.or(kind)
                    };
                    if let Some(kind) = selected {
                        if self.rewrite && annotation.is_none() {
                            *annotation = Some(kind.typ());
                        }
                        self.inferred.insert(name.clone(), kind);
                        env.insert(name.clone(), kind.typ());
                    } else if let Some(typ) = annotation
                        .clone()
                        .or_else(|| self.checker.expr_type(value, self.facts, env))
                    {
                        env.insert(name.clone(), typ);
                    }
                }
                Stmt::Assign(name, value) => {
                    let expected =
                        self.inferred.get(name).copied().or_else(|| {
                            env.get(name).and_then(|typ| typ_kind(typ, self.semantics))
                        });
                    let actual = if expected.is_some() {
                        self.expression(value, expected, env, ret)
                    } else {
                        let typ = env.get(name).cloned();
                        self.value(value, typ.as_ref(), env, ret)
                    };
                    if !self.rewrite
                        && self.flexible.contains(name)
                        && !self.opaque.contains(name)
                        && let Some(actual) = actual
                    {
                        self.inferred.insert(
                            name.clone(),
                            expected.map_or(actual, |expected| expected.prefer_source(actual)),
                        );
                    }
                    if self.rewrite && self.const_env.is_some() {
                        let value = self.constant(value);
                        if let Some(constants) = &mut self.const_env {
                            if let Some(value) = value {
                                constants.insert(name.clone(), value);
                            } else {
                                constants.remove(name);
                            }
                        }
                    }
                }
                Stmt::Return(Some(value)) => {
                    let expected = self.return_type.clone();
                    self.value(value, expected.as_ref(), env, ret);
                }
                Stmt::Expr(value) | Stmt::Throw(value) => {
                    self.expression(value, None, env, ret);
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    self.expression(cond, None, env, ret);
                    let condition = match self.folded_constant(cond) {
                        Expr::BoolLit(value) => Some(value),
                        _ => None,
                    };
                    let constants = self.const_env.clone();
                    self.body(then_body, &mut env.clone(), ret);
                    let then_constants = self.const_env.clone();
                    self.const_env = constants;
                    self.body(else_body, &mut env.clone(), ret);
                    if condition == Some(true) {
                        self.const_env = then_constants;
                    } else if condition.is_none() {
                        if let (Some(left), Some(right)) = (then_constants, self.const_env.as_mut())
                        {
                            right.retain(|name, value| left.get(name) == Some(value));
                        }
                    }
                }
                Stmt::Loop { cond, body, .. } => {
                    if let Some(constants) = &mut self.const_env {
                        constants.clear();
                    }
                    if let Some(cond) = cond {
                        self.expression(cond, None, env, ret);
                    }
                    self.body(body, &mut env.clone(), ret);
                    if let Some(constants) = &mut self.const_env {
                        constants.clear();
                    }
                }
                Stmt::FieldAssign { base, name, value } => {
                    let typ = self.checker.expr_type(
                        &Expr::Field {
                            base: Box::new(base.clone()),
                            name: name.clone(),
                        },
                        self.facts,
                        env,
                    );
                    self.expression(base, None, env, ret);
                    self.value(value, typ.as_ref(), env, ret);
                }
                Stmt::IndexAssign { base, index, value } => {
                    let typ = self.checker.expr_type(
                        &Expr::Index {
                            base: Box::new(base.clone()),
                            index: Box::new(index.clone()),
                        },
                        self.facts,
                        env,
                    );
                    self.expression(base, None, env, ret);
                    self.expression(index, None, env, ret);
                    self.value(value, typ.as_ref(), env, ret);
                }
                Stmt::Match { scrutinee, arms } => {
                    let hint = arms.iter().find_map(|arm| self.pattern_hint(&arm.pattern));
                    let scrutinee_kind = self.expression(scrutinee, hint, env, ret);
                    let typ = scrutinee_kind
                        .map(Kind::typ)
                        .or_else(|| self.checker.expr_type(scrutinee, self.facts, env));
                    for arm in arms {
                        let mut inner = typ
                            .as_ref()
                            .and_then(|typ| {
                                self.checker
                                    .pattern_environment(&arm.pattern, typ, self.facts, env)
                            })
                            .unwrap_or_else(|| env.clone());
                        self.flexible_pattern(&inner, env, scrutinee_kind.is_none());
                        if let Some(guard) = &mut arm.guard {
                            self.expression(guard, None, &mut inner, ret);
                        }
                        self.body(&mut arm.body, &mut inner, ret);
                    }
                }
                Stmt::Try { body, catches } => {
                    self.body(body, &mut env.clone(), ret);
                    for catch in catches {
                        self.body(&mut catch.body, &mut env.clone(), ret);
                    }
                }
                Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
            }
        }
    }
}

fn origin_marker(stmt: &Stmt) -> bool {
    matches!(stmt, Stmt::Expr(Expr::Unary { op, expr })
        if op == core_ir::RUST_DECL_ORIGIN && matches!(expr.as_ref(), Expr::UnitLit))
}

/// Attach ownership only where Rust integer syntax does not already identify it.
/// Normalization consumes this metadata before any executable body is lowered.
pub(crate) fn mark_declaration_origins(decls: &mut [Decl]) {
    for decl in decls {
        let needs_origin = !decl_has_metadata(decl);
        match decl {
            Decl::Function { body, .. } if needs_origin && !body.is_empty() => {
                // Preserve terminal returns for other frontend consumers.
                let at = match body.last() {
                    Some(Stmt::Return(_)) => body.len() - 1,
                    _ => body.len(),
                };
                body.insert(
                    at,
                    Stmt::Expr(Expr::Unary {
                        op: core_ir::RUST_DECL_ORIGIN.into(),
                        expr: Box::new(Expr::UnitLit),
                    }),
                );
            }
            Decl::Class { methods, .. } => mark_declaration_origins(methods),
            _ => {}
        }
    }
}

fn decl_has_metadata(value: &Decl) -> bool {
    fn typ(ty: &Typ) -> bool {
        match ty {
            Typ::Named(name) => name.starts_with("#rust-integer-"),
            Typ::Array(item) | Typ::Vector(item) => typ(item),
            _ => false,
        }
    }
    fn expr(value: &Expr) -> bool {
        if matches!(value, Expr::Ident(name) if primitive_member(name, IntegerSemantics::Word64).is_some())
        {
            return true;
        }
        if matches!(value, Expr::Unary { op, expr } if op == core_ir::RUST_DECL_ORIGIN && matches!(expr.as_ref(), Expr::UnitLit))
        {
            return true;
        }
        if matches!(value,Expr::Unary{op,..} if (op.starts_with("rust-integer-literal") || op.starts_with("rust-integer-value-")))
        {
            return true;
        }
        if let Expr::Block { body, .. } = value
            && stmts(body)
        {
            return true;
        }
        if let Expr::Closure {
            params, ret, body, ..
        } = value
            && (typ(ret) || params.iter().any(|(_, ty)| typ(ty)) || stmts(body))
        {
            return true;
        }
        let mut found = false;
        core_ir::for_each_expr_child(value, &mut |value| {
            found |= expr(value);
        });
        found
    }
    fn stmts(body: &[Stmt]) -> bool {
        body.iter().any(|stmt| match stmt {
            Stmt::Declare(_, annotation) => typ(annotation),
            Stmt::Let(_, annotation, value) => annotation.as_ref().is_some_and(typ) || expr(value),
            Stmt::Assign(_, value)
            | Stmt::Expr(value)
            | Stmt::Throw(value)
            | Stmt::Return(Some(value)) => expr(value),
            Stmt::FieldAssign { base, value, .. } => expr(base) || expr(value),
            Stmt::IndexAssign { base, index, value } => expr(base) || expr(index) || expr(value),
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => expr(cond) || stmts(then_body) || stmts(else_body),
            Stmt::Loop { cond, body, .. } => cond.as_ref().is_some_and(expr) || stmts(body),
            Stmt::Match { scrutinee, arms } => {
                expr(scrutinee)
                    || arms
                        .iter()
                        .any(|arm| arm.guard.as_ref().is_some_and(expr) || stmts(&arm.body))
            }
            Stmt::Try { body, catches } => {
                stmts(body) || catches.iter().any(|catch| stmts(&catch.body))
            }
            _ => false,
        })
    }
    fn decl(value: &Decl) -> bool {
        match value {
            Decl::Function {
                params, ret, body, ..
            } => typ(ret) || params.iter().any(|(_, ty)| typ(ty)) || stmts(body),
            Decl::Global { typ: ty, init, .. } => typ(ty) || init.as_deref().is_some_and(expr),
            Decl::Struct { fields, .. } => fields.iter().any(|(_, ty)| typ(ty)),
            Decl::Class {
                fields, methods, ..
            } => fields.iter().any(|(_, ty)| typ(ty)) || methods.iter().any(decl),
            _ => false,
        }
    }
    decl(value)
}

pub(crate) fn has_metadata(module: &UnifiedModule) -> bool {
    module.decls.iter().any(decl_has_metadata)
}

pub(crate) fn normalize(
    checker: &TypeChecker,
    module: &mut UnifiedModule,
    semantics: IntegerSemantics,
) {
    if !has_metadata(module) {
        return;
    }
    crate::core_bindings::normalize_decls(&mut module.decls);
    // Expose ordered updates while their destination integer kinds remain available.
    crate::lower_core::lower_value_spills(&mut module.decls);
    let facts = checker.collect_facts(module);
    let globals: HashMap<_, _> = module
        .decls
        .iter()
        .filter_map(|decl| {
            if let Decl::Global { name, typ, .. } = decl {
                Some((name.clone(), typ.clone()))
            } else {
                None
            }
        })
        .collect();
    for decl in &mut module.decls {
        if !decl_has_metadata(decl) {
            // A nominal signature can still contain a Rust field wider than
            // the target, even when its body comes from another frontend.
            match decl {
                Decl::Function {
                    params, ret, body, ..
                } if params.iter().any(|(_, typ)| {
                    unsupported_type(typ, &facts, semantics, &mut HashSet::new())
                }) || unsupported_type(ret, &facts, semantics, &mut HashSet::new()) =>
                {
                    body.insert(0, Stmt::Expr(invalid()));
                }
                Decl::Global { typ, init, .. }
                    if unsupported_type(typ, &facts, semantics, &mut HashSet::new()) =>
                {
                    *init = Some(Box::new(invalid()));
                }
                _ => {}
            }
            continue;
        }
        let mut pass = Pass {
            checker,
            facts: &facts,
            semantics,
            inferred: HashMap::new(),
            flexible: HashSet::new(),
            opaque: HashSet::new(),
            rewrite: false,
            return_type: None,
            const_env: None,
        };
        match decl {
            Decl::Function {
                params, ret, body, ..
            } => {
                body.retain(|stmt| !origin_marker(stmt));
                pass.return_type = Some(ret.clone());
                let mut initial = globals.clone();
                initial.extend(params.iter().cloned());
                loop {
                    let before = pass.inferred.clone();
                    pass.body(body, &mut initial.clone(), typ_kind(ret, semantics));
                    if before == pass.inferred {
                        break;
                    }
                }
                pass.rewrite = true;
                pass.body(body, &mut initial, typ_kind(ret, semantics));
                if params
                    .iter()
                    .any(|(_, typ)| unsupported_type(typ, &facts, semantics, &mut HashSet::new()))
                    || unsupported_type(ret, &facts, semantics, &mut HashSet::new())
                {
                    body.insert(0, Stmt::Expr(invalid()));
                }
            }
            Decl::Global {
                typ,
                init: Some(value),
                ..
            } => {
                loop {
                    let before = pass.inferred.clone();
                    pass.value(value, Some(typ), &mut globals.clone(), None);
                    if before == pass.inferred {
                        break;
                    }
                }
                pass.rewrite = true;
                pass.const_env = Some(HashMap::new());
                pass.value(value, Some(typ), &mut globals.clone(), None);
                pass.validate_constant(value);
                if unsupported_type(typ, &facts, semantics, &mut HashSet::new()) {
                    **value = invalid();
                }
            }
            _ => {}
        }
    }
    crate::lower_core::validate_constant_arithmetic_with_semantics(&mut module.decls, semantics);
    core_ir::tuples::project_declarations(&mut module.decls, |decls| {
        crate::core_bindings::rewrite_named_types(decls, &|name| {
            core_ir::rust_integer_type_name(name)
                .and_then(|name| Kind::parse(name, semantics))
                .filter(|kind| kind.supported(semantics))
                .map(|_| Typ::Int)
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn resolved(source: &str, semantics: IntegerSemantics) -> UnifiedModule {
        let mut module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        let checker = TypeChecker::new();
        super::super::rust_arrays::normalize(&checker, &mut module);
        checker.resolve_complements(&mut module);
        normalize(&checker, &mut module, semantics);
        module
    }
    #[test]
    fn direct_primitive_wrapping_uses_owner_width_and_normalizes_once() {
        for source in [
            "fn main()->u8{u8::wrapping_add(255,43)}",
            "fn main()->u8{u8::wrapping_sub(0,214)}",
            "fn main()->i8{i8::wrapping_add(-128i8,-86i8)}",
            "fn main()->i8{i8::wrapping_mul(-7i8,-6i8)}",
            "fn main()->u64{u64::wrapping_mul(u64::MAX,18446744073709551574u64)}",
            "fn main()->u64{u64::wrapping_sub(0u64,18446744073709551574u64)}",
            "fn main()->u16{core::primitive::u16::wrapping_add(65535u16,43u16)}",
            "fn main()->u32{::std::primitive::u32::wrapping_add(u32::MAX,43u32)}",
            "fn main()->usize{usize::wrapping_add(usize::MAX,43usize)}",
        ] {
            let mut module = resolved(source, IntegerSemantics::Word64);
            let value = crate::core_opt::map_expr(
                return_value(&module).clone(),
                &mut crate::core_opt::fold_expr,
            );
            assert_eq!(value, Expr::IntLit(42), "{source}");
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{source}"
            );
            let once = module.clone();
            normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
            assert_eq!(module, once);
        }
        for semantics in [IntegerSemantics::Word32, IntegerSemantics::Word64] {
            let module = resolved(
                "fn main()->usize{usize::wrapping_add(usize::MAX,43usize)}",
                semantics,
            );
            let value = crate::core_opt::map_expr(return_value(&module).clone(), &mut |value| {
                crate::core_opt::fold_expr_with_semantics(value, semantics)
            });
            assert_eq!(value, Expr::IntLit(42));
        }
        let module = resolved(
            "fn main()->i32{u64::wrapping_add(1u64,2u64) as i32}",
            IntegerSemantics::Word32,
        );
        let proofs = TypeChecker::new().typed_proof_nodes(&module.decls);
        assert!(!proofs.is_empty());
        assert!(
            proofs.values().all(|proof| !proof.is_invalid()),
            "{proofs:?}"
        );
    }

    #[test]
    fn direct_primitive_wrapping_retains_source_errors_and_unproven_refusals() {
        for source in [
            "fn main()->i64{if false{u8::wrapping_add(1u16,2u8);}42}",
            "fn main()->i64{if false{u8::wrapping_sub(1u8,2u16);}42}",
            "fn main()->i64{if false{u8::wrapping_mul(1u8);}42}",
            "fn main()->i64{if false{let value:u64=u8::wrapping_add(1u8,2u8);}42}",
            r#"fn main(args:Vec<String>)->i64{u8::wrapping_add("x",1);42}"#,
            "fn main()->i64{if false{u8::wrapping_add(true,1);}42}",
            "fn main()->i64{if false{u8::wrapping_add(1,1.0);}42}",
            "fn main()->i64{if false{u8::wrapping_add(1.0f32,1);}42}",
            "fn main()->i64{if false{u8::wrapping_add(1,1.0f64 as f32);}42}",
            "fn main()->i64{let value:f32=1.0;if false{u8::wrapping_add(1,value);}42}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .values()
                    .any(|proof| proof.is_invalid()),
                "{source}: {module:?}"
            );
        }
        for kind in [
            core_ir::rust_floats::Kind::F32,
            core_ir::rust_floats::Kind::F64,
        ] {
            let mut module = constant_module(
                Expr::Call {
                    callee: Box::new(Expr::Ident("#rust-integer-u8::wrapping_add".into())),
                    args: vec![
                        Expr::IntLit(1),
                        core_ir::rust_floats::value(Expr::FloatLit(core_ir::FloatVal(1.0)), kind),
                    ],
                },
                Typ::Int,
            );
            normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .values()
                    .any(|proof| proof.is_invalid()),
                "private float value {kind:?}: {module:?}"
            );
        }
        for source in [
            "fn main()->i64{if false{u64::NO_SUCH_MEMBER();}42}",
            "fn main()->i64{let method=u8::wrapping_add;42}",
            "struct Row;trait Build{type Output;fn make();}impl Build for Row{type Output=u8;fn make(){let method=Self::Output::wrapping_add;}}fn main()->i64{<Row as Build>::make();42}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            let proofs = TypeChecker::new().typed_proof_nodes(&module.decls);
            assert!(!proofs.is_empty(), "{source}");
            assert!(
                proofs.values().all(|proof| !proof.is_invalid()),
                "{source}: {proofs:?}"
            );
        }
        let mut raw = constant_module(
            Expr::Call {
                callee: Box::new(Expr::Ident("#rust-integer-u8::wrapping_add".into())),
                args: vec![Expr::IntLit(1), Expr::IntLit(2)],
            },
            Typ::Int,
        );
        assert!(has_metadata(&raw));
        normalize(&TypeChecker::new(), &mut raw, IntegerSemantics::Word64);
        let proofs = TypeChecker::new().typed_proof_nodes(&raw.decls);
        assert!(!proofs.is_empty());
        assert!(proofs.values().all(|proof| !proof.is_invalid()));
    }

    #[test]
    fn direct_wrapping_keeps_effect_order_and_scoped_associated_owners() {
        let module = resolved(
            "fn left()->u8{255}fn right()->u8{43}fn main()->u8{u8::wrapping_add(left(),right())}",
            IntegerSemantics::Word64,
        );
        let mut main = module
            .decls
            .iter()
            .filter(|decl| matches!(decl,Decl::Function { name, .. } if name=="main"))
            .cloned()
            .collect::<Vec<_>>();
        let mut calls = vec![];
        crate::core_opt::map_decl_exprs_mut(&mut main, &mut |value| {
            if let Expr::Call { callee, .. } = value
                && let Expr::Ident(name) = callee.as_ref()
            {
                calls.push(name.clone());
            }
        });
        assert_eq!(calls, ["left", "right"]);
        let module = resolved(
            "struct Row;trait Build{type Actual;type Output;fn make()->Self::Output;}impl Build for Row{type Actual=u8;type Output=Self::Actual;fn make()->Self::Output{Self::Output::wrapping_add(255,43)}}fn main()->u8{<Row as Build>::make()}",
            IntegerSemantics::Word64,
        );
        assert!(
            TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty(),
            "{module:?}"
        );
        let mut view = module.decls.clone();
        crate::core_opt::map_decl_exprs_mut(&mut view, &mut |value| {
            assert!(
                !matches!(value,Expr::Ident(name) if wrapping_target(name, IntegerSemantics::Word64).is_some())
            );
        });
    }

    #[test]
    fn canonical_width_constraints_do_not_become_source_errors() {
        let source = include_str!("../../tests/fixtures/rust_integer_inference_selfhost.rs");
        let module = resolved(source, IntegerSemantics::Word64);
        let proofs = TypeChecker::new().typed_proof_nodes(&module.decls);
        assert!(
            proofs.values().all(|proof| !proof.is_invalid()),
            "{proofs:?}"
        );
        let module = resolved(
            "fn next(at:usize)->usize{at+1}fn main()->i64{let mut at=0;at=next(at);42}",
            IntegerSemantics::Word64,
        );
        assert!(
            TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .values()
                .all(|proof| !proof.is_invalid())
        );
        let module = resolved(
            "fn next(at:usize)->usize{at+1}fn main()->i64{let mut opening=1;let delimiter=opening+1;let mut at=delimiter;at=next(at);if at==3usize{42}else{7}}",
            IntegerSemantics::Word64,
        );
        assert!(
            TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty(),
            "{module:?}"
        );
        let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
        assert!(report.ok, "{report:?}");
        let source = Kind::parse("u64", IntegerSemantics::Word64).unwrap();
        let weak = Kind {
            source: false,
            ..source
        };
        assert_ne!(weak, source);
        assert_eq!(weak.typ(), Typ::Int);
        assert_eq!(typ_kind(&weak.typ(), IntegerSemantics::Word64), None);
        assert_eq!(
            typ_kind(&source.typ(), IntegerSemantics::Word64),
            Some(source)
        );
        assert!(weak.same_width(source));
        assert_eq!(weak.prefer_source(source), source);
        assert_eq!(source.prefer_source(weak), source);
        assert!(
            !kind_mismatch(
                source,
                Kind {
                    bits: 64,
                    signed: true,
                    source: false
                }
            )
            .is_invalid()
        );
        assert!(
            kind_mismatch(
                source,
                Kind::parse("i64", IntegerSemantics::Word64).unwrap()
            )
            .is_invalid()
        );
    }

    #[test]
    fn source_mismatches_and_unsuffixed_integer_callees_remain_invalid() {
        for source in [
            "fn main()->i64{if false{let value:i64=u8::MAX;}42}",
            "fn main()->i64{if false{let value=1u8+2u16;}42}",
            "fn main()->i64{if false{42();}42}",
            "fn main()->i64{let value=42;if false{value();}42}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            let proofs = TypeChecker::new().typed_proof_nodes(&module.decls);
            assert!(
                proofs.values().any(|proof| proof.is_invalid()),
                "{source}: {proofs:?}"
            );
        }
    }

    #[test]
    fn opaque_canonical_callback_carriers_are_not_proven_integer_callees() {
        let mut module = UnifiedModule::new(vec![Decl::Function {
            name: "invoke".into(),
            params: vec![("callback".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![
                Stmt::Let(
                    "value".into(),
                    Some(Typ::Int),
                    Expr::Ident("callback".into()),
                ),
                Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("value".into())),
                    args: vec![],
                })),
                Stmt::Expr(Expr::Unary {
                    op: core_ir::RUST_DECL_ORIGIN.into(),
                    expr: Box::new(Expr::UnitLit),
                }),
            ],
            type_params: vec![],
        }]);
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        let proofs = TypeChecker::new().typed_proof_nodes(&module.decls);
        assert!(
            proofs.values().all(|proof| !proof.is_invalid()),
            "{proofs:?}"
        );
    }

    fn constant_module(value: Expr, ret: Typ) -> UnifiedModule {
        UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret,
            body: vec![Stmt::Return(Some(value))],
            type_params: vec![],
        }])
    }
    fn constant_path(kind: &str, member: &str) -> Expr {
        Expr::Ident(format!("#rust-integer-{kind}::{member}"))
    }
    fn return_value(module: &UnifiedModule) -> &Expr {
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!("function")
        };
        let Stmt::Return(Some(value)) = &body[0] else {
            panic!("return")
        };
        value
    }
    #[test]
    fn private_primitive_constants_preserve_target_width_and_consume_metadata() {
        for (name, minimum, maximum, bits) in [
            ("i8", -128, 127, 8),
            ("u8", 0, 255, 8),
            ("i16", -32768, 32767, 16),
            ("u16", 0, 65535, 16),
            ("i32", -2147483648, 2147483647, 32),
            ("u32", 0, 4294967295, 32),
            ("i64", i64::MIN, i64::MAX, 64),
            ("u64", 0, -1, 64),
        ] {
            for semantics in [IntegerSemantics::Word32, IntegerSemantics::Word64] {
                for (member, expected) in [("MIN", minimum), ("MAX", maximum), ("BITS", bits)] {
                    let mut module = constant_module(constant_path(name, member), Typ::Int);
                    assert!(has_metadata(&module));
                    normalize(&TypeChecker::new(), &mut module, semantics);
                    let value =
                        crate::core_opt::map_expr(return_value(&module).clone(), &mut |value| {
                            crate::core_opt::fold_expr_with_semantics(value, semantics)
                        });
                    if semantics == IntegerSemantics::Word32 && bits == 64 && member != "BITS" {
                        assert_eq!(value, invalid(), "{name}::{member}");
                    } else {
                        let expected = semantics.normalize_word(expected).unwrap();
                        assert_eq!(
                            value,
                            Expr::IntLit(expected),
                            "{name}::{member} {semantics:?}"
                        );
                    }
                    assert!(!has_metadata(&module));
                    let once = module.clone();
                    normalize(&TypeChecker::new(), &mut module, semantics);
                    assert_eq!(module, once);
                }
            }
        }
        for semantics in [IntegerSemantics::Word32, IntegerSemantics::Word64] {
            let bits = if semantics == IntegerSemantics::Word32 {
                32
            } else {
                64
            };
            let signed_max = if bits == 32 {
                i64::from(i32::MAX)
            } else {
                i64::MAX
            };
            for (kind, member, expected) in [
                ("isize", "MIN", -signed_max - 1),
                ("isize", "MAX", signed_max),
                ("usize", "MIN", 0),
                ("usize", "MAX", if bits == 32 { 4294967295 } else { -1 }),
                ("isize", "BITS", bits),
                ("usize", "BITS", bits),
            ] {
                assert_eq!(
                    primitive_constant(&format!("#rust-integer-{kind}::{member}"), semantics)
                        .unwrap()
                        .1,
                    expected
                );
            }
        }
        for path in [
            "u64::MAX",
            "domain::u64::MAX",
            "#rust-integer-u64::OTHER",
            "#rust-integer-i128::MAX",
        ] {
            assert!(
                primitive_constant(path, IntegerSemantics::Word64).is_none(),
                "{path}"
            );
        }
    }
    #[test]
    fn primitive_constant_kinds_drive_unsigned_operations_and_reject_mismatches() {
        let unsigned = Expr::Binary {
            op: ">".into(),
            lhs: Box::new(constant_path("u64", "MAX")),
            rhs: Box::new(Expr::Unary {
                op: "rust-integer-literal-u64".into(),
                expr: Box::new(Expr::IntLit(0)),
            }),
        };
        let mut module = constant_module(unsigned, Typ::Bool);
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        let value = crate::core_opt::map_expr(
            return_value(&module).clone(),
            &mut crate::core_opt::fold_expr,
        );
        assert_eq!(value, Expr::BoolLit(true));
        let mut module = constant_module(
            Expr::Binary {
                op: ">>".into(),
                lhs: Box::new(constant_path("u64", "MAX")),
                rhs: Box::new(Expr::IntLit(1)),
            },
            Typ::Named("#rust-integer-u64".into()),
        );
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        assert!(
            matches!(return_value(&module), Expr::Binary { op, .. } if op == "lshr"),
            "{:?}",
            return_value(&module)
        );
        let value = crate::core_opt::map_expr(
            return_value(&module).clone(),
            &mut crate::core_opt::fold_expr,
        );
        assert_eq!(value, Expr::IntLit(i64::MAX));
        for value in [constant_path("u8", "MAX"), constant_path("u64", "BITS")] {
            let mut module = constant_module(value, Typ::Named("#rust-integer-i8".into()));
            normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
            assert!(
                matches!(return_value(&module), Expr::Ident(name) if core_ir::type_proofs::decode(name).is_some_and(|proof| proof.is_invalid()))
            );
        }
        let mut module = UnifiedModule::new(vec![Decl::Global {
            name: "VALUE".into(),
            typ: Typ::Named("#rust-integer-u8".into()),
            init: Some(Box::new(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(constant_path("u8", "MAX")),
                rhs: Box::new(Expr::Unary {
                    op: "rust-integer-literal-u8".into(),
                    expr: Box::new(Expr::IntLit(1)),
                }),
            })),
            mutable: false,
            constant_value: true,
        }]);
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        assert!(
            matches!(&module.decls[0], Decl::Global { init: Some(value), .. }
                if matches!(value.as_ref(), Expr::Ident(name)
                    if core_ir::type_proofs::decode(name).is_some_and(|proof| proof.is_invalid())))
        );
    }

    #[test]
    fn collapsed_integer_errors_preserve_invalid_child_proofs_first() {
        use core_ir::type_proofs::ResolutionFailure;
        let unsupported = ResolutionFailure::Unsupported("unsupported child".into()).symbol();
        let first = ResolutionFailure::Invalid("first invalid child".into()).symbol();
        let second = ResolutionFailure::Invalid("second invalid child".into()).symbol();
        let mut expr = Expr::Call {
            callee: Box::new(Expr::Ident(unsupported)),
            args: vec![Expr::Ident(first.clone()), Expr::Ident(second)],
        };
        assert_eq!(invalid_preserving_proof(&mut expr), Expr::Ident(first));
        let unsupported = ResolutionFailure::Unsupported("only unsupported child".into()).symbol();
        let mut expr = Expr::Unary {
            op: "-".into(),
            expr: Box::new(Expr::Ident(unsupported.clone())),
        };
        assert_eq!(
            invalid_preserving_proof(&mut expr),
            Expr::Ident(unsupported)
        );
        assert_eq!(invalid_preserving_proof(&mut Expr::IntLit(42)), invalid());
    }

    #[test]
    fn integer_kind_mismatches_remain_invalid_in_dead_code() {
        for source in [
            "fn main()->i64{if false{let value:i64=u8::MAX;}42}",
            "fn main()->i64{if false{let value:i8=u64::BITS;}42}",
            "fn main()->i64{if false{let value=u8::MAX+1u16;}42}",
            "fn main()->i64{if false{let value=u8::MAX==1u16;}42}",
            "fn main()->i64{if false{let value=(u8::MAX+1u16).wrapping_add(1u8);}42}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            assert!(core_ir::type_proofs::present(&module.decls));
            assert!(crate::core_ir_verifier::check_execution_contracts(&module).is_err());
        }
    }

    #[test]
    fn integer_value_callees_keep_invalid_proofs_before_dead_code_elimination() {
        for source in [
            "fn main(){let value=u64::MAX;if false{value();}}",
            "fn main(){if false{({u64::MAX})();}}",
            "fn main(){if false{(if true{u64::MAX}else{0u64})();}}",
            "fn make()->u64{u64::MAX}fn main(){if false{make()();}}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            assert!(core_ir::type_proofs::present(&module.decls), "{source}");
            assert!(
                crate::core_ir_verifier::check_execution_contracts(&module).is_err(),
                "{source}"
            );
        }
        for source in [
            "fn make()->u64{u64::MAX}fn main()->u64{make()}",
            "fn main()->u64{let make=||u64::MAX;make()}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            assert!(!core_ir::type_proofs::present(&module.decls), "{source}");
        }
    }

    #[test]
    fn rust_origin_selects_void_global_shift_helper_and_is_consumed() {
        let source = "struct Row{bits:u64} static mut ROW:Row=Row{bits:0x8000000000000000u64}; fn change(){unsafe{ROW.bits>>=\"x\".len();}} fn main()->i64{change();unsafe{if ROW.bits==0x4000000000000000u64{42}else{7}}}";
        let mut module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        let helper = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..} if name == "change"))
            .unwrap();
        assert!(matches!(helper, Decl::Function{body,..} if body.iter().any(origin_marker)));
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        let helper = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..} if name == "change"))
            .unwrap();
        let mut logical_shift = false;
        let mut helpers = vec![helper.clone()];
        crate::core_opt::map_decl_exprs_mut(&mut helpers, &mut |expr| {
            logical_shift |= matches!(expr, Expr::Binary{op,..} if op == "lshr");
        });
        assert!(logical_shift, "{helper:?}");
        assert!(!has_metadata(&module), "{module:?}");
        let once = module.clone();
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        assert_eq!(module, once);
    }

    #[test]
    fn rust_origin_does_not_select_unmarked_mixed_language_functions() {
        let mut module = crate::compiler::rust_front::parse_rust_source(
            "struct Row{bits:u64} static mut ROW:Row=Row{bits:0x8000000000000000u64}; fn change(){unsafe{ROW.bits>>=\"x\".len();}}",
        ).unwrap();
        let foreign = Decl::Function {
            name: "foreign_word_shift".into(),
            params: vec![("value".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: ">>".into(),
                lhs: Box::new(Expr::Ident("value".into())),
                rhs: Box::new(Expr::IntLit(1)),
            }))],
            type_params: vec![],
        };
        module.decls.push(foreign.clone());
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        assert_eq!(module.decls.last(), Some(&foreign));
        assert!(!has_metadata(&module));
    }

    #[test]
    fn rust_origin_preserves_bodyless_foreign_declarations() {
        let mut module = crate::compiler::rust_front::parse_rust_source(
            "extern \"C\" { fn external(); } fn main() {}",
        )
        .unwrap();
        let external = module
            .decls
            .iter()
            .find(|decl| matches!(decl, Decl::Function{name,..} if name == "external"))
            .unwrap()
            .clone();
        // Mixed-source linking identifies foreign declarations before normalization.
        assert!(matches!(&external, Decl::Function{body,..} if body.is_empty()));
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        assert_eq!(
            module
                .decls
                .iter()
                .find(|decl| matches!(decl, Decl::Function{name,..} if name == "external")),
            Some(&external)
        );
        assert!(!has_metadata(&module));
    }

    #[test]
    fn backward_integer_inference_and_normalization_are_idempotent() {
        for source in [
            "fn main()->i64{let first=0;let second=first;second+42}",
            "fn add(value:u64)->u64{value+1} fn main()->u64{let value=41;add(value)}",
            "fn main()->u8{let value=254u8;value+1u8}",
        ] {
            let mut module = resolved(source, IntegerSemantics::Word64);
            assert!(
                crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
            let once = module.clone();
            normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
            assert_eq!(module, once);
        }
    }
    #[test]
    fn unsigned_operations_and_complement_keep_widths() {
        let mut module = resolved(
            "fn main()->u64{let value=0xffffffffffffffffu64;value/2u64}",
            IntegerSemantics::Word64,
        );
        let mut operations = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Binary { op, .. } = expr {
                operations.push(op.clone());
            }
        });
        assert!(operations.iter().any(|op| op == "udiv"));
        let module = resolved("fn main()->u8{!0u8}", IntegerSemantics::Word64);
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!("main")
        };
        let Stmt::Return(Some(value)) = &body[0] else {
            panic!("return")
        };
        let value = crate::core_opt::map_expr(value.clone(), &mut crate::core_opt::fold_expr);
        assert_eq!(value, Expr::IntLit(255));
    }
    #[test]
    fn incompatible_kinds_and_unrepresentable_values_refuse() {
        for (source, semantics) in [
            ("fn main()->u8{256}", IntegerSemantics::Word64),
            ("fn main()->u32{1u8+2u32}", IntegerSemantics::Word64),
            ("fn main()->u64{42u64}", IntegerSemantics::Word32),
            ("fn main()->i128{42i128}", IntegerSemantics::Word64),
        ] {
            let module = resolved(source, semantics);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
    }
    #[test]
    fn constant_shift_counts_are_checked_against_operand_width() {
        for source in [
            "fn main()->u8{1u8<<8}",
            "fn main()->u16{1u16>>16}",
            "fn main()->u32{1u32<<-1}",
            "fn main()->u8{1u8<<(4+4)}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}"
            );
        }
        let module = resolved("fn main()->u8{128u8>>7}", IntegerSemantics::Word64);
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
    }

    #[test]
    fn nested_wide_types_refuse_only_when_used_on_word32() {
        let wide = Typ::Named("#rust-integer-u64".into());
        let schema = Decl::Struct {
            name: "Nested".into(),
            fields: vec![(
                "values".into(),
                Typ::Vector(Box::new(Typ::Array(Box::new(wide.clone())))),
            )],
            type_params: vec![],
        };
        let checker = TypeChecker::new();
        for typ in [Typ::Array(Box::new(wide)), Typ::Named("Nested".into())] {
            let mut module = UnifiedModule::new(vec![
                schema.clone(),
                Decl::Function {
                    name: "main".into(),
                    params: vec![("value".into(), typ.clone())],
                    ret: typ,
                    body: vec![Stmt::Return(Some(Expr::Ident("value".into())))],
                    type_params: vec![],
                },
            ]);
            normalize(&checker, &mut module, IntegerSemantics::Word32);
            let Decl::Function { body, .. } = &module.decls[1] else {
                panic!("main")
            };
            assert!(
                matches!(body.first(),Some(Stmt::Expr(Expr::Ident(name))) if name.starts_with("#unsupported-rust-integer"))
            );
        }
        let mut module = UnifiedModule::new(vec![
            schema,
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
        ]);
        normalize(&checker, &mut module, IntegerSemantics::Word32);
        let Decl::Function { body, .. } = &module.decls[1] else {
            panic!("main")
        };
        assert_eq!(body, &vec![Stmt::Return(Some(Expr::IntLit(42)))]);
    }
    #[test]
    fn legacy_builtin_and_pending_closure_arithmetic_keeps_carrier_compatibility() {
        for source in [
            "fn accept(text:String)->i64{text.len()+40} fn main()->i64{accept(\"hi\")}",
            "fn main()->i64{let f=|x|x+2;f(40)}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            let mut invalid = false;
            let mut declarations = module.decls.clone();
            crate::core_opt::map_decl_exprs_mut(&mut declarations, &mut |expr| {
                invalid |= matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
            });
            assert!(!invalid, "{source}: {module:?}");
        }
    }

    #[test]
    fn known_constant_arithmetic_overflow_is_not_hidden_by_width_masks() {
        for source in [
            "fn main()->u8{255u8+1u8}",
            "fn main()->u8{0u8-1u8}",
            "fn main()->u16{65535u16*2u16}",
            "fn main()->u64{0xffffffffffffffffu64+1u64}",
            "fn main()->i8{127i8+1i8}",
            "fn main()->i8{(-128i8)/(-1i8)}",
            "fn main()->i8{-(-128i8)}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn inline_const_local_values_cannot_hide_overflow_behind_runtime_masks() {
        for (source, overflow) in [
            ("fn main()->u8{const{let x:u8=255;x+1}}", true),
            ("fn main()->u8{const{let mut x:u8=254;x=255;x+1}}", true),
            ("fn main()->u8{const{let x:u8=254;x+1}}", false),
            (
                "fn main()->u8{const{let x:u8=255;if true{x+1}else{42}}}",
                true,
            ),
            (
                "fn main()->u8{const{let x:u8=255;if false{x+1}else{42}}}",
                false,
            ),
        ] {
            let mut module = resolved(source, IntegerSemantics::Word64);
            let mut rejected = false;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                rejected |= matches!(expr,Expr::Ident(name)
                    if name.starts_with("#unsupported-rust-integer")
                        || core_ir::type_proofs::decode(name).is_some_and(|proof| proof.is_invalid()));
            });
            assert_eq!(rejected, overflow, "{source}: {module:?}");
        }
    }
    #[test]
    fn constant_negation_keeps_exact_width_and_reached_path_semantics() {
        for semantics in [IntegerSemantics::Word32, IntegerSemantics::Word64] {
            for kind in ["i8", "i16", "i32", "isize", "i64"] {
                if !Kind::parse(kind, semantics).unwrap().supported(semantics) {
                    continue;
                }
                for cold in [false, true] {
                    for inline in [false, true] {
                        let value = if cold {
                            format!("if true{{42}}else{{-{kind}::MIN}}")
                        } else {
                            format!("-{kind}::MIN")
                        };
                        let source = if inline {
                            format!("fn main()->{kind}{{const{{{value}}}}}")
                        } else {
                            format!("const N:{kind}={value};fn main()->{kind}{{N}}")
                        };
                        let module = resolved(&source, semantics);
                        let proofs = TypeChecker::new().typed_proof_nodes(&module.decls);
                        assert_eq!(
                            proofs
                                .values()
                                .any(core_ir::type_proofs::ResolutionFailure::is_invalid),
                            !cold,
                            "{source}: {module:?}",
                        );
                        if cold {
                            assert!(proofs.is_empty(), "{source}: {module:?}");
                            let value = if inline {
                                return_value(&module)
                            } else {
                                module
                                    .decls
                                    .iter()
                                    .find_map(|decl| match decl {
                                        Decl::Global {
                                            name,
                                            init: Some(value),
                                            ..
                                        } if name == "N" => Some(value.as_ref()),
                                        _ => None,
                                    })
                                    .unwrap()
                            };
                            assert_eq!(
                                crate::lower_core::evaluate_pure_constant(value, semantics),
                                Ok(Some(Expr::IntLit(42))),
                                "{source}",
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn source_constant_alias_errors_survive_unused_declarations() {
        for (semantics, kind) in [
            (IntegerSemantics::Word32, "i32"),
            (IntegerSemantics::Word64, "i64"),
        ] {
            for declarations in [
                format!("const MIN:{kind}={kind}::MIN;const BAD:{kind}=-MIN;"),
                format!("const BAD:{kind}=-MIN;const MIN:{kind}={kind}::MIN;"),
                format!("const MAX:{kind}={kind}::MAX;const BAD:{kind}=MAX+1;"),
            ] {
                let source = format!("{declarations}fn main()->{kind}{{42}}");
                let module = resolved(&source, semantics);
                assert!(
                    TypeChecker::new()
                        .typed_proof_nodes(&module.decls)
                        .values()
                        .any(core_ir::type_proofs::ResolutionFailure::is_invalid),
                    "{source}: {module:?}"
                );
            }
            let source = format!(
                "const MIN:{kind}={kind}::MIN;const GOOD:{kind}=if true{{42}}else{{-MIN}};fn main()->{kind}{{GOOD}}"
            );
            let module = resolved(&source, semantics);
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn source_negation_preserves_valid_values_and_nominal_dispatch() {
        for (source, expected) in [
            ("fn main()->i8{-42i8}", -42),
            ("fn main()->i8{-128i8}", -128),
            ("fn main()->i64{-i64::MAX}", -i64::MAX),
            ("fn main()->i8{const{let value:i8=42;-value}}", -42),
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{source}: {module:?}"
            );
            assert_eq!(
                crate::lower_core::evaluate_pure_constant(
                    return_value(&module),
                    IntegerSemantics::Word64
                ),
                Ok(Some(Expr::IntLit(expected))),
                "{source}: {module:?}",
            );
        }
        let mut module = resolved(
            "struct Row{x:i64}impl std::ops::Neg for Row{type Output=i64;fn neg(self)->i64{77}}fn negate(value:Row)->i64{-value}fn main()->i64{42}",
            IntegerSemantics::Word64,
        );
        assert!(
            TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty(),
            "{module:?}"
        );
        let mut nominal_negation = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            nominal_negation |= matches!(expr, Expr::Call { callee, .. }
                if matches!(callee.as_ref(), Expr::Ident(name) if name == "Row::std::ops::Neg::neg"));
        });
        assert!(nominal_negation, "{module:?}");
    }

    #[test]
    fn aggregate_result_containers_keep_contextual_element_widths() {
        for source in [
            "fn main()->[u8;1]{{[256]}}",
            "fn main()->[u8;1]{if true{[256]}else{[0]}}",
            "fn main()->[u8;1]{match true{true=>[256],false=>[0]}}",
            "static VALUES:[u8;1]={[256]};fn main()->i64{42}",
        ] {
            let mut module = resolved(source, IntegerSemantics::Word64);
            let mut rejected = false;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                rejected |= matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
            });
            assert!(rejected, "{source}: {module:?}");
        }
        let module = resolved(
            "fn main()->[u8;1]{if true{[255]}else{[0]}}",
            IntegerSemantics::Word64,
        );
        assert!(
            crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
            "{module:?}"
        );
    }
    #[test]
    fn direct_closure_callees_normalize_aggregate_returns() {
        let mut module = UnifiedModule::new(vec![Decl::Global {
            name: "VALUES".into(),
            typ: Typ::Array(Box::new(Typ::Named("#rust-integer-u8".into()))),
            mutable: false,
            constant_value: true,
            init: Some(Box::new(Expr::Call {
                callee: Box::new(Expr::Closure {
                    params: vec![],
                    ret: Typ::Array(Box::new(Typ::Named("#rust-integer-u8".into()))),
                    captures: vec![],
                    body: vec![Stmt::Return(Some(Expr::ArrayLit(vec![Expr::Unary {
                        op: "rust-integer-literal".into(),
                        expr: Box::new(Expr::IntLit(256)),
                    }])))],
                }),
                args: vec![],
            })),
        }]);
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        let mut rejected = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            rejected |=
                matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
        });
        assert!(rejected, "{module:?}");
    }

    #[test]
    fn signed_word_static_loop_keeps_existing_strict_evaluation_path() {
        let mut module = resolved(
            "static VALUE:i64={let mut i:i64=0;while i<2{i=i+1;}i+40};fn main()->i64{42}",
            IntegerSemantics::Word64,
        );
        let mut rejected = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            rejected |=
                matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
        });
        assert!(!rejected, "{module:?}");
    }
    #[test]
    fn checked_integer_evaluation_enforces_width_on_each_const_iteration() {
        let semantics = IntegerSemantics::Word64;
        assert_eq!(
            fold_checked_integer("u8", "+", 254, Some(1), semantics),
            Some(Expr::IntLit(255))
        );
        assert_eq!(
            fold_checked_integer("u8", "+", 255, Some(1), semantics),
            None
        );
        assert_eq!(
            fold_checked_integer("u64", "+", -1, Some(1), semantics),
            None
        );
        assert_eq!(
            fold_checked_integer("i8", "/", -128, Some(-1), semantics),
            None
        );
        assert_eq!(fold_checked_integer("i8", "-", -128, None, semantics), None);
        assert_eq!(
            fold_checked_integer("u8", "~", 213, None, semantics),
            Some(Expr::IntLit(42))
        );
        assert_eq!(
            fold_checked_integer("u8", "<<", 1, Some(8), semantics),
            None
        );
        assert_eq!(
            fold_checked_integer("i8", ">>", -128, Some(1), semantics),
            Some(Expr::IntLit(-64))
        );
        assert_eq!(
            fold_checked_integer("u64", "/", -1, Some(2), semantics),
            Some(Expr::IntLit(i64::MAX))
        );
        assert_eq!(
            fold_checked_integer("u64", "+", 1, Some(1), IntegerSemantics::Word32),
            None
        );
    }

    #[test]
    fn vector_mutation_uses_receiver_integer_element_metadata() {
        let mut module = resolved(
            "fn main()->i64{let mut values:Vec<u8>=vec![];values.push(256);42}",
            IntegerSemantics::Word64,
        );
        let mut rejected = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            rejected |=
                matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
        });
        assert!(rejected, "{module:?}");
        let module = resolved(
            "fn main()->u8{let mut values:Vec<u8>=vec![];values.push(213);!values[0]}",
            IntegerSemantics::Word64,
        );
        assert!(
            crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
            "{module:?}"
        );
    }
    #[test]
    fn const_local_shift_count_is_checked_and_mixed_language_decls_stay_unchanged() {
        let mut module = resolved(
            "fn main()->i64{let value=const{let x:u8=1;let amount:u32=8;x<<amount};value as i64}",
            IntegerSemantics::Word64,
        );
        let mut rejected = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            rejected |=
                matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
        });
        assert!(rejected, "{module:?}");
        // Use the same structured cast produced by the other frontend.
        let legacy = Decl::Function {
            name: "legacy".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::Unary {
                    op: "int-cast-u8".into(),
                    expr: Box::new(Expr::IntLit(511)),
                }),
                rhs: Box::new(Expr::IntLit(1)),
            }))],
        };
        let mut module = UnifiedModule::new(vec![legacy.clone()]);
        module.decls.extend(
            crate::compiler::rust_front::parse_rust_source("fn rust_value()->u8{42u8}")
                .unwrap()
                .decls,
        );
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        assert_eq!(module.decls[0], legacy);
    }

    #[test]
    fn atomic_constructor_data_uses_its_unsigned_width() {
        for literal in ["1u32", "4294967295u32", "4294967295"] {
            let source = format!(
                "use std::sync::atomic::AtomicU32;static VALUE:AtomicU32=AtomicU32::new({literal});fn main()->i64{{42}}"
            );
            let mut module = resolved(&source, IntegerSemantics::Word64);
            let mut rejected = false;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                rejected |= matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
            });
            assert!(!rejected, "{source}: {module:?}");
        }
    }
    #[test]
    fn canonical_word_abi_accepts_large_literals_without_executing_invalid_inputs() {
        for source in [
            "fn main()->i64{__inrt_vector_clone(8,0,1152921504606846976);0}",
            "fn main()->i64{let items=Vec::<i64>::with_capacity(576460752303423488);items.len()}",
            "fn main()->i64{let mut items:Vec<i64>=Vec::new();items.ptr=8;items.cap=576460752303423488;items.push(42);0}",
        ] {
            let mut module = resolved(source, IntegerSemantics::Word64);
            let mut rejected = false;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                rejected |= matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
            });
            assert!(!rejected, "{source}: {module:?}");
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{source}: {verified:?}");
        }
    }
    #[test]
    fn internal_range_bounds_keep_existing_word_context() {
        for source in [
            "fn main()->i64{for value in 9223372036854775807..=9223372036854775807{return 42;}0}",
            "fn main()->i64{for value in 9223372036854775807..{return 42;}0}",
            "fn main()->i64{for value in 9223372036854775806..9223372036854775807{return 42;}0}",
        ] {
            let mut module = resolved(source, IntegerSemantics::Word64);
            let mut rejected = false;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                rejected |= matches!(expr,Expr::Ident(name) if name.starts_with("#unsupported-rust-integer"));
            });
            assert!(!rejected, "{source}: {module:?}");
            crate::lower_core::desugar_module(&mut module);
            assert!(
                crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{module:?}"
            );
        }
    }
    #[test]
    fn cast_origin_metadata_keeps_boolean_only_rust_declarations_in_scope() {
        let legacy = Decl::Function {
            name: "legacy".into(),
            params: vec![],
            ret: Typ::Bool,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::BoolLit(true)))],
        };
        let mut module = UnifiedModule::new(vec![
            legacy.clone(),
            Decl::Function {
                name: "check".into(),
                params: vec![],
                ret: Typ::Bool,
                type_params: vec![],
                body: vec![Stmt::Return(Some(Expr::Binary {
                    op: "==".into(),
                    lhs: Box::new(Expr::Unary {
                        op: "rust-integer-value-u8".into(),
                        expr: Box::new(Expr::Unary {
                            op: "int-cast-u8".into(),
                            expr: Box::new(Expr::IntLit(256)),
                        }),
                    }),
                    rhs: Box::new(Expr::Unary {
                        op: "rust-integer-literal".into(),
                        expr: Box::new(Expr::IntLit(0)),
                    }),
                }))],
            },
        ]);
        assert!(has_metadata(&module));
        normalize(&TypeChecker::new(), &mut module, IntegerSemantics::Word64);
        assert_eq!(module.decls[0], legacy);
        assert!(!has_metadata(&module));
        assert!(
            crate::core_ir_verifier::verify_for_entry(&module, "check").ok,
            "{module:?}"
        );
    }
    #[test]
    fn wrapping_integer_methods_use_fixed_width_bits_and_preserve_user_dispatch() {
        for (source, expected) in [
            ("fn main()->u8{255u8.wrapping_add(43)}", 42),
            ("fn main()->u16{0u16.wrapping_sub(65494)}", 42),
            ("fn main()->u32{4294967295u32.wrapping_mul(4294967254)}", 42),
            ("fn main()->u64{0xffffffffffffffffu64.wrapping_add(43)}", 42),
            ("fn main()->i8{127i8.wrapping_add(1)}", -128),
            ("fn main()->i8{(-128i8).wrapping_sub(1)}", 127),
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            let Decl::Function { body, .. } = &module.decls[0] else {
                panic!("main")
            };
            assert!(
                matches!(body.last(),Some(Stmt::Return(Some(Expr::IntLit(value)))) if *value==expected),
                "{source}: {module:?}"
            );
        }
        let mut module = resolved(
            "struct Counter{value:u8}impl Counter{fn wrapping_add(&self,extra:u8)->u8{self.value+extra}}fn main()->i64{let value=Counter{value:20};value.wrapping_add(22) as i64}",
            IntegerSemantics::Word64,
        );
        let mut dispatch = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            dispatch |= matches!(expr,Expr::Call{callee,..} if matches!(callee.as_ref(),Expr::Ident(name) if name=="Counter::wrapping_add"));
        });
        assert!(dispatch, "{module:?}");
    }

    #[test]
    fn wrapping_methods_keep_order_once_and_lazy_context() {
        let mut module = resolved(
            "fn left()->u8{255}fn right()->u8{43}fn main()->bool{false && left().wrapping_add(right())==42u8}",
            IntegerSemantics::Word64,
        );
        let mut calls = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Call { callee, .. } = expr
                && let Expr::Ident(name) = callee.as_ref()
                && matches!(name.as_str(), "left" | "right")
            {
                calls.push(name.clone());
            }
        });
        assert_eq!(calls, ["left", "right"]);
        let Decl::Function { body, .. } = module.decls.last().unwrap() else {
            panic!("main")
        };
        assert!(
            matches!(body.last(),Some(Stmt::Return(Some(Expr::Binary{op,lhs,..}))) if op=="&&"&&matches!(lhs.as_ref(),Expr::BoolLit(false)))
        );
    }

    #[test]
    fn wrapping_methods_refuse_mismatched_and_unrepresentable_receivers() {
        for (source, semantics) in [
            (
                "fn main()->u8{1u8.wrapping_add(1u16)}",
                IntegerSemantics::Word64,
            ),
            (
                "fn main()->u64{1u64.wrapping_mul(2u64)}",
                IntegerSemantics::Word32,
            ),
            (
                "fn main()->u8{1u8.wrapping_add()}",
                IntegerSemantics::Word64,
            ),
        ] {
            let module = resolved(source, semantics);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
        assert_eq!(
            fold_wrapping_integer("u64", "*", -1, Some(-42), IntegerSemantics::Word64),
            Some(Expr::IntLit(42))
        );
        assert_eq!(
            fold_checked_integer("u8", "+", 255, Some(1), IntegerSemantics::Word64),
            None
        );
    }

    #[test]
    fn wrapping_methods_do_not_invent_primitive_nominal_receivers() {
        let module = resolved(
            "struct Counter{value:u8}fn main()->u8{let value=Counter{value:42};value.wrapping_add(1u8)}",
            IntegerSemantics::Word64,
        );
        assert!(
            !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
            "{module:?}"
        );
    }
    #[test]
    fn wrapping_methods_require_exact_argument_and_receiver_kinds() {
        for source in [
            "fn main()->u8{let text:String=\"hi\";1u8.wrapping_add(text.len())}",
            "fn main()->u8{let text:String=\"hi\";let length=text.len();1u8.wrapping_add(length)}",
            "fn main()->u8{let text:String=\"hi\";let length=text.len()+1;1u8.wrapping_add(length)}",
            "fn main()->u8{let text:String=\"hi\";let length={text.len()};1u8.wrapping_add(length)}",
            "fn main()->i64{let text:String=\"hi\";text.len().wrapping_add(1i64)}",
        ] {
            let module = resolved(source, IntegerSemantics::Word64);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
    }
}
