//! Give shadowing bindings distinct names before name-keyed optimization and lowering.
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

pub(crate) fn normalize_decls(decls: &mut [Decl]) {
    let names = module_names(decls);
    normalize_members(decls, &names);
}

fn normalize_members(decls: &mut [Decl], names: &HashSet<String>) {
    for decl in decls {
        match decl {
            Decl::Function { params, body, .. } => normalize_function(params, body, names),
            Decl::Class { methods, .. } => normalize_members(methods, names),
            _ => {}
        }
    }
}

pub(crate) fn normalize_function(
    params: &mut [(String, Typ)],
    body: &mut [Stmt],
    names: &HashSet<String>,
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
    let mut scope = HashMap::new();
    for (name, _) in params {
        bindings.bind(name, &mut scope);
    }
    bindings.body(body, &mut scope);
}

pub(crate) fn reserve_expr(expr: &Expr, names: &mut HashSet<String>) {
    match expr {
        Expr::Ident(name) => {
            names.insert(name.clone());
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

fn render_pattern(pattern: &MatchPattern) -> String {
    match pattern {
        MatchPattern::IntPat(value) => value.to_string(),
        MatchPattern::BoolPat(value) => value.to_string(),
        MatchPattern::StringPat(value) => format!("\"{value}\""),
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
            Expr::Unary { expr, .. } => self.expr(expr, scope),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs, scope);
                self.expr(rhs, scope);
            }
            Expr::Call { callee, args } => {
                self.expr(callee, scope);
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
