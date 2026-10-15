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
    let functions: Vec<_> = functions.into_iter().collect();
    let declared: HashSet<_> = functions.iter().map(|(name, _, _, _)| *name).collect();
    let mut callers: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut fallible = HashSet::new();
    let mut queue = VecDeque::new();
    for (name, body, ret, params) in functions {
        let mut calls = HashSet::new();
        let aggregate = matches!(
            ret.canonical(),
            crate::core_ir::Typ::String
                | crate::core_ir::Typ::Array(_)
                | crate::core_ir::Typ::Vector(_)
                | crate::core_ir::Typ::Named(_)
        );
        let body_fallible = scan_body(body, &mut calls, aggregate)
            || calls.iter().any(|callee| {
                matches!(*callee, "invoke" | "invoke1" | "invoke2") && !declared.contains(callee)
            });
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

fn scan_expr<'a>(
    expr: &'a Expr,
    calls: &mut HashSet<&'a str>,
    checked: &mut bool,
    fallible: &mut bool,
    aggregate: bool,
) {
    if let Expr::Block { body, .. } = expr {
        *fallible |= scan_body(body, calls, aggregate);
    }
    if matches!(
        expr,
        Expr::Index { .. } | Expr::Field { .. } | Expr::StructInit { .. }
    ) || matches!(expr, Expr::Binary { op, .. } if matches!(op.as_str(), "/" | "%" | "/=" | "%=" | "udiv" | "urem"))
    {
        *checked = true;
    }
    if let Expr::Call { callee, .. } = expr
        && let Expr::Ident(name) = callee.as_ref()
    {
        calls.insert(name);
    }
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        scan_expr(child, calls, checked, fallible, aggregate)
    });
}

fn scan_body<'a>(body: &'a [Stmt], calls: &mut HashSet<&'a str>, aggregate: bool) -> bool {
    let mut fallible = false;
    let mut checked = false;
    for stmt in body {
        match stmt {
            Stmt::Throw(expr) => {
                fallible = true;
                scan_expr(expr, calls, &mut checked, &mut fallible, aggregate);
            }
            Stmt::Propagate => fallible = true,
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::Expr(expr)
            | Stmt::Return(Some(expr)) => {
                scan_expr(expr, calls, &mut checked, &mut fallible, aggregate)
            }
            Stmt::FieldAssign { base, value, .. } => {
                checked = true;
                scan_expr(base, calls, &mut checked, &mut fallible, aggregate);
                scan_expr(value, calls, &mut checked, &mut fallible, aggregate);
            }
            Stmt::IndexAssign { base, index, value } => {
                checked = true;
                scan_expr(base, calls, &mut checked, &mut fallible, aggregate);
                scan_expr(index, calls, &mut checked, &mut fallible, aggregate);
                scan_expr(value, calls, &mut checked, &mut fallible, aggregate);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                scan_expr(cond, calls, &mut checked, &mut fallible, aggregate);
                fallible |= scan_body(then_body, calls, aggregate);
                fallible |= scan_body(else_body, calls, aggregate);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    scan_expr(cond, calls, &mut checked, &mut fallible, aggregate);
                }
                fallible |= scan_body(body, calls, aggregate);
            }
            Stmt::Match { scrutinee, arms } => {
                scan_expr(scrutinee, calls, &mut checked, &mut fallible, aggregate);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        scan_expr(guard, calls, &mut checked, &mut fallible, aggregate);
                    }
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
    fn expr(value: &Expr, seeds: &mut Vec<String>, aliases: &mut HashMap<String, Vec<String>>) {
        if let Expr::Closure { .. } = value {
            return;
        }
        if let Expr::Block { body, .. } = value {
            scan(body, seeds, aliases);
        }
        crate::core_ir::for_each_expr_child(value, &mut |child| expr(child, seeds, aliases));
    }
    fn scan(body: &[Stmt], seeds: &mut Vec<String>, aliases: &mut HashMap<String, Vec<String>>) {
        for stmt in body {
            match stmt {
                Stmt::Let(_, _, value)
                | Stmt::Assign(_, value)
                | Stmt::Expr(value)
                | Stmt::Throw(value)
                | Stmt::Return(Some(value)) => expr(value, seeds, aliases),
                Stmt::FieldAssign { base, value, .. } => {
                    expr(base, seeds, aliases);
                    expr(value, seeds, aliases);
                }
                Stmt::IndexAssign { base, index, value } => {
                    expr(base, seeds, aliases);
                    expr(index, seeds, aliases);
                    expr(value, seeds, aliases);
                }
                Stmt::If { cond, .. } => expr(cond, seeds, aliases),
                Stmt::Loop {
                    cond: Some(cond), ..
                } => expr(cond, seeds, aliases),
                Stmt::Match { scrutinee, arms } => {
                    expr(scrutinee, seeds, aliases);
                    for arm in arms {
                        if let Some(guard) = &arm.guard {
                            expr(guard, seeds, aliases);
                        }
                    }
                }
                _ => {}
            }
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
    fn raw_native_calls_do_not_require_a_language_error_carrier() {
        let module = crate::in_lang_parse::parse_in_source("fn target(value:Int)->Int{return value+1;}\nfn raw()->Int{return invoke-native1(target,41);}\nfn raw_alias()->Int{return invoke_native1(target,41);}\nfn checked()->Int{return invoke1(target,41);}\nfn main()->Int{return raw();}").unwrap();
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
            HashSet::from(["checked".into()])
        );
    }

    #[test]
    fn indirect_calls_are_fallible_and_declared_overrides_remain_pure() {
        let module=crate::in_lang_parse::parse_in_source("fn indirect() -> Int { return invoke1(0, 1); }\nfn relay() -> Int { return indirect(); }\nfn aggregate() -> [Int] { return [relay()]; }\nfn invoke2(pointer: Int, first: Int, second: Int) -> Int { return 42; }\nfn shadow() -> Int { return invoke2(0, 0, 0); }\nfn main() -> Int { return 0; }").unwrap();
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
            HashSet::from(["indirect".into(), "relay".into(), "aggregate".into()])
        );
    }

    #[test]
    fn scalar_block_throws_and_calls_propagate_fallibility() {
        let bodies = [
            (
                "fail",
                vec![Stmt::Expr(Expr::Block {
                    body: vec![Stmt::Throw(Expr::IntLit(42))],
                    result: Some(Box::new(Expr::IntLit(0))),
                })],
            ),
            (
                "relay",
                vec![Stmt::Return(Some(Expr::Block {
                    body: vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("fail".into())),
                        args: vec![],
                    })],
                    result: Some(Box::new(Expr::IntLit(0))),
                }))],
            ),
            (
                "propagate",
                vec![Stmt::Expr(Expr::Block {
                    body: vec![Stmt::Propagate],
                    result: None,
                })],
            ),
        ];
        let ret = crate::core_ir::Typ::Int;
        assert_eq!(
            fallible_functions(bodies.iter().map(|(name, body)| (
                *name,
                body.as_slice(),
                &ret,
                &[][..]
            ))),
            HashSet::from(["fail".into(), "relay".into(), "propagate".into()])
        );
    }

    #[test]
    fn block_expression_catches_and_aliases_get_error_slots() {
        let body = vec![Stmt::Expr(Expr::Block {
            body: vec![Stmt::Try {
                body: vec![Stmt::Throw(Expr::IntLit(42))],
                catches: vec![crate::core_ir::CatchArm {
                    pattern: "caught".into(),
                    body: vec![Stmt::Let(
                        "alias".into(),
                        None,
                        Expr::Ident("caught".into()),
                    )],
                }],
            }],
            result: Some(Box::new(Expr::IntLit(0))),
        })];
        assert_eq!(
            error_bindings(&body),
            ["alias".into(), "caught".into()].into_iter().collect()
        );
    }

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
