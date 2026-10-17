//! Give shadowing bindings distinct names before name-keyed optimization and lowering.
#[path = "core_bindings/namespace.rs"]
mod namespace;
pub(crate) use namespace::{qualify_module_artifact, rewrite_named_types, rewrite_nominal_names};

use crate::core_ir::{Decl, Expr, LoopKind, MatchPattern, Stmt, Typ};
use std::collections::{HashMap, HashSet};

pub(crate) fn module_names(decls: &[Decl]) -> HashSet<String> {
    decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, .. } | Decl::Function { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Resolve only spellings actually present in this module. Exact declarations
/// win over the surface underscore-to-kebab alias; lexical scopes override these
/// references during normalization.
pub(crate) fn function_references(decls: &[Decl]) -> HashMap<String, String> {
    fn gather(decls: &[Decl], spellings: &mut HashSet<String>) {
        for decl in decls {
            match decl {
                Decl::Function { body, .. } => reserve_body(body, spellings),
                Decl::Global {
                    init: Some(expr), ..
                } => reserve_expr(expr, spellings),
                Decl::Class { methods, .. } => gather(methods, spellings),
                _ => {}
            }
        }
    }
    let names = module_names(decls);
    let functions: HashSet<_> = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let mut spellings = HashSet::new();
    gather(decls, &mut spellings);
    spellings
        .into_iter()
        .filter_map(|spelling| {
            if names.contains(&spelling) || functions.contains(&spelling) {
                return None;
            }
            let canonical = spelling.replace('_', "-");
            if canonical != spelling && functions.contains(&canonical) {
                return Some((spelling, canonical));
            }
            crate::core_ir::legacy_compiler_callee(&spelling)
                .map(|private| (spelling, private.into()))
        })
        .collect()
}

pub(crate) fn normalize_decls(decls: &mut [Decl]) {
    let names = module_names(decls);
    let references = function_references(decls);
    normalize_members(decls, &names, &references);
}

fn normalize_members(
    decls: &mut [Decl],
    names: &HashSet<String>,
    references: &HashMap<String, String>,
) {
    for decl in decls {
        match decl {
            Decl::Function { params, body, .. } => {
                normalize_function_with_references(params, body, names, references)
            }
            // Methods are not module-level functions until class desugaring.
            // Preserve their member lookup for that phase; normalize only locals.
            Decl::Class { methods, .. } => normalize_members(methods, names, &HashMap::new()),
            Decl::Global {
                init: Some(expr), ..
            } => normalize_initializer_with_references(expr, names, references),
            _ => {}
        }
    }
}

pub(crate) fn normalize_function(
    params: &mut [(String, Typ)],
    body: &mut [Stmt],
    names: &HashSet<String>,
) {
    normalize_function_with_references(params, body, names, &HashMap::new());
}

pub(crate) fn normalize_function_with_references(
    params: &mut [(String, Typ)],
    body: &mut [Stmt],
    names: &HashSet<String>,
    references: &HashMap<String, String>,
) {
    let mut reserved = HashSet::new();
    reserved.extend(params.iter().map(|(name, _)| name.clone()));
    reserve_body(body, &mut reserved);
    let mut bindings = Bindings {
        reserved,
        module_names: names,
        declared: HashSet::new(),
        next: 0,
    };
    let mut scope = references.clone();
    for (name, _) in params {
        bindings.bind(name, &mut scope);
    }
    bindings.body(body, &mut scope);
}

pub(crate) fn normalize_initializer_with_references(
    expr: &mut Expr,
    names: &HashSet<String>,
    references: &HashMap<String, String>,
) {
    let mut reserved = HashSet::new();
    reserve_expr(expr, &mut reserved);
    let mut bindings = Bindings {
        reserved,
        module_names: names,
        declared: HashSet::new(),
        next: 0,
    };
    bindings.expr(expr, references);
}

pub(crate) fn reserve_expr(expr: &Expr, names: &mut HashSet<String>) {
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            reserve_expr(scrutinee, names);
            for arm in arms {
                if let Ok(mut pattern) = MatchPattern::parse(&arm.pattern) {
                    pattern_names(&mut pattern, &mut |name| {
                        names.insert(name.clone());
                    });
                }
                if let Some(guard) = &arm.guard {
                    reserve_expr(guard, names);
                }
                reserve_expr(&arm.value, names);
            }
        }
        Expr::Ident(name) => {
            names.insert(name.clone());
        }
        Expr::Block { body, result } => {
            reserve_body(body, names);
            if let Some(result) = result {
                reserve_expr(result, names);
            }
        }
        Expr::Closure {
            params,
            body,
            captures,
            ..
        } => {
            names.extend(params.iter().map(|(name, _)| name.clone()));
            names.extend(captures.iter().cloned());
            reserve_body(body, names);
        }
        _ => crate::core_ir::for_each_expr_child(expr, &mut |child| reserve_expr(child, names)),
    }
}

pub(crate) fn reserve_body(body: &[Stmt], names: &mut HashSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::Let(name, _, expr) | Stmt::Assign(name, expr) => {
                names.insert(name.clone());
                reserve_expr(expr, names);
            }
            Stmt::Expr(expr) | Stmt::Throw(expr) | Stmt::Return(Some(expr)) => {
                reserve_expr(expr, names)
            }
            Stmt::FieldAssign { base, value, .. } => {
                reserve_expr(base, names);
                reserve_expr(value, names);
            }
            Stmt::IndexAssign { base, index, value } => {
                reserve_expr(base, names);
                reserve_expr(index, names);
                reserve_expr(value, names);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                reserve_expr(cond, names);
                reserve_body(then_body, names);
                reserve_body(else_body, names);
            }
            Stmt::Loop { kind, cond, body } => {
                if let LoopKind::For { binding } = kind {
                    names.insert(binding.clone());
                }
                if let Some(cond) = cond {
                    reserve_expr(cond, names);
                }
                reserve_body(body, names);
            }
            Stmt::Match { scrutinee, arms } => {
                reserve_expr(scrutinee, names);
                for arm in arms {
                    reserve_pattern(&arm.pattern, names);
                    if let Some(guard) = &arm.guard {
                        reserve_expr(guard, names);
                    }
                    reserve_body(&arm.body, names);
                }
            }
            Stmt::Try { body, catches } => {
                reserve_body(body, names);
                for catch in catches {
                    reserve_pattern(&catch.pattern, names);
                    reserve_body(&catch.body, names);
                }
            }
            _ => {}
        }
    }
}

fn pattern_names(pattern: &mut MatchPattern, visit: &mut impl FnMut(&mut String)) {
    match pattern {
        MatchPattern::IdentPat(name) => visit(name),
        MatchPattern::TuplePat(items) | MatchPattern::ArrayPat(items) => {
            for item in items {
                pattern_names(item, visit);
            }
        }
        MatchPattern::StructPat { fields, .. } => {
            for (_, item) in fields {
                pattern_names(item, visit);
            }
        }
        _ => {}
    }
}

pub(crate) fn reserve_pattern(source: &str, names: &mut HashSet<String>) {
    if let Ok(mut pattern) = MatchPattern::parse(source) {
        pattern_names(&mut pattern, &mut |name| {
            names.insert(name.clone());
        });
    }
}

pub(crate) fn render_pattern(pattern: &MatchPattern) -> String {
    match pattern {
        MatchPattern::IntPat(value) => value.to_string(),
        MatchPattern::BoolPat(value) => value.to_string(),
        MatchPattern::StringPat(value) => format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t")
        ),
        MatchPattern::IdentPat(name) => name.clone(),
        MatchPattern::WildPat => "_".into(),
        MatchPattern::RestPat => "..".into(),
        MatchPattern::TuplePat(items) => format!(
            "({})",
            items
                .iter()
                .map(render_pattern)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MatchPattern::ArrayPat(items) => format!(
            "[{}]",
            items
                .iter()
                .map(render_pattern)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        MatchPattern::StructPat { name, fields } => format!(
            "{name} {{ {} }}",
            fields
                .iter()
                .map(|(field, value)| format!("{field}: {}", render_pattern(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

struct Bindings<'a> {
    reserved: HashSet<String>,
    module_names: &'a HashSet<String>,
    declared: HashSet<String>,
    next: usize,
}

impl Bindings<'_> {
    fn bind(&mut self, name: &mut String, scope: &mut HashMap<String, String>) {
        let original = name.clone();
        let duplicate = !self.declared.insert(original.clone());
        if duplicate || self.module_names.contains(&original) {
            loop {
                let candidate = format!("in-local-{}", self.next);
                self.next += 1;
                if !self.module_names.contains(&candidate)
                    && self.reserved.insert(candidate.clone())
                {
                    self.declared.insert(candidate.clone());
                    *name = candidate;
                    break;
                }
            }
        }
        scope.insert(original, name.clone());
    }

    fn pattern(&mut self, source: &mut String, scope: &mut HashMap<String, String>) {
        if let Ok(mut pattern) = MatchPattern::parse(source) {
            let original = pattern.clone();
            pattern_names(&mut pattern, &mut |name| self.bind(name, scope));
            if pattern != original {
                *source = render_pattern(&pattern);
            }
        }
    }

    fn reference(name: &mut String, scope: &HashMap<String, String>) {
        if let Some(resolved) = scope.get(name) {
            *name = resolved.clone();
        }
    }

    fn expr(&mut self, expr: &mut Expr, scope: &HashMap<String, String>) {
        match expr {
            Expr::Ident(name) => Self::reference(name, scope),
            Expr::MatchValue { scrutinee, arms } => {
                self.expr(scrutinee, scope);
                for arm in arms {
                    let mut inner = scope.clone();
                    self.pattern(&mut arm.pattern, &mut inner);
                    if let Some(guard) = &mut arm.guard {
                        self.expr(guard, &inner);
                    }
                    self.expr(&mut arm.value, &inner);
                }
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => {
                self.expr(cond, scope);
                self.expr(then_expr, scope);
                self.expr(else_expr, scope);
            }
            Expr::Block { body, result } => {
                let mut inner = scope.clone();
                self.body(body, &mut inner);
                if let Some(result) = result {
                    self.expr(result, &inner);
                }
            }
            Expr::Unary { expr, .. } => self.expr(expr, scope),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs, scope);
                self.expr(rhs, scope);
            }
            Expr::Call { callee, args } => {
                // Compiler operations cannot be captured by a same-spelled local.
                if !matches!(callee.as_ref(), Expr::Ident(name) if crate::core_ir::is_compiler_callee(name))
                {
                    self.expr(callee, scope);
                }
                for arg in args {
                    self.expr(arg, scope);
                }
            }
            Expr::StructInit { fields, .. } => {
                for (_, value) in fields {
                    self.expr(value, scope);
                }
            }
            Expr::Field { base, .. } => self.expr(base, scope),
            Expr::ArrayLit(items) => {
                for item in items {
                    self.expr(item, scope);
                }
            }
            Expr::Index { base, index } => {
                self.expr(base, scope);
                self.expr(index, scope);
            }
            Expr::Closure {
                params,
                body,
                captures,
                ..
            } => {
                for capture in captures {
                    Self::reference(capture, scope);
                }
                let mut inner = scope.clone();
                for (name, _) in params {
                    self.bind(name, &mut inner);
                }
                self.body(body, &mut inner);
            }
            _ => {}
        }
    }

    fn body(&mut self, body: &mut [Stmt], scope: &mut HashMap<String, String>) {
        for stmt in body {
            match stmt {
                Stmt::Let(name, _, expr) => {
                    self.expr(expr, scope);
                    self.bind(name, scope);
                }
                Stmt::Assign(name, expr) => {
                    Self::reference(name, scope);
                    self.expr(expr, scope);
                }
                Stmt::Expr(expr) | Stmt::Throw(expr) | Stmt::Return(Some(expr)) => {
                    self.expr(expr, scope)
                }
                Stmt::FieldAssign { base, value, .. } => {
                    self.expr(base, scope);
                    self.expr(value, scope);
                }
                Stmt::IndexAssign { base, index, value } => {
                    self.expr(base, scope);
                    self.expr(index, scope);
                    self.expr(value, scope);
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    self.expr(cond, scope);
                    self.body(then_body, &mut scope.clone());
                    self.body(else_body, &mut scope.clone());
                }
                Stmt::Loop { kind, cond, body } => {
                    if let Some(cond) = cond {
                        self.expr(cond, scope);
                    }
                    let mut inner = scope.clone();
                    if let LoopKind::For { binding } = kind {
                        self.bind(binding, &mut inner);
                    }
                    self.body(body, &mut inner);
                }
                Stmt::Match { scrutinee, arms } => {
                    self.expr(scrutinee, scope);
                    for arm in arms {
                        let mut inner = scope.clone();
                        self.pattern(&mut arm.pattern, &mut inner);
                        if let Some(guard) = &mut arm.guard {
                            self.expr(guard, &inner);
                        }
                        self.body(&mut arm.body, &mut inner);
                    }
                }
                Stmt::Try { body, catches } => {
                    self.body(body, &mut scope.clone());
                    for catch in catches {
                        let mut inner = scope.clone();
                        self.pattern(&mut catch.pattern, &mut inner);
                        self.body(&mut catch.body, &mut inner);
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_block_shadowing_does_not_escape() {
        let mut params = vec![("x".into(), Typ::Int)];
        let mut body = vec![
            Stmt::Expr(Expr::Block {
                body: vec![Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(1))],
                result: Some(Box::new(Expr::Ident("x".into()))),
            }),
            Stmt::Return(Some(Expr::Ident("x".into()))),
        ];
        normalize_function(&mut params, &mut body, &HashSet::new());
        let Stmt::Expr(Expr::Block {
            body: inner,
            result: Some(result),
        }) = &body[0]
        else {
            panic!()
        };
        let Stmt::Let(local, _, _) = &inner[0] else {
            panic!()
        };
        assert_ne!(local, &params[0].0);
        assert_eq!(result.as_ref(), &Expr::Ident(local.clone()));
        assert_eq!(
            body[1],
            Stmt::Return(Some(Expr::Ident(params[0].0.clone())))
        );
    }

    #[test]
    fn nested_binders_preserve_fields_and_closure_captures() {
        let mut params = vec![("x".into(), Typ::Int)];
        let mut body = vec![
            Stmt::Expr(Expr::Closure {
                params: vec![("x".into(), Typ::Int)],
                ret: Typ::Int,
                captures: vec!["x".into()],
                body: vec![Stmt::Return(Some(Expr::Ident("x".into())))],
            }),
            Stmt::Match {
                scrutinee: Expr::Ident("x".into()),
                arms: vec![crate::core_ir::MatchArm {
                    guard: None,
                    pattern: "Point { x }".into(),
                    body: vec![Stmt::Return(Some(Expr::Ident("x".into())))],
                }],
            },
            Stmt::Return(Some(Expr::Ident("x".into()))),
        ];
        normalize_function(&mut params, &mut body, &HashSet::new());
        let Stmt::Expr(Expr::Closure {
            params: inner,
            captures,
            body: closure_body,
            ..
        }) = &body[0]
        else {
            panic!("closure");
        };
        assert_eq!(captures, &[params[0].0.clone()]);
        assert_ne!(inner[0].0, params[0].0);
        assert_eq!(
            closure_body,
            &[Stmt::Return(Some(Expr::Ident(inner[0].0.clone())))]
        );
        let Stmt::Match { scrutinee, arms } = &body[1] else {
            panic!("match");
        };
        assert_eq!(scrutinee, &Expr::Ident(params[0].0.clone()));
        let MatchPattern::StructPat { name, fields } =
            MatchPattern::parse(&arms[0].pattern).unwrap()
        else {
            panic!("struct pattern");
        };
        assert_eq!(name, "Point");
        assert_eq!(fields[0].0, "x");
        let MatchPattern::IdentPat(binding) = &fields[0].1 else {
            panic!("binding");
        };
        assert_ne!(binding, &params[0].0);
        assert_eq!(
            arms[0].body,
            [Stmt::Return(Some(Expr::Ident(binding.clone())))]
        );
        assert_eq!(
            body[2],
            Stmt::Return(Some(Expr::Ident(params[0].0.clone())))
        );
    }

    #[test]
    fn binding_names_are_stable_and_do_not_capture_existing_names() {
        let mut module = crate::in_lang_parse::parse_in_source("var x: Int = 42;\nfn main(x: Int) -> Int { let in-local-0 = 9; let x = x + 1; return x + in-local-0; }").unwrap();
        normalize_decls(&mut module.decls);
        let normalized = module.decls.clone();
        normalize_decls(&mut module.decls);
        assert_eq!(module.decls, normalized);
        let Decl::Function { params, body, .. } = &module.decls[1] else {
            panic!("function");
        };
        assert_ne!(params[0].0, "x");
        assert_ne!(params[0].0, "in-local-0");
        let Stmt::Let(name, _, Expr::Binary { lhs, .. }) = &body[1] else {
            panic!("shadowing declaration");
        };
        assert_ne!(name, &params[0].0);
        assert_eq!(lhs.as_ref(), &Expr::Ident(params[0].0.clone()));
    }
}

#[cfg(test)]
mod function_alias_tests {
    use super::*;

    fn function(name: &str, params: Vec<(String, Typ)>, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params,
            ret: Typ::Int,
            body,
            type_params: vec![],
        }
    }

    fn returned_name(decl: &Decl) -> &str {
        let Decl::Function { body, .. } = decl else {
            panic!("function");
        };
        let Some(Stmt::Return(Some(Expr::Ident(name)))) = body.last() else {
            panic!("return: {body:?}");
        };
        name
    }

    #[test]
    fn class_members_do_not_create_or_block_module_function_aliases() {
        let mut decls = vec![
            function(
                "helper-name",
                vec![],
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "main",
                vec![],
                vec![Stmt::Return(Some(Expr::Ident("helper_name".into())))],
            ),
            function(
                "unresolved",
                vec![],
                vec![Stmt::Return(Some(Expr::Ident("private_name".into())))],
            ),
            Decl::Class {
                name: "Thing".into(),
                fields: vec![],
                visibility: crate::core_ir::Visibility::Private,
                extends: None,
                implements: vec![],
                type_params: vec![],
                methods: vec![
                    function(
                        "helper_name",
                        vec![],
                        vec![Stmt::Return(Some(Expr::Ident("helper_name".into())))],
                    ),
                    function(
                        "private-name",
                        vec![],
                        vec![Stmt::Return(Some(Expr::IntLit(1)))],
                    ),
                ],
            },
        ];
        let refs = function_references(&decls);
        assert_eq!(
            refs.get("helper_name").map(String::as_str),
            Some("helper-name")
        );
        assert!(!refs.contains_key("private_name"));
        normalize_decls(&mut decls);
        assert_eq!(returned_name(&decls[1]), "helper-name");
        assert_eq!(returned_name(&decls[2]), "private_name");
        let Decl::Class { methods, .. } = &decls[3] else {
            panic!("class");
        };
        assert_eq!(returned_name(&methods[0]), "helper_name");
    }

    #[test]
    fn observed_function_aliases_normalize_values_calls_and_initializers_idempotently() {
        let mut decls = vec![
            function(
                "helper-name-more",
                vec![],
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "main",
                vec![],
                vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("helper_name-more".into())),
                    args: vec![],
                }))],
            ),
            Decl::Global {
                name: "callback".into(),
                typ: Typ::Int,
                init: Some(Box::new(Expr::Ident("helper-name_more".into()))),
                mutable: true,
                constant_value: false,
            },
        ];
        // Direct backend APIs normalize their cloned function bodies too, even
        // when no optimizer has run over the original module.
        let direct = crate::core_ir::UnifiedModule::new(decls[..2].to_vec());
        assert!(
            crate::native_emit::lower::lower_module(
                &direct,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .is_ok()
        );
        assert!(crate::native_emit::x86_64_lower::lower_module(&direct, "main").is_ok());
        let references = function_references(&decls);
        assert_eq!(
            references.get("helper_name-more").map(String::as_str),
            Some("helper-name-more")
        );
        assert_eq!(
            references.get("helper-name_more").map(String::as_str),
            Some("helper-name-more")
        );
        assert_eq!(references.len(), 2);
        normalize_decls(&mut decls);
        assert!(matches!(&decls[1], Decl::Function { body, .. }
            if matches!(body.last(), Some(Stmt::Return(Some(Expr::Call { callee, .. })))
                if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper-name-more"))));
        assert!(matches!(&decls[2], Decl::Global { init: Some(init), .. }
            if matches!(init.as_ref(), Expr::Ident(name) if name == "helper-name-more")));
        let once = decls.clone();
        normalize_decls(&mut decls);
        assert_eq!(decls, once);
    }

    #[test]
    fn exact_module_names_and_lexical_bindings_override_function_aliases() {
        let alias = "helper_name";
        let value = || Expr::Ident(alias.into());
        let mut decls = vec![
            function(
                "helper-name",
                vec![],
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "parameter",
                vec![(alias.into(), Typ::Int)],
                vec![Stmt::Return(Some(value()))],
            ),
            function(
                "local",
                vec![],
                vec![
                    Stmt::Let(alias.into(), Some(Typ::Int), Expr::IntLit(7)),
                    Stmt::Return(Some(value())),
                ],
            ),
            function(
                "closure",
                vec![],
                vec![Stmt::Expr(Expr::Closure {
                    params: vec![(alias.into(), Typ::Int)],
                    ret: Typ::Int,
                    body: vec![Stmt::Return(Some(value()))],
                    captures: vec![],
                })],
            ),
            function(
                "pattern",
                vec![],
                vec![Stmt::Match {
                    scrutinee: Expr::IntLit(9),
                    arms: vec![crate::core_ir::MatchArm {
                        pattern: alias.into(),
                        guard: Some(Expr::Binary {
                            op: "==".into(),
                            lhs: Box::new(value()),
                            rhs: Box::new(Expr::IntLit(9)),
                        }),
                        body: vec![Stmt::Return(Some(value()))],
                    }],
                }],
            ),
        ];
        normalize_decls(&mut decls);
        assert_eq!(returned_name(&decls[1]), alias);
        assert_eq!(returned_name(&decls[2]), alias);
        assert!(matches!(&decls[3], Decl::Function { body, .. }
            if matches!(&body[0], Stmt::Expr(Expr::Closure { body, .. })
                if matches!(body.last(), Some(Stmt::Return(Some(Expr::Ident(name)))) if name == alias))));
        assert!(matches!(&decls[4], Decl::Function { body, .. }
            if matches!(&body[0], Stmt::Match { arms, .. }
                if arms[0].pattern == alias && matches!(arms[0].body.last(), Some(Stmt::Return(Some(Expr::Ident(name)))) if name == alias)
                    && matches!(&arms[0].guard, Some(Expr::Binary { lhs, .. }) if matches!(lhs.as_ref(), Expr::Ident(name) if name == alias)))));
        decls.push(function(
            alias,
            vec![],
            vec![Stmt::Return(Some(Expr::IntLit(1)))],
        ));
        decls.push(function("exact", vec![], vec![Stmt::Return(Some(value()))]));
        assert!(!function_references(&decls).contains_key(alias));
        normalize_decls(&mut decls);
        assert_eq!(returned_name(decls.last().unwrap()), alias);
        decls.pop();
        decls.pop();
        decls.push(Decl::Global {
            name: alias.into(),
            typ: Typ::Int,
            init: Some(Box::new(Expr::IntLit(3))),
            mutable: true,
            constant_value: false,
        });
        decls.push(function(
            "exact_global",
            vec![],
            vec![Stmt::Return(Some(value()))],
        ));
        assert!(!function_references(&decls).contains_key(alias));
        normalize_decls(&mut decls);
        assert_eq!(returned_name(decls.last().unwrap()), alias);
    }

    fn operation_call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args,
        }
    }

    fn assert_operation_callee(expr: &Expr, expected: &str) {
        assert!(
            matches!(expr, Expr::Call {callee,..} if matches!(callee.as_ref(),Expr::Ident(name) if name==expected)),
            "{expr:?}: expected {expected}"
        );
    }

    #[test]
    fn legacy_operations_honor_module_functions_globals_and_declared_underscore_aliases() {
        for public in [
            "index-assign-value-first",
            "index-modify-value-first",
            "index-modify-place-first",
            "field-modify-value-first",
            "modify-value-first",
            "array-repeat",
        ] {
            let private = crate::core_ir::legacy_compiler_callee(public).unwrap();
            let main = || {
                function(
                    "main",
                    vec![],
                    vec![Stmt::Return(Some(operation_call(public, vec![])))],
                )
            };
            let mut unbound = vec![main()];
            normalize_decls(&mut unbound);
            let Decl::Function { body, .. } = &unbound[0] else {
                panic!()
            };
            let Stmt::Return(Some(value)) = &body[0] else {
                panic!()
            };
            assert_operation_callee(value, private);
            let once = unbound.clone();
            normalize_decls(&mut unbound);
            assert_eq!(unbound, once);

            for binding in [
                function(public, vec![], vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                Decl::Global {
                    name: public.into(),
                    typ: Typ::Int,
                    init: Some(Box::new(Expr::IntLit(42))),
                    mutable: true,
                    constant_value: false,
                },
            ] {
                let mut decls = vec![binding, main()];
                assert!(!function_references(&decls).contains_key(public));
                normalize_decls(&mut decls);
                let Decl::Function { body, .. } = &decls[1] else {
                    panic!()
                };
                let Stmt::Return(Some(value)) = &body[0] else {
                    panic!()
                };
                assert_operation_callee(value, public);
            }
            let alias = public.replace('-', "_");
            let mut decls = vec![
                function(public, vec![], vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                function(
                    "main",
                    vec![],
                    vec![Stmt::Return(Some(operation_call(&alias, vec![])))],
                ),
            ];
            assert_eq!(
                function_references(&decls).get(&alias),
                Some(&public.to_string())
            );
            normalize_decls(&mut decls);
            let Decl::Function { body, .. } = &decls[1] else {
                panic!()
            };
            let Stmt::Return(Some(value)) = &body[0] else {
                panic!()
            };
            assert_operation_callee(value, public);
        }
    }

    #[test]
    fn legacy_operations_honor_local_closures_parameters_and_initializer_scope() {
        for public in [
            "index-assign-value-first",
            "index-modify-value-first",
            "index-modify-place-first",
            "field-modify-value-first",
            "modify-value-first",
            "array-repeat",
        ] {
            let private = crate::core_ir::legacy_compiler_callee(public).unwrap();
            let mut decls = vec![
                function(
                    "main",
                    vec![],
                    vec![
                        Stmt::Let(
                            public.into(),
                            None,
                            Expr::Block {
                                body: vec![Stmt::Expr(operation_call(public, vec![]))],
                                result: Some(Box::new(Expr::Closure {
                                    params: vec![],
                                    ret: Typ::Int,
                                    captures: vec![],
                                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                                })),
                            },
                        ),
                        Stmt::Return(Some(operation_call(public, vec![]))),
                    ],
                ),
                function(
                    "parameter",
                    vec![(public.into(), Typ::Generic("_".into()))],
                    vec![Stmt::Return(Some(operation_call(public, vec![])))],
                ),
            ];
            normalize_decls(&mut decls);
            let Decl::Function { body, .. } = &decls[0] else {
                panic!()
            };
            let Stmt::Let(local, _, Expr::Block { body: initial, .. }) = &body[0] else {
                panic!()
            };
            let Stmt::Expr(initial) = &initial[0] else {
                panic!()
            };
            assert_operation_callee(initial, private);
            let Stmt::Return(Some(value)) = &body[1] else {
                panic!()
            };
            assert_operation_callee(value, local);
            let Decl::Function { params, body, .. } = &decls[1] else {
                panic!()
            };
            let Stmt::Return(Some(value)) = &body[0] else {
                panic!()
            };
            assert_operation_callee(value, &params[0].0);
            let once = decls.clone();
            normalize_decls(&mut decls);
            assert_eq!(decls, once);
        }
    }

    #[test]
    fn operation_aliases_honor_pattern_and_catch_scopes_without_leaking() {
        let public = "array-repeat";
        let mut decls = vec![function(
            "main",
            vec![],
            vec![
                Stmt::Match {
                    scrutinee: Expr::IntLit(42),
                    arms: vec![crate::core_ir::MatchArm {
                        pattern: public.into(),
                        guard: Some(operation_call(public, vec![])),
                        body: vec![Stmt::Expr(operation_call(public, vec![]))],
                    }],
                },
                Stmt::Try {
                    body: vec![],
                    catches: vec![crate::core_ir::CatchArm {
                        pattern: public.into(),
                        body: vec![Stmt::Expr(operation_call(public, vec![]))],
                    }],
                },
                Stmt::Return(Some(operation_call(public, vec![]))),
            ],
        )];
        normalize_decls(&mut decls);
        let Decl::Function { body, .. } = &decls[0] else {
            panic!()
        };
        let Stmt::Match { arms, .. } = &body[0] else {
            panic!()
        };
        assert_operation_callee(arms[0].guard.as_ref().unwrap(), &arms[0].pattern);
        let Stmt::Expr(value) = &arms[0].body[0] else {
            panic!()
        };
        assert_operation_callee(value, &arms[0].pattern);
        let Stmt::Try { catches, .. } = &body[1] else {
            panic!()
        };
        let Stmt::Expr(value) = &catches[0].body[0] else {
            panic!()
        };
        assert_operation_callee(value, &catches[0].pattern);
        let Stmt::Return(Some(value)) = &body[2] else {
            panic!()
        };
        assert_operation_callee(value, crate::core_ir::ARRAY_REPEAT);
        let once = decls.clone();
        normalize_decls(&mut decls);
        assert_eq!(decls, once);
    }

    #[test]
    fn reserved_callees_keep_identity_while_their_operands_follow_lexical_bindings() {
        let private = crate::core_ir::ARRAY_REPEAT;
        let mut decls = vec![function(
            "main",
            vec![(private.into(), Typ::Int)],
            vec![
                Stmt::Let(private.into(), Some(Typ::Int), Expr::IntLit(42)),
                Stmt::Return(Some(operation_call(
                    private,
                    vec![Expr::Ident(private.into())],
                ))),
            ],
        )];
        normalize_decls(&mut decls);
        let Decl::Function { body, .. } = &decls[0] else {
            panic!()
        };
        let Stmt::Let(local, _, _) = &body[0] else {
            panic!()
        };
        assert_ne!(local, private);
        let Stmt::Return(Some(value)) = &body[1] else {
            panic!()
        };
        assert_operation_callee(value, private);
        let Expr::Call { args, .. } = value else {
            panic!()
        };
        assert_eq!(args, &vec![Expr::Ident(local.clone())]);
        let once = decls.clone();
        normalize_decls(&mut decls);
        assert_eq!(decls, once);
    }
    #[test]
    fn global_initializer_operations_use_the_same_scoped_resolution() {
        let public = "array-repeat";
        let mut decls = vec![Decl::Global {
            name: "values".into(),
            typ: Typ::Array(Box::new(Typ::Int)),
            init: Some(Box::new(operation_call(
                public,
                vec![Expr::IntLit(42), Expr::IntLit(1)],
            ))),
            mutable: true,
            constant_value: false,
        }];
        normalize_decls(&mut decls);
        let Decl::Global {
            init: Some(value), ..
        } = &decls[0]
        else {
            panic!()
        };
        assert_operation_callee(value, crate::core_ir::ARRAY_REPEAT);
        let mut declared = vec![
            function(public, vec![], vec![Stmt::Return(Some(Expr::IntLit(42)))]),
            Decl::Global {
                name: "ordinary".into(),
                typ: Typ::Int,
                init: Some(Box::new(operation_call(public, vec![]))),
                mutable: true,
                constant_value: false,
            },
        ];
        normalize_decls(&mut declared);
        let Decl::Global {
            init: Some(value), ..
        } = &declared[1]
        else {
            panic!()
        };
        assert_operation_callee(value, public);
    }
}
