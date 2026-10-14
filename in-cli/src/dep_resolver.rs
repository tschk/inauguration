//! Lazy dependency resolver — replaces stub mechanism.
//!
//! When a module references an external function, resolver finds its
//! crate, loads and parses its source, registers the definitions, and
//! recurses until all symbols resolve.

use std::collections::HashSet;

use crate::core_ir::{Decl, Expr, Stmt, UnifiedModule};
use crate::crate_db::CrateDb;

/// Result of resolving dependencies for a module.
pub struct ResolveResult {
    /// Expanded module with all resolved dependencies merged in.
    pub module: UnifiedModule,
    pub files_parsed: usize,
    pub functions_added: usize,
}

/// Collect external function calls from a module.
fn collect_externals(module: &UnifiedModule) -> Vec<String> {
    let mut locals: HashSet<String> = HashSet::new();
    for decl in &module.decls {
        if let Decl::Function { name, .. } = decl {
            locals.insert(name.clone());
        }
    }

    let mut out = Vec::new();
    for decl in &module.decls {
        if let Decl::Function { body, params, .. } = decl {
            let mut scope = locals.clone();
            scope.extend(params.iter().map(|(name, _)| name.clone()));
            walk_stmts(body, &mut scope, &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn walk_stmts(stmts: &[Stmt], locals: &mut HashSet<String>, out: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Expr(e) | Stmt::Return(Some(e)) => walk_expr(e, locals, out),
            Stmt::Let(name, _, e) => {
                walk_expr(e, locals, out);
                locals.insert(name.clone());
            }
            Stmt::Assign(_, e) => walk_expr(e, locals, out),
            Stmt::FieldAssign { base, value, .. } => {
                walk_expr(base, locals, out);
                walk_expr(value, locals, out);
            }
            Stmt::IndexAssign { base, index, value } => {
                walk_expr(base, locals, out);
                walk_expr(index, locals, out);
                walk_expr(value, locals, out);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                walk_expr(cond, locals, out);
                walk_stmts(then_body, &mut locals.clone(), out);
                walk_stmts(else_body, &mut locals.clone(), out);
            }
            Stmt::Loop { kind, cond, body } => {
                if let Some(c) = cond {
                    walk_expr(c, locals, out);
                }
                let mut scope = locals.clone();
                if let crate::core_ir::LoopKind::For { binding } = kind {
                    scope.insert(binding.clone());
                }
                walk_stmts(body, &mut scope, out);
            }
            Stmt::Match { scrutinee, arms } => {
                walk_expr(scrutinee, locals, out);
                for arm in arms {
                    let mut scope = locals.clone();
                    crate::core_bindings::reserve_pattern(&arm.pattern, &mut scope);
                    if let Some(guard) = &arm.guard {
                        walk_expr(guard, &scope, out);
                    }
                    walk_stmts(&arm.body, &mut scope, out);
                }
            }
            Stmt::Throw(e) => walk_expr(e, locals, out),
            Stmt::Try { body, catches } => {
                walk_stmts(body, &mut locals.clone(), out);
                for c in catches {
                    let mut scope = locals.clone();
                    crate::core_bindings::reserve_pattern(&c.pattern, &mut scope);
                    walk_stmts(&c.body, &mut scope, out);
                }
            }
            _ => {}
        }
    }
}

fn walk_expr(expr: &Expr, locals: &HashSet<String>, out: &mut Vec<String>) {
    if let Expr::Block { body, result } = expr {
        let mut scope = locals.clone();
        walk_stmts(body, &mut scope, out);
        if let Some(result) = result {
            walk_expr(result, &scope, out);
        }
        return;
    }
    if let Expr::Closure { params, body, .. } = expr {
        let mut scope = locals.clone();
        scope.extend(params.iter().map(|(name, _)| name.clone()));
        walk_stmts(body, &mut scope, out);
        return;
    }

    if let Expr::Call { callee, .. } = expr {
        if let Expr::Ident(target) = callee.as_ref() {
            if !locals.contains(target) && !target.starts_with("__inrt_") {
                out.push(target.clone());
            }
        }
    }
    crate::core_ir::for_each_expr_child(expr, &mut |child| walk_expr(child, locals, out));
}

/// Main entry: resolve all external deps by loading crate sources.
pub fn resolve_deps(module: &UnifiedModule, crate_db: &CrateDb) -> ResolveResult {
    let mut expanded = module.clone();
    let mut files_parsed = 0;
    let mut functions_added = 0;
    let mut seen: HashSet<String> = HashSet::new();
    // Seed with already-known functions
    for decl in &expanded.decls {
        if let Decl::Function { name, .. } = decl {
            seen.insert(name.clone());
        }
    }

    loop {
        let externals = collect_externals(&expanded);
        let new: Vec<&String> = externals
            .iter()
            .filter(|n| !seen.contains(n.as_str()))
            .collect();
        if new.is_empty() {
            break;
        }
        for target in &new {
            seen.insert((*target).clone());
        }

        let mut resolved_modules = Vec::new();
        for target in &new {
            if let Ok((_cname, _mpath, dep_mod)) = crate_db.resolve(target) {
                resolved_modules.push(dep_mod);
            }
        }

        let mut processed_modules = std::collections::HashSet::new();
        for dep_mod in resolved_modules {
            let mod_ptr = std::sync::Arc::as_ptr(&dep_mod) as usize;
            if processed_modules.insert(mod_ptr) {
                for decl in &dep_mod.decls {
                    if let Decl::Function { name, .. } = decl {
                        if !seen.contains(name) {
                            expanded.decls.push(decl.clone());
                            seen.insert(name.clone());
                            functions_added += 1;
                        }
                    }
                }
                files_parsed += 1;
            }
        }
    }

    ResolveResult {
        module: expanded,
        files_parsed,
        functions_added,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{Expr, Stmt, Typ};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn expression_block_dependencies_respect_sequential_local_scope() {
        fn call(name: &str) -> Expr {
            Expr::Call {
                callee: Box::new(Expr::Ident(name.into())),
                args: vec![],
            }
        }
        let expr = Expr::Block {
            body: vec![
                Stmt::Expr(call("before")),
                Stmt::Let("local".into(), None, call("initializer")),
                Stmt::Expr(call("local")),
            ],
            result: Some(Box::new(call("result"))),
        };
        let mut locals = HashSet::new();
        let mut out = vec![];
        walk_stmts(
            &[Stmt::Expr(expr), Stmt::Expr(call("local"))],
            &mut locals,
            &mut out,
        );
        assert_eq!(out, vec!["before", "initializer", "result", "local"]);
        assert!(!locals.contains("local"));
    }

    #[test]
    fn test_resolve_deps_complex() {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let temp_dir =
            std::env::temp_dir().join(format!("test_resolve_deps_{}_{}", std::process::id(), id));
        let src_dir = temp_dir.join("dummy_crate/src/dummy_crate");
        fs::create_dir_all(&src_dir).unwrap();
        let mod_rs = src_dir.join("foo.rs");
        fs::write(&mod_rs, "pub fn bar() {}").unwrap();

        let mut db = CrateDb::new();
        db.search_roots.push(temp_dir.clone());
        db.register_crate("dummy_crate", temp_dir.join("dummy_crate"));

        let module = UnifiedModule::new(vec![Decl::Function {
            name: "main".to_string(),
            params: vec![],
            ret: Typ::Void,
            body: vec![
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("dummy_crate::foo::bar".to_string())),
                    args: vec![],
                }),
                Stmt::If {
                    cond: Expr::BoolLit(true),
                    then_body: vec![Stmt::Let(
                        "x".to_string(),
                        None,
                        Expr::Call {
                            callee: Box::new(Expr::Ident("dummy_crate::foo::baz".to_string())),
                            args: vec![],
                        },
                    )],
                    else_body: vec![],
                },
            ],
            type_params: vec![],
        }]);

        let res = resolve_deps(&module, &db);
        assert!(res.files_parsed > 0);
        assert!(res.functions_added > 0);

        let mut found_bar = false;
        for decl in &res.module.decls {
            if let Decl::Function { name, .. } = decl {
                if name == "bar" || name == "dummy_crate::foo::bar" {
                    found_bar = true;
                }
            }
        }
        assert!(found_bar, "bar should be added to the expanded module");
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_resolve_deps_loop_and_match() {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let temp_dir =
            std::env::temp_dir().join(format!("test_resolve_deps_2_{}_{}", std::process::id(), id));
        let src_dir = temp_dir.join("dummy_crate/src/dummy_crate");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(&src_dir.join("func1.rs"), "pub fn func1() {}").unwrap();
        fs::write(&src_dir.join("func2.rs"), "pub fn func2() {}").unwrap();
        fs::write(&src_dir.join("func3.rs"), "pub fn func3() {}").unwrap();

        let mut db = CrateDb::new();
        db.search_roots.push(temp_dir.clone());
        db.register_crate("dummy_crate", temp_dir.join("dummy_crate"));

        let module = UnifiedModule::new(vec![Decl::Function {
            name: "main".to_string(),
            params: vec![],
            ret: Typ::Void,
            body: vec![
                Stmt::Loop {
                    kind: crate::core_ir::LoopKind::While,
                    cond: Some(Expr::Call {
                        callee: Box::new(Expr::Ident("dummy_crate::func1::func1".to_string())),
                        args: vec![],
                    }),
                    body: vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("dummy_crate::func2::func2".to_string())),
                        args: vec![],
                    })],
                },
                Stmt::Match {
                    scrutinee: Expr::Ident("x".to_string()),
                    arms: vec![crate::core_ir::MatchArm {
                        guard: None,
                        pattern: "1".to_string(),
                        body: vec![Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("dummy_crate::func3::func3".to_string())),
                            args: vec![],
                        })],
                    }],
                },
            ],
            type_params: vec![],
        }]);

        let res = resolve_deps(&module, &db);
        assert_eq!(res.functions_added, 3);
        assert_eq!(res.files_parsed, 3);
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
