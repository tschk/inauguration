//! Cross-frontend core AST (v0). Bodies may be empty until a frontend fills statements.

pub(crate) mod tuples;

pub(crate) const INLINE_CONST: &str = "#rust-inline-const";
pub(crate) const VECTOR_ALLOCATION: &str = "#vector-with-capacity";
pub(crate) const BORROW: &str = "#rust-borrow";
pub(crate) const METHOD_PREFIX: &str = "#receiver-method:";
pub(crate) fn is_compiler_callee(name: &str) -> bool {
    tuples::is_marker(name)
        || name == INLINE_CONST
        || name == BORROW
        || name == VECTOR_ALLOCATION
        || name.starts_with(METHOD_PREFIX)
}

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;

thread_local! {
    static INTERRUPT_FNS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

pub fn register_interrupt_fn(name: &str) {
    INTERRUPT_FNS.with(|fns| fns.borrow_mut().insert(name.to_string()));
}

pub fn is_interrupt_fn(name: &str) -> bool {
    INTERRUPT_FNS.with(|fns| fns.borrow().contains(name))
}

/// Source position span.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub line: u32,
    pub col: u32,
    pub file: String,
}

impl Span {
    #[must_use]
    pub fn new(line: u32, col: u32, file: &str) -> Self {
        Self {
            line,
            col,
            file: file.to_string(),
        }
    }
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            line: 0,
            col: 0,
            file: String::new(),
        }
    }
}

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.file.is_empty() {
            write!(f, "{}:{}", self.line, self.col)
        } else {
            write!(f, "{}:{}:{}", self.file, self.line, self.col)
        }
    }
}

/// Source position attached to a Core IR node.
pub type NodeSpan = Option<Span>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FloatVal(pub f64);

impl PartialEq for FloatVal {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}
impl Eq for FloatVal {}

impl std::hash::Hash for FloatVal {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.0.to_bits(), state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Typ {
    Int,
    String,
    Bool,
    Float,
    Void,
    Array(Box<Typ>),
    Vector(Box<Typ>),
    Named(String),
    Generic(String),
}

/// Reserved frontend destination-width metadata; source nominal names are not metadata.
pub(crate) fn integer_type_name(name: &str) -> Option<&str> {
    let width = name.strip_prefix("#integer-")?;
    matches!(width, "i8" | "u8" | "i16" | "u16" | "i32" | "u32").then_some(width)
}

impl Typ {
    /// Normalize language-specific named types to canonical Core IR primitives.
    pub fn canonical(&self) -> Typ {
        match self {
            Typ::Named(name) if integer_type_name(name).is_some() => Typ::Int,
            Typ::Named(name) => match name.trim() {
                "Int" | "int" | "i64" | "i32" | "i16" | "i8" | "u64" | "u32" | "u16" | "u8" => {
                    Typ::Int
                }
                "String" | "string" | "str" => Typ::String,
                "Bool" | "bool" => Typ::Bool,
                "Float" | "float" | "Double" | "double" | "f64" | "f32" => Typ::Float,
                "Void" | "void" | "Unit" | "unit" | "()" => Typ::Void,
                _ => self.clone(),
            },
            Typ::Array(item) => Typ::Array(Box::new(item.canonical())),
            Typ::Vector(item) => Typ::Vector(Box::new(item.canonical())),
            _ => self.clone(),
        }
    }

    pub fn is_any(&self) -> bool {
        matches!(self, Typ::Named(name) if name == "Any")
    }

    pub(crate) fn vector_allocation_intrinsic(&self) -> Option<&'static str> {
        match self.canonical() {
            Typ::Vector(item) => match item.canonical() {
                Typ::Int => Some("vector-with-capacity-int"),
                Typ::Float => Some("vector-with-capacity-float"),
                Typ::Bool => Some("vector-with-capacity-bool"),
                _ => None,
            },
            _ => None,
        }
    }

    pub(crate) fn accepts_vector_source(&self, expr: &Expr, actual: &Typ) -> bool {
        let Typ::Vector(item) = self.canonical() else {
            return false;
        };
        matches!(actual.canonical(), Typ::Vector(source) | Typ::Array(source) if item.canonical() == source.canonical())
            || (matches!(expr, Expr::ArrayLit(items) if items.is_empty())
                && self.accepts_sequence_literal(expr, actual))
    }

    /// Literals acquire their sequence layout from the typed destination.
    pub(crate) fn accepts_sequence_literal(&self, expr: &Expr, actual: &Typ) -> bool {
        match expr {
            Expr::MatchValue { arms, .. } => {
                return !arms.is_empty()
                    && arms.iter().all(|arm| {
                        expr_diverges(&arm.value)
                            || self.accepts_sequence_literal(&arm.value, actual)
                    });
            }
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => {
                return [then_expr, else_expr].into_iter().all(|branch| {
                    expr_diverges(branch) || self.accepts_sequence_literal(branch, actual)
                });
            }
            Expr::Block {
                result: Some(result),
                ..
            } => return self.accepts_sequence_literal(result, actual),
            _ => {}
        }
        let Expr::ArrayLit(items) = expr else {
            return false;
        };
        let Typ::Array(actual_item) = actual.canonical() else {
            return false;
        };
        match self.canonical() {
            Typ::Array(item) | Typ::Vector(item) => items.iter().all(|value| {
                if matches!(value, Expr::ArrayLit(_)) {
                    let nested_actual = if matches!(actual_item.as_ref(), Typ::Array(_)) {
                        actual_item.as_ref().clone()
                    } else {
                        Typ::Array(Box::new(Typ::Void))
                    };
                    item.accepts_sequence_literal(value, &nested_actual)
                } else {
                    item.compatible_with(&actual_item)
                }
            }),
            _ => false,
        }
    }

    /// Two types are compatible if canonical forms are equal or either is Any.
    pub fn compatible_with(&self, other: &Typ) -> bool {
        let a = self.canonical();
        let b = other.canonical();
        a == b || a.is_any() || b.is_any()
    }
}

pub(crate) fn empty_sequence_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::ArrayLit(items) if items.iter().all(empty_sequence_literal))
}

/// Bitwise operations preserve either the integer or boolean operand type.
pub(crate) fn bitwise_result_type(lhs: Option<Typ>, rhs: Option<Typ>) -> Option<Typ> {
    match (lhs?.canonical(), rhs?.canonical()) {
        (Typ::Int, Typ::Int) => Some(Typ::Int),
        (Typ::Bool, Typ::Bool) => Some(Typ::Bool),
        _ => None,
    }
}

pub(crate) fn stmts_diverge(body: &[Stmt]) -> bool {
    body.iter().any(|stmt| match stmt {
        Stmt::Return(_) | Stmt::Throw(_) | Stmt::Break | Stmt::Continue => true,
        Stmt::Let(_, _, value) | Stmt::Assign(_, value) | Stmt::Expr(value) => expr_diverges(value),
        Stmt::FieldAssign { base, value, .. } => expr_diverges(base) || expr_diverges(value),
        Stmt::IndexAssign { base, index, value } => {
            [base, index, value].into_iter().any(expr_diverges)
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            expr_diverges(cond)
                || (matches!(cond, Expr::BoolLit(true))
                    && else_body.is_empty()
                    && stmts_diverge(then_body))
                || (!else_body.is_empty() && stmts_diverge(then_body) && stmts_diverge(else_body))
        }
        Stmt::Try { body, catches } => {
            stmts_diverge(body) && catches.iter().all(|catch| stmts_diverge(&catch.body))
        }
        Stmt::Match { scrutinee, arms } => {
            let exhaustive = arms.iter().filter(|arm| arm.guard.is_none()).any(|arm| {
                MatchPattern::parse(&arm.pattern)
                    .is_ok_and(|pattern| pattern.irrefutable_after_shape_check())
            }) || ["true", "false"].iter().all(|value| {
                arms.iter()
                    .any(|arm| arm.guard.is_none() && arm.pattern.trim() == *value)
            });
            expr_diverges(scrutinee)
                || (exhaustive
                    && !arms.is_empty()
                    && arms.iter().all(|arm| stmts_diverge(&arm.body)))
        }
        _ => false,
    })
}

pub(crate) fn expr_diverges(expr: &Expr) -> bool {
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            let exhaustive = arms.iter().any(|arm| {
                arm.guard.is_none()
                    && matches!(
                        MatchPattern::parse(&arm.pattern),
                        Ok(MatchPattern::WildPat | MatchPattern::IdentPat(_))
                    )
            }) || ["true", "false"].iter().all(|value| {
                arms.iter()
                    .any(|arm| arm.guard.is_none() && arm.pattern.trim() == *value)
            });
            expr_diverges(scrutinee)
                || (exhaustive
                    && !arms.is_empty()
                    && arms.iter().all(|arm| expr_diverges(&arm.value)))
        }
        Expr::Block { body, result } => {
            stmts_diverge(body) || result.as_deref().is_some_and(expr_diverges)
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => expr_diverges(cond) || (expr_diverges(then_expr) && expr_diverges(else_expr)),
        _ => false,
    }
}

pub(crate) fn conditional_type(
    then_expr: &Expr,
    then_type: Option<Typ>,
    else_expr: &Expr,
    else_type: Option<Typ>,
) -> Option<Typ> {
    if expr_diverges(then_expr) {
        return else_type;
    }
    if expr_diverges(else_expr) {
        return then_type;
    }
    let (then_type, else_type) = (then_type?, else_type?);
    if then_type.canonical() == else_type.canonical() {
        return Some(then_type.canonical());
    }
    if then_type.accepts_sequence_literal(else_expr, &else_type) {
        return Some(then_type.canonical());
    }
    if else_type.accepts_sequence_literal(then_expr, &then_type) {
        return Some(else_type.canonical());
    }
    None
}

pub(crate) fn match_value_type(
    arms: &[MatchValueArm],
    types: impl IntoIterator<Item = Option<Typ>>,
) -> Option<Typ> {
    if arms.is_empty() {
        return None;
    }
    let mut result: Option<(&Expr, Typ)> = None;
    for (arm, typ) in arms.iter().zip(types) {
        if expr_diverges(&arm.value) {
            continue;
        }
        let typ = typ?;
        result = Some(match result {
            None => (&arm.value, typ),
            Some((value, previous)) => (
                value,
                conditional_type(value, Some(previous), &arm.value, Some(typ))?,
            ),
        });
    }
    Some(result.map(|(_, typ)| typ.canonical()).unwrap_or(Typ::Void))
}

pub(crate) fn match_value_exhaustive<'a>(
    typ: &Typ,
    arms: &[MatchValueArm],
    schema: &impl Fn(&str) -> Option<&'a [(String, Typ)]>,
) -> bool {
    let mut bools = [false; 2];
    let mut exhaustive = false;
    for arm in arms {
        let Ok(pattern) = MatchPattern::parse(&arm.pattern) else {
            return false;
        };
        if pattern.typed_bindings(typ, schema).is_err() {
            return false;
        }
        if arm.guard.is_some() {
            continue;
        }
        if pattern.irrefutable(typ, schema)
            || matches!(&pattern, MatchPattern::ArrayPat(items) if matches!(typ.canonical(), Typ::Array(_) | Typ::Vector(_)) && matches!(items.as_slice(), [MatchPattern::RestPat]))
        {
            exhaustive = true;
        }
        if let MatchPattern::BoolPat(value) = pattern {
            bools[usize::from(value)] = true;
        }
    }
    exhaustive || (typ.canonical() == Typ::Bool && bools.into_iter().all(|covered| covered))
}

pub(crate) fn expr_contains_call(expr: &Expr) -> bool {
    match expr {
        Expr::Call { .. } | Expr::Block { .. } | Expr::MatchValue { .. } => true,
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => [cond, then_expr, else_expr]
            .into_iter()
            .any(|value| expr_contains_call(value)),
        Expr::Unary { expr, .. } => expr_contains_call(expr),
        Expr::Binary { lhs, rhs, .. } => expr_contains_call(lhs) || expr_contains_call(rhs),
        Expr::StructInit { fields, .. } => fields.iter().any(|(_, expr)| expr_contains_call(expr)),
        Expr::Field { base, .. } => expr_contains_call(base),
        Expr::ArrayLit(items) => items.iter().any(expr_contains_call),
        Expr::Index { base, index, .. } => expr_contains_call(base) || expr_contains_call(index),
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_)
        | Expr::Closure { .. } => false,
    }
}

pub(crate) fn saturating_float_cast_spec(op: &str) -> Option<(u8, bool)> {
    integer_cast_spec(&format!("int-cast-{}", op.strip_prefix("float-saturate-")?))
}

pub(crate) fn saturating_float_cast(value: f64, bits: u8, signed: bool) -> i64 {
    match (bits, signed) {
        (8, true) => value as i8 as i64,
        (16, true) => value as i16 as i64,
        (32, true) => value as i32 as i64,
        (64, true) => value as i64,
        (8, false) => value as u8 as i64,
        (16, false) => value as u16 as i64,
        (32, false) => value as u32 as i64,
        (64, false) => value as u64 as i64,
        _ => unreachable!("validated numeric cast width"),
    }
}

pub(crate) fn integer_cast_spec(op: &str) -> Option<(u8, bool)> {
    match op.strip_prefix("int-cast-")? {
        "i8" => Some((8, true)),
        "u8" => Some((8, false)),
        "i16" => Some((16, true)),
        "u16" => Some((16, false)),
        "i32" => Some((32, true)),
        "u32" => Some((32, false)),
        "i64" | "isize" => Some((64, true)),
        "u64" | "usize" => Some((64, false)),
        _ => None,
    }
}

pub(crate) fn integer_cast_expr(value: Expr, op: &str) -> Option<Expr> {
    let (width, signed) = integer_cast_spec(op)?;
    if width == 64 {
        return Some(value);
    }
    let binary = |op: &str, lhs, rhs| Expr::Binary {
        op: op.into(),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    };
    let value = binary("&", value, Expr::IntLit((1i64 << width) - 1));
    Some(if signed {
        let sign = Expr::IntLit(1i64 << (width - 1));
        binary("-", binary("^", value, sign.clone()), sign)
    } else {
        value
    })
}

/// Compare unsigned fixed-width values through the signed native Int carrier.
/// Widths up to 32 work on both word sizes. Width 64 needs target-specific
/// handling and is deliberately refused here. Each operand occurs once.
pub(crate) fn unsigned_compare_expr(op: &str, lhs: Expr, rhs: Expr, width: u32) -> Option<Expr> {
    if !matches!(op, "<" | "<=" | ">" | ">=") {
        return None;
    }
    let cast = match width {
        8 => "int-cast-i8",
        16 => "int-cast-i16",
        32 => "int-cast-i32",
        _ => return None,
    };
    let biased = |value| {
        integer_cast_expr(
            Expr::Binary {
                op: "^".into(),
                lhs: Box::new(value),
                rhs: Box::new(Expr::IntLit(1i64 << (width - 1))),
            },
            cast,
        )
    };
    Some(Expr::Binary {
        op: op.into(),
        lhs: Box::new(biased(lhs)?),
        rhs: Box::new(biased(rhs)?),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Expr {
    UnitLit,
    IntLit(i64),
    FloatLit(FloatVal),
    StringLit(String),
    BoolLit(bool),
    Ident(String),
    Unary {
        op: String,
        expr: Box<Expr>,
    },
    Binary {
        op: String,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    StructInit {
        name: String,
        fields: Vec<(String, Expr)>,
    },
    Field {
        base: Box<Expr>,
        name: String,
    },
    ArrayLit(Vec<Expr>),
    Index {
        base: Box<Expr>,
        index: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    MatchValue {
        scrutinee: Box<Expr>,
        arms: Vec<MatchValueArm>,
    },
    Conditional {
        cond: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
    Block {
        body: Vec<Stmt>,
        result: Option<Box<Expr>>,
    },
    Closure {
        params: Vec<(String, Typ)>,
        ret: Typ,
        body: Vec<Stmt>,
        captures: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Stmt {
    Let(String, Option<Typ>, Expr),
    Assign(String, Expr),
    IndexAssign {
        base: Expr,
        index: Expr,
        value: Expr,
    },
    /// Assign to a struct field: `s.x = val`.
    FieldAssign {
        base: Expr,
        name: String,
        value: Expr,
    },
    Return(Option<Expr>),
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    Loop {
        kind: LoopKind,
        cond: Option<Expr>,
        body: Vec<Stmt>,
    },
    Match {
        scrutinee: Expr,
        arms: Vec<MatchArm>,
    },
    Throw(Expr),
    Try {
        body: Vec<Stmt>,
        catches: Vec<CatchArm>,
    },
    /// Return from the current function when the runtime error flag is set.
    Propagate,
    /// Evaluated for side effects (e.g. `.in` expression statements).
    Expr(Expr),
    /// Break out of the current loop.
    Break,
    /// Continue the current loop.
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LoopKind {
    For { binding: String },
    While,
    Infinite,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MatchPattern {
    IntPat(i64),
    StringPat(String),
    BoolPat(bool),
    WildPat,
    IdentPat(String),
    RestPat,
    TuplePat(Vec<MatchPattern>),
    StructPat {
        name: String,
        fields: Vec<(String, MatchPattern)>,
    },
    ArrayPat(Vec<MatchPattern>),
}

fn trim_match_pat(s: &str) -> &str {
    s.trim()
}

fn split_match_pat_args(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in inner.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                let arg = trim_match_pat(&inner[start..i]);
                if !arg.is_empty() {
                    out.push(arg.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = trim_match_pat(&inner[start..]);
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

/// Parse signed integer literals without accepting a sign inside a radix prefix.
pub(crate) fn parse_int_literal(s: &str) -> Option<i64> {
    let (negative, magnitude) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s.strip_prefix('+').unwrap_or(s))
    };
    let (digits, radix) = [("0x", "0X", 16), ("0o", "0O", 8), ("0b", "0B", 2)]
        .into_iter()
        .find_map(|(lower, upper, radix)| {
            magnitude
                .strip_prefix(lower)
                .or_else(|| magnitude.strip_prefix(upper))
                .map(|digits| (digits, radix))
        })
        .unwrap_or((magnitude, 10));
    if digits.is_empty() || !digits.chars().all(|digit| digit.is_digit(radix)) {
        return None;
    }
    let magnitude = u64::from_str_radix(digits, radix).ok()?;
    if negative && magnitude == 1_u64 << 63 {
        Some(i64::MIN)
    } else {
        let value = i64::try_from(magnitude).ok()?;
        Some(if negative { -value } else { value })
    }
}

/// Recognize decimal, hexadecimal, octal, or binary digits regardless of range.
pub(crate) fn looks_like_int_literal(s: &str) -> bool {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    for (lower, upper, radix) in [("0x", "0X", 16), ("0o", "0O", 8), ("0b", "0B", 2)] {
        if let Some(digits) = digits
            .strip_prefix(lower)
            .or_else(|| digits.strip_prefix(upper))
        {
            return !digits.is_empty() && digits.chars().all(|digit| digit.is_digit(radix));
        }
    }
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Decode the common quoted string syntax used by Core IR patterns and `.in` literals.
pub(crate) fn decode_string_literal(quoted: &str) -> Option<String> {
    let raw = quoted.strip_prefix('"')?.strip_suffix('"')?;
    // Process escape sequences: \n, \t, \r, \\, \"
    let mut processed = String::with_capacity(raw.len());
    let mut esc = false;
    for c in raw.chars() {
        if esc {
            match c {
                'n' => processed.push('\n'),
                't' => processed.push('\t'),
                'r' => processed.push('\r'),
                '\\' => processed.push('\\'),
                '"' => processed.push('"'),
                _ => {
                    processed.push('\\');
                    processed.push(c);
                }
            }
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return None;
        } else {
            processed.push(c);
        }
    }
    (!esc).then_some(processed)
}

impl MatchPattern {
    /// The verifier must still prove the struct name and field types match.
    pub(crate) fn irrefutable_after_shape_check(&self) -> bool {
        match self {
            Self::WildPat | Self::IdentPat(_) => true,
            Self::StructPat { fields, .. } => fields
                .iter()
                .all(|(_, pattern)| pattern.irrefutable_after_shape_check()),
            Self::TuplePat(items) => items.iter().all(|pattern| {
                matches!(pattern, Self::RestPat) || pattern.irrefutable_after_shape_check()
            }),
            _ => false,
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let s = trim_match_pat(s).trim_end_matches(':').trim();
        let s = s.strip_prefix("case ").unwrap_or(s).trim();
        if s.is_empty() {
            return Err(".in: empty pattern".into());
        }
        if s == "_" || s == "-" || s == "else" || s == "default" {
            return Ok(MatchPattern::WildPat);
        }
        if s == ".." {
            return Ok(MatchPattern::RestPat);
        }
        if s == "true" {
            return Ok(MatchPattern::BoolPat(true));
        }
        if s == "false" {
            return Ok(MatchPattern::BoolPat(false));
        }
        if let Some(n) = parse_int_literal(s) {
            return Ok(MatchPattern::IntPat(n));
        }
        // Out-of-range numeric tokens must not fall through to binding or
        // variant patterns, where they would silently never match.
        if looks_like_int_literal(s) {
            return Err(format!(".in: integer pattern `{s}` out of i64 range"));
        }
        if let Some(value) = decode_string_literal(s) {
            return Ok(MatchPattern::StringPat(value));
        }
        if s.starts_with('(') && s.ends_with(')') {
            let inner = &s[1..s.len() - 1];
            let parts = split_match_pat_args(inner);
            let pats: Result<Vec<_>, _> = parts.iter().map(|p| MatchPattern::parse(p)).collect();
            return Ok(MatchPattern::TuplePat(pats?));
        }
        if s.starts_with('[') && s.ends_with(']') {
            let inner = &s[1..s.len() - 1];
            let parts = split_match_pat_args(inner);
            let pats: Result<Vec<_>, _> = parts.iter().map(|p| MatchPattern::parse(p)).collect();
            return Ok(MatchPattern::ArrayPat(pats?));
        }
        if let Some(open) = s.find('{')
            && s.ends_with('}')
        {
            let name = trim_match_pat(&s[..open]);
            let valid_path = name.split("::").all(|segment| {
                // Rust's identifier parser includes Unicode XID and raw identifiers.
                syn::parse_str::<syn::Ident>(segment).is_ok()
                    || (segment
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_ascii_alphabetic())
                        && segment
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
            });
            if valid_path {
                let inner = &s[open + 1..s.len() - 1];
                let field_strs = split_match_pat_args(inner);
                let mut fields = Vec::new();
                for f in field_strs {
                    if let Some((field_name, field_pat)) = f.split_once(':') {
                        let fn_trim = trim_match_pat(field_name);
                        let fp_trim = trim_match_pat(field_pat);
                        if fn_trim.is_empty() {
                            return Err(format!(".in: empty field name in struct pattern `{s}`"));
                        }
                        fields.push((fn_trim.to_string(), MatchPattern::parse(fp_trim)?));
                    } else {
                        let fn_trim = trim_match_pat(&f);
                        if fn_trim.is_empty() {
                            return Err(format!(".in: empty field name in struct pattern `{s}`"));
                        }
                        fields.push((
                            fn_trim.to_string(),
                            MatchPattern::IdentPat(fn_trim.to_string()),
                        ));
                    }
                }
                return Ok(MatchPattern::StructPat {
                    name: name.to_string(),
                    fields,
                });
            }
        }
        // A lone `_` discards; other source identifiers bind a value.
        if s == "_"
            || (!s.is_empty()
                && s.chars().enumerate().all(|(i, c)| match (i, c) {
                    (0, ch) => ch.is_ascii_alphabetic() || ch == '_',
                    (_, ch) => ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                }))
        {
            return Ok(MatchPattern::IdentPat(s.to_string()));
        }
        Err(format!(".in: unknown pattern `{s}`"))
    }
}

impl MatchPattern {
    pub(crate) fn typed_bindings<'a>(
        &self,
        typ: &Typ,
        schema: &impl Fn(&str) -> Option<&'a [(String, Typ)]>,
    ) -> Result<Vec<(String, Typ)>, String> {
        let mut bindings = Vec::new();
        match self {
            Self::IdentPat(name) => bindings.push((name.clone(), typ.clone())),
            Self::TuplePat(items) if items.is_empty() => {
                if typ.canonical() != Typ::Void {
                    return Err(format!("unit pattern does not match {typ:?}"));
                }
            }
            Self::StructPat { name, fields } => {
                if typ.canonical() != Typ::Named(name.clone()) {
                    return Err(format!("struct pattern `{name}` does not match {typ:?}"));
                }
                let fields_schema =
                    schema(name).ok_or_else(|| format!("unknown pattern struct `{name}`"))?;
                for (field, pattern) in fields {
                    let field_type = fields_schema
                        .iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, typ)| typ)
                        .ok_or_else(|| format!("unknown pattern field `{name}.{field}`"))?;
                    bindings.extend(pattern.typed_bindings(field_type, schema)?);
                }
            }
            Self::TuplePat(items) | Self::ArrayPat(items) => {
                let rest = items
                    .iter()
                    .position(|pattern| matches!(pattern, Self::RestPat));
                if items
                    .iter()
                    .filter(|pattern| matches!(pattern, Self::RestPat))
                    .count()
                    > 1
                {
                    return Err("sequence pattern has multiple rests".into());
                }
                let tuple_fields = match typ {
                    Typ::Array(_) | Typ::Vector(_) if matches!(self, Self::ArrayPat(_)) => None,
                    Typ::Named(name) if matches!(self, Self::TuplePat(_)) => {
                        let fields = schema(name)
                            .ok_or_else(|| format!("unknown tuple pattern type `{name}`"))?;
                        let fixed = items.len() - usize::from(rest.is_some());
                        if (rest.is_none() && fields.len() != fixed) || fields.len() < fixed {
                            return Err(format!("tuple pattern arity does not match {typ:?}"));
                        }
                        if !(0..fields.len()).all(|index| {
                            fields.iter().any(|(name, _)| name == &format!("_{index}"))
                        }) {
                            return Err(format!("invalid tuple pattern schema for {typ:?}"));
                        }
                        Some(fields)
                    }
                    _ => return Err(format!("sequence pattern does not match {typ:?}")),
                };
                for (index, pattern) in items.iter().enumerate() {
                    if matches!(pattern, Self::RestPat) {
                        continue;
                    }
                    let item_type = if let Some(fields) = tuple_fields {
                        let index = if rest.is_some_and(|rest| index > rest) {
                            fields.len() - (items.len() - index)
                        } else {
                            index
                        };
                        &fields
                            .iter()
                            .find(|(name, _)| name == &format!("_{index}"))
                            .ok_or_else(|| format!("unknown tuple pattern field {index}"))?
                            .1
                    } else {
                        match typ {
                            Typ::Array(item) | Typ::Vector(item) => item.as_ref(),
                            _ => unreachable!(),
                        }
                    };
                    bindings.extend(pattern.typed_bindings(item_type, schema)?);
                }
            }
            Self::RestPat => return Err("rest pattern requires a sequence".into()),
            Self::IntPat(_) | Self::StringPat(_) | Self::BoolPat(_) => {
                let expected = match self {
                    Self::IntPat(_) => Typ::Int,
                    Self::StringPat(_) => Typ::String,
                    Self::BoolPat(_) => Typ::Bool,
                    _ => unreachable!(),
                };
                if !expected.compatible_with(typ) {
                    return Err(format!("literal pattern {self:?} does not match {typ:?}"));
                }
            }
            _ => {}
        }
        Ok(bindings)
    }

    pub(crate) fn irrefutable<'a>(
        &self,
        typ: &Typ,
        schema: &impl Fn(&str) -> Option<&'a [(String, Typ)]>,
    ) -> bool {
        match self {
            Self::WildPat | Self::IdentPat(_) | Self::RestPat => true,
            Self::TuplePat(items) if items.is_empty() && typ.canonical() == Typ::Void => true,
            Self::StructPat { name, fields } if typ.canonical() == Typ::Named(name.clone()) => {
                schema(name).is_some_and(|declared| {
                    fields.iter().all(|(field, pattern)| {
                        declared
                            .iter()
                            .find(|(name, _)| name == field)
                            .is_some_and(|(_, typ)| pattern.irrefutable(typ, schema))
                    })
                })
            }
            Self::TuplePat(items)
                if matches!(typ, Typ::Named(_)) && self.typed_bindings(typ, schema).is_ok() =>
            {
                let Typ::Named(name) = typ else {
                    return false;
                };
                let Some(fields) = schema(name) else {
                    return false;
                };
                let rest = items
                    .iter()
                    .position(|pattern| matches!(pattern, Self::RestPat));
                items.iter().enumerate().all(|(index, pattern)| {
                    if matches!(pattern, Self::RestPat) {
                        return true;
                    }
                    let index = if rest.is_some_and(|rest| index > rest) {
                        fields.len() - (items.len() - index)
                    } else {
                        index
                    };
                    fields
                        .iter()
                        .find(|(name, _)| name == &format!("_{index}"))
                        .is_some_and(|(_, typ)| pattern.irrefutable(typ, schema))
                })
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MatchValueArm {
    pub pattern: String,
    pub guard: Option<Expr>,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MatchArm {
    pub pattern: String,
    /// Evaluated after a pattern matches; false proceeds to the next arm.
    pub guard: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CatchArm {
    pub pattern: String,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Visibility {
    Pub,
    Private,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Import {
    pub path: String,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MethodSig {
    pub name: String,
    pub params: Vec<(String, Typ)>,
    pub ret: Typ,
}

/// Single-module view produced by language fronts before lowering to textual SIL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnifiedModule {
    pub identity: CoreModuleIdentity,
    pub decls: Vec<Decl>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CoreModuleIdentity {
    pub package: Option<String>,
    pub module: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModuleIdentityReport {
    pub package: Option<String>,
    pub module: Option<String>,
    pub requested_module_id: String,
    pub effective_module_id: String,
}

impl UnifiedModule {
    #[must_use]
    pub fn new(decls: Vec<Decl>) -> Self {
        Self {
            identity: CoreModuleIdentity::default(),
            decls,
        }
    }

    #[must_use]
    pub fn with_identity(decls: Vec<Decl>, identity: CoreModuleIdentity) -> Self {
        Self { identity, decls }
    }

    #[must_use]
    pub fn effective_module_id<'a>(&'a self, requested: &'a str) -> &'a str {
        if requested != "App" {
            return requested;
        }
        self.identity
            .module
            .as_deref()
            .or(self.identity.package.as_deref())
            .unwrap_or(requested)
    }

    #[must_use]
    pub fn identity_report(&self, requested: &str) -> ModuleIdentityReport {
        ModuleIdentityReport {
            package: self.identity.package.clone(),
            module: self.identity.module.clone(),
            requested_module_id: requested.to_string(),
            effective_module_id: self.effective_module_id(requested).to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComponentImport {
    pub name: String,
    pub interface: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComponentExport {
    pub name: String,
    pub interface: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComponentCapability {
    pub name: String,
    pub capability_type: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Decl {
    Struct {
        name: String,
        fields: Vec<(String, Typ)>,
        type_params: Vec<String>,
    },
    Function {
        name: String,
        params: Vec<(String, Typ)>,
        ret: Typ,
        body: Vec<Stmt>,
        type_params: Vec<String>,
    },
    Class {
        name: String,
        fields: Vec<(String, Typ)>,
        methods: Vec<Decl>,
        visibility: Visibility,
        extends: Option<String>,
        implements: Vec<String>,
        type_params: Vec<String>,
    },
    Interface {
        name: String,
        methods: Vec<MethodSig>,
        visibility: Visibility,
        type_params: Vec<String>,
    },
    Component {
        name: String,
        target: String,
        deterministic: bool,
        checkpoint: String,
        imports: Vec<ComponentImport>,
        exports: Vec<ComponentExport>,
        capabilities: Vec<ComponentCapability>,
    },
    /// A global variable or constant declaration.
    /// `mutable` is true for `var`, false for `const`.
    Global {
        name: String,
        typ: Typ,
        init: Option<Box<Expr>>,
        mutable: bool,
        /// A compile-time value copied at each use, rather than stored globally.
        constant_value: bool,
    },
}

/// Visit every immediate child expression of `e`, calling `f` for each.
/// Useful for building recursive AST walkers without duplicating the variant match.
pub fn for_each_expr_child<'a>(e: &'a Expr, f: &mut impl FnMut(&'a Expr)) {
    match e {
        Expr::Call { callee, args, .. } => {
            f(callee);
            for a in args {
                f(a);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            f(cond);
            f(then_expr);
            f(else_expr);
        }
        Expr::MatchValue { scrutinee, arms } => {
            f(scrutinee);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    f(guard);
                }
                f(&arm.value);
            }
        }
        // Statements need a scope-aware traversal, like closure bodies.
        Expr::Block { result, .. } => {
            if let Some(result) = result {
                f(result);
            }
        }
        Expr::Unary { expr, .. } => f(expr),
        Expr::Field { base, .. } => f(base),
        Expr::Index { base, index, .. } => {
            f(base);
            f(index);
        }
        Expr::StructInit { fields, .. } => {
            for (_, fe) in fields {
                f(fe);
            }
        }
        Expr::ArrayLit(items) => {
            for i in items {
                f(i);
            }
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_)
        | Expr::Closure { .. } => {}
    }
}

/// Visit immediate expression children for in-place Core IR normalization.
pub(crate) fn for_each_expr_child_mut(e: &mut Expr, f: &mut impl FnMut(&mut Expr)) {
    match e {
        Expr::Call { callee, args, .. } => {
            f(callee);
            for a in args {
                f(a);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            f(lhs);
            f(rhs);
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            f(cond);
            f(then_expr);
            f(else_expr);
        }
        Expr::MatchValue { scrutinee, arms } => {
            f(scrutinee);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    f(guard);
                }
                f(&mut arm.value);
            }
        }
        // Statements need a scope-aware traversal, like closure bodies.
        Expr::Block { result, .. } => {
            if let Some(result) = result {
                f(result);
            }
        }
        Expr::Unary { expr, .. } => f(expr),
        Expr::Field { base, .. } => f(base),
        Expr::Index { base, index, .. } => {
            f(base);
            f(index);
        }
        Expr::StructInit { fields, .. } => {
            for (_, fe) in fields {
                f(fe);
            }
        }
        Expr::ArrayLit(items) => {
            for i in items {
                f(i);
            }
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_)
        | Expr::Closure { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unsigned_fixed_comparisons_preserve_order_on_both_word_sizes() {
        for width in [8, 16, 32] {
            let high = 1i64 << (width - 1);
            let maximum = (1i64 << width) - 1;
            for (lhs, rhs) in [
                (0, high),
                (high, 0),
                (high - 1, high),
                (maximum, maximum),
                (-1, 1),
            ] {
                let mask = maximum as u64;
                let (a, b) = (lhs as u64 & mask, rhs as u64 & mask);
                for (op, expected) in [("<", a < b), ("<=", a <= b), (">", a > b), (">=", a >= b)] {
                    let value = super::unsigned_compare_expr(
                        op,
                        super::Expr::IntLit(lhs),
                        super::Expr::IntLit(rhs),
                        width,
                    )
                    .unwrap();
                    for semantics in [
                        crate::integer_semantics::IntegerSemantics::Word32,
                        crate::integer_semantics::IntegerSemantics::Word64,
                    ] {
                        let result = crate::core_opt::map_expr(value.clone(), &mut |expr| {
                            crate::core_opt::fold_expr_with_semantics(expr, semantics)
                        });
                        assert_eq!(
                            result,
                            super::Expr::BoolLit(expected),
                            "{width}/{lhs}/{op}/{rhs}/{semantics:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn unsigned_comparison_keeps_operands_once_and_rejects_unsupported_forms() {
        let call = |name: &str| super::Expr::Call {
            callee: Box::new(super::Expr::Ident(name.into())),
            args: vec![],
        };
        let value = super::unsigned_compare_expr("<", call("left"), call("right"), 32).unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        crate::core_opt::map_expr(value, &mut |expr| {
            if let super::Expr::Call { callee, .. } = &expr
                && let super::Expr::Ident(name) = callee.as_ref()
            {
                calls.borrow_mut().push(name.clone());
            }
            expr
        });
        assert_eq!(calls.into_inner(), ["left", "right"]);
        for width in [0, 1, 7, 64, 128] {
            assert!(
                super::unsigned_compare_expr("<", call("left"), call("right"), width).is_none()
            );
        }
        for op in ["==", "!=", "+", "lt"] {
            assert!(super::unsigned_compare_expr(op, call("left"), call("right"), 32).is_none());
        }
    }

    #[test]
    fn reserved_integer_destination_metadata_has_exact_supported_widths() {
        for width in ["i8", "u8", "i16", "u16", "i32", "u32"] {
            let name = format!("#integer-{width}");
            assert_eq!(super::integer_type_name(&name), Some(width));
            assert_eq!(super::Typ::Named(name).canonical(), super::Typ::Int);
            assert_eq!(super::integer_type_name(width), None);
            // Existing public canonical aliases retain their previous behavior.
            assert_eq!(super::Typ::Named(width.into()).canonical(), super::Typ::Int);
        }
        for name in [
            "#integer-u7",
            "#integer-i64",
            "#integer-",
            "#integer-u8-extra",
            "prefix#integer-u8",
            "u7",
            "Widget",
        ] {
            assert_eq!(super::integer_type_name(name), None);
            let typ = super::Typ::Named(name.into());
            assert_eq!(typ.canonical(), typ);
        }
    }

    #[test]
    fn radix_patterns_share_signed_literal_bounds() {
        for (literal, expected) in [
            ("9223372036854775807", i64::MAX),
            ("0x7fffffffffffffff", i64::MAX),
            ("0O777777777777777777777", i64::MAX),
            (
                "0B111111111111111111111111111111111111111111111111111111111111111",
                i64::MAX,
            ),
            ("-9223372036854775807", i64::MIN + 1),
            ("-0X7fffffffffffffff", i64::MIN + 1),
            ("-0o777777777777777777777", i64::MIN + 1),
            (
                "-0b111111111111111111111111111111111111111111111111111111111111111",
                i64::MIN + 1,
            ),
        ] {
            assert_eq!(
                super::parse_int_literal(literal),
                Some(expected),
                "{literal}"
            );
            assert_eq!(
                super::MatchPattern::parse(literal),
                Ok(super::MatchPattern::IntPat(expected))
            );
        }
        for literal in [
            "-9223372036854775808",
            "-0x8000000000000000",
            "-0O1000000000000000000000",
            "-0B1000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert_eq!(super::parse_int_literal(literal), Some(i64::MIN));
            assert_eq!(
                super::MatchPattern::parse(literal),
                Ok(super::MatchPattern::IntPat(i64::MIN))
            );
        }
        for literal in ["0o755", "0O755", "0b111101101", "0B111101101", "0x1ed"] {
            assert_eq!(
                super::MatchPattern::parse(literal),
                Ok(super::MatchPattern::IntPat(493))
            );
        }
        for literal in [
            "0x8000000000000000",
            "0o1000000000000000000000",
            "0b1000000000000000000000000000000000000000000000000000000000000000",
            "-0x8000000000000001",
            "0x-1",
            "0x+1",
        ] {
            assert_eq!(super::parse_int_literal(literal), None, "{literal}");
            assert!(super::MatchPattern::parse(literal).is_err(), "{literal}");
        }
    }

    #[test]
    fn numeric_literal_detection_checks_radix_digits_without_parsing_range() {
        for literal in [
            "9223372036854775808",
            "-0x8000000000000000",
            "+0XFF",
            "0o1000000000000000000000",
            "-0O7",
            "0b100",
            "+0B1",
        ] {
            assert!(super::looks_like_int_literal(literal), "{literal}");
        }
        for literal in ["0x", "0o8", "0b2", "0x-1", "0x+1", "--1", "0o_7", "name"] {
            assert!(!super::looks_like_int_literal(literal), "{literal}");
        }
    }

    #[test]
    fn unit_patterns_keep_unit_distinct_from_zero_and_empty_arrays() {
        use super::*;
        let unit = MatchPattern::parse("()").unwrap();
        assert_eq!(unit.typed_bindings(&Typ::Void, &|_| None), Ok(vec![]));
        assert!(unit.irrefutable(&Typ::Void, &|_| None));
        assert!(unit.typed_bindings(&Typ::Int, &|_| None).is_err());
        assert!(
            unit.typed_bindings(&Typ::Array(Box::new(Typ::Int)), &|_| None)
                .is_err()
        );
        assert!(
            MatchPattern::parse("[]")
                .unwrap()
                .typed_bindings(&Typ::Void, &|_| None)
                .is_err()
        );
        assert!(!expr_contains_call(&Expr::UnitLit));
    }

    #[test]
    fn value_matches_require_unguarded_exhaustive_patterns_and_compatible_values() {
        use super::*;
        let arm = |pattern: &str, guard: Option<Expr>, value| MatchValueArm {
            pattern: pattern.into(),
            guard,
            value,
        };
        let arms = vec![
            arm("true", None, Expr::IntLit(42)),
            arm("false", None, Expr::IntLit(0)),
        ];
        assert!(match_value_exhaustive(&Typ::Bool, &arms, &|_| None));
        assert_eq!(
            match_value_type(&arms, [Some(Typ::Int), Some(Typ::Int)]),
            Some(Typ::Int)
        );
        assert_eq!(
            match_value_type(&arms, [Some(Typ::Int), Some(Typ::Bool)]),
            None
        );
        assert!(!match_value_exhaustive(&Typ::Bool, &arms[..1], &|_| None));
        let guarded = vec![arm("_", Some(Expr::BoolLit(true)), Expr::IntLit(42))];
        assert!(!match_value_exhaustive(&Typ::Int, &guarded, &|_| None));
        let fixed_array = vec![arm("[x]", None, Expr::IntLit(42))];
        assert!(!match_value_exhaustive(
            &Typ::Array(Box::new(Typ::Int)),
            &fixed_array,
            &|_| None
        ));
        let rest_array = vec![arm("[..]", None, Expr::IntLit(42))];
        assert!(match_value_exhaustive(
            &Typ::Array(Box::new(Typ::Int)),
            &rest_array,
            &|_| None
        ));
    }

    #[test]
    fn vector_sources_only_infer_empty_literals() {
        let destination = super::Typ::Vector(Box::new(super::Typ::Int));
        let unknown = super::Typ::Array(Box::new(super::Typ::Named("Any".into())));
        assert!(destination.accepts_vector_source(&super::Expr::ArrayLit(vec![]), &unknown));
        assert!(!destination.accepts_vector_source(
            &super::Expr::ArrayLit(vec![super::Expr::IntLit(42)]),
            &unknown
        ));
        assert!(!destination.accepts_vector_source(&super::Expr::Ident("source".into()), &unknown));
        assert!(
            !destination.accepts_vector_source(&super::Expr::ArrayLit(vec![]), &super::Typ::Int)
        );
    }

    #[test]
    fn tuple_rest_bindings_use_actual_suffix_field_types() {
        let fields = vec![
            ("_0".into(), Typ::Int),
            ("_1".into(), Typ::String),
            ("_2".into(), Typ::Bool),
        ];
        let schema = |name: &str| (name == "Tuple").then_some(fields.as_slice());
        let typ = Typ::Named("Tuple".into());
        assert!(
            MatchPattern::parse("(a,..,b)")
                .unwrap()
                .irrefutable(&typ, &schema)
        );
        assert!(
            !MatchPattern::parse("(1,..,b)")
                .unwrap()
                .irrefutable(&typ, &schema)
        );
        assert!(
            !MatchPattern::parse("(a,b)")
                .unwrap()
                .irrefutable(&typ, &schema)
        );
        let array_fields = vec![("_0".into(), Typ::Array(Box::new(Typ::Int)))];
        let array_schema = |name: &str| (name == "WithArray").then_some(array_fields.as_slice());
        assert!(
            !MatchPattern::parse("([a,b],)")
                .unwrap()
                .irrefutable(&Typ::Named("WithArray".into()), &array_schema)
        );

        assert!(
            MatchPattern::parse("(.., true)")
                .unwrap()
                .typed_bindings(&typ, &schema)
                .is_ok()
        );
        assert!(
            MatchPattern::parse("(.., \"wrong\")")
                .unwrap()
                .typed_bindings(&typ, &schema)
                .is_err()
        );
        assert_eq!(
            MatchPattern::parse("(a, .., b)")
                .unwrap()
                .typed_bindings(&typ, &schema)
                .unwrap(),
            vec![("a".into(), Typ::Int), ("b".into(), Typ::Bool)]
        );
        assert_eq!(
            MatchPattern::parse("(.., b)")
                .unwrap()
                .typed_bindings(&typ, &schema)
                .unwrap(),
            vec![("b".into(), Typ::Bool)]
        );
        assert!(
            MatchPattern::parse("(a,b)")
                .unwrap()
                .typed_bindings(&typ, &schema)
                .is_err()
        );
        assert!(
            MatchPattern::parse("(a,b,c,d,..)")
                .unwrap()
                .typed_bindings(&typ, &schema)
                .is_err()
        );
        assert!(
            MatchPattern::parse("[]")
                .unwrap()
                .typed_bindings(&Typ::Int, &schema)
                .is_err()
        );
        assert!(
            MatchPattern::parse("(..)")
                .unwrap()
                .typed_bindings(&Typ::Int, &schema)
                .is_err()
        );
    }

    #[test]
    fn typed_pattern_bindings_preserve_fields_and_reject_wrong_shapes() {
        let fields = vec![("value".to_string(), Typ::Float)];
        let schema = |name: &str| (name == "Point").then_some(fields.as_slice());
        let pattern = MatchPattern::parse("Point { value: item }").unwrap();
        assert_eq!(
            pattern
                .typed_bindings(&Typ::Named("Point".into()), &schema)
                .unwrap(),
            vec![("item".into(), Typ::Float)]
        );
        assert!(pattern.irrefutable(&Typ::Named("Point".into()), &schema));
        assert!(pattern.typed_bindings(&Typ::Int, &schema).is_err());
        assert!(
            MatchPattern::parse("Point { missing: item }")
                .unwrap()
                .typed_bindings(&Typ::Named("Point".into()), &schema)
                .is_err()
        );
        let array = MatchPattern::parse("[a, b]").unwrap();
        assert_eq!(
            array
                .typed_bindings(&Typ::Array(Box::new(Typ::Bool)), &schema)
                .unwrap(),
            vec![("a".into(), Typ::Bool), ("b".into(), Typ::Bool)]
        );
        assert!(!array.irrefutable(&Typ::Array(Box::new(Typ::Bool)), &schema));
        assert!(array.typed_bindings(&Typ::Int, &schema).is_err());
        assert!(
            MatchPattern::parse("(a,b)")
                .unwrap()
                .typed_bindings(&Typ::Array(Box::new(Typ::Int)), &schema)
                .is_err()
        );
        assert!(
            MatchPattern::parse("[a, .., ..]")
                .unwrap()
                .typed_bindings(&Typ::Array(Box::new(Typ::Int)), &schema)
                .is_err()
        );
        assert!(
            MatchPattern::parse("Point { value: .. }")
                .unwrap()
                .typed_bindings(&Typ::Named("Point".into()), &schema)
                .is_err()
        );
        for (pattern, typ) in [("1", Typ::String), ("true", Typ::Int), ("\"x\"", Typ::Bool)] {
            assert!(
                MatchPattern::parse(pattern)
                    .unwrap()
                    .typed_bindings(&typ, &schema)
                    .is_err()
            );
        }
        assert!(
            MatchPattern::parse("Point { value: true }")
                .unwrap()
                .typed_bindings(&Typ::Named("Point".into()), &schema)
                .is_err()
        );
        assert!(
            MatchPattern::parse("1")
                .unwrap()
                .typed_bindings(&Typ::Int, &schema)
                .is_ok()
        );
    }

    use super::*;

    // ─── Span ───────────────────────────────────────────────────────────

    #[test]
    fn span_new() {
        let s = Span::new(10, 5, "test.in");
        assert_eq!(s.line, 10);
        assert_eq!(s.col, 5);
        assert_eq!(s.file, "test.in");
    }

    #[test]
    fn span_unknown() {
        let s = Span::unknown();
        assert_eq!(s.line, 0);
        assert_eq!(s.col, 0);
        assert!(s.file.is_empty());
    }

    #[test]
    fn span_display_with_file() {
        let s = Span::new(10, 5, "test.in");
        assert_eq!(format!("{s}"), "test.in:10:5");
    }

    #[test]
    fn span_display_without_file() {
        let s = Span::new(10, 5, "");
        assert_eq!(format!("{s}"), "10:5");
    }

    #[test]
    fn span_default() {
        let s = Span::default();
        assert_eq!(s.line, 0);
        assert_eq!(s.col, 0);
        assert!(s.file.is_empty());
    }

    // ─── FloatVal ──────────────────────────────────────────────────────

    #[test]
    fn float_val_equality() {
        assert_eq!(FloatVal(1.0), FloatVal(1.0));
        assert_ne!(FloatVal(1.0), FloatVal(2.0));
    }

    #[test]
    fn float_val_nan_equality() {
        let nan1 = FloatVal(f64::NAN);
        let nan2 = FloatVal(f64::NAN);
        assert_eq!(nan1, nan2);
    }

    // ─── Typ ───────────────────────────────────────────────────────────

    #[test]
    fn typ_array_nesting() {
        let t = Typ::Array(Box::new(Typ::Int));
        assert_eq!(t, Typ::Array(Box::new(Typ::Int)));
    }

    #[test]
    fn typ_named() {
        let t = Typ::Named("MyStruct".to_string());
        if let Typ::Named(n) = &t {
            assert_eq!(n, "MyStruct");
        } else {
            panic!("expected Named");
        }
    }

    #[test]
    fn typ_generic() {
        let t = Typ::Generic("T".to_string());
        if let Typ::Generic(n) = &t {
            assert_eq!(n, "T");
        } else {
            panic!("expected Generic");
        }
    }

    // ─── MatchPattern ──────────────────────────────────────────────────

    #[test]
    fn match_pattern_wild() {
        assert_eq!(MatchPattern::parse("_").unwrap(), MatchPattern::WildPat);
        assert_eq!(MatchPattern::parse("-").unwrap(), MatchPattern::WildPat);
        assert_eq!(MatchPattern::parse("else").unwrap(), MatchPattern::WildPat);
        assert_eq!(
            MatchPattern::parse("default").unwrap(),
            MatchPattern::WildPat
        );
    }

    #[test]
    fn match_pattern_rest() {
        assert_eq!(MatchPattern::parse("..").unwrap(), MatchPattern::RestPat);
    }

    #[test]
    fn match_pattern_bool() {
        assert_eq!(
            MatchPattern::parse("true").unwrap(),
            MatchPattern::BoolPat(true)
        );
        assert_eq!(
            MatchPattern::parse("false").unwrap(),
            MatchPattern::BoolPat(false)
        );
    }

    #[test]
    fn match_pattern_int() {
        assert_eq!(MatchPattern::parse("42").unwrap(), MatchPattern::IntPat(42));
        assert_eq!(MatchPattern::parse("-1").unwrap(), MatchPattern::IntPat(-1));
    }

    #[test]
    fn match_pattern_string() {
        assert_eq!(
            MatchPattern::parse("\"hello\"").unwrap(),
            MatchPattern::StringPat("hello".to_string())
        );
    }

    #[test]
    fn string_boundaries_reject_interior_quotes_and_incomplete_escapes() {
        for source in [r#""a"b""#, r#""a\""#] {
            assert_eq!(decode_string_literal(source), None, "{source}");
            assert!(MatchPattern::parse(source).is_err(), "{source}");
        }
        for (source, value) in [
            (r#""a\"b""#, "a\"b"),
            (r#""a\\""#, "a\\"),
            (r#""\q""#, r"\q"),
            (r#""line\nnext""#, "line\nnext"),
            ("\"\"", ""),
        ] {
            assert_eq!(
                decode_string_literal(source).as_deref(),
                Some(value),
                "{source}"
            );
            assert_eq!(
                MatchPattern::parse(source),
                Ok(MatchPattern::StringPat(value.into()))
            );
        }
    }

    #[test]
    fn match_pattern_ident() {
        for name in ["x", "helper_name", "_value", "mixed_name-more", "value2"] {
            let pattern = MatchPattern::parse(name).unwrap();
            assert_eq!(pattern, MatchPattern::IdentPat(name.into()));
            assert_eq!(
                pattern.typed_bindings(&Typ::Int, &|_| None).unwrap(),
                vec![(name.into(), Typ::Int)]
            );
        }
        assert_eq!(MatchPattern::parse("_").unwrap(), MatchPattern::WildPat);
        assert!(MatchPattern::parse("2value").is_err());
    }

    #[test]
    fn match_pattern_tuple() {
        let pat = MatchPattern::parse("(1, 2)").unwrap();
        assert_eq!(
            pat,
            MatchPattern::TuplePat(vec![MatchPattern::IntPat(1), MatchPattern::IntPat(2)])
        );
    }

    #[test]
    fn match_pattern_array() {
        let pat = MatchPattern::parse("[1, 2]").unwrap();
        assert_eq!(
            pat,
            MatchPattern::ArrayPat(vec![MatchPattern::IntPat(1), MatchPattern::IntPat(2)])
        );
    }

    #[test]
    fn qualified_struct_patterns_preserve_names_and_typed_bindings() {
        for name in [
            "dep::Entry",
            "dep_name::Entry_type",
            "依存::回答",
            "dep::Cafe\u{301}",
            "dep-name::Entry-type",
        ] {
            let source = format!("{name}{{value: answer}}");
            let pattern = MatchPattern::parse(&source).expect("qualified struct pattern");
            let rendered = crate::core_bindings::render_pattern(&pattern);
            assert_eq!(MatchPattern::parse(&rendered).unwrap(), pattern);
            let schema = vec![("value".into(), Typ::Int)];
            assert_eq!(
                pattern
                    .typed_bindings(&Typ::Named(name.into()), &|lookup| (lookup == name)
                        .then_some(schema.as_slice()))
                    .unwrap(),
                vec![("answer".into(), Typ::Int)]
            );
        }
        for name in [
            "",
            "::Entry",
            "dep::",
            "dep::::Entry",
            "dep:Entry",
            "dep::9Entry",
            "dep::Entry<T>",
            "dep::Entry value",
        ] {
            assert!(
                MatchPattern::parse(&format!("{name}{{value: answer}}")).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn match_pattern_struct() {
        let pat = MatchPattern::parse("Point{x: 1, y: 2}").unwrap();
        if let MatchPattern::StructPat { name, fields } = pat {
            assert_eq!(name, "Point");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0], ("x".to_string(), MatchPattern::IntPat(1)));
            assert_eq!(fields[1], ("y".to_string(), MatchPattern::IntPat(2)));
        } else {
            panic!("expected StructPat");
        }
    }

    #[test]
    fn match_pattern_struct_shorthand() {
        let pat = MatchPattern::parse("Point{x, y}").unwrap();
        if let MatchPattern::StructPat { name, fields } = pat {
            assert_eq!(name, "Point");
            assert_eq!(
                fields[0],
                ("x".to_string(), MatchPattern::IdentPat("x".to_string()))
            );
        } else {
            panic!("expected StructPat");
        }
    }

    #[test]
    fn match_pattern_case_prefix() {
        assert_eq!(
            MatchPattern::parse("case 42").unwrap(),
            MatchPattern::IntPat(42)
        );
    }

    #[test]
    fn match_pattern_trailing_colon() {
        assert_eq!(
            MatchPattern::parse("42:").unwrap(),
            MatchPattern::IntPat(42)
        );
    }

    #[test]
    fn match_pattern_empty_error() {
        assert!(MatchPattern::parse("").is_err());
    }

    // ─── split_match_pat_args ──────────────────────────────────────────

    #[test]
    fn split_simple_args() {
        let args = split_match_pat_args("1, 2, 3");
        assert_eq!(args, vec!["1", "2", "3"]);
    }

    #[test]
    fn split_nested_args() {
        let args = split_match_pat_args("(1, 2), 3");
        assert_eq!(args, vec!["(1, 2)", "3"]);
    }

    #[test]
    fn quoted_pattern_separators_roundtrip_without_changing_shape() {
        for value in [
            "a,b",
            "[({",
            "])}",
            "quote\",comma",
            "slash\\",
            "slash\\\",comma",
            "line\n,next",
        ] {
            let pattern = MatchPattern::StructPat {
                name: "dep::Entry".into(),
                fields: vec![
                    ("text".into(), MatchPattern::StringPat(value.into())),
                    (
                        "nested".into(),
                        MatchPattern::TuplePat(vec![
                            MatchPattern::ArrayPat(vec![
                                MatchPattern::StringPat(value.into()),
                                MatchPattern::IntPat(7),
                            ]),
                            MatchPattern::IntPat(42),
                        ]),
                    ),
                ],
            };
            let rendered = crate::core_bindings::render_pattern(&pattern);
            assert_eq!(MatchPattern::parse(&rendered), Ok(pattern), "{rendered}");
            let string =
                crate::core_bindings::render_pattern(&MatchPattern::StringPat(value.into()));
            assert_eq!(
                split_match_pat_args(&format!("{string}, 42")),
                vec![string, "42".into()]
            );
        }
    }

    #[test]
    fn split_empty() {
        let args = split_match_pat_args("");
        assert!(args.is_empty());
    }

    // ─── UnifiedModule ──────────────────────────────────────────────────

    #[test]
    fn unified_module_new() {
        let m = UnifiedModule::new(vec![]);
        assert!(m.decls.is_empty());
        assert_eq!(m.identity, CoreModuleIdentity::default());
    }

    #[test]
    fn unified_module_with_identity() {
        let id = CoreModuleIdentity {
            package: Some("my_pkg".to_string()),
            module: Some("my_mod".to_string()),
        };
        let m = UnifiedModule::with_identity(vec![], id.clone());
        assert_eq!(m.identity, id);
    }

    #[test]
    fn effective_module_id_uses_requested_when_not_app() {
        let m = UnifiedModule::new(vec![]);
        assert_eq!(m.effective_module_id("Custom"), "Custom");
    }

    #[test]
    fn effective_module_id_falls_back_to_module() {
        let id = CoreModuleIdentity {
            package: Some("pkg".to_string()),
            module: Some("mod_name".to_string()),
        };
        let m = UnifiedModule::with_identity(vec![], id);
        assert_eq!(m.effective_module_id("App"), "mod_name");
    }

    #[test]
    fn effective_module_id_falls_back_to_package() {
        let id = CoreModuleIdentity {
            package: Some("pkg".to_string()),
            module: None,
        };
        let m = UnifiedModule::with_identity(vec![], id);
        assert_eq!(m.effective_module_id("App"), "pkg");
    }

    #[test]
    fn effective_module_id_falls_back_to_app() {
        let m = UnifiedModule::new(vec![]);
        assert_eq!(m.effective_module_id("App"), "App");
    }

    #[test]
    fn identity_report() {
        let id = CoreModuleIdentity {
            package: Some("pkg".to_string()),
            module: Some("mod_name".to_string()),
        };
        let m = UnifiedModule::with_identity(vec![], id);
        let report = m.identity_report("App");
        assert_eq!(report.requested_module_id, "App");
        assert_eq!(report.effective_module_id, "mod_name");
        assert_eq!(report.package, Some("pkg".to_string()));
    }

    // ─── Interrupt FNs ──────────────────────────────────────────────────

    #[test]
    fn interrupt_fn_registration() {
        register_interrupt_fn("my_isr");
        assert!(is_interrupt_fn("my_isr"));
        assert!(!is_interrupt_fn("not_registered"));
    }

    // ─── Visibility / Import / MethodSig ───────────────────────────────

    #[test]
    fn visibility_variants() {
        let v = Visibility::Pub;
        assert_eq!(v, Visibility::Pub);
        assert_ne!(v, Visibility::Private);
        assert_ne!(v, Visibility::Internal);
    }

    #[test]
    fn import_with_alias() {
        let imp = Import {
            path: "std.io".to_string(),
            alias: Some("io".to_string()),
        };
        assert_eq!(imp.path, "std.io");
        assert_eq!(imp.alias.unwrap(), "io");
    }

    #[test]
    fn method_sig_round_trip() {
        let sig = MethodSig {
            name: "foo".to_string(),
            params: vec![("x".to_string(), Typ::Int)],
            ret: Typ::Void,
        };
        assert_eq!(sig.name, "foo");
        assert_eq!(sig.params.len(), 1);
    }
}
