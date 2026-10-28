//! Lazy dependency resolver — replaces stub mechanism.
//!
//! When a module references an external function, resolver finds its
//! crate, loads and parses its source, registers the definitions, and
//! recurses until all symbols resolve.

use std::collections::{HashMap, HashSet};

use crate::boundary_ir::{UnaryMethodBindings, merge_unary_method_bindings};
use crate::core_ir::{Decl, Expr, MatchPattern, Stmt, Typ, UnifiedModule};
use crate::crate_db::CrateDb;

/// Result of resolving dependencies for a module.
pub struct ResolveResult {
    /// Expanded module with all resolved dependencies merged in.
    pub module: UnifiedModule,
    pub unary_methods: UnaryMethodBindings,
    pub files_parsed: usize,
    pub functions_added: usize,
}

/// Collect unbound function names used as callees or address values.
fn collect_externals(module: &UnifiedModule) -> Vec<String> {
    let locals: HashSet<_> = module
        .decls
        .iter()
        .map(|decl| declaration_name(decl).to_string())
        .collect();
    let mut out = Vec::new();
    for decl in &module.decls {
        walk_decl(decl, &locals, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// Avoid initializing a source database when every reference is local or compiler-owned.
pub(crate) fn requires_source_resolution(module: &UnifiedModule) -> bool {
    collect_externals(module).iter().any(|name| {
        name.contains("::")
            && !matches!(
                name.as_str(),
                "Vec::new"
                    | "std::vec::Vec::new"
                    | "alloc::vec::Vec::new"
                    | "Vec::with_capacity"
                    | "std::vec::Vec::with_capacity"
                    | "alloc::vec::Vec::with_capacity"
                    | "Vec::<_>::new"
                    | "std::vec::Vec::<_>::new"
                    | "alloc::vec::Vec::<_>::new"
                    | "Vec::<_>::with_capacity"
                    | "std::vec::Vec::<_>::with_capacity"
                    | "alloc::vec::Vec::<_>::with_capacity"
            )
    })
}

fn walk_type(typ: &Typ, locals: &HashSet<String>, out: &mut Vec<String>) {
    match typ.canonical() {
        Typ::Array(item) | Typ::Vector(item) => walk_type(&item, locals, out),
        Typ::Named(name) => {
            if let Some(fields) = crate::core_ir::tuples::fields(&name) {
                for (_, typ) in fields {
                    walk_type(&typ, locals, out);
                }
            } else if !locals.contains(&name) {
                out.push(name);
            }
        }
        _ => {}
    }
}

fn walk_pattern(pattern: &str, locals: &HashSet<String>, out: &mut Vec<String>) {
    fn visit(pattern: &MatchPattern, locals: &HashSet<String>, out: &mut Vec<String>) {
        match pattern {
            MatchPattern::StructPat { name, fields } => {
                walk_type(&Typ::Named(name.clone()), locals, out);
                for (_, child) in fields {
                    visit(child, locals, out);
                }
            }
            MatchPattern::TuplePat(items) | MatchPattern::ArrayPat(items) => {
                for child in items {
                    visit(child, locals, out);
                }
            }
            _ => {}
        }
    }
    if let Ok(pattern) = MatchPattern::parse(pattern) {
        visit(&pattern, locals, out);
    }
}

fn walk_decl(decl: &Decl, locals: &HashSet<String>, out: &mut Vec<String>) {
    match decl {
        Decl::Function {
            body,
            params,
            ret,
            type_params,
            ..
        } => {
            let mut scope = locals.clone();
            scope.extend(type_params.iter().cloned());
            for (_, typ) in params {
                walk_type(typ, &scope, out);
            }
            walk_type(ret, &scope, out);
            scope.extend(params.iter().map(|(name, _)| name.clone()));
            walk_stmts(body, &mut scope, out);
        }
        Decl::Global { typ, init, .. } => {
            walk_type(typ, locals, out);
            if let Some(value) = init {
                walk_expr(value, locals, out);
            }
        }
        Decl::Struct {
            fields,
            type_params,
            ..
        }
        | Decl::Class {
            fields,
            type_params,
            ..
        } => {
            let mut scope = locals.clone();
            scope.extend(type_params.iter().cloned());
            for (_, typ) in fields {
                walk_type(typ, &scope, out);
            }
            if let Decl::Class {
                methods,
                extends,
                implements,
                ..
            } = decl
            {
                for name in extends.iter().chain(implements) {
                    walk_type(&Typ::Named(name.clone()), &scope, out);
                }
                for method in methods {
                    walk_decl(method, &scope, out);
                }
            }
        }
        Decl::Interface {
            methods,
            type_params,
            ..
        } => {
            let mut scope = locals.clone();
            scope.extend(type_params.iter().cloned());
            for method in methods {
                for (_, typ) in &method.params {
                    walk_type(typ, &scope, out);
                }
                walk_type(&method.ret, &scope, out);
            }
        }
        Decl::Component { .. } => {}
    }
}

fn walk_stmts(stmts: &[Stmt], locals: &mut HashSet<String>, out: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Expr(e) | Stmt::Return(Some(e)) => walk_expr(e, locals, out),
            Stmt::Let(name, typ, e) => {
                if let Some(typ) = typ {
                    walk_type(typ, locals, out);
                }
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
                    walk_pattern(&arm.pattern, locals, out);
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
                    walk_pattern(&c.pattern, locals, out);
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
    if let Expr::MatchValue { scrutinee, arms } = expr {
        walk_expr(scrutinee, locals, out);
        for arm in arms {
            walk_pattern(&arm.pattern, locals, out);
            let mut scope = locals.clone();
            crate::core_bindings::reserve_pattern(&arm.pattern, &mut scope);
            if let Some(guard) = &arm.guard {
                walk_expr(guard, &scope, out);
            }
            walk_expr(&arm.value, &scope, out);
        }
        return;
    }
    if let Expr::Block { body, result } = expr {
        let mut scope = locals.clone();
        walk_stmts(body, &mut scope, out);
        if let Some(result) = result {
            walk_expr(result, &scope, out);
        }
        return;
    }
    if let Expr::Closure {
        params, ret, body, ..
    } = expr
    {
        for (_, typ) in params {
            walk_type(typ, locals, out);
        }
        walk_type(ret, locals, out);
        let mut scope = locals.clone();
        scope.extend(params.iter().map(|(name, _)| name.clone()));
        walk_stmts(body, &mut scope, out);
        return;
    }

    if let Expr::StructInit { name, .. } = expr {
        walk_type(&Typ::Named(name.clone()), locals, out);
    }
    if let Expr::Ident(target) = expr
        && !locals.contains(target)
        && !target.starts_with("__inrt_")
        && !crate::core_ir::is_compiler_callee(target)
    {
        out.push(target.clone());
    }
    crate::core_ir::for_each_expr_child(expr, &mut |child| walk_expr(child, locals, out));
}

pub(crate) fn declaration_name(decl: &Decl) -> &str {
    match decl {
        Decl::Function { name, .. }
        | Decl::Struct { name, .. }
        | Decl::Global { name, .. }
        | Decl::Class { name, .. }
        | Decl::Interface { name, .. }
        | Decl::Component { name, .. } => name,
    }
}

/// Main entry: resolve all external deps by loading crate sources.
pub fn resolve_deps(module: &UnifiedModule, crate_db: &CrateDb) -> Result<ResolveResult, String> {
    resolve_deps_for_entry(module, crate_db, None)
}

/// Discover dependencies only through the selected entry, or all declarations for libraries.
pub fn resolve_deps_for_entry(
    module: &UnifiedModule,
    crate_db: &CrateDb,
    entry: Option<&str>,
) -> Result<ResolveResult, String> {
    let mut resolved =
        resolve_deps_for_entry_with_bindings(module, &UnaryMethodBindings::new(), crate_db, entry)?;
    crate::core_ir::rust_references::erase_declarations(&mut resolved.module.decls);
    resolved.unary_methods.clear();
    Ok(resolved)
}

pub fn resolve_deps_for_entry_with_bindings(
    module: &UnifiedModule,
    bindings: &UnaryMethodBindings,
    crate_db: &CrateDb,
    entry: Option<&str>,
) -> Result<ResolveResult, String> {
    let mut unary_methods = bindings.clone();
    let mut expanded = module.clone();
    let mut files_parsed = 0;
    let mut functions_added = 0;
    let mut declarations = HashMap::new();
    let mut attempted = HashSet::new();
    let mut loaded_modules = HashSet::new();
    // Index existing canonical declarations before merging dependencies.
    for (index, decl) in expanded.decls.iter().enumerate() {
        declarations
            .entry(declaration_name(decl).to_string())
            .or_insert(index);
    }

    let mut pending: std::collections::VecDeque<String> = if let Some(entry) = entry {
        std::iter::once(entry.to_string()).collect()
    } else {
        expanded
            .decls
            .iter()
            .map(|decl| declaration_name(decl).to_string())
            .collect()
    };
    let mut visited = HashSet::new();
    while let Some(target) = pending.pop_front() {
        if let Some(index) = declarations.get(&target).copied() {
            if visited.insert(target) {
                let mut references = Vec::new();
                // Module bindings remain visible as dependency edges; only lexical locals shadow them.
                walk_decl(&expanded.decls[index], &HashSet::new(), &mut references);
                references.sort();
                references.dedup();
                pending.extend(references);
            }
            continue;
        }
        if !attempted.insert(target.clone()) {
            continue;
        }
        let (crate_name, module_path, dep_mod) = match crate_db.resolve_artifact_detailed(&target) {
            Ok(artifact) => artifact,
            Err(crate::crate_db::ArtifactResolveError::Unavailable(_)) => continue,
            Err(crate::crate_db::ArtifactResolveError::Invalid(error)) => return Err(error),
        };
        if !dep_mod
            .semantic
            .decls
            .iter()
            .any(|decl| declaration_name(decl) == target)
        {
            continue;
        }
        if !loaded_modules.insert((crate_name, module_path)) {
            continue;
        }
        merge_unary_method_bindings(&mut unary_methods, dep_mod.unary_methods.clone())?;
        for decl in &dep_mod.semantic.decls {
            let name = declaration_name(decl);
            if let Some(index) = declarations.get(name) {
                if &expanded.decls[*index] != decl {
                    return Err(format!("dependency declaration conflict for `{name}`"));
                }
                continue;
            }
            declarations.insert(name.to_string(), expanded.decls.len());
            expanded.decls.push(decl.clone());
            if entry.is_none() || attempted.contains(name) {
                // Revisit an earlier missing edge when its declaration arrives as support.
                pending.push_back(name.to_string());
            }
            if matches!(decl, Decl::Function { .. }) {
                functions_added += 1;
            }
        }
        files_parsed += 1;
    }

    if entry.is_some() {
        // Loading a Rust file also discovers constants unrelated to the entry.
        // They are not roots: do not validate or emit their unevaluated values.
        // Keep caller declarations, reached constants, runtime initializers and
        // private source-validation globals (which are not constant values).
        // Retained method bodies can become reachable after operator dispatch.
        let mut references = Vec::new();
        for decl in &expanded.decls {
            walk_decl(decl, &HashSet::new(), &mut references);
        }
        let references: HashSet<_> = references.into_iter().collect();
        let mut index = 0;
        expanded.decls.retain(|decl| {
            let caller = index < module.decls.len();
            index += 1;
            caller
                || visited.contains(declaration_name(decl))
                || references.contains(declaration_name(decl))
                || !matches!(decl, Decl::Global {
                    mutable: false, constant_value: true, init, ..
                } if init.as_deref().is_none_or(|expr| !crate::core_opt::expr_has_effects(expr)))
        });
    }

    Ok(ResolveResult {
        module: expanded,
        unary_methods,
        files_parsed,
        functions_added,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{Expr, Stmt, Typ};
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn entry_discovery_follows_globals_addresses_schemas_and_guards_only_when_reached() {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("resolve_entry_{}_{}", std::process::id(), id));
        fs::create_dir_all(root.join("src")).unwrap();
        for (name, source) in [
            (
                "callback",
                "pub const UNUSED:i64=unknown::VALUE;pub const VALUE:i64=37;pub const EFFECT:i64=unknown::effect();pub static STATE:i64=0;const SUPPORT:i64=11;pub fn unselected()->i64{SUPPORT}pub fn answer()->i64{VALUE+5}",
            ),
            ("schema", "pub struct Entry{pub value:i64}"),
            ("guard", "pub fn allowed()->bool{true}"),
            ("unrelated", "pub fn answer()->i64{0}"),
        ] {
            fs::write(root.join("src").join(format!("{name}.rs")), source).unwrap();
        }
        let mut db = CrateDb::new();
        db.search_roots.clear();
        db.register_crate("sample", root.clone());
        let call = |name: &str| Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args: vec![],
        };
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![
                    Stmt::Let(
                        "entry".into(),
                        Some(Typ::Named("Local".into())),
                        Expr::StructInit {
                            name: "Local".into(),
                            fields: vec![],
                        },
                    ),
                    Stmt::Match {
                        scrutinee: Expr::BoolLit(true),
                        arms: vec![crate::core_ir::MatchArm {
                            pattern: "_".into(),
                            guard: Some(call("sample::guard::allowed")),
                            body: vec![Stmt::Return(Some(Expr::Ident("CALLBACK".into())))],
                        }],
                    },
                ],
            },
            Decl::Global {
                name: "CALLBACK".into(),
                typ: Typ::Int,
                mutable: false,
                constant_value: false,
                init: Some(Box::new(Expr::Ident("sample::callback::answer".into()))),
            },
            Decl::Struct {
                name: "Local".into(),
                fields: vec![("nested".into(), Typ::Named("sample::schema::Entry".into()))],
                type_params: vec![],
            },
            Decl::Function {
                name: "unused".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![Stmt::Return(Some(call("sample::unrelated::answer")))],
            },
        ]);
        let reached = resolve_deps_for_entry(&module, &db, Some("main")).unwrap();
        assert_eq!(reached.files_parsed, 3);
        for name in [
            "sample::callback::answer",
            "sample::callback::VALUE",
            "sample::callback::SUPPORT",
            "sample::callback::EFFECT",
            "sample::callback::STATE",
            "sample::schema::Entry",
            "sample::guard::allowed",
        ] {
            assert!(
                reached
                    .module
                    .decls
                    .iter()
                    .any(|decl| declaration_name(decl) == name),
                "{name}"
            );
        }
        assert!(
            !reached
                .module
                .decls
                .iter()
                .any(|decl| declaration_name(decl) == "sample::unrelated::answer")
        );
        assert!(
            !reached
                .module
                .decls
                .iter()
                .any(|decl| declaration_name(decl) == "sample::callback::UNUSED")
        );
        let all = resolve_deps(&module, &db).unwrap();
        assert!(
            all.module
                .decls
                .iter()
                .any(|decl| declaration_name(decl) == "sample::callback::UNUSED")
        );
        assert_eq!(resolve_deps(&module, &db).unwrap().files_parsed, 4);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_resolution_guard_skips_local_and_compiler_owned_names() {
        for (source, required) in [
            (
                "fn main()->i64{let mut values=Vec::<_>::new();values.push(42);values[0]}",
                false,
            ),
            ("fn main()->i64{42}", false),
            (
                "fn main()->i64{let mut values=Vec::new();values.push(42);values[0]}",
                false,
            ),
            (
                "fn main()->i64{let mut values=std::vec::Vec::with_capacity(2);values.push(42);values[0]}",
                false,
            ),
            (
                "mod local{pub fn answer()->i64{42}} fn main()->i64{local::answer()}",
                false,
            ),
            ("fn main()->i64{dependency::source::answer()}", true),
            (
                "fn main()->i64{let value=dependency::source::Entry{answer:42};value.answer}",
                true,
            ),
        ] {
            let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            assert_eq!(
                requires_source_resolution(&module),
                required,
                "{source}: {module:?}"
            );
        }
    }

    #[test]
    fn real_modules_keep_helpers_globals_schemas_addresses_and_transitive_calls_distinct() {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("resolve_modules_{}_{}", std::process::id(), id));
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        for (name, value) in [("left", 20), ("right", 22)] {
            fs::write(src.join(format!("{name}.rs")), format!("const VALUE:i64={value}; pub struct Entry{{pub value:i64}} fn helper()->i64{{VALUE}} pub fn answer()->i64{{helper()}} pub fn produce()->Entry{{Entry{{value:VALUE}}}} pub fn callback()->usize{{helper as usize}}")).unwrap();
        }
        fs::write(
            src.join("alpha.rs"),
            "pub fn answer()->i64{crate::shared::helper()}",
        )
        .unwrap();
        fs::write(src.join("shared.rs"), "pub fn helper()->i64{42}").unwrap();
        let mut db = CrateDb::new();
        db.search_roots.clear();
        db.register_crate("sample", root.clone());
        for source in [
            "fn main()->i64{sample::left::answer()+sample::right::answer()}",
            "fn main()->i64{sample::left::produce().value+sample::right::produce().value}",
            "fn main()->i64{invoke(sample::left::callback())+invoke(sample::right::callback())}",
            "fn main()->i64{let left=sample::left::Entry{value:20};let right=sample::right::Entry{value:22};left.value+right.value}",
            "fn main()->i64{sample::alpha::answer()}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let result = resolve_deps(&parsed, &db).unwrap();
            let normalized = crate::typecheck::normalize_module(
                crate::parser_registry::ParserId::Rust,
                &result.module,
            );
            let checked = crate::typecheck::TypeChecker::new().check_module(&normalized);
            let verified = crate::core_ir_verifier::verify_for_entry(&normalized, "main");
            assert!(checked.is_ok(), "{source}: {checked:?}; {normalized:?}");
            assert!(verified.ok, "{source}: {verified:?}; {normalized:?}");
        }
        let (_, _, loaded) = db.resolve("sample::left::answer").unwrap();
        let helper = loaded
            .decls
            .iter()
            .find(|decl| declaration_name(decl) == "sample::left::helper")
            .unwrap()
            .clone();
        let mut module = crate::compiler::rust_front::parse_rust_source(
            "fn main()->i64{sample::left::answer()}",
        )
        .unwrap();
        module.decls.push(helper.clone());
        assert_eq!(
            resolve_deps(&module, &db)
                .unwrap()
                .module
                .decls
                .iter()
                .filter(|decl| declaration_name(decl) == "sample::left::helper")
                .count(),
            1
        );
        let Decl::Function { body, .. } = module.decls.last_mut().unwrap() else {
            unreachable!()
        };
        *body = vec![Stmt::Return(Some(Expr::IntLit(0)))];
        assert!(
            resolve_deps(&module, &db)
                .err()
                .unwrap()
                .contains("dependency declaration conflict for `sample::left::helper`")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn function_addresses_resolve_without_confusing_attempted_and_loaded_symbols() {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("resolve_addresses_{}_{}", std::process::id(), id));
        let dir = root.join("dummy_crate/src");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("foo.rs"),
            "pub mod dummy_crate { pub mod foo { pub fn bar()->i64 {42} } }",
        )
        .unwrap();
        let mut db = CrateDb::new();
        db.search_roots = vec![root.clone()];
        db.register_crate("dummy_crate", root.join("dummy_crate"));
        let target = "dummy_crate::foo::bar";
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![
                Stmt::Let("pointer".into(), None, Expr::Ident(target.into())),
                Stmt::Return(Some(Expr::IntLit(42))),
            ],
        }]);
        let resolved = resolve_deps(&module, &db).unwrap();
        assert_eq!(resolved.functions_added, 1);
        assert!(
            resolved
                .module
                .decls
                .iter()
                .any(|decl| matches!(decl, Decl::Function { name, .. } if name == target))
        );
        let mut missing = module.clone();
        let Decl::Function { body, .. } = &mut missing.decls[0] else {
            unreachable!()
        };
        body[0] = Stmt::Expr(Expr::Ident("dummy_crate::foo::NOT_A_FUNCTION".into()));
        assert_eq!(resolve_deps(&missing, &db).unwrap().functions_added, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn address_discovery_respects_globals_parameters_and_block_local_scope() {
        let module = UnifiedModule::new(vec![
            Decl::Global {
                name: "global".into(),
                typ: Typ::Int,
                mutable: false,
                constant_value: false,
                init: Some(Box::new(Expr::Ident("dep::global_target".into()))),
            },
            Decl::Function {
                name: "main".into(),
                params: vec![("parameter".into(), Typ::Int)],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![
                    Stmt::Expr(Expr::Ident("global".into())),
                    Stmt::Expr(Expr::Ident("parameter".into())),
                    Stmt::Expr(Expr::Block {
                        body: vec![Stmt::Let(
                            "inside".into(),
                            None,
                            Expr::Ident("dep::target".into()),
                        )],
                        result: Some(Box::new(Expr::Ident("inside".into()))),
                    }),
                    Stmt::Expr(Expr::Ident("inside".into())),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            },
        ]);
        assert_eq!(
            collect_externals(&module),
            vec!["dep::global_target", "dep::target", "inside"]
        );
    }

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
        let src_dir = temp_dir.join("dummy_crate/src");
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

        let res = resolve_deps(&module, &db).unwrap();
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
        let src_dir = temp_dir.join("dummy_crate/src");
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

        let res = resolve_deps(&module, &db).unwrap();
        assert_eq!(res.functions_added, 3);
        assert_eq!(res.files_parsed, 3);
        let _ = fs::remove_dir_all(&temp_dir);
    }
}

#[cfg(test)]
mod retained_binding_tests {
    use super::*;
    #[test]
    fn missing_dependency_is_optional_but_loaded_invalid_source_is_not() {
        let root = std::env::temp_dir().join(format!("retained_dep_error_{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "this is not Rust").unwrap();
        let mut db = CrateDb::new();
        db.search_roots.clear();
        db.register_crate("broken", root.clone());
        let module = |target: &str| {
            UnifiedModule::new(vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident(target.into())),
                    args: vec![],
                }))],
            }])
        };
        assert!(resolve_deps_for_entry(&module("missing::answer"), &db, Some("main")).is_ok());
        assert!(resolve_deps_for_entry(&module("broken::answer"), &db, Some("main")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entry_resolution_preserves_metadata_even_without_added_declarations() {
        let mut db = CrateDb::new();
        db.search_roots.clear();
        let module = UnifiedModule::new(vec![]);
        let bindings = [(("&Row".into(), "-".into()), Err("unresolved".into()))]
            .into_iter()
            .collect();
        let result = resolve_deps_for_entry_with_bindings(&module, &bindings, &db, None).unwrap();
        assert_eq!(result.module, module);
        assert_eq!(result.unary_methods, bindings);
        assert_eq!(result.functions_added, 0);
        assert_eq!(result.files_parsed, 0);
    }
}
