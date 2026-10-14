//! Track fallible calls and caught payload aliases for native error lowering.
use crate::core_ir::{Expr, Stmt};
use std::collections::{HashMap, HashSet, VecDeque};

pub(crate) fn fallible_functions<'a>(
    functions: impl IntoIterator<
        Item = (
            &'a str,
            &'a [Stmt],
            &'a crate::core_ir::Typ,
            &'a [(String, crate::core_ir::Typ)],
        ),
    >,
) -> HashSet<String> {
    let mut callers: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut fallible = HashSet::new();
    let mut queue = VecDeque::new();
    for (name, body, ret, params) in functions {
        let mut calls = HashSet::new();
        let aggregate = matches!(
            ret.canonical(),
            crate::core_ir::Typ::Array(_)
                | crate::core_ir::Typ::Vector(_)
                | crate::core_ir::Typ::Named(_)
        );
        let body_fallible = scan_body(body, &mut calls, aggregate);
        // Aggregate pointer checks in parameter unpacking and copies also fail before the body.
        let pointer_checks = aggregate
            && (matches!(ret.canonical(), crate::core_ir::Typ::Named(_))
                || params
                    .iter()
                    .any(|(_, typ)| matches!(typ.canonical(), crate::core_ir::Typ::Named(_))));
        if body_fallible || pointer_checks {
            fallible.insert(name);
            queue.push_back(name);
        }
        for callee in calls {
            callers.entry(callee).or_default().push(name);
        }
    }
    while let Some(callee) = queue.pop_front() {
        for &caller in callers.get(callee).into_iter().flatten() {
            if fallible.insert(caller) {
                queue.push_back(caller);
            }
        }
    }
    fallible.into_iter().map(str::to_owned).collect()
}

fn scan_expr<'a>(expr: &'a Expr, calls: &mut HashSet<&'a str>, checked: &mut bool) {
    if matches!(
        expr,
        Expr::Index { .. } | Expr::Field { .. } | Expr::StructInit { .. }
    ) || matches!(expr, Expr::Binary { op, .. } if matches!(op.as_str(), "/" | "%" | "/=" | "%="))
    {
        *checked = true;
    }
    if let Expr::Call { callee, .. } = expr
        && let Expr::Ident(name) = callee.as_ref()
    {
        calls.insert(name);
    }
    crate::core_ir::for_each_expr_child(expr, &mut |child| scan_expr(child, calls, checked));
}

fn scan_body<'a>(body: &'a [Stmt], calls: &mut HashSet<&'a str>, aggregate: bool) -> bool {
    let mut fallible = false;
    let mut checked = false;
    for stmt in body {
        match stmt {
            Stmt::Throw(expr) => {
                fallible = true;
                scan_expr(expr, calls, &mut checked);
            }
            Stmt::Propagate => fallible = true,
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::Expr(expr)
            | Stmt::Return(Some(expr)) => scan_expr(expr, calls, &mut checked),
            Stmt::FieldAssign { base, value, .. } => {
                checked = true;
                scan_expr(base, calls, &mut checked);
                scan_expr(value, calls, &mut checked);
            }
            Stmt::IndexAssign { base, index, value } => {
                checked = true;
                scan_expr(base, calls, &mut checked);
                scan_expr(index, calls, &mut checked);
                scan_expr(value, calls, &mut checked);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                scan_expr(cond, calls, &mut checked);
                fallible |= scan_body(then_body, calls, aggregate);
                fallible |= scan_body(else_body, calls, aggregate);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    scan_expr(cond, calls, &mut checked);
                }
                fallible |= scan_body(body, calls, aggregate);
            }
            Stmt::Match { scrutinee, arms } => {
                scan_expr(scrutinee, calls, &mut checked);
                for arm in arms {
                    fallible |= scan_body(&arm.body, calls, aggregate);
                }
            }
            Stmt::Try { body, catches } => {
                fallible = true;
                scan_body(body, calls, aggregate);
                for catch in catches {
                    scan_body(&catch.body, calls, aggregate);
                }
            }
            _ => {}
        }
    }
    fallible || (aggregate && checked)
}

/// Reserve type slots only for catch bindings and their direct local aliases.
pub(crate) fn error_bindings(body: &[Stmt]) -> std::collections::BTreeSet<String> {
    fn scan(body: &[Stmt], seeds: &mut Vec<String>, aliases: &mut HashMap<String, Vec<String>>) {
        for stmt in body {
            match stmt {
                Stmt::Let(name, _, Expr::Ident(source))
                | Stmt::Assign(name, Expr::Ident(source)) => {
                    aliases
                        .entry(source.clone())
                        .or_default()
                        .push(name.clone());
                }
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    scan(then_body, seeds, aliases);
                    scan(else_body, seeds, aliases);
                }
                Stmt::Loop { body, .. } => scan(body, seeds, aliases),
                Stmt::Match { scrutinee, arms } => {
                    for arm in arms {
                        if let Expr::Ident(source) = scrutinee
                            && let Ok(crate::core_ir::MatchPattern::IdentPat(name)) =
                                crate::core_ir::MatchPattern::parse(&arm.pattern)
                        {
                            aliases.entry(source.clone()).or_default().push(name);
                        }
                        scan(&arm.body, seeds, aliases);
                    }
                }
                Stmt::Try { body, catches } => {
                    scan(body, seeds, aliases);
                    for catch in catches {
                        if let Ok(crate::core_ir::MatchPattern::IdentPat(name)) =
                            crate::core_ir::MatchPattern::parse(&catch.pattern)
                        {
                            seeds.push(name);
                        }
                        scan(&catch.body, seeds, aliases);
                    }
                }
                _ => {}
            }
        }
    }
    let mut seeds = Vec::new();
    let mut aliases = HashMap::new();
    scan(body, &mut seeds, &mut aliases);
    let mut found = std::collections::BTreeSet::new();
    let mut queue: VecDeque<_> = seeds.into();
    while let Some(name) = queue.pop_front() {
        if found.insert(name.clone()) {
            queue.extend(aliases.get(&name).into_iter().flatten().cloned());
        }
    }
    found
}

/// Tags in the native error carrier at +16; +0 is the flag and +8 the value.
pub(crate) fn error_type_tag(typ: &crate::core_ir::Typ) -> Option<i64> {
    use crate::core_ir::Typ;
    match typ.canonical() {
        Typ::Int => Some(1),
        Typ::Bool => Some(2),
        Typ::Float => Some(3),
        Typ::String => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caught_type_slots_follow_alias_cycles_and_skip_unrelated_locals() {
        let module = crate::in_lang_parse::parse_in_source("fn main() -> Int { let plain = 7; try { throw false; } catch value { let alias = value; let next = alias; alias = next; try { throw 42; } catch other { let copy = other; } } return plain; }").unwrap();
        let crate::core_ir::Decl::Function { body, .. } = &module.decls[0] else {
            panic!("missing main")
        };
        assert_eq!(
            error_bindings(body),
            ["alias", "copy", "next", "other", "value"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert!(error_bindings(&[Stmt::Let("plain".into(), None, Expr::IntLit(7))]).is_empty());
    }

    #[test]
    fn checked_aggregate_failures_reach_callers_without_changing_scalar_failures() {
        let module = crate::in_lang_parse::parse_in_source("fn values() -> [Int] { let items = [20]; return [items[5]]; }\nfn relay() -> [Int] { return values(); }\nfn main() -> Int { return relay()[0]; }\nfn scalar() -> Int { let items = [20]; return items[5]; }\nfn pure() -> [Int] { return [42]; }").unwrap();
        let functions = module.decls.iter().filter_map(|decl| match decl {
            crate::core_ir::Decl::Function {
                name,
                body,
                ret,
                params,
                ..
            } => Some((name.as_str(), body.as_slice(), ret, params.as_slice())),
            _ => None,
        });
        assert_eq!(
            fallible_functions(functions),
            HashSet::from(["values".into(), "relay".into(), "main".into()])
        );
    }

    #[test]
    fn error_effects_reach_recursive_callers_but_not_pure_functions() {
        let module = crate::in_lang_parse::parse_in_source("fn fail() -> Int { throw 42; return 0; }\nfn first() -> Int { return second(); }\nfn second() -> Int { first(); return fail(); }\nfn pure() -> Int { return 7; }\nfn main() -> Int { return pure(); }").unwrap();
        let functions = module.decls.iter().filter_map(|decl| match decl {
            crate::core_ir::Decl::Function {
                name,
                body,
                ret,
                params,
                ..
            } => Some((name.as_str(), body.as_slice(), ret, params.as_slice())),
            _ => None,
        });
        assert_eq!(
            fallible_functions(functions),
            HashSet::from(["fail".into(), "first".into(), "second".into()])
        );
    }
}
