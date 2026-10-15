use super::*;

/// Qualify a dependency before merging its declarations into another module.
pub(crate) fn qualify_module_decls(decls: &mut [Decl], module: &str) {
    let module = module.trim_matches(':');
    if module.is_empty() {
        return;
    }
    let root = module.split("::").next().unwrap();
    let mut values = HashSet::new();
    let mut types = HashSet::new();
    for decl in decls.iter() {
        match decl {
            Decl::Function { name, .. } | Decl::Global { name, .. } => {
                values.insert(qualified(name, module, root));
            }
            Decl::Struct { name, .. } | Decl::Class { name, .. } | Decl::Interface { name, .. } => {
                types.insert(qualified(name, module, root));
            }
            _ => {}
        }
    }
    for decl in decls {
        let associated = matches!(decl, Decl::Function { .. } | Decl::Global { .. });
        let mut scope = match decl {
            Decl::Function { name, .. }
            | Decl::Global { name, .. }
            | Decl::Struct { name, .. }
            | Decl::Class { name, .. }
            | Decl::Interface { name, .. } => {
                *name = qualified(name, module, root);
                name.rsplit_once("::")
                    .map(|(scope, _)| scope.to_string())
                    .unwrap_or_else(|| module.into())
            }
            _ => continue,
        };
        // Flattened inherent and trait method symbols retain their owner's module.
        if associated {
            let mut owner = Some(scope.as_str());
            let mut lexical = None;
            while let Some(candidate) = owner {
                if types.contains(candidate) {
                    lexical = candidate
                        .rsplit_once("::")
                        .map(|(module, _)| module.to_string());
                    break;
                }
                owner = candidate.rsplit_once("::").map(|(parent, _)| parent);
            }
            if let Some(module) = lexical {
                scope = module;
            }
        }
        let mut candidates = HashSet::new();
        match decl {
            Decl::Function { body, .. } => super::reserve_body(body, &mut candidates),
            Decl::Global {
                init: Some(value), ..
            } => super::reserve_expr(value, &mut candidates),
            Decl::Class { methods, .. } => {
                for method in methods {
                    if let Decl::Function { body, .. } = method {
                        super::reserve_body(body, &mut candidates);
                    }
                }
            }
            _ => {}
        }
        let references = references(&scope, root, &values, candidates);
        let mut reserved = values.clone();
        reserved.extend(references.keys().cloned());
        match decl {
            Decl::Function { params, body, .. } => {
                super::normalize_function_with_references(params, body, &reserved, &references)
            }
            Decl::Global {
                init: Some(value), ..
            } => super::normalize_initializer_with_references(value, &reserved, &references),
            Decl::Class { methods, .. } => {
                for method in methods {
                    if let Decl::Function { params, body, .. } = method {
                        super::normalize_function_with_references(
                            params,
                            body,
                            &reserved,
                            &references,
                        );
                    }
                }
            }
            _ => {}
        }
        let type_name = |name: &str| resolve(name, &scope, root, &types);
        let names = NominalNames {
            type_name: &type_name,
            value_name: &|_| None,
        };
        declaration(decl, &names, &HashSet::new());
    }
}

fn qualified(name: &str, module: &str, root: &str) -> String {
    if crate::core_ir::tuples::fields(name).is_some() || name.starts_with(&format!("{root}::")) {
        name.into()
    } else {
        format!("{module}::{name}")
    }
}
fn resolve(name: &str, scope: &str, root: &str, known: &HashSet<String>) -> Option<String> {
    if known.contains(name) {
        return Some(name.into());
    }
    if let Some(tail) = name.strip_prefix("crate::") {
        let candidate = format!("{root}::{tail}");
        return Some(candidate);
    }
    if let Some(tail) = name.strip_prefix("self::") {
        let candidate = format!("{scope}::{tail}");
        return Some(candidate);
    }
    if name.starts_with("super::") {
        let mut parent = scope;
        let mut tail = name;
        while let Some(rest) = tail.strip_prefix("super::") {
            parent = parent.rsplit_once("::")?.0;
            tail = rest;
        }
        let candidate = format!("{parent}::{tail}");
        return Some(candidate);
    }
    let candidate = format!("{scope}::{name}");
    if known.contains(&candidate) {
        return Some(candidate);
    }
    // Frontends may already prefix nested names relative to the crate root.
    if name.contains("::") {
        let candidate = format!("{root}::{name}");
        return known.contains(&candidate).then_some(candidate);
    }
    None
}

fn references(
    scope: &str,
    root: &str,
    known: &HashSet<String>,
    candidates: HashSet<String>,
) -> HashMap<String, String> {
    candidates
        .into_iter()
        .filter_map(|name| resolve(&name, scope, root, known).map(|target| (name, target)))
        .collect()
}
struct NominalNames<'a> {
    type_name: &'a dyn Fn(&str) -> Option<String>,
    value_name: &'a dyn Fn(&str) -> Option<String>,
}

/// Rewrite contextual nominal names without changing declarations or member names.
/// Value resolution is for nominal paths such as `Self::new`, not local bindings.
pub(crate) fn rewrite_nominal_names(
    decls: &mut [Decl],
    resolve_type: &impl Fn(&str) -> Option<String>,
    resolve_value: &impl Fn(&str) -> Option<String>,
) {
    let names = NominalNames {
        type_name: resolve_type,
        value_name: resolve_value,
    };
    for decl in decls {
        declaration(decl, &names, &HashSet::new());
    }
}

fn typ(value: &mut Typ, names: &NominalNames<'_>, generics: &HashSet<String>) {
    match value {
        Typ::Named(name) if !generics.contains(name) => {
            if let Some(fields) = crate::core_ir::tuples::fields(name) {
                let mut items: Vec<_> = fields.into_iter().map(|(_, typ)| typ).collect();
                for item in &mut items {
                    typ(item, names, generics);
                }
                *value = crate::core_ir::tuples::typ(&items);
                return;
            }
            if let Some(target) = (names.type_name)(name) {
                *name = target;
            }
        }
        Typ::Array(item) | Typ::Vector(item) => typ(item, names, generics),
        _ => {}
    }
}
fn pattern(source: &mut String, names: &NominalNames<'_>, generics: &HashSet<String>) {
    fn walk(value: &mut MatchPattern, names: &NominalNames<'_>, generics: &HashSet<String>) {
        match value {
            MatchPattern::StructPat { name, fields } => {
                let mut value = Typ::Named(name.clone());
                typ(&mut value, names, generics);
                if let Typ::Named(target) = value {
                    *name = target;
                }
                for (_, value) in fields {
                    walk(value, names, generics);
                }
            }
            MatchPattern::TuplePat(items) | MatchPattern::ArrayPat(items) => {
                for item in items {
                    walk(item, names, generics);
                }
            }
            _ => {}
        }
    }
    if let Ok(mut value) = MatchPattern::parse(source) {
        walk(&mut value, names, generics);
        *source = super::render_pattern(&value);
    }
}
fn expr(value: &mut Expr, names: &NominalNames<'_>, generics: &HashSet<String>) {
    match value {
        Expr::Ident(name) => {
            if let Some(target) = (names.value_name)(name) {
                *name = target;
            }
        }
        Expr::StructInit { name, .. } => {
            let mut value = Typ::Named(name.clone());
            typ(&mut value, names, generics);
            if let Typ::Named(target) = value {
                *name = target;
            }
        }
        Expr::Closure {
            params, ret, body, ..
        } => {
            for (_, value) in params {
                typ(value, names, generics);
            }
            typ(ret, names, generics);
            statements(body, names, generics);
            return;
        }
        Expr::Block { body, .. } => statements(body, names, generics),
        Expr::MatchValue { arms, .. } => {
            for arm in arms {
                pattern(&mut arm.pattern, names, generics);
            }
        }
        _ => {}
    }
    crate::core_ir::for_each_expr_child_mut(value, &mut |value| expr(value, names, generics));
}
fn statements(body: &mut [Stmt], names: &NominalNames<'_>, generics: &HashSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::Let(_, annotation, value) => {
                if let Some(annotation) = annotation {
                    typ(annotation, names, generics);
                }
                expr(value, names, generics);
            }
            Stmt::Assign(_, value)
            | Stmt::Expr(value)
            | Stmt::Return(Some(value))
            | Stmt::Throw(value) => expr(value, names, generics),
            Stmt::FieldAssign { base, value, .. } => {
                expr(base, names, generics);
                expr(value, names, generics);
            }
            Stmt::IndexAssign { base, index, value } => {
                for value in [base, index, value] {
                    expr(value, names, generics);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                expr(cond, names, generics);
                statements(then_body, names, generics);
                statements(else_body, names, generics);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    expr(cond, names, generics);
                }
                statements(body, names, generics);
            }
            Stmt::Match { scrutinee, arms } => {
                expr(scrutinee, names, generics);
                for arm in arms {
                    pattern(&mut arm.pattern, names, generics);
                    if let Some(guard) = &mut arm.guard {
                        expr(guard, names, generics);
                    }
                    statements(&mut arm.body, names, generics);
                }
            }
            Stmt::Try { body, catches } => {
                statements(body, names, generics);
                for catch in catches {
                    pattern(&mut catch.pattern, names, generics);
                    statements(&mut catch.body, names, generics);
                }
            }
            _ => {}
        }
    }
}
fn declaration(decl: &mut Decl, names: &NominalNames<'_>, outer: &HashSet<String>) {
    let mut generics = outer.clone();
    match decl {
        Decl::Function { type_params, .. }
        | Decl::Struct { type_params, .. }
        | Decl::Class { type_params, .. }
        | Decl::Interface { type_params, .. } => generics.extend(type_params.iter().cloned()),
        _ => {}
    }
    match decl {
        Decl::Function {
            params, ret, body, ..
        } => {
            for (_, value) in params {
                typ(value, names, &generics);
            }
            typ(ret, names, &generics);
            statements(body, names, &generics);
        }
        Decl::Global {
            typ: annotation,
            init,
            ..
        } => {
            typ(annotation, names, &generics);
            if let Some(value) = init {
                expr(value, names, &generics);
            }
        }
        Decl::Struct { name, fields, .. } => {
            let tuple = crate::core_ir::tuples::fields(name).is_some();
            for (_, value) in fields.iter_mut() {
                typ(value, names, &generics);
            }
            if tuple {
                if let Typ::Named(target) = crate::core_ir::tuples::typ(
                    &fields
                        .iter()
                        .map(|(_, typ)| typ.clone())
                        .collect::<Vec<_>>(),
                ) {
                    *name = target;
                }
            }
        }
        Decl::Class {
            fields,
            methods,
            extends,
            implements,
            ..
        } => {
            for (_, value) in fields {
                typ(value, names, &generics);
            }
            for method in methods {
                declaration(method, names, &generics);
            }
            for name in extends.iter_mut().chain(implements) {
                if let Some(target) = (names.type_name)(name) {
                    *name = target;
                }
            }
        }
        Decl::Interface { methods, .. } => {
            for method in methods {
                for (_, value) in &mut method.params {
                    typ(value, names, &generics);
                }
                typ(&mut method.ret, names, &generics);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn function(name: &str, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params: vec![],
            ret: Typ::Int,
            body,
            type_params: vec![],
        }
    }
    #[test]
    fn contextual_nominal_rewrite_is_exact_and_preserves_non_names() {
        let resolve = |name: &str| {
            if name == "Self" {
                Some("Thing".into())
            } else {
                name.strip_prefix("Self::")
                    .map(|tail| format!("Thing::{tail}"))
            }
        };
        let tuple = crate::core_ir::tuples::typ(&[Typ::Named("Self".into()), Typ::Int]);
        let mut decls = vec![Decl::Function {
            name: "Thing::new".into(),
            params: vec![("self".into(), Typ::Named("Self".into()))],
            ret: tuple,
            type_params: vec![],
            body: vec![
                Stmt::Let(
                    "Selfish".into(),
                    Some(Typ::Named("Selfish".into())),
                    Expr::StringLit("Self::new".into()),
                ),
                Stmt::Expr(Expr::ArrayLit(vec![Expr::StructInit {
                    name: "Self".into(),
                    fields: vec![("Self".into(), Expr::IntLit(42))],
                }])),
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("Self::new".into())),
                    args: vec![],
                }),
                Stmt::Expr(Expr::Field {
                    base: Box::new(Expr::Ident("self".into())),
                    name: "Self".into(),
                }),
                Stmt::Match {
                    scrutinee: Expr::Ident("self".into()),
                    arms: vec![crate::core_ir::MatchArm {
                        pattern: "Self { value: local }".into(),
                        guard: None,
                        body: vec![Stmt::Expr(Expr::Ident("Selfish".into()))],
                    }],
                },
            ],
        }];
        rewrite_nominal_names(&mut decls, &resolve, &resolve);
        let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = &decls[0]
        else {
            panic!()
        };
        assert_eq!(name, "Thing::new");
        assert_eq!(params[0].1, Typ::Named("Thing".into()));
        assert_eq!(
            ret,
            &crate::core_ir::tuples::typ(&[Typ::Named("Thing".into()), Typ::Int])
        );
        assert!(
            matches!(&body[0],Stmt::Let(name,Some(Typ::Named(typ)),Expr::StringLit(text)) if name=="Selfish"&&typ=="Selfish"&&text=="Self::new")
        );
        assert!(
            matches!(&body[1],Stmt::Expr(Expr::ArrayLit(items)) if matches!(&items[0],Expr::StructInit {name,fields} if name=="Thing" && fields[0].0=="Self"))
        );
        assert!(
            matches!(&body[2],Stmt::Expr(Expr::Call {callee,..}) if matches!(callee.as_ref(),Expr::Ident(name) if name=="Thing::new"))
        );
        assert!(matches!(&body[3],Stmt::Expr(Expr::Field {name,..}) if name=="Self"));
        let Stmt::Match { arms, .. } = &body[4] else {
            panic!()
        };
        assert!(
            matches!(MatchPattern::parse(&arms[0].pattern).unwrap(),MatchPattern::StructPat {name,..} if name=="Thing")
        );
    }

    #[test]
    fn nominal_type_rewrite_respects_generic_parameters() {
        let mut decls = vec![Decl::Function {
            name: "generic".into(),
            params: vec![("value".into(), Typ::Named("Alias".into()))],
            ret: Typ::Generic("Alias".into()),
            type_params: vec!["Alias".into()],
            body: vec![],
        }];
        let original = decls.clone();
        rewrite_nominal_names(
            &mut decls,
            &|name| (name == "Alias").then(|| "Thing".into()),
            &|_| None,
        );
        assert_eq!(decls, original);
    }

    #[test]
    fn dependencies_keep_distinct_helpers_and_lexical_shadows() {
        for module in ["first", "second"] {
            let mut decls = vec![
                function("helper", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                function(
                    "answer",
                    vec![Stmt::Return(Some(Expr::Ident("helper".into())))],
                ),
                function(
                    "shadow",
                    vec![
                        Stmt::Let("helper".into(), Some(Typ::Int), Expr::IntLit(7)),
                        Stmt::Return(Some(Expr::Ident("helper".into()))),
                    ],
                ),
            ];
            qualify_module_decls(&mut decls, module);
            let Decl::Function { name, body, .. } = &decls[1] else {
                panic!()
            };
            assert_eq!(name, &format!("{module}::answer"));
            assert_eq!(
                body,
                &vec![Stmt::Return(Some(Expr::Ident(format!("{module}::helper"))))]
            );
            let Decl::Function { body, .. } = &decls[2] else {
                panic!()
            };
            let Stmt::Let(local, _, _) = &body[0] else {
                panic!()
            };
            assert_ne!(local, "helper");
            assert_eq!(body[1], Stmt::Return(Some(Expr::Ident(local.clone()))));
            let once = decls.clone();
            qualify_module_decls(&mut decls, module);
            assert_eq!(decls, once);
        }
    }
    #[test]
    fn schemas_initializers_and_patterns_share_qualified_type_names() {
        let mut decls = vec![
            Decl::Struct {
                name: "Pair".into(),
                fields: vec![(
                    "nested".into(),
                    Typ::Vector(Box::new(Typ::Named("Pair".into()))),
                )],
                type_params: vec![],
            },
            Decl::Function {
                name: "answer".into(),
                params: vec![("pair".into(), Typ::Named("Pair".into()))],
                ret: Typ::Named("Pair".into()),
                type_params: vec!["T".into()],
                body: vec![
                    Stmt::Let(
                        "other".into(),
                        Some(Typ::Generic("T".into())),
                        Expr::StructInit {
                            name: "Pair".into(),
                            fields: vec![],
                        },
                    ),
                    Stmt::Match {
                        scrutinee: Expr::Ident("pair".into()),
                        arms: vec![crate::core_ir::MatchArm {
                            pattern: "Pair { nested: value }".into(),
                            guard: None,
                            body: vec![Stmt::Return(Some(Expr::Ident("pair".into())))],
                        }],
                    },
                ],
            },
        ];
        qualify_module_decls(&mut decls, "dep");
        let Decl::Struct { fields, .. } = &decls[0] else {
            panic!()
        };
        assert_eq!(
            fields[0].1,
            Typ::Vector(Box::new(Typ::Named("dep::Pair".into())))
        );
        let Decl::Function {
            params, ret, body, ..
        } = &decls[1]
        else {
            panic!()
        };
        assert_eq!(params[0].1, Typ::Named("dep::Pair".into()));
        assert_eq!(ret, &Typ::Named("dep::Pair".into()));
        let Stmt::Let(_, Some(annotation), Expr::StructInit { name, .. }) = &body[0] else {
            panic!()
        };
        assert_eq!(annotation, &Typ::Generic("T".into()));
        assert_eq!(name, "dep::Pair");
        let Stmt::Match { arms, .. } = &body[1] else {
            panic!()
        };
        assert!(
            matches!(MatchPattern::parse(&arms[0].pattern).unwrap(),MatchPattern::StructPat {name,..} if name=="dep::Pair")
        );
        let parsed = MatchPattern::parse(&arms[0].pattern).unwrap();
        assert_eq!(
            parsed
                .typed_bindings(
                    &Typ::Named("dep::Pair".into()),
                    &|name| if name == "dep::Pair" {
                        Some(fields.as_slice())
                    } else {
                        None
                    }
                )
                .unwrap(),
            vec![(
                "value".into(),
                Typ::Vector(Box::new(Typ::Named("dep::Pair".into())))
            )]
        );
    }
    #[test]
    fn rust_impl_methods_use_the_owning_types_lexical_module() {
        let source = "struct Thing { x:i64 } fn helper()->i64 {11} impl Thing { fn read(&self)->i64 {helper()+self.x} fn answer()->i64 {helper()} } mod child {pub struct Other {pub x:i64} fn helper()->i64 {20} impl Other {pub fn read(&self)->i64 {helper()+self.x} pub fn answer()->i64 {helper()} } pub fn run()->i64 {Other::answer()} } fn main()->i64 {Thing {x:20}.read()+Thing::answer()+child::run()}";
        let mut module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        qualify_module_decls(&mut module.decls, "dep");
        for (symbol, owner, helper) in [
            ("dep::Thing::read", "dep::Thing", "dep::helper"),
            (
                "dep::child::Other::read",
                "dep::child::Other",
                "dep::child::helper",
            ),
        ] {
            let Decl::Function { params, body, .. } = module
                .decls
                .iter()
                .find(|decl| matches!(decl,Decl::Function {name,..} if name==symbol))
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(params[0].1, Typ::Named(owner.into()));
            let mut names = HashSet::new();
            super::super::reserve_body(body, &mut names);
            assert!(names.contains(helper));
        }
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
        crate::lower_core::desugar_module(&mut module);
        crate::typecheck::TypeChecker::new()
            .check_module(&module)
            .unwrap();
        let report = crate::core_ir_verifier::verify_for_entry(&module, "dep::main");
        assert!(report.ok, "{report:?}");
    }

    #[test]
    fn prequalified_nested_trait_method_names_keep_owner_schema_and_helpers() {
        let source = "mod child {pub struct Thing {pub x:i64} pub trait Read {fn read(&self)->i64;} fn helper()->i64 {20} impl Read for Thing {fn read(&self)->i64 {helper()+self.x}}} fn main()->i64 {42}";
        let mut module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        qualify_module_decls(&mut module.decls, "dep");
        let Decl::Function {params,body,..}=module.decls.iter().find(|decl|matches!(decl,Decl::Function {name,..} if name.contains("Thing::Read::read"))).unwrap() else {panic!()};
        assert_eq!(params[0].1, Typ::Named("dep::child::Thing".into()));
        let mut names = HashSet::new();
        super::super::reserve_body(body, &mut names);
        assert!(names.contains("dep::child::helper"));
        let mut normalized =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
        crate::lower_core::desugar_module(&mut normalized);
        crate::typecheck::TypeChecker::new()
            .check_module(&normalized)
            .unwrap();
    }

    #[test]
    fn tuple_shapes_qualify_embedded_local_structs_structurally() {
        let original = crate::core_ir::tuples::typ(&[Typ::Named("Pair".into()), Typ::Int]);
        let Typ::Named(original_name) = &original else {
            panic!()
        };
        let mut decls = vec![
            Decl::Struct {
                name: "Pair".into(),
                fields: vec![("value".into(), Typ::Int)],
                type_params: vec![],
            },
            Decl::Struct {
                name: original_name.clone(),
                fields: crate::core_ir::tuples::fields(original_name).unwrap(),
                type_params: vec![],
            },
            Decl::Function {
                name: "answer".into(),
                params: vec![("input".into(), original.clone())],
                ret: original.clone(),
                type_params: vec![],
                body: vec![Stmt::Return(Some(Expr::StructInit {
                    name: original_name.clone(),
                    fields: vec![],
                }))],
            },
        ];
        qualify_module_decls(&mut decls, "dep");
        let expected = crate::core_ir::tuples::typ(&[Typ::Named("dep::Pair".into()), Typ::Int]);
        let Typ::Named(expected_name) = &expected else {
            panic!()
        };
        let Decl::Struct { name, fields, .. } = &decls[1] else {
            panic!()
        };
        assert_eq!(name, expected_name);
        assert!(!name.starts_with("dep::"));
        assert_eq!(fields[0].1, Typ::Named("dep::Pair".into()));
        let Decl::Function {
            params, ret, body, ..
        } = &decls[2]
        else {
            panic!()
        };
        assert_eq!(params[0].1, expected);
        assert_eq!(ret, &expected);
        let Stmt::Return(Some(Expr::StructInit { name, .. })) = &body[0] else {
            panic!()
        };
        assert_eq!(name, expected_name);
        let once = decls.clone();
        qualify_module_decls(&mut decls, "dep");
        assert_eq!(decls, once);
    }

    #[test]
    fn literal_assignment_targets_qualify_without_a_global_read() {
        let mut decls = vec![
            Decl::Global {
                name: "VALUE".into(),
                typ: Typ::Int,
                init: Some(Box::new(Expr::IntLit(0))),
                mutable: true,
                constant_value: false,
            },
            function(
                "set",
                vec![
                    Stmt::Assign("VALUE".into(), Expr::IntLit(42)),
                    Stmt::Return(Some(Expr::UnitLit)),
                ],
            ),
        ];
        qualify_module_decls(&mut decls, "dep");
        let Decl::Function { body, .. } = &decls[1] else {
            panic!()
        };
        assert_eq!(body[0], Stmt::Assign("dep::VALUE".into(), Expr::IntLit(42)));
    }

    #[test]
    fn explicit_relative_paths_resolve_across_unloaded_files() {
        let mut decls = vec![function(
            "answer",
            vec![Stmt::Return(Some(Expr::ArrayLit(vec![
                Expr::Ident("crate::shared::answer".into()),
                Expr::Ident("super::shared::answer".into()),
                Expr::Ident("self::child::answer".into()),
                Expr::Ident("external::answer".into()),
                Expr::Ident("super::super::super::answer".into()),
            ])))],
        )];
        qualify_module_decls(&mut decls, "dep::nested");
        let Decl::Function { body, .. } = &decls[0] else {
            panic!()
        };
        assert_eq!(
            body,
            &vec![Stmt::Return(Some(Expr::ArrayLit(vec![
                Expr::Ident("dep::shared::answer".into()),
                Expr::Ident("dep::shared::answer".into()),
                Expr::Ident("dep::nested::child::answer".into()),
                Expr::Ident("external::answer".into()),
                Expr::Ident("super::super::super::answer".into())
            ])))]
        );
    }

    #[test]
    fn qualified_patterns_preserve_escaped_string_values() {
        let original = MatchPattern::StructPat {
            name: "Pair".into(),
            fields: vec![(
                "text".into(),
                MatchPattern::StringPat("line\n\"quote\"\\tail".into()),
            )],
        };
        let mut source = super::super::render_pattern(&original);
        pattern(
            &mut source,
            &NominalNames {
                type_name: &|name| (name == "Pair").then(|| "dep::Pair".into()),
                value_name: &|_| None,
            },
            &HashSet::new(),
        );
        let mut expected = original;
        let MatchPattern::StructPat { name, .. } = &mut expected else {
            panic!()
        };
        *name = "dep::Pair".into();
        assert_eq!(MatchPattern::parse(&source).unwrap(), expected);
    }

    #[test]
    fn parent_bare_names_are_not_implicitly_imported() {
        let mut decls = vec![
            function("helper", vec![]),
            function(
                "sub::answer",
                vec![Stmt::Return(Some(Expr::Ident("helper".into())))],
            ),
        ];
        qualify_module_decls(&mut decls, "dep");
        let Decl::Function { body, .. } = &decls[1] else {
            panic!()
        };
        assert_eq!(
            body,
            &vec![Stmt::Return(Some(Expr::Ident("helper".into())))]
        );
    }

    #[test]
    fn nested_relative_references_use_the_declaring_module() {
        let mut decls = vec![
            function("helper", vec![]),
            function("sub::helper", vec![]),
            function(
                "sub::answer",
                vec![Stmt::Return(Some(Expr::ArrayLit(vec![
                    Expr::Ident("helper".into()),
                    Expr::Ident("self::helper".into()),
                    Expr::Ident("super::helper".into()),
                    Expr::Ident("unknown::helper".into()),
                ])))],
            ),
        ];
        qualify_module_decls(&mut decls, "dep");
        let Decl::Function { body, .. } = &decls[2] else {
            panic!()
        };
        assert_eq!(
            body,
            &vec![Stmt::Return(Some(Expr::ArrayLit(vec![
                Expr::Ident("dep::sub::helper".into()),
                Expr::Ident("dep::sub::helper".into()),
                Expr::Ident("dep::helper".into()),
                Expr::Ident("unknown::helper".into())
            ])))]
        );
        let once = decls.clone();
        qualify_module_decls(&mut decls, "dep");
        assert_eq!(decls, once);
    }
}
