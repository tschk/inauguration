//! Canonical unsigned atomic cells and their Core IR operation signatures.
use crate::core_ir::{Expr, Typ};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    U32,
    U64,
    Usize,
}
impl Kind {
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::Usize => "usize",
        }
    }
    pub(crate) fn type_name(self) -> &'static str {
        match self {
            Self::U32 => "std::sync::atomic::AtomicU32",
            Self::U64 => "std::sync::atomic::AtomicU64",
            Self::Usize => "std::sync::atomic::AtomicUsize",
        }
    }
    pub(crate) fn typ(self) -> Typ {
        Typ::Named(self.type_name().into())
    }
    pub(crate) fn wide(self) -> bool {
        self != Self::U32
    }
    pub(crate) fn from_type(typ: &Typ) -> Option<Self> {
        [Self::U32, Self::U64, Self::Usize]
            .into_iter()
            .find(|kind| typ == &kind.typ())
    }
    pub(crate) fn intrinsic(self, op: &str) -> String {
        format!("__inrt_atomic_{}_{}", self.suffix(), op)
    }
}
pub(crate) struct Signature {
    pub(crate) ret: Typ,
    pub(crate) params: Vec<Typ>,
}
static SIGNATURES: LazyLock<HashMap<String, Signature>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for kind in [Kind::U32, Kind::U64, Kind::Usize] {
        for (op, ret, params) in [
            ("new", kind.typ(), vec![Typ::Int]),
            (
                "load",
                Typ::Int,
                vec![kind.typ(), Typ::Named("std::sync::atomic::Ordering".into())],
            ),
            (
                "store",
                Typ::Void,
                vec![
                    kind.typ(),
                    Typ::Int,
                    Typ::Named("std::sync::atomic::Ordering".into()),
                ],
            ),
            (
                "fetch_add",
                Typ::Int,
                vec![
                    kind.typ(),
                    Typ::Int,
                    Typ::Named("std::sync::atomic::Ordering".into()),
                ],
            ),
        ] {
            map.insert(kind.intrinsic(op), Signature { ret, params });
        }
    }
    for order in 0..5 {
        map.insert(
            format!("__inrt_atomic_order_{order}"),
            Signature {
                ret: Typ::Named("std::sync::atomic::Ordering".into()),
                params: vec![],
            },
        );
    }
    map
});
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    SIGNATURES.keys().map(String::as_str)
}
pub(crate) fn signature(name: &str) -> Option<&'static Signature> {
    SIGNATURES.get(name)
}
pub(crate) fn operation(name: &str) -> Option<(Kind, &str)> {
    for kind in [Kind::U32, Kind::U64, Kind::Usize] {
        let prefix = format!("__inrt_atomic_{}_", kind.suffix());
        if let Some(op) = name.strip_prefix(&prefix) {
            if matches!(op, "new" | "load" | "store" | "fetch_add") {
                return Some((kind, op));
            }
        }
    }
    None
}
pub(crate) fn ordering(expr: &Expr) -> Option<i64> {
    let Expr::Call { callee, args } = expr else {
        return None;
    };
    if !args.is_empty() {
        return None;
    }
    let Expr::Ident(name) = callee.as_ref() else {
        return None;
    };
    name.strip_prefix("__inrt_atomic_order_")?
        .parse()
        .ok()
        .filter(|value| (0..5).contains(value))
}
pub(crate) fn check_order(op: &str, order: &Expr) -> Result<(), String> {
    let order =
        ordering(order).ok_or("native atomic operations currently require a constant ordering")?;
    let allowed = match op {
        "load" => matches!(order, 0 | 1 | 4),
        "store" => matches!(order, 0 | 2 | 4),
        "fetch_add" => matches!(order, 0..=4),
        _ => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(format!("invalid ordering for atomic {op}"))
    }
}
