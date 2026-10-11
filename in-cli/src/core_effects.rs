//! Identify fallible direct calls without charging pure functions for error checks.
use crate::core_ir::{Expr, Stmt};
use std::collections::{HashMap, HashSet, VecDeque};

pub(crate) fn fallible_functions<'a>(
    functions: impl IntoIterator<Item = (&'a str, &'a [Stmt])>,
) -> HashSet<String> {
    let mut callers: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut fallible = HashSet::new();
    let mut queue = VecDeque::new();
    for (name, body) in functions {
        let mut calls = HashSet::new();
        if scan_body(body, &mut calls) {
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

fn scan_expr<'a>(expr: &'a Expr, calls: &mut HashSet<&'a str>) {
    if let Expr::Call { callee, .. } = expr
        && let Expr::Ident(name) = callee.as_ref()
    {
        calls.insert(name);
    }
    crate::core_ir::for_each_expr_child(expr, &mut |child| scan_expr(child, calls));
}

fn scan_body<'a>(body: &'a [Stmt], calls: &mut HashSet<&'a str>) -> bool {
    let mut fallible = false;
    for stmt in body {
        match stmt {
            Stmt::Throw(expr) => {
                fallible = true;
                scan_expr(expr, calls);
            }
            Stmt::Propagate => fallible = true,
            Stmt::Let(_, _, expr)
            | Stmt::Assign(_, expr)
            | Stmt::Expr(expr)
            | Stmt::Return(Some(expr)) => scan_expr(expr, calls),
            Stmt::FieldAssign { base, value, .. } => {
                scan_expr(base, calls);
                scan_expr(value, calls);
            }
            Stmt::IndexAssign { base, index, value } => {
                scan_expr(base, calls);
                scan_expr(index, calls);
                scan_expr(value, calls);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                scan_expr(cond, calls);
                fallible |= scan_body(then_body, calls);
                fallible |= scan_body(else_body, calls);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    scan_expr(cond, calls);
                }
                fallible |= scan_body(body, calls);
            }
            Stmt::Match { scrutinee, arms } => {
                scan_expr(scrutinee, calls);
                for arm in arms {
                    fallible |= scan_body(&arm.body, calls);
                }
            }
            Stmt::Try { body, catches } => {
                fallible = true;
                scan_body(body, calls);
                for catch in catches {
                    scan_body(&catch.body, calls);
                }
            }
            _ => {}
        }
    }
    fallible
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn error_effects_reach_recursive_callers_but_not_pure_functions() {
        let module = crate::in_lang_parse::parse_in_source("fn fail() -> Int { throw 42; return 0; }\nfn first() -> Int { return second(); }\nfn second() -> Int { first(); return fail(); }\nfn pure() -> Int { return 7; }\nfn main() -> Int { return pure(); }").unwrap();
        let functions = module.decls.iter().filter_map(|decl| match decl {
            crate::core_ir::Decl::Function { name, body, .. } => {
                Some((name.as_str(), body.as_slice()))
            }
            _ => None,
        });
        assert_eq!(
            fallible_functions(functions),
            HashSet::from(["fail".into(), "first".into(), "second".into()])
        );
    }
}
