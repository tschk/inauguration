//! Shared Core IR desugaring before native lowering.

use crate::core_ir::{Decl, LoopKind, MatchPattern, Typ, UnifiedModule};
use crate::core_ir::{Expr, Stmt, for_each_expr_child};
use std::collections::{HashMap, HashSet};

#[path = "lower_core/constant_values.rs"]
mod constant_values;
#[path = "lower_core/static_initializers.rs"]
mod static_initializers;
#[path = "lower_core/value_spills.rs"]
mod value_spills;
pub(crate) use constant_values::lower_named_constants_with_semantics;
pub(crate) use static_initializers::{
    ConstantEvalError, evaluate_pure_constant, fold_static_initializers_with_semantics,
    lower_inline_constants_with_semantics, validate_constant_arithmetic_with_semantics,
};
#[path = "lower_core/match_guards.rs"]
mod match_guards;
pub(crate) use match_guards::{
    lower_guarded_decls, prepared_native_module, prepared_native_module_with_semantics,
    value_pattern_test,
};
pub(crate) use value_spills::lower_value_spills;

/// Flatten classes and rewrite method symbols without folding source facts.
pub(crate) fn desugar_class_declarations(module: &mut UnifiedModule) {
    let mut method_map: HashMap<String, String> = HashMap::new();
    let mut new_decls: Vec<Decl> = Vec::new();

    for decl in std::mem::take(&mut module.decls) {
        match decl {
            Decl::Class {
                name,
                fields,
                methods,
                ..
            } => {
                new_decls.push(Decl::Struct {
                    name: name.clone(),
                    fields: fields.clone(),
                    type_params: vec![],
                });
                for method in methods {
                    if let Decl::Function {
                        name: method_name,
                        params,
                        ret,
                        mut body,
                        ..
                    } = method
                    {
                        let mangled = format!("{}_{}", name, method_name);
                        method_map.insert(method_name.clone(), mangled.clone());
                        method_map.insert(format!("{}-{}", name, method_name), mangled.clone());
                        let mut new_params = vec![("self".to_string(), Typ::Named(name.clone()))];
                        new_params.extend(params);
                        // Bare field references inside methods mean `self.field`
                        // (Java/Kotlin-style implicit this). Rewrite them before
                        // verification, which would otherwise refuse the bare
                        // name as an unresolved symbol. Locals and params that
                        // shadow a field keep the bare name.
                        let field_names: HashSet<&str> =
                            fields.iter().map(|(n, _)| n.as_str()).collect();
                        rewrite_field_refs_in_body(&mut body, &field_names);
                        new_decls.push(Decl::Function {
                            name: mangled,
                            params: new_params,
                            ret,
                            body,
                            type_params: vec![],
                        });
                    }
                }
            }
            Decl::Interface { .. } => {}
            other => new_decls.push(other),
        }
    }

    // Expose method receivers and field types before spilling repeat operands.
    if !method_map.is_empty() {
        for decl in &mut new_decls {
            if let Decl::Function { body, .. } = decl {
                rewrite_method_calls_in_body(body, &method_map);
            }
        }
    }
    module.decls = new_decls;
}

pub fn desugar_module(module: &mut UnifiedModule) {
    desugar_module_with_semantics(module, crate::integer_semantics::IntegerSemantics::Word64);
}

pub fn desugar_module_with_semantics(
    module: &mut UnifiedModule,
    semantics: crate::integer_semantics::IntegerSemantics,
) {
    if crate::typecheck::rust_integers::has_metadata(module)
        || crate::core_ir::rust_arrays::has_metadata(module)
        || crate::core_ir::rust_floats::present(&module.decls)
        || crate::core_ir::type_proofs::metadata_present(
            &module.decls,
            crate::core_ir::type_proofs::MetadataKind::RustCallables,
        )
    {
        *module = crate::typecheck::normalize_module_with_semantics(
            crate::parser_registry::ParserId::Rust,
            module,
            semantics,
        );
    }
    crate::core_bindings::normalize_decls(&mut module.decls);
    lower_guarded_decls(&mut module.decls);
    desugar_class_declarations(module);
    let mut new_decls = std::mem::take(&mut module.decls);
    lower_inline_constants_with_semantics(&mut new_decls, semantics);
    static_initializers::fold_static_initializers_with_semantics(&mut new_decls, semantics);
    lower_named_constants_with_semantics(&mut new_decls, semantics);
    lower_value_spills(&mut new_decls);

    let mut for_counter = 0usize;
    let declared_names = crate::core_bindings::module_names(&new_decls);
    let mut for_names = declared_names.clone();
    for_names.extend(new_decls.iter().filter_map(|decl| match decl {
        Decl::Struct { name, .. } => Some(name.clone()),
        _ => None,
    }));
    for decl in &new_decls {
        if let Decl::Function { params, body, .. } = decl {
            for_names.extend(params.iter().map(|(name, _)| name.clone()));
            crate::core_bindings::reserve_body(body, &mut for_names);
        }
    }
    for decl in &mut new_decls {
        if let Decl::Function { params, body, .. } = decl {
            let mut bound = declared_names.clone();
            bound.extend(params.iter().map(|(name, _)| name.clone()));
            collect_bound_names_in_body(body, &mut bound);
            desugar_for_loops(body, &mut for_counter, &for_names, &bound);
        }
    }
    module.decls = new_decls;
    lower_closures(module);
}

/// Lift closure values with their source type facts still available to callers.
/// Existing generated declarations are reserved, so repeated lowering is safe.
pub(crate) fn lower_closures(module: &mut UnifiedModule) {
    let mut closure_counter = 0usize;
    let mut extra_decls = Vec::new();
    let declared_names = crate::core_bindings::module_names(&module.decls);
    let mut names = declared_names.clone();
    names.extend(module.decls.iter().filter_map(|decl| match decl {
        Decl::Struct { name, .. } => Some(name.clone()),
        _ => None,
    }));
    for decl in &module.decls {
        if let Decl::Function { params, body, .. } = decl {
            names.extend(params.iter().map(|(name, _)| name.clone()));
            crate::core_bindings::reserve_body(body, &mut names);
        }
    }
    for decl in &mut module.decls {
        if let Decl::Function { body, .. } = decl {
            desugar_closures_in_body(
                body,
                &mut closure_counter,
                &mut extra_decls,
                &names,
                &declared_names,
            );
        }
    }
    let closures = extra_decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, params, .. } => match params.first() {
                Some((receiver, Typ::Named(captures))) if receiver == "self" => {
                    Some((captures.clone(), name.clone()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    module.decls.extend(extra_decls);
    crate::typecheck::TypeChecker::new().resolve_closure_calls(module, &closures);
}

fn desugar_for_loops(
    body: &mut Vec<Stmt>,
    counter: &mut usize,
    names: &HashSet<String>,
    declared: &HashSet<String>,
) {
    let mut out = Vec::with_capacity(body.len());
    for mut stmt in std::mem::take(body) {
        crate::core_opt::map_stmt_exprs_mut(&mut stmt, &mut |expr| {
            if let Expr::Block { body, .. } = expr {
                desugar_for_loops(body, counter, names, declared);
            }
        });
        match stmt {
            Stmt::Loop {
                kind: LoopKind::For { binding },
                cond: Some(mut iter),
                body: mut loop_body,
            } => {
                desugar_for_loops(&mut loop_body, counter, names, declared);
                while ["__for_", "__for_end_", "__for_items_"]
                    .iter()
                    .any(|prefix| names.contains(&format!("{prefix}{counter}")))
                {
                    *counter += 1;
                }
                normalize_range_iterator(&mut iter, |name| declared.contains(name));
                out.extend(expand_for_loop(binding, iter, loop_body, counter));
            }
            Stmt::If {
                cond,
                mut then_body,
                mut else_body,
            } => {
                desugar_for_loops(&mut then_body, counter, names, declared);
                desugar_for_loops(&mut else_body, counter, names, declared);
                out.push(Stmt::If {
                    cond,
                    then_body,
                    else_body,
                });
            }
            Stmt::Loop {
                kind,
                cond,
                mut body,
            } => {
                desugar_for_loops(&mut body, counter, names, declared);
                out.push(Stmt::Loop { kind, cond, body });
            }
            Stmt::Match {
                scrutinee,
                mut arms,
            } => {
                for arm in &mut arms {
                    desugar_for_loops(&mut arm.body, counter, names, declared);
                }
                out.push(Stmt::Match { scrutinee, arms });
            }
            Stmt::Try {
                mut body,
                mut catches,
            } => {
                desugar_for_loops(&mut body, counter, names, declared);
                for c in &mut catches {
                    desugar_for_loops(&mut c.body, counter, names, declared);
                }
                out.push(Stmt::Try { body, catches });
            }
            other => out.push(other),
        }
    }
    *body = out;
}

/// Legacy Core IR's `range` call denotes a builtin only when the name is unbound.
pub(crate) fn normalize_range_iterator(iter: &mut Expr, is_bound: impl Fn(&str) -> bool) {
    if let Expr::Call { callee, args } = iter
        && args.len() == 2
        && let Expr::Ident(name) = callee.as_mut()
        && name == "range"
        && !is_bound(name)
    {
        *name = "range-exclusive".into();
    }
}

pub(crate) fn expand_for_loop(
    binding: String,
    iter: Expr,
    mut loop_body: Vec<Stmt>,
    counter: &mut usize,
) -> Vec<Stmt> {
    let binding = binding.trim().to_string();
    if let Expr::Call { callee, args } = &iter {
        let open =
            matches!(callee.as_ref(), Expr::Ident(name) if name == "range-from") && args.len() == 1;
        if open
            || (matches!(callee.as_ref(), Expr::Ident(name) if name == "range-exclusive" || name == "range-inclusive")
                && args.len() == 2)
        {
            let inclusive =
                matches!(callee.as_ref(), Expr::Ident(name) if name == "range-inclusive");
            let index = format!("__for_{counter}");
            let end = format!("__for_end_{counter}");
            *counter += 1;
            let advance = Stmt::Assign(
                index.clone(),
                Expr::Binary {
                    op: "+".into(),
                    lhs: Box::new(Expr::Ident(index.clone())),
                    rhs: Box::new(Expr::IntLit(1)),
                },
            );
            if open {
                // The iterator advances before yielding, independently of binding mutation
                // and continue paths. Int arithmetic follows the native wrapping profile.
                loop_body.insert(0, advance);
                loop_body.insert(
                    0,
                    Stmt::Let(binding, Some(Typ::Int), Expr::Ident(index.clone())),
                );
                return vec![
                    Stmt::Let(index, Some(Typ::Int), args[0].clone()),
                    Stmt::Loop {
                        kind: LoopKind::Infinite,
                        cond: None,
                        body: loop_body,
                    },
                ];
            }
            // Stop before incrementing the inclusive endpoint, including Int::MAX.
            // The same advance also runs before continues targeting this loop.
            let advance = if inclusive {
                Stmt::If {
                    cond: Expr::Binary {
                        op: "==".into(),
                        lhs: Box::new(Expr::Ident(index.clone())),
                        rhs: Box::new(Expr::Ident(end.clone())),
                    },
                    then_body: vec![Stmt::Break],
                    else_body: vec![advance],
                }
            } else {
                advance
            };
            loop_body.insert(
                0,
                Stmt::Let(binding, Some(Typ::Int), Expr::Ident(index.clone())),
            );
            advance_for_continues(&mut loop_body, &advance);
            loop_body.push(advance);
            return vec![
                Stmt::Let(index.clone(), Some(Typ::Int), args[0].clone()),
                Stmt::Let(end.clone(), Some(Typ::Int), args[1].clone()),
                Stmt::Loop {
                    kind: LoopKind::While,
                    cond: Some(Expr::Binary {
                        op: if inclusive { "<=" } else { "<" }.into(),
                        lhs: Box::new(Expr::Ident(index)),
                        rhs: Box::new(Expr::Ident(end)),
                    }),
                    body: loop_body,
                },
            ];
        }
    }

    let idx = format!("__for_{counter}");
    let collection = format!("__for_items_{counter}");
    *counter += 1;
    let len_expr = Expr::Call {
        callee: Box::new(Expr::Ident("array-len".into())),
        args: vec![Expr::Ident(collection.clone())],
    };
    let mut body = vec![Stmt::Let(
        binding,
        None,
        Expr::Index {
            base: Box::new(Expr::Ident(collection.clone())),
            index: Box::new(Expr::Ident(idx.clone())),
        },
    )];
    body.append(&mut loop_body);
    let advance = Stmt::Assign(
        idx.clone(),
        Expr::Binary {
            op: "+".into(),
            lhs: Box::new(Expr::Ident(idx.clone())),
            rhs: Box::new(Expr::IntLit(1)),
        },
    );
    advance_for_continues(&mut body, &advance);
    body.push(advance);
    vec![
        Stmt::Let(collection, None, iter),
        Stmt::Let(idx.clone(), Some(Typ::Int), Expr::IntLit(0)),
        Stmt::Loop {
            kind: LoopKind::While,
            cond: Some(Expr::Binary {
                op: "<".into(),
                lhs: Box::new(Expr::Ident(idx)),
                rhs: Box::new(len_expr),
            }),
            body,
        },
    ]
}

// Nested-loop continues must not advance the enclosing for loop.
pub(crate) fn advance_for_continues(body: &mut Vec<Stmt>, advance: &Stmt) {
    let mut rewritten = Vec::with_capacity(body.len());
    for mut stmt in std::mem::take(body) {
        advance_expr_continues_in_stmt(&mut stmt, advance);
        match &mut stmt {
            Stmt::Continue => rewritten.push(advance.clone()),
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                advance_for_continues(then_body, advance);
                advance_for_continues(else_body, advance);
            }
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    advance_for_continues(&mut arm.body, advance);
                }
            }
            Stmt::Try { body, catches } => {
                advance_for_continues(body, advance);
                for catch in catches {
                    advance_for_continues(&mut catch.body, advance);
                }
            }
            _ => {}
        }
        rewritten.push(stmt);
    }
    *body = rewritten;
}

fn advance_expr_continues_in_stmt(stmt: &mut Stmt, advance: &Stmt) {
    if matches!(stmt, Stmt::Loop { .. }) {
        return;
    }
    fn expr(expr: &mut Expr, advance: &Stmt) {
        if let Expr::Closure { .. } = expr {
            return;
        }
        if let Expr::Block { body, .. } = expr {
            advance_for_continues(body, advance);
        }
        crate::core_ir::for_each_expr_child_mut(expr, &mut |child| expr_walk(child, advance));
    }
    fn expr_walk(value: &mut Expr, advance: &Stmt) {
        expr(value, advance);
    }
    match stmt {
        Stmt::Let(_, _, e)
        | Stmt::Assign(_, e)
        | Stmt::Expr(e)
        | Stmt::Throw(e)
        | Stmt::Return(Some(e)) => expr(e, advance),
        Stmt::If { cond, .. } => expr(cond, advance),
        Stmt::Match { scrutinee, arms } => {
            expr(scrutinee, advance);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    expr(guard, advance);
                }
            }
        }
        Stmt::FieldAssign { base, value, .. } => {
            expr(base, advance);
            expr(value, advance);
        }
        Stmt::IndexAssign { base, index, value } => {
            expr(base, advance);
            expr(index, advance);
            expr(value, advance);
        }
        _ => {}
    }
}

/// Rewrite bare field reads and writes inside a class method body to
/// `self.field` (implicit this). `fields` are the enclosing class's field
/// names; names bound by locals, params, or closures shadow fields and are
/// left untouched.
fn rewrite_field_refs_in_body(body: &mut [Stmt], fields: &HashSet<&str>) {
    let mut bound = HashSet::new();
    collect_bound_names_in_body(body, &mut bound);
    for stmt in body {
        rewrite_field_refs_in_stmt(stmt, fields, &bound);
    }
}

fn collect_bound_names_in_body(body: &[Stmt], bound: &mut HashSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::Declare(name, _) => {
                bound.insert(name.clone());
            }
            Stmt::Let(name, _, e) => {
                collect_bound_names_in_expr(e, bound);
                bound.insert(name.clone());
            }
            Stmt::Assign(_, e) | Stmt::Throw(e) | Stmt::Return(Some(e)) | Stmt::Expr(e) => {
                collect_bound_names_in_expr(e, bound)
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                collect_bound_names_in_expr(base, bound);
                collect_bound_names_in_expr(index, bound);
                collect_bound_names_in_expr(value, bound);
            }
            Stmt::FieldAssign { base, value, .. } => {
                collect_bound_names_in_expr(base, bound);
                collect_bound_names_in_expr(value, bound);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                collect_bound_names_in_expr(cond, bound);
                collect_bound_names_in_body(then_body, bound);
                collect_bound_names_in_body(else_body, bound);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(c) = cond {
                    collect_bound_names_in_expr(c, bound);
                }
                collect_bound_names_in_body(body, bound);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                collect_bound_names_in_expr(scrutinee, bound);
                for arm in arms {
                    collect_bound_names_in_body(&arm.body, bound);
                }
            }
            Stmt::Try { body, catches } => {
                collect_bound_names_in_body(body, bound);
                for catch in catches {
                    collect_bound_names_in_body(&catch.body, bound);
                }
            }
            Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
        }
    }
}

fn collect_bound_names_in_expr(expr: &Expr, bound: &mut HashSet<String>) {
    match expr {
        Expr::Block { body, result } => {
            collect_bound_names_in_body(body, bound);
            if let Some(result) = result {
                collect_bound_names_in_expr(result, bound);
            }
        }
        Expr::Closure { params, body, .. } => {
            for (p, _) in params {
                bound.insert(p.clone());
            }
            collect_bound_names_in_body(body, bound);
        }
        _ => {
            for_each_expr_child(expr, &mut |child| collect_bound_names_in_expr(child, bound));
        }
    }
}

fn rewrite_field_refs_in_stmt(stmt: &mut Stmt, fields: &HashSet<&str>, bound: &HashSet<String>) {
    match stmt {
        Stmt::Declare(_, _) => {}
        Stmt::Let(_, _, e) => rewrite_field_refs_in_expr(e, fields, bound),
        Stmt::Assign(name, e) => {
            rewrite_field_refs_in_expr(e, fields, bound);
            if fields.contains(&name.as_str()) && !bound.contains(name) {
                let value = std::mem::replace(e, Expr::IntLit(0));
                let field = std::mem::take(name);
                *stmt = Stmt::FieldAssign {
                    base: Expr::Ident("self".into()),
                    name: field,
                    value,
                };
            }
        }
        Stmt::Throw(e) | Stmt::Return(Some(e)) | Stmt::Expr(e) => {
            rewrite_field_refs_in_expr(e, fields, bound)
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            rewrite_field_refs_in_expr(base, fields, bound);
            rewrite_field_refs_in_expr(index, fields, bound);
            rewrite_field_refs_in_expr(value, fields, bound);
        }
        Stmt::FieldAssign { base, name, value } => {
            rewrite_field_refs_in_expr(base, fields, bound);
            rewrite_field_refs_in_expr(value, fields, bound);
            // Field names in an explicit `base.name` write are not idents.
            let _ = name;
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            rewrite_field_refs_in_expr(cond, fields, bound);
            for s in then_body {
                rewrite_field_refs_in_stmt(s, fields, bound);
            }
            for s in else_body {
                rewrite_field_refs_in_stmt(s, fields, bound);
            }
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(c) = cond {
                rewrite_field_refs_in_expr(c, fields, bound);
            }
            for s in body {
                rewrite_field_refs_in_stmt(s, fields, bound);
            }
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            rewrite_field_refs_in_expr(scrutinee, fields, bound);
            for arm in arms {
                for s in &mut arm.body {
                    rewrite_field_refs_in_stmt(s, fields, bound);
                }
            }
        }
        Stmt::Try { body, catches } => {
            for s in body {
                rewrite_field_refs_in_stmt(s, fields, bound);
            }
            for catch in catches {
                for s in &mut catch.body {
                    rewrite_field_refs_in_stmt(s, fields, bound);
                }
            }
        }
        Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
    }
}

fn rewrite_field_refs_in_expr(expr: &mut Expr, fields: &HashSet<&str>, bound: &HashSet<String>) {
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            rewrite_field_refs_in_expr(scrutinee, fields, bound);
            for arm in arms {
                let mut scope = bound.clone();
                crate::core_bindings::reserve_pattern(&arm.pattern, &mut scope);
                if let Some(guard) = &mut arm.guard {
                    rewrite_field_refs_in_expr(guard, fields, &scope);
                }
                rewrite_field_refs_in_expr(&mut arm.value, fields, &scope);
            }
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            rewrite_field_refs_in_expr(cond, fields, bound);
            rewrite_field_refs_in_expr(then_expr, fields, bound);
            rewrite_field_refs_in_expr(else_expr, fields, bound);
        }
        Expr::Block { body, result } => {
            let mut inner = bound.clone();
            collect_bound_names_in_body(body, &mut inner);
            for stmt in body {
                rewrite_field_refs_in_stmt(stmt, fields, &inner);
            }
            if let Some(result) = result {
                rewrite_field_refs_in_expr(result, fields, &inner);
            }
        }
        Expr::Ident(n) => {
            if fields.contains(&n.as_str()) && !bound.contains(n) {
                let field = std::mem::take(n);
                *expr = Expr::Field {
                    base: Box::new(Expr::Ident("self".into())),
                    name: field,
                };
            }
        }
        Expr::Call { args, .. } => {
            // The callee is left alone: bare method names resolve through the
            // method map, and a field name is never a callable here.
            for a in args {
                rewrite_field_refs_in_expr(a, fields, bound);
            }
        }
        Expr::Closure { body, .. } => {
            // Bound names were collected including closure params, so nested
            // shadowing is already respected.
            for s in body {
                rewrite_field_refs_in_stmt(s, fields, bound);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_field_refs_in_expr(lhs, fields, bound);
            rewrite_field_refs_in_expr(rhs, fields, bound);
        }
        Expr::Unary { expr: inner, .. } => rewrite_field_refs_in_expr(inner, fields, bound),
        Expr::Field { base, .. } => rewrite_field_refs_in_expr(base, fields, bound),
        Expr::Index { base, index, .. } => {
            rewrite_field_refs_in_expr(base, fields, bound);
            rewrite_field_refs_in_expr(index, fields, bound);
        }
        Expr::StructInit { fields: init, .. } => {
            for (_, fe) in init {
                rewrite_field_refs_in_expr(fe, fields, bound);
            }
        }
        Expr::ArrayLit(items) => {
            for item in items {
                rewrite_field_refs_in_expr(item, fields, bound);
            }
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_) => {}
    }
}

fn desugar_closures_in_body(
    body: &mut [Stmt],
    counter: &mut usize,
    extra_decls: &mut Vec<Decl>,
    names: &HashSet<String>,
    globals: &HashSet<String>,
) {
    for stmt in body {
        match stmt {
            Stmt::Declare(_, _) => {}
            Stmt::Let(_, _, e)
            | Stmt::Assign(_, e)
            | Stmt::FieldAssign { value: e, .. }
            | Stmt::Return(Some(e))
            | Stmt::Expr(e) => {
                desugar_closures_in_expr(e, counter, extra_decls, names, globals);
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                desugar_closures_in_expr(base, counter, extra_decls, names, globals);
                desugar_closures_in_expr(index, counter, extra_decls, names, globals);
                desugar_closures_in_expr(value, counter, extra_decls, names, globals);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                desugar_closures_in_expr(cond, counter, extra_decls, names, globals);
                desugar_closures_in_body(then_body, counter, extra_decls, names, globals);
                desugar_closures_in_body(else_body, counter, extra_decls, names, globals);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(c) = cond {
                    desugar_closures_in_expr(c, counter, extra_decls, names, globals);
                }
                desugar_closures_in_body(body, counter, extra_decls, names, globals);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                desugar_closures_in_expr(scrutinee, counter, extra_decls, names, globals);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        desugar_closures_in_expr(guard, counter, extra_decls, names, globals);
                    }
                    desugar_closures_in_body(&mut arm.body, counter, extra_decls, names, globals);
                }
            }
            Stmt::Return(None) => {}
            Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
            Stmt::Throw(e) => {
                desugar_closures_in_expr(e, counter, extra_decls, names, globals);
            }
            Stmt::Try { body, catches, .. } => {
                desugar_closures_in_body(body, counter, extra_decls, names, globals);
                for catch in catches {
                    desugar_closures_in_body(&mut catch.body, counter, extra_decls, names, globals);
                }
            }
        }
    }
}

fn collect_declared_vars_in_body(body: &[Stmt], out: &mut HashSet<String>) {
    for stmt in body {
        let mut copied = stmt.clone();
        crate::core_opt::map_stmt_exprs_mut(&mut copied, &mut |expr| {
            if let Expr::Block { body, .. } = expr {
                collect_declared_vars_in_body(body, out);
            }
            if let Expr::MatchValue { arms, .. } = expr {
                for arm in arms {
                    crate::core_bindings::reserve_pattern(&arm.pattern, out);
                }
            }
        });
        match stmt {
            Stmt::Declare(name, _) | Stmt::Let(name, _, _) => {
                out.insert(name.clone());
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                collect_declared_vars_in_body(then_body, out);
                collect_declared_vars_in_body(else_body, out);
            }
            Stmt::Loop { body, .. } => {
                collect_declared_vars_in_body(body, out);
            }
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    crate::core_bindings::reserve_pattern(&arm.pattern, out);
                    collect_declared_vars_in_body(&arm.body, out);
                }
            }
            Stmt::Try { body, catches, .. } => {
                collect_declared_vars_in_body(body, out);
                for catch in catches {
                    crate::core_bindings::reserve_pattern(&catch.pattern, out);
                    collect_declared_vars_in_body(&catch.body, out);
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn collect_free_vars(body: &[Stmt], params: &[(String, Typ)]) -> Vec<String> {
    let mut reads = HashSet::new();
    collect_body_reads(body, &mut reads);

    let mut declared: HashSet<String> = HashSet::new();
    collect_declared_vars_in_body(body, &mut declared);

    let mut param_refs = HashSet::with_capacity(params.len());
    for (pname, _) in params {
        param_refs.insert(pname.as_str());
    }

    let mut captures = Vec::new();
    for read in reads {
        if !declared.contains(read.as_str()) && !param_refs.contains(read.as_str()) {
            captures.push(read);
        }
    }

    captures.sort();
    captures
}

fn rewrite_captures_in_body(body: &mut [Stmt], captures: &HashSet<&str>) {
    for stmt in body {
        if let Stmt::Assign(name, value) = stmt
            && captures.contains(name.as_str())
        {
            // Captured stores must name the same environment place as captured reads.
            rewrite_captures_in_expr(value, captures);
            *stmt = Stmt::FieldAssign {
                base: Expr::Ident("self".into()),
                name: name.clone(),
                value: std::mem::replace(value, Expr::UnitLit),
            };
            continue;
        }
        match stmt {
            Stmt::Declare(_, _) => {}
            Stmt::Let(_, _, e) | Stmt::Assign(_, e) | Stmt::Return(Some(e)) | Stmt::Expr(e) => {
                rewrite_captures_in_expr(e, captures);
            }
            Stmt::FieldAssign { base, value, .. } => {
                rewrite_captures_in_expr(base, captures);
                rewrite_captures_in_expr(value, captures);
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                rewrite_captures_in_expr(base, captures);
                rewrite_captures_in_expr(index, captures);
                rewrite_captures_in_expr(value, captures);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                rewrite_captures_in_expr(cond, captures);
                rewrite_captures_in_body(then_body, captures);
                rewrite_captures_in_body(else_body, captures);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(c) = cond {
                    rewrite_captures_in_expr(c, captures);
                }
                rewrite_captures_in_body(body, captures);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                rewrite_captures_in_expr(scrutinee, captures);
                for arm in arms {
                    let mut names = HashSet::new();
                    crate::core_bindings::reserve_pattern(&arm.pattern, &mut names);
                    let inner = captures
                        .iter()
                        .copied()
                        .filter(|name| !names.contains(*name))
                        .collect();
                    if let Some(guard) = &mut arm.guard {
                        rewrite_captures_in_expr(guard, &inner);
                    }
                    rewrite_captures_in_body(&mut arm.body, &inner);
                }
            }
            Stmt::Return(None) => {}
            Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
            Stmt::Throw(e) => {
                rewrite_captures_in_expr(e, captures);
            }
            Stmt::Try { body, catches, .. } => {
                rewrite_captures_in_body(body, captures);
                for catch in catches {
                    rewrite_captures_in_body(&mut catch.body, captures);
                }
            }
        }
    }
}

fn rewrite_captures_in_expr(expr: &mut Expr, captures: &HashSet<&str>) {
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            rewrite_captures_in_expr(scrutinee, captures);
            for arm in arms {
                let mut names = HashSet::new();
                crate::core_bindings::reserve_pattern(&arm.pattern, &mut names);
                let inner = captures
                    .iter()
                    .copied()
                    .filter(|name| !names.contains(*name))
                    .collect();
                if let Some(guard) = &mut arm.guard {
                    rewrite_captures_in_expr(guard, &inner);
                }
                rewrite_captures_in_expr(&mut arm.value, &inner);
            }
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            rewrite_captures_in_expr(cond, captures);
            rewrite_captures_in_expr(then_expr, captures);
            rewrite_captures_in_expr(else_expr, captures);
        }
        Expr::Block { body, result } => {
            rewrite_captures_in_body(body, captures);
            if let Some(result) = result {
                rewrite_captures_in_expr(result, captures);
            }
        }
        Expr::Ident(name) if captures.contains(name.as_str()) => {
            *expr = Expr::Field {
                base: Box::new(Expr::Ident("self".into())),
                name: std::mem::take(name),
            };
        }
        Expr::Unary { expr: inner, .. } => rewrite_captures_in_expr(inner, captures),
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_captures_in_expr(lhs, captures);
            rewrite_captures_in_expr(rhs, captures);
        }
        Expr::StructInit { fields, .. } => {
            for (_, field_expr) in fields {
                rewrite_captures_in_expr(field_expr, captures);
            }
        }
        Expr::Field { base, .. } => rewrite_captures_in_expr(base, captures),
        Expr::ArrayLit(items) => {
            for item in items {
                rewrite_captures_in_expr(item, captures);
            }
        }
        Expr::Index { base, index, .. } => {
            rewrite_captures_in_expr(base, captures);
            rewrite_captures_in_expr(index, captures);
        }
        Expr::Call { callee, args, .. } => {
            rewrite_captures_in_expr(callee, captures);
            for arg in args {
                rewrite_captures_in_expr(arg, captures);
            }
        }
        Expr::UnitLit
        | Expr::Ident(_)
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_) => {}
        Expr::Closure { .. } => {}
    }
}

fn desugar_closures_in_expr(
    expr: &mut Expr,
    counter: &mut usize,
    extra_decls: &mut Vec<Decl>,
    names: &HashSet<String>,
    globals: &HashSet<String>,
) {
    match expr {
        Expr::MatchValue { .. } => crate::core_ir::for_each_expr_child_mut(expr, &mut |child| {
            desugar_closures_in_expr(child, counter, extra_decls, names, globals)
        }),
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            desugar_closures_in_expr(cond, counter, extra_decls, names, globals);
            desugar_closures_in_expr(then_expr, counter, extra_decls, names, globals);
            desugar_closures_in_expr(else_expr, counter, extra_decls, names, globals);
        }
        Expr::Block { body, result } => {
            desugar_closures_in_body(body, counter, extra_decls, names, globals);
            if let Some(result) = result {
                desugar_closures_in_expr(result, counter, extra_decls, names, globals);
            }
        }
        Expr::Closure {
            params, ret, body, ..
        } => {
            crate::core_bindings::normalize_function(
                params,
                body,
                &HashSet::from(["self".to_string()]),
            );
            desugar_closures_in_body(body, counter, extra_decls, names, globals);
            while names.contains(&format!("__closure_{counter}"))
                || names.contains(&format!("__closure_{counter}_captures"))
            {
                *counter += 1;
            }
            let id = *counter;
            *counter += 1;
            let fn_name = format!("__closure_{id}");
            let caps_name = format!("{fn_name}_captures");
            let closure_params = params.clone();
            // shortcut: globals named self need distinct receiver identity before capture-free dispatch.
            let captures: Vec<_> = collect_free_vars(body, &closure_params)
                .into_iter()
                .filter(|name| name == "self" || !globals.contains(name))
                .collect();
            let captures_set: HashSet<&str> = captures.iter().map(|s| s.as_str()).collect();
            rewrite_captures_in_body(body, &captures_set);
            let mut caps_fields = Vec::with_capacity(captures.len());
            let mut init_fields = Vec::with_capacity(captures.len());
            for c in captures.into_iter() {
                caps_fields.push((c.clone(), Typ::Named("Any".into())));
                init_fields.push((c.clone(), Expr::Ident(c)));
            }
            let mut fn_params = vec![("self".to_string(), Typ::Named(caps_name.clone()))];
            fn_params.extend(std::mem::take(params));
            let closure_ret = std::mem::replace(ret, Typ::Void);
            let closure_body = std::mem::take(body);
            extra_decls.push(Decl::Struct {
                name: caps_name.clone(),
                fields: caps_fields,
                type_params: vec![],
            });
            extra_decls.push(Decl::Function {
                name: fn_name,
                params: fn_params,
                ret: closure_ret,
                body: closure_body,
                type_params: vec![],
            });
            *expr = Expr::StructInit {
                name: caps_name,
                fields: init_fields,
            };
        }
        Expr::Unary { expr: inner, .. } => {
            desugar_closures_in_expr(inner, counter, extra_decls, names, globals)
        }
        Expr::Binary { lhs, rhs, .. } => {
            desugar_closures_in_expr(lhs, counter, extra_decls, names, globals);
            desugar_closures_in_expr(rhs, counter, extra_decls, names, globals);
        }
        Expr::StructInit { fields, .. } => {
            for (_, field_expr) in fields {
                desugar_closures_in_expr(field_expr, counter, extra_decls, names, globals);
            }
        }
        Expr::Field { base, .. } => {
            desugar_closures_in_expr(base, counter, extra_decls, names, globals);
        }
        Expr::ArrayLit(items) => {
            for item in items {
                desugar_closures_in_expr(item, counter, extra_decls, names, globals);
            }
        }
        Expr::Index { base, index, .. } => {
            desugar_closures_in_expr(base, counter, extra_decls, names, globals);
            desugar_closures_in_expr(index, counter, extra_decls, names, globals);
        }
        Expr::Call { callee, args, .. } => {
            desugar_closures_in_expr(callee, counter, extra_decls, names, globals);
            let preserves_map_closure =
                matches!(callee.as_ref(), Expr::Ident(name) if name == "map") && args.len() == 2;
            for (index, arg) in args.iter_mut().enumerate() {
                if preserves_map_closure && index == 1 && matches!(arg, Expr::Closure { .. }) {
                    continue;
                }
                desugar_closures_in_expr(arg, counter, extra_decls, names, globals);
            }
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_) => {}
    }
}

fn rewrite_method_calls_in_body(body: &mut [Stmt], method_map: &HashMap<String, String>) {
    for stmt in body {
        match stmt {
            Stmt::Declare(_, _) => {}
            Stmt::Let(_, _, e)
            | Stmt::Assign(_, e)
            | Stmt::FieldAssign { value: e, .. }
            | Stmt::Return(Some(e))
            | Stmt::Expr(e) => {
                rewrite_method_calls_in_expr(e, method_map);
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                rewrite_method_calls_in_expr(base, method_map);
                rewrite_method_calls_in_expr(index, method_map);
                rewrite_method_calls_in_expr(value, method_map);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                rewrite_method_calls_in_expr(cond, method_map);
                rewrite_method_calls_in_body(then_body, method_map);
                rewrite_method_calls_in_body(else_body, method_map);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(c) = cond {
                    rewrite_method_calls_in_expr(c, method_map);
                }
                rewrite_method_calls_in_body(body, method_map);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                rewrite_method_calls_in_expr(scrutinee, method_map);
                for arm in arms {
                    rewrite_method_calls_in_body(&mut arm.body, method_map);
                }
            }
            Stmt::Return(None) => {}
            Stmt::Throw(e) => {
                rewrite_method_calls_in_expr(e, method_map);
            }
            Stmt::Try { body, catches, .. } => {
                rewrite_method_calls_in_body(body, method_map);
                for catch in catches {
                    rewrite_method_calls_in_body(&mut catch.body, method_map);
                }
            }
            Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
        }
    }
}

fn rewrite_method_calls_in_expr(expr: &mut Expr, method_map: &HashMap<String, String>) {
    match expr {
        Expr::MatchValue { .. } => crate::core_ir::for_each_expr_child_mut(expr, &mut |child| {
            rewrite_method_calls_in_expr(child, method_map)
        }),
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            rewrite_method_calls_in_expr(cond, method_map);
            rewrite_method_calls_in_expr(then_expr, method_map);
            rewrite_method_calls_in_expr(else_expr, method_map);
        }
        Expr::Block { body, result } => {
            rewrite_method_calls_in_body(body, method_map);
            if let Some(result) = result {
                rewrite_method_calls_in_expr(result, method_map);
            }
        }
        Expr::Call { callee, args, .. } => {
            for arg in args.iter_mut() {
                rewrite_method_calls_in_expr(arg, method_map);
            }
            if let Expr::Field { base, name, .. } = callee.as_mut() {
                if let Some(mangled) = method_map.get(name) {
                    let new_args: Vec<Expr> =
                        std::iter::once(*std::mem::replace(base, Box::new(Expr::IntLit(0))))
                            .chain(args.drain(..))
                            .collect();
                    **callee = Expr::Ident(mangled.clone());
                    *args = new_args;
                } else {
                    rewrite_method_calls_in_expr(base, method_map);
                }
            } else if let Expr::Ident(name) = callee.as_mut() {
                if name.contains('-') {
                    if let Some(mangled) = method_map.get(name.as_str()) {
                        **callee = Expr::Ident(mangled.clone());
                    }
                } else if let Some(method) = name.strip_prefix("__method__") {
                    if let Some(mangled) = method_map.get(method) {
                        **callee = Expr::Ident(mangled.clone());
                    }
                }
            } else {
                rewrite_method_calls_in_expr(callee, method_map);
            }
        }
        Expr::Unary { expr: inner, .. } => {
            rewrite_method_calls_in_expr(inner, method_map);
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_method_calls_in_expr(lhs, method_map);
            rewrite_method_calls_in_expr(rhs, method_map);
        }
        Expr::StructInit { fields, .. } => {
            for (_, field_expr) in fields {
                rewrite_method_calls_in_expr(field_expr, method_map);
            }
        }
        Expr::Field { base, .. } => {
            rewrite_method_calls_in_expr(base, method_map);
        }
        Expr::ArrayLit(items) => {
            for item in items {
                rewrite_method_calls_in_expr(item, method_map);
            }
        }
        Expr::Index { base, index, .. } => {
            rewrite_method_calls_in_expr(base, method_map);
            rewrite_method_calls_in_expr(index, method_map);
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_) => {}
        Expr::Closure { body, .. } => {
            rewrite_method_calls_in_body(body, method_map);
        }
    }
}

fn lower_int_lit(n: i64, ssa: &mut usize, out: &mut String) -> usize {
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, {n}\n"));
    id
}

fn lower_float_lit(f: &f64, ssa: &mut usize, out: &mut String) -> usize {
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = float_literal $Builtin.FPIEEE64, {}\n", f));
    id
}

fn lower_bool_lit(b: bool, ssa: &mut usize, out: &mut String) -> usize {
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = bool_literal {b}\n"));
    id
}

fn lower_string_lit(s: &str, ssa: &mut usize, out: &mut String) -> usize {
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = string_literal {s:?}\n"));
    id
}

fn lower_ident(
    name: &str,
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    if direct_env.contains(name) {
        if let Some(id) = env.get(name) {
            return *id;
        }
    }
    if env.contains_key(name) {
        let id = *ssa;
        *ssa += 1;
        out.push_str(&format!("%{id} = load_var {name}\n"));
        return id;
    }
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, 0\n"));
    id
}

fn lower_unary(
    op: &str,
    expr: &Expr,
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    if let Some(n) = fold_unary_int(op, expr) {
        return lower_int_lit(n, ssa, out);
    }
    if let Some(b) = fold_unary_bool(op, expr) {
        return lower_bool_lit(b, ssa, out);
    }
    let arg = lower_expr(expr, env, direct_env, ssa, out);
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = builtin_unop {op:?} %{arg}\n"));
    id
}

fn lower_binary(
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    if let Some(n) = fold_int_binop(op, lhs, rhs) {
        return lower_int_lit(n, ssa, out);
    }
    if let Some(b) = fold_bool_binop(op, lhs, rhs) {
        return lower_bool_lit(b, ssa, out);
    }
    let lhs_id = lower_expr(lhs, env, direct_env, ssa, out);
    let rhs_id = lower_expr(rhs, env, direct_env, ssa, out);
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!(
        "%{id} = builtin_binop {op:?} %{lhs_id}, %{rhs_id}\n"
    ));
    id
}

fn lower_struct_init(
    name: &str,
    fields: &[(String, Expr)],
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    let mut rendered_fields = Vec::new();
    for (field, expr) in fields {
        let value_id = lower_expr(expr, env, direct_env, ssa, out);
        rendered_fields.push(format!("{field}:%{value_id}"));
    }
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!(
        "%{id} = struct_init {name} {}\n",
        rendered_fields.join(", ")
    ));
    id
}

fn lower_field_access(
    base: &Expr,
    name: &str,
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    let base_id = lower_expr(base, env, direct_env, ssa, out);
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = field_access %{base_id} {name}\n"));
    id
}

fn lower_array_lit(
    items: &[Expr],
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    let mut item_ids = Vec::new();
    for item in items {
        item_ids.push(lower_expr(item, env, direct_env, ssa, out));
    }
    let id = *ssa;
    *ssa += 1;
    let rendered_items = item_ids
        .iter()
        .map(|id| format!("%{id}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("%{id} = array_init {rendered_items}\n"));
    id
}

fn lower_index_access(
    base: &Expr,
    index: &Expr,
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    let base_id = lower_expr(base, env, direct_env, ssa, out);
    let index_id = lower_expr(index, env, direct_env, ssa, out);
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = index_access %{base_id}, %{index_id}\n"));
    id
}

fn lower_call(
    callee: &Expr,
    args: &[Expr],
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    let mut arg_ids = Vec::new();
    if let Expr::Ident(name) = callee {
        let r = *ssa;
        *ssa += 1;
        out.push_str(&format!(
            "%{r} = function_ref @{name} : $@convention(thin)\n"
        ));
        for arg in args {
            arg_ids.push(lower_expr(arg, env, direct_env, ssa, out));
        }
        let id = *ssa;
        *ssa += 1;
        let rendered_args = arg_ids
            .iter()
            .map(|id| format!("%{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "%{id} = apply %{r}({rendered_args}) : $@convention(thin)\n"
        ));
        id
    } else {
        let _ = lower_expr(callee, env, direct_env, ssa, out);
        for arg in args {
            let _ = lower_expr(arg, env, direct_env, ssa, out);
        }
        let id = *ssa;
        *ssa += 1;
        out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, 0\n"));
        id
    }
}

fn lower_closure(ssa: &mut usize, out: &mut String) -> usize {
    let id = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, 0\n"));
    id
}

fn lower_expr(
    e: &Expr,
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    match e {
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. } => {
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = unsupported_structured_value\n"));
            id
        }
        Expr::UnitLit => {
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = tuple ()\n"));
            id
        }
        Expr::IntLit(n) => lower_int_lit(*n, ssa, out),
        Expr::FloatLit(f) => lower_float_lit(&f.0, ssa, out),
        Expr::BoolLit(b) => lower_bool_lit(*b, ssa, out),
        Expr::StringLit(s) => lower_string_lit(s, ssa, out),
        Expr::Ident(name) => lower_ident(name, env, direct_env, ssa, out),
        Expr::Unary { op, expr, .. } => lower_unary(op, expr, env, direct_env, ssa, out),
        Expr::Binary { op, lhs, rhs, .. } => lower_binary(op, lhs, rhs, env, direct_env, ssa, out),
        Expr::StructInit { name, fields, .. } => {
            lower_struct_init(name, fields, env, direct_env, ssa, out)
        }
        Expr::Field { base, name, .. } => lower_field_access(base, name, env, direct_env, ssa, out),
        Expr::ArrayLit(items) => lower_array_lit(items, env, direct_env, ssa, out),
        Expr::Index { base, index, .. } => {
            lower_index_access(base, index, env, direct_env, ssa, out)
        }
        Expr::Call { callee, args, .. } => lower_call(callee, args, env, direct_env, ssa, out),
        Expr::Closure { .. } => lower_closure(ssa, out),
    }
}
fn const_int(e: &Expr) -> Option<i64> {
    match e {
        Expr::IntLit(n) => Some(*n),
        Expr::Unary { op, expr, .. } => fold_unary_int(op, expr),
        Expr::Binary { op, lhs, rhs, .. } => fold_int_binop(op, lhs, rhs),
        _ => None,
    }
}

fn const_bool(e: &Expr) -> Option<bool> {
    match e {
        Expr::BoolLit(b) => Some(*b),
        Expr::Unary { op, expr, .. } => fold_unary_bool(op, expr),
        Expr::Binary { op, lhs, rhs, .. } => fold_bool_binop(op, lhs, rhs),
        _ => None,
    }
}

fn fold_unary_int(op: &str, expr: &Expr) -> Option<i64> {
    match op {
        "-" => const_int(expr).and_then(i64::checked_neg),
        _ => None,
    }
}

fn fold_unary_bool(op: &str, expr: &Expr) -> Option<bool> {
    match op {
        "!" => const_bool(expr).map(|b| !b),
        _ => None,
    }
}

fn fold_int_binop(op: &str, lhs: &Expr, rhs: &Expr) -> Option<i64> {
    let lhs = const_int(lhs)?;
    let rhs = const_int(rhs)?;
    match op {
        "+" => lhs.checked_add(rhs),
        "-" => lhs.checked_sub(rhs),
        "*" => lhs.checked_mul(rhs),
        "/" if rhs != 0 => lhs.checked_div(rhs),
        "%" if rhs != 0 => lhs.checked_rem(rhs),
        _ => None,
    }
}

fn fold_bool_binop(op: &str, lhs: &Expr, rhs: &Expr) -> Option<bool> {
    match op {
        "&&" => Some(const_bool(lhs)? && const_bool(rhs)?),
        "||" => Some(const_bool(lhs)? || const_bool(rhs)?),
        "==" => {
            if let (Some(lhs), Some(rhs)) = (const_bool(lhs), const_bool(rhs)) {
                return Some(lhs == rhs);
            }
            Some(const_int(lhs)? == const_int(rhs)?)
        }
        "!=" => {
            if let (Some(lhs), Some(rhs)) = (const_bool(lhs), const_bool(rhs)) {
                return Some(lhs != rhs);
            }
            Some(const_int(lhs)? != const_int(rhs)?)
        }
        "<" => Some(const_int(lhs)? < const_int(rhs)?),
        ">" => Some(const_int(lhs)? > const_int(rhs)?),
        "<=" => Some(const_int(lhs)? <= const_int(rhs)?),
        ">=" => Some(const_int(lhs)? >= const_int(rhs)?),
        _ => None,
    }
}

fn collect_expr_reads(e: &Expr, reads: &mut HashSet<String>) {
    match e {
        Expr::MatchValue { scrutinee, arms } => {
            collect_expr_reads(scrutinee, reads);
            for arm in arms {
                let mut inner = HashSet::new();
                if let Some(guard) = &arm.guard {
                    collect_expr_reads(guard, &mut inner);
                }
                collect_expr_reads(&arm.value, &mut inner);
                let mut bound = HashSet::new();
                crate::core_bindings::reserve_pattern(&arm.pattern, &mut bound);
                reads.extend(inner.into_iter().filter(|name| !bound.contains(name)));
            }
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            collect_expr_reads(cond, reads);
            collect_expr_reads(then_expr, reads);
            collect_expr_reads(else_expr, reads);
        }
        Expr::Block { body, result } => {
            collect_body_reads(body, reads);
            if let Some(result) = result {
                collect_expr_reads(result, reads);
            }
        }
        Expr::Ident(name) => {
            reads.insert(name.clone());
        }
        Expr::Unary { expr, .. } => collect_expr_reads(expr, reads),
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr_reads(lhs, reads);
            collect_expr_reads(rhs, reads);
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                collect_expr_reads(expr, reads);
            }
        }
        Expr::Field { base, .. } => collect_expr_reads(base, reads),
        Expr::ArrayLit(items) => {
            for item in items {
                collect_expr_reads(item, reads);
            }
        }
        Expr::Index { base, index, .. } => {
            collect_expr_reads(base, reads);
            collect_expr_reads(index, reads);
        }
        Expr::Call { callee, args, .. } => {
            if !matches!(callee.as_ref(), Expr::Ident(name) if crate::core_ir::is_compiler_callee(name) || matches!(name.as_str(), "vector-with-capacity-int" | "vector-with-capacity-float" | "vector-with-capacity-bool"))
            {
                collect_expr_reads(callee, reads);
            }
            for arg in args {
                collect_expr_reads(arg, reads);
            }
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_) => {}
        Expr::Closure { .. } => {}
    }
}

fn collect_stmt_reads(st: &Stmt, reads: &mut HashSet<String>) {
    match st {
        Stmt::Declare(_, _) => {}
        Stmt::Let(_, _, e)
        | Stmt::Assign(_, e)
        | Stmt::FieldAssign { value: e, .. }
        | Stmt::Expr(e)
        | Stmt::Return(Some(e)) => collect_expr_reads(e, reads),
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            collect_expr_reads(base, reads);
            collect_expr_reads(index, reads);
            collect_expr_reads(value, reads);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            collect_expr_reads(cond, reads);
            collect_body_reads(then_body, reads);
            collect_body_reads(else_body, reads);
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(cond) = cond {
                collect_expr_reads(cond, reads);
            }
            collect_body_reads(body, reads);
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            collect_expr_reads(scrutinee, reads);
            for arm in arms {
                let mut inner = HashSet::new();
                if let Some(guard) = &arm.guard {
                    collect_expr_reads(guard, &mut inner);
                }
                collect_body_reads(&arm.body, &mut inner);
                let mut bound = HashSet::new();
                crate::core_bindings::reserve_pattern(&arm.pattern, &mut bound);
                reads.extend(inner.into_iter().filter(|name| !bound.contains(name)));
            }
        }
        Stmt::Return(None) => {}
        Stmt::Throw(e) => {
            collect_expr_reads(e, reads);
        }
        Stmt::Try { body, catches, .. } => {
            collect_body_reads(body, reads);
            for arm in catches {
                collect_body_reads(&arm.body, reads);
            }
        }
        Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
    }
}

fn collect_body_reads(body: &[Stmt], reads: &mut HashSet<String>) {
    for st in body {
        collect_stmt_reads(st, reads);
    }
}

fn future_reads(body: &[Stmt], idx: usize) -> HashSet<String> {
    let mut reads = HashSet::new();
    collect_body_reads(&body[idx + 1..], &mut reads);
    reads
}

/// Recursively check a pattern against a value, emitting SIL code.
/// Branches to `success_label` on match, `fail_label` on mismatch.
/// Returns variable bindings from IdentPats.
fn lower_pattern_into(
    pattern: &MatchPattern,
    value_id: usize,
    success_label: &str,
    fail_label: &str,
    ssa: &mut usize,
    out: &mut String,
) -> Vec<(String, usize)> {
    match pattern {
        MatchPattern::IntPat(n) => {
            let pat_id = *ssa;
            *ssa += 1;
            out.push_str(&format!(
                "%{pat_id} = integer_literal $Builtin.Int64, {n}\n"
            ));
            let cmp_id = *ssa;
            *ssa += 1;
            out.push_str(&format!(
                "%{cmp_id} = builtin_binop \"==\" %{value_id}, %{pat_id}\n"
            ));
            out.push_str(&format!(
                "cond_br %{cmp_id}, {success_label}, {fail_label}\n"
            ));
            vec![]
        }
        MatchPattern::BoolPat(b) => {
            let pat_id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{pat_id} = bool_literal {b}\n"));
            let cmp_id = *ssa;
            *ssa += 1;
            out.push_str(&format!(
                "%{cmp_id} = builtin_binop \"==\" %{value_id}, %{pat_id}\n"
            ));
            out.push_str(&format!(
                "cond_br %{cmp_id}, {success_label}, {fail_label}\n"
            ));
            vec![]
        }
        MatchPattern::StringPat(s) => {
            let pat_id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{pat_id} = string_literal {s:?}\n"));
            let cmp_id = *ssa;
            *ssa += 1;
            out.push_str(&format!(
                "%{cmp_id} = builtin_binop \"==\" %{value_id}, %{pat_id}\n"
            ));
            out.push_str(&format!(
                "cond_br %{cmp_id}, {success_label}, {fail_label}\n"
            ));
            vec![]
        }
        MatchPattern::WildPat | MatchPattern::RestPat => {
            out.push_str(&format!("br {success_label}\n"));
            vec![]
        }
        MatchPattern::IdentPat(name) => {
            out.push_str(&format!("br {success_label}\n"));
            vec![(name.clone(), value_id)]
        }
        MatchPattern::TuplePat(pats) => {
            let mut bindings = Vec::new();
            for (i, subpat) in pats.iter().enumerate() {
                let idx_id = *ssa;
                *ssa += 1;
                out.push_str(&format!(
                    "%{idx_id} = integer_literal $Builtin.Int64, {i}\n"
                ));
                let elem_id = *ssa;
                *ssa += 1;
                out.push_str(&format!(
                    "%{elem_id} = index_access %{value_id}, %{idx_id}\n"
                ));
                let sub_success = if i + 1 == pats.len() {
                    success_label.to_string()
                } else {
                    format!("{success_label}_tup_{i}")
                };
                bindings.extend(lower_pattern_into(
                    subpat,
                    elem_id,
                    &sub_success,
                    fail_label,
                    ssa,
                    out,
                ));
                if i + 1 < pats.len() {
                    out.push_str(&format!("label {sub_success}\n"));
                }
            }
            bindings
        }
        MatchPattern::StructPat { name: _, fields } => {
            let mut bindings = Vec::new();
            for (i, (field_name, subpat)) in fields.iter().enumerate() {
                let field_id = *ssa;
                *ssa += 1;
                out.push_str(&format!(
                    "%{field_id} = field_access %{value_id} {field_name}\n"
                ));
                let sub_success = if i + 1 == fields.len() {
                    success_label.to_string()
                } else {
                    format!("{success_label}_st_{i}")
                };
                bindings.extend(lower_pattern_into(
                    subpat,
                    field_id,
                    &sub_success,
                    fail_label,
                    ssa,
                    out,
                ));
                if i + 1 < fields.len() {
                    out.push_str(&format!("label {sub_success}\n"));
                }
            }
            bindings
        }
        MatchPattern::ArrayPat(pats) => {
            let mut bindings = Vec::new();
            for (i, subpat) in pats.iter().enumerate() {
                match subpat {
                    MatchPattern::RestPat => {
                        if i + 1 == pats.len() {
                            out.push_str(&format!("br {success_label}\n"));
                        } else {
                            let sub_success = format!("{success_label}_arr_rest_{i}");
                            out.push_str(&format!("br {sub_success}\n"));
                            out.push_str(&format!("label {sub_success}\n"));
                        }
                    }
                    _ => {
                        let idx_id = *ssa;
                        *ssa += 1;
                        out.push_str(&format!(
                            "%{idx_id} = integer_literal $Builtin.Int64, {i}\n"
                        ));
                        let elem_id = *ssa;
                        *ssa += 1;
                        out.push_str(&format!(
                            "%{elem_id} = index_access %{value_id}, %{idx_id}\n"
                        ));
                        let sub_success = if i + 1 == pats.len() {
                            success_label.to_string()
                        } else {
                            format!("{success_label}_arr_{i}")
                        };
                        bindings.extend(lower_pattern_into(
                            subpat,
                            elem_id,
                            &sub_success,
                            fail_label,
                            ssa,
                            out,
                        ));
                        if i + 1 < pats.len() {
                            out.push_str(&format!("label {sub_success}\n"));
                        }
                    }
                }
            }
            bindings
        }
    }
}

/// Emit `bb0` instructions (params + statements). If `finish_with_return`, append `bb1` + `return`.
fn lower_stmts_into(
    params: &[(String, Typ)],
    body: &[Stmt],
    ssa: &mut usize,
    finish_with_return: bool,
) -> String {
    let mut out = String::new();
    let mut env: HashMap<String, usize> = HashMap::new();
    let mut direct_env: HashSet<String> = HashSet::new();
    for (idx, (pname, _)) in params.iter().enumerate() {
        let id = *ssa;
        *ssa += 1;
        out.push_str(&format!("%{id} = argument {idx} : $Builtin.Int64\n"));
        env.insert(pname.clone(), id);
        direct_env.insert(pname.clone());
    }
    out.push_str(&lower_stmts_with_env(
        body,
        ssa,
        finish_with_return,
        true,
        &mut env,
        &direct_env,
        false,
    ));
    out
}

fn lower_stmts_with_env(
    body: &[Stmt],
    ssa: &mut usize,
    finish_with_return: bool,
    implicit_default: bool,
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
    force_stores: bool,
) -> String {
    let mut out = String::new();
    for (idx, st) in body.iter().enumerate() {
        match st {
            Stmt::Declare(name, _) => {
                env.remove(name);
            }
            Stmt::Let(name, _, e) => {
                let id = lower_expr(e, env, direct_env, ssa, &mut out);
                env.insert(name.clone(), id);
                if force_stores || future_reads(body, idx).contains(name) {
                    out.push_str(&format!("store_var {name} %{id}\n"));
                }
            }
            Stmt::Assign(name, e) => {
                let id = lower_expr(e, env, direct_env, ssa, &mut out);
                env.insert(name.clone(), id);
                if force_stores || future_reads(body, idx).contains(name) {
                    out.push_str(&format!("store_var {name} %{id}\n"));
                }
            }
            Stmt::FieldAssign { base, value, .. } => {
                // Lower as: load base, modify field, store base
                let base_id = lower_expr(base, env, direct_env, ssa, &mut out);
                let val_id = lower_expr(value, env, direct_env, ssa, &mut out);
                out.push_str(&format!("field_assign %{base_id} %{val_id}\n"));
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                let Expr::Ident(name) = base else {
                    let _ = lower_expr(base, env, direct_env, ssa, &mut out);
                    let _ = lower_expr(index, env, direct_env, ssa, &mut out);
                    let _ = lower_expr(value, env, direct_env, ssa, &mut out);
                    continue;
                };
                let index_id = lower_expr(index, env, direct_env, ssa, &mut out);
                let value_id = lower_expr(value, env, direct_env, ssa, &mut out);
                out.push_str(&format!(
                    "index_store_var {name} %{index_id}, %{value_id}\n"
                ));
            }
            Stmt::Expr(e) => {
                let _ = lower_expr(e, env, direct_env, ssa, &mut out);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                lower_if_stmt(
                    cond,
                    then_body,
                    else_body,
                    env,
                    direct_env,
                    ssa,
                    finish_with_return,
                    &mut out,
                );
            }
            Stmt::Loop { cond, body, .. } => {
                lower_loop_stmt(
                    cond,
                    body,
                    env,
                    direct_env,
                    ssa,
                    finish_with_return,
                    &mut out,
                );
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                lower_match_stmt(
                    scrutinee,
                    arms,
                    env,
                    direct_env,
                    ssa,
                    finish_with_return,
                    &mut out,
                );
            }
            Stmt::Return(None) => {
                let id = *ssa;
                *ssa += 1;
                out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, 0\n"));
                if finish_with_return {
                    out.push_str(&format!("bb1:\nreturn %{id} : $Builtin.Int64\n"));
                }
                return out;
            }
            Stmt::Return(Some(e)) => {
                let id = lower_expr(e, env, direct_env, ssa, &mut out);
                if finish_with_return {
                    out.push_str(&format!("bb1:\nreturn %{id} : $Builtin.Int64\n"));
                }
                return out;
            }
            Stmt::Throw(e) => {
                let val_id = lower_expr(e, env, direct_env, ssa, &mut out);
                out.push_str(&format!("builtin_call \"throw_error\" %{val_id}\n"));
                if finish_with_return {
                    let id = *ssa;
                    *ssa += 1;
                    out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, 0\n"));
                    out.push_str(&format!("bb1:\nreturn %{id} : $Builtin.Int64\n"));
                }
                return out;
            }
            Stmt::Try { body, catches, .. } => {
                lower_try_stmt(
                    body,
                    catches,
                    env,
                    direct_env,
                    ssa,
                    finish_with_return,
                    force_stores,
                    &mut out,
                );
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Propagate => out.push_str("builtin_call \"propagate_error\"\n"),
        }
    }
    if !implicit_default {
        return out;
    }
    let v = *ssa;
    *ssa += 1;
    out.push_str(&format!("%{v} = integer_literal $Builtin.Int64, 0\n"));
    if finish_with_return {
        out.push_str(&format!("bb1:\nreturn %{v} : $Builtin.Int64\n"));
    }
    out
}

fn lower_if_stmt(
    cond: &Expr,
    then_body: &[Stmt],
    else_body: &[Stmt],
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    finish_with_return: bool,
    out: &mut String,
) {
    let cond_id = lower_expr(cond, env, direct_env, ssa, out);
    let label_id = *ssa;
    *ssa += 1;
    let then_label = format!("bb_if_then_{label_id}");
    let else_label = format!("bb_if_else_{label_id}");
    let end_label = format!("bb_if_end_{label_id}");
    out.push_str(&format!("cond_br %{cond_id}, {then_label}, {else_label}\n"));
    out.push_str(&format!("label {then_label}\n"));
    let mut then_env = env.clone();
    out.push_str(&lower_stmts_with_env(
        then_body,
        ssa,
        finish_with_return,
        false,
        &mut then_env,
        direct_env,
        true,
    ));
    out.push_str(&format!("br {end_label}\n"));
    out.push_str(&format!("label {else_label}\n"));
    if !else_body.is_empty() {
        let mut else_env = env.clone();
        out.push_str(&lower_stmts_with_env(
            else_body,
            ssa,
            finish_with_return,
            false,
            &mut else_env,
            direct_env,
            true,
        ));
    }
    out.push_str(&format!("br {end_label}\n"));
    out.push_str(&format!("label {end_label}\n"));
}

fn lower_loop_stmt(
    cond: &Option<Expr>,
    body: &[Stmt],
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    finish_with_return: bool,
    out: &mut String,
) {
    let label_id = *ssa;
    *ssa += 1;
    let head_label = format!("bb_loop_head_{label_id}");
    let body_label = format!("bb_loop_body_{label_id}");
    let end_label = format!("bb_loop_end_{label_id}");
    out.push_str(&format!("br {head_label}\n"));
    out.push_str(&format!("label {head_label}\n"));
    if let Some(c) = cond {
        let cond_id = lower_expr(c, env, direct_env, ssa, out);
        out.push_str(&format!("cond_br %{cond_id}, {body_label}, {end_label}\n"));
    } else {
        out.push_str(&format!("br {body_label}\n"));
    }
    out.push_str(&format!("label {body_label}\n"));
    let mut loop_env = env.clone();
    out.push_str(&lower_stmts_with_env(
        body,
        ssa,
        finish_with_return,
        false,
        &mut loop_env,
        direct_env,
        true,
    ));
    out.push_str(&format!("br {head_label}\n"));
    out.push_str(&format!("label {end_label}\n"));
}

fn lower_match_stmt(
    scrutinee: &Expr,
    arms: &[crate::core_ir::MatchArm],
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    finish_with_return: bool,
    out: &mut String,
) {
    let scrutinee_id = lower_expr(scrutinee, env, direct_env, ssa, out);
    let label_id = *ssa;
    *ssa += 1;
    let end_label = format!("bb_match_end_{label_id}");
    let mut default_arm = None;
    for arm in arms {
        let pattern = MatchPattern::parse(&arm.pattern).unwrap_or(MatchPattern::WildPat);
        if pattern == MatchPattern::WildPat {
            default_arm = Some(arm);
            continue;
        }
        let next_label = format!("bb_match_next_{label_id}_{}", *ssa);
        let arm_label = format!("bb_match_arm_{label_id}_{}", *ssa);
        let bindings =
            lower_pattern_into(&pattern, scrutinee_id, &arm_label, &next_label, ssa, out);
        out.push_str(&format!("label {arm_label}\n"));
        out.push_str("// match.arm\n");
        let pre_keys: Vec<String> = env.keys().cloned().collect();
        for (name, id) in &bindings {
            env.insert(name.clone(), *id);
            out.push_str(&format!("store_var {name} %{id}\n"));
        }
        out.push_str(&lower_stmts_with_env(
            &arm.body,
            ssa,
            finish_with_return,
            false,
            env,
            direct_env,
            true,
        ));
        env.retain(|k, _| pre_keys.contains(k));
        out.push_str(&format!("br {end_label}\n"));
        out.push_str(&format!("label {next_label}\n"));
    }
    if let Some(arm) = default_arm {
        out.push_str("// match.arm\n");
        let pre_keys: Vec<String> = env.keys().cloned().collect();
        out.push_str(&lower_stmts_with_env(
            &arm.body,
            ssa,
            finish_with_return,
            false,
            env,
            direct_env,
            true,
        ));
        env.retain(|k, _| pre_keys.contains(k));
    }
    out.push_str(&format!("label {end_label}\n"));
}

fn lower_try_stmt(
    body: &[Stmt],
    catches: &[crate::core_ir::CatchArm],
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    finish_with_return: bool,
    force_stores: bool,
    out: &mut String,
) {
    let label_id = *ssa;
    *ssa += 1;
    let try_body_label = format!("bb_try_body_{label_id}");
    let try_catch_label = format!("bb_try_catch_{label_id}");
    let try_end_label = format!("bb_try_end_{label_id}");
    out.push_str(&format!("br {try_body_label}\n"));
    out.push_str(&format!("label {try_body_label}\n"));
    let pre_keys: Vec<String> = env.keys().cloned().collect();
    out.push_str(&lower_stmts_with_env(
        body,
        ssa,
        finish_with_return,
        false,
        env,
        direct_env,
        force_stores,
    ));
    env.retain(|k, _| pre_keys.contains(k));
    out.push_str(&format!("br {try_end_label}\n"));
    out.push_str(&format!("label {try_catch_label}\n"));
    for arm in catches {
        let pre_keys: Vec<String> = env.keys().cloned().collect();
        out.push_str(&lower_stmts_with_env(
            &arm.body,
            ssa,
            finish_with_return,
            false,
            env,
            direct_env,
            force_stores,
        ));
        env.retain(|k, _| pre_keys.contains(k));
    }
    out.push_str(&format!("label {try_end_label}\n"));
}

fn helper_stub(ssa: &mut usize) -> String {
    let v = *ssa;
    *ssa += 1;
    format!("%{v} = integer_literal $Builtin.Int64, 0\nbb1:\nreturn %{v} : $Builtin.Int64\n")
}

/// Emit textual SIL: helper functions first (sorted), then `@main` with `function_ref` callees and a
/// unique SSA id space.
pub fn lower_to_textual_sil(module: UnifiedModule, _module_id: &str) -> String {
    lower_to_textual_sil_inner(module)
}

fn lower_to_textual_sil_inner(module: UnifiedModule) -> String {
    let mut module = module;
    desugar_module(&mut module);

    let mut fn_map = std::collections::HashMap::new();
    let mut fn_names = Vec::new();
    for d in &module.decls {
        if let Decl::Function { name, .. } = d {
            fn_map.insert(name.as_str(), d);
            fn_names.push(name.clone());
        }
    }
    fn_names.sort();

    let mut sil = String::from("// inauguration core → textual SIL (multi-front v0)\n");
    let mut ssa = 0usize;
    for name in &fn_names {
        if name == "main" {
            continue;
        }
        let Some(Decl::Function { params, body, .. }) = fn_map.get(name.as_str()).copied() else {
            continue;
        };
        sil.push_str(&format!("sil @{name}\nbb0:\n"));
        if body.is_empty() {
            sil.push_str(&helper_stub(&mut ssa));
        } else {
            sil.push_str(&lower_stmts_into(params, body, &mut ssa, true));
        }
    }

    sil.push_str("sil @main\nbb0:\n");
    if let Some(Decl::Function { params, body, .. }) = fn_map.get("main").copied() {
        if body.is_empty() {
            let ret = ssa;
            sil.push_str(&format!("%{ret} = integer_literal $Builtin.Int64, 0\n"));
        } else {
            sil.push_str(&lower_stmts_into(params, body, &mut ssa, true));
        }
    } else {
        let ret = ssa;
        sil.push_str(&format!("%{ret} = integer_literal $Builtin.Int64, 0\n"));
    }
    sil
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_desugar_module_basic() {
        let mut module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Component {
                name: "TestComponent".into(),
                target: "wasm".into(),
                deterministic: true,
                checkpoint: "".into(),
                imports: vec![],
                exports: vec![],
                capabilities: vec![],
            }],
        };
        desugar_module(&mut module);
        assert_eq!(module.decls.len(), 1, "should have one declaration");
        assert!(
            matches!(module.decls[0], Decl::Component { .. }),
            "should keep component declaration"
        );
    }

    #[test]
    fn test_desugar_module_interface_skipped() {
        let mut module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Interface {
                name: "TestInterface".into(),
                methods: vec![],
                visibility: crate::core_ir::Visibility::Pub,
                type_params: vec![],
            }],
        };
        desugar_module(&mut module);
        assert_eq!(module.decls.len(), 0, "should skip interface declaration");
    }

    #[test]
    fn test_desugar_module_class_desugaring() {
        let mut module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Class {
                name: "TestClass".into(),
                fields: vec![("x".into(), Typ::Int)],
                methods: vec![Decl::Function {
                    name: "method".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![],
                    type_params: vec![],
                }],
                visibility: crate::core_ir::Visibility::Pub,
                extends: None,
                implements: vec![],
                type_params: vec![],
            }],
        };
        desugar_module(&mut module);
        assert_eq!(
            module.decls.len(),
            2,
            "should split class into struct and function"
        );
        assert!(
            matches!(module.decls[0], Decl::Struct { ref name, .. } if name == "TestClass"),
            "first decl should be struct TestClass"
        );
        assert!(
            matches!(module.decls[1], Decl::Function { ref name, .. } if name == "TestClass_method"),
            "second decl should be mangled function TestClass_method"
        );
    }

    #[test]
    fn unknown_catch_payloads_do_not_infer_closure_parameters() {
        let parsed=crate::compiler::rust_front::parse_rust_source("fn fail()->Result<i64,i64> { Err(42) } fn main()->i64 { let answer=||{ if let Err(error)=fail() { let f=|x|x; return f(error); } return 0; }; answer() }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        super::desugar_module(&mut module);
        assert!(!crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        assert!(module.decls.iter().any(|decl| matches!(decl,crate::core_ir::Decl::Function{params,..} if params.iter().any(|(_,typ)|matches!(typ,crate::core_ir::Typ::Generic(_))))));
    }

    #[test]
    fn named_generic_closure_types_are_not_erased_by_call_inference() {
        use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule};
        for (params, ret, args) in [
            (
                vec![
                    ("a".into(), Typ::Generic("T".into())),
                    ("b".into(), Typ::Generic("T".into())),
                ],
                Typ::Int,
                vec![
                    Expr::IntLit(42),
                    Expr::FloatLit(crate::core_ir::FloatVal(1.0)),
                ],
            ),
            (
                vec![("x".into(), Typ::Generic("_".into()))],
                Typ::Generic("T".into()),
                vec![Expr::IntLit(42)],
            ),
        ] {
            let mut module = UnifiedModule {
                identity: Default::default(),
                decls: vec![Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Int,
                    type_params: vec![],
                    body: vec![
                        Stmt::Let(
                            "f".into(),
                            None,
                            Expr::Closure {
                                params,
                                ret,
                                captures: vec![],
                                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                            },
                        ),
                        Stmt::Return(Some(Expr::Call {
                            callee: Box::new(Expr::Ident("f".into())),
                            args,
                        })),
                    ],
                }],
            };
            super::desugar_module(&mut module);
            assert!(!crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        }
    }

    #[test]
    fn closure_calls_preserve_signature_checks_and_refuse_unproven_environments() {
        for (source, code) in [
            (
                "fn main()->i64 { let f=|x:i64| -> i64 { x }; f() }",
                "call-arity-mismatch",
            ),
            (
                "fn main()->i64 { let f=|x:i64| -> i64 { x }; f(true) }",
                "type-mismatch",
            ),
            (
                "fn main()->i64 { let f=|| -> bool { 42 }; if f() { return 42; } return 0; }",
                "return-type-mismatch",
            ),
            (
                "fn main()->i64 { let mut x=41; let mut f=||{x+=1;x}; f() }",
                "unresolved-symbol",
            ),
            (
                "fn main()->i64 { let f=|x|x; f(42)+f(true) }",
                "type-mismatch",
            ),
            ("fn main()->i64 { let f=|x|x; f() }", "unresolved-symbol"),
            (
                "fn main()->i64 { let f=|| ||42; f() }",
                "return-type-mismatch",
            ),
            (
                "fn main()->i64 { let f=|| -> i64 { ||42 }; f() }",
                "return-type-mismatch",
            ),
            (
                "fn main()->i64 { let mut f=||42; f=||7; f() }",
                "type-mismatch",
            ),
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            super::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(!report.ok, "{source}");
            assert_eq!(
                report.reason_code.as_deref(),
                Some(code),
                "{source}: {report:?}"
            );
        }
    }

    use super::*;

    #[test]
    fn expression_block_continue_advances_enclosing_for() {
        let advance = Stmt::Assign("index".into(), Expr::IntLit(1));
        let mut body = vec![Stmt::Let(
            "value".into(),
            Some(Typ::Int),
            Expr::Block {
                body: vec![Stmt::Continue],
                result: Some(Box::new(Expr::IntLit(42))),
            },
        )];
        advance_for_continues(&mut body, &advance);
        let Stmt::Let(_, _, Expr::Block { body, .. }) = &body[0] else {
            panic!()
        };
        assert_eq!(body, &vec![advance, Stmt::Continue]);
    }

    use crate::core_ir::MethodSig;
    use crate::core_ir::Typ;
    use crate::core_ir::{Expr, Stmt};

    #[test]
    fn test_lower_to_textual_sil() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "helper".into(),
                    params: vec![],
                    ret: Typ::Int,
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module, "App");

        // Check header
        assert!(sil.starts_with("// inauguration core → textual SIL (multi-front v0)\n"));

        // Helper should be processed first because of alphabetical sort
        assert!(sil.contains("sil @helper"));
        assert!(sil.contains("sil @main"));

        // Ensure main logic correctly handles non-empty body
        assert!(sil.contains("integer_literal $Builtin.Int64, 42"));
        assert!(sil.contains("integer_literal $Builtin.Int64, 0"));
    }

    #[test]
    fn test_lower_to_textual_sil_empty_bodies() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "helper".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module, "App");

        assert!(sil.contains("sil @helper\nbb0:\n%0 = integer_literal $Builtin.Int64, 0\nbb1:\nreturn %0 : $Builtin.Int64"));
        // main should be emitted last and use the ssa which was updated by helper
        assert!(sil.contains("sil @main\nbb0:\n%1 = integer_literal $Builtin.Int64, 0"));
    }

    #[test]
    fn lower_orders_helpers_and_main() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Struct {
                    name: "S".into(),
                    fields: vec![],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "zeta".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "alpha".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(sil.contains("sil @main"));
        assert!(sil.contains("sil @alpha"));
        assert!(sil.contains("sil @zeta"));
        let pa = sil.find("sil @alpha").expect("alpha");
        let pz = sil.find("sil @zeta").expect("zeta");
        let pm = sil.find("sil @main").expect("main");
        assert!(pa < pz);
        assert!(pz < pm);
    }

    #[test]
    fn lower_emits_let_and_return_for_helper() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Function {
                    name: "twice".into(),
                    params: vec![],
                    ret: Typ::Int,
                    body: vec![
                        Stmt::Let("y".into(), None, Expr::IntLit(2)),
                        Stmt::Return(Some(Expr::Ident("y".into()))),
                    ],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(sil.contains("sil @twice"));
        assert!(sil.contains("integer_literal $Builtin.Int64, 2"));
        assert!(sil.contains("return %"));
    }

    #[test]
    fn lower_emits_function_ref_for_explicit_call() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Function {
                    name: "helper".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".into())),
                        args: vec![],
                    })],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(sil.contains("function_ref @helper"));
        assert!(sil.contains("apply %"));
    }

    #[test]
    fn lower_omits_store_var_for_never_read_let_and_param() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("unused".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let("dead".into(), None, Expr::IntLit(2)),
                    Stmt::Return(Some(Expr::IntLit(3))),
                ],
                type_params: vec![],
            }],
        };

        let sil = lower_to_textual_sil(module.clone(), "App");

        assert!(sil.contains("argument 0"));
        assert!(!sil.contains("store_var unused"));
        assert!(!sil.contains("store_var dead"));
    }

    #[test]
    fn lower_keeps_store_var_for_read_variable() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let("used".into(), None, Expr::IntLit(2)),
                    Stmt::Return(Some(Expr::Ident("used".into()))),
                ],
                type_params: vec![],
            }],
        };

        let sil = lower_to_textual_sil(module.clone(), "App");

        assert!(sil.contains("store_var used"));
    }

    #[test]
    fn lower_folds_constant_integer_binop() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Binary {
                    op: "+".into(),
                    lhs: Box::new(Expr::IntLit(2)),
                    rhs: Box::new(Expr::IntLit(3)),
                }))],
                type_params: vec![],
            }],
        };

        let sil = lower_to_textual_sil(module.clone(), "App");

        assert!(sil.contains("integer_literal $Builtin.Int64, 5"));
        assert!(!sil.contains("builtin_binop"));
    }

    #[test]
    fn lower_folds_parsed_modulo_expression() {
        let module = crate::in_lang_parse::parse_in_source("fn main() -> Int { return 7 % 4; }\n")
            .expect("parse");

        let sil = lower_to_textual_sil(module.clone(), "App");

        assert!(sil.contains("integer_literal $Builtin.Int64, 3"));
        assert!(!sil.contains("builtin_binop"));
    }

    #[test]
    fn lower_folds_constant_unary_and_bool_binop() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Bool,
                body: vec![
                    Stmt::Let(
                        "n".into(),
                        Some(Typ::Int),
                        Expr::Unary {
                            op: "-".into(),
                            expr: Box::new(Expr::IntLit(3)),
                        },
                    ),
                    Stmt::Return(Some(Expr::Binary {
                        op: "&&".into(),
                        lhs: Box::new(Expr::Unary {
                            op: "!".into(),
                            expr: Box::new(Expr::BoolLit(false)),
                        }),
                        rhs: Box::new(Expr::Binary {
                            op: "==".into(),
                            lhs: Box::new(Expr::IntLit(2)),
                            rhs: Box::new(Expr::IntLit(2)),
                        }),
                    })),
                ],
                type_params: vec![],
            }],
        };

        let sil = lower_to_textual_sil(module.clone(), "App");

        assert!(sil.contains("integer_literal $Builtin.Int64, -3"));
        assert!(sil.contains("bool_literal true"));
        assert!(!sil.contains("builtin_unop"));
        assert!(!sil.contains("builtin_binop"));
    }

    #[test]
    fn lower_match_emits_conditional_arm_branches() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("tag".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let("out".into(), Some(Typ::Int), Expr::IntLit(0)),
                    Stmt::Match {
                        scrutinee: Expr::Ident("tag".into()),
                        arms: vec![
                            crate::core_ir::MatchArm {
                                guard: None,
                                pattern: "1".into(),
                                body: vec![Stmt::Assign("out".into(), Expr::IntLit(10))],
                            },
                            crate::core_ir::MatchArm {
                                guard: None,
                                pattern: "_".into(),
                                body: vec![Stmt::Assign("out".into(), Expr::IntLit(20))],
                            },
                        ],
                    },
                    Stmt::Return(Some(Expr::Ident("out".into()))),
                ],
                type_params: vec![],
            }],
        };

        let sil = lower_to_textual_sil(module.clone(), "App");

        assert!(sil.contains("builtin_binop \"==\""));
        assert!(sil.contains("cond_br"));
        assert!(sil.contains("bb_match_end_"));
    }

    #[test]
    fn desugar_class_one_method() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Class {
                name: "Point".into(),
                fields: vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
                methods: vec![Decl::Function {
                    name: "sum".into(),
                    params: vec![],
                    ret: Typ::Int,
                    body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                    type_params: vec![],
                }],
                visibility: crate::core_ir::Visibility::Pub,
                extends: None,
                implements: vec![],
                type_params: vec![],
            }],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(
            sil.contains("sil @Point_sum"),
            "should contain mangled function"
        );
        assert!(
            !sil.lines().any(|l| l.trim() == "sil @Point"),
            "struct should not appear as sil function"
        );
    }

    #[test]
    fn desugar_class_two_methods() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Class {
                name: "Point".into(),
                fields: vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
                methods: vec![
                    Decl::Function {
                        name: "sum".into(),
                        params: vec![],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                        type_params: vec![],
                    },
                    Decl::Function {
                        name: "scale".into(),
                        params: vec![("factor".into(), Typ::Int)],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                        type_params: vec![],
                    },
                ],
                visibility: crate::core_ir::Visibility::Pub,
                extends: None,
                implements: vec![],
                type_params: vec![],
            }],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(sil.contains("sil @Point_sum"), "should contain Point_sum");
        assert!(
            sil.contains("sil @Point_scale"),
            "should contain Point_scale"
        );
    }

    #[test]
    fn desugar_method_call_rewrite() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Class {
                    name: "Point".into(),
                    fields: vec![("x".into(), Typ::Int)],
                    methods: vec![Decl::Function {
                        name: "move_x".into(),
                        params: vec![("dx".into(), Typ::Int)],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                        type_params: vec![],
                    }],
                    visibility: crate::core_ir::Visibility::Pub,
                    extends: None,
                    implements: vec![],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![
                        Stmt::Let(
                            "obj".into(),
                            None,
                            Expr::StructInit {
                                name: "Point".into(),
                                fields: vec![("x".into(), Expr::IntLit(1))],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Field {
                                base: Box::new(Expr::Ident("obj".into())),
                                name: "move_x".into(),
                            }),
                            args: vec![Expr::IntLit(5)],
                        }),
                    ],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(
            sil.contains("function_ref @Point_move_x"),
            "should rewrite to Point_move_x"
        );
        assert!(
            sil.contains("sil @Point_move_x"),
            "should emit Point_move_x function"
        );
        assert!(
            !sil.lines().any(|l| l.trim() == "sil @Point"),
            "struct should not emit as function"
        );
    }

    #[test]
    fn desugar_closure_captures_one_var() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("x".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Let(
                    "f".into(),
                    None,
                    Expr::Closure {
                        params: vec![("a".into(), Typ::Int)],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("x".into())),
                            rhs: Box::new(Expr::IntLit(1)),
                        }))],
                        captures: vec![],
                    },
                )],
                type_params: vec![],
            }],
        };
        let mut module = module;
        desugar_module(&mut module);
        let cap_struct = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Struct { name, fields, .. } if name.contains("_captures") => {
                    Some(fields.clone())
                }
                _ => None,
            })
            .expect("captures struct should exist");
        assert_eq!(cap_struct, vec![("x".into(), Typ::Int)]);
    }

    #[test]
    fn desugar_closure_captures_two_vars() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Let(
                    "f".into(),
                    None,
                    Expr::Closure {
                        params: vec![("a".into(), Typ::Int)],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("x".into())),
                            rhs: Box::new(Expr::Ident("y".into())),
                        }))],
                        captures: vec![],
                    },
                )],
                type_params: vec![],
            }],
        };
        let mut module = module;
        desugar_module(&mut module);
        let cap_struct = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Struct { name, fields, .. } if name.contains("_captures") => {
                    Some(fields.clone())
                }
                _ => None,
            })
            .expect("captures struct should exist");
        assert_eq!(
            cap_struct,
            vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)]
        );
    }

    #[test]
    fn desugar_closure_rewrites_captured_var_to_self_field() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("x".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Let(
                    "f".into(),
                    None,
                    Expr::Closure {
                        params: vec![("a".into(), Typ::Int)],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("x".into())),
                            rhs: Box::new(Expr::IntLit(1)),
                        }))],
                        captures: vec![],
                    },
                )],
                type_params: vec![],
            }],
        };
        let mut module = module;
        desugar_module(&mut module);
        let body = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name.starts_with("__closure_") => {
                    Some(body.clone())
                }
                _ => None,
            })
            .expect("closure function should exist");
        let has_self_field = body.iter().any(|stmt| match stmt {
            Stmt::Return(Some(Expr::Binary { lhs, .. })) => matches!(
                lhs.as_ref(),
                Expr::Field { name, .. } if name == "x"
            ),
            _ => false,
        });
        assert!(has_self_field, "captured x should be rewritten to self.x");
    }

    #[test]
    fn desugar_closure_hidden_fn_has_self_param() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("x".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Let(
                    "f".into(),
                    None,
                    Expr::Closure {
                        params: vec![("a".into(), Typ::Int)],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("x".into())),
                            rhs: Box::new(Expr::IntLit(1)),
                        }))],
                        captures: vec![],
                    },
                )],
                type_params: vec![],
            }],
        };
        let mut module = module;
        desugar_module(&mut module);
        let params = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, params, .. } if name.starts_with("__closure_") => {
                    Some(params.clone())
                }
                _ => None,
            })
            .expect("closure function should exist");
        assert_eq!(
            params.first().map(|(n, _)| n.as_str()),
            Some("self"),
            "first param should be self: {params:?}"
        );
        assert!(
            params
                .first()
                .map(|(_, t)| matches!(t, Typ::Named(n) if n.contains("_captures")))
                .unwrap_or(false),
            "self param should be the captures struct type"
        );
        assert_eq!(params.len(), 2, "should have self + original param a");
    }

    #[test]
    fn desugar_closure_no_captures_empty_struct() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Let(
                    "f".into(),
                    None,
                    Expr::Closure {
                        params: vec![],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                        captures: vec![],
                    },
                )],
                type_params: vec![],
            }],
        };
        let mut module = module;
        desugar_module(&mut module);
        let cap_struct = module
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Struct { name, fields, .. } if name.contains("_captures") => {
                    Some(fields.clone())
                }
                _ => None,
            })
            .expect("captures struct should exist even when empty");
        assert!(cap_struct.is_empty(), "no captures -> empty struct");
    }

    #[test]
    fn statement_match_capture_reads_and_rewrites_respect_arm_bindings() {
        let mut body = vec![Stmt::Match {
            scrutinee: Expr::IntLit(42),
            arms: vec![crate::core_ir::MatchArm {
                pattern: "n".into(),
                guard: Some(Expr::Binary {
                    op: ">".into(),
                    lhs: Box::new(Expr::Ident("n".into())),
                    rhs: Box::new(Expr::Ident("threshold".into())),
                }),
                body: vec![Stmt::Return(Some(Expr::Binary {
                    op: "+".into(),
                    lhs: Box::new(Expr::Ident("n".into())),
                    rhs: Box::new(Expr::Ident("threshold".into())),
                }))],
            }],
        }];
        assert_eq!(collect_free_vars(&body, &[]), vec!["threshold".to_string()]);
        rewrite_captures_in_body(&mut body, &HashSet::from(["n", "threshold"]));
        let Stmt::Match { arms, .. } = &body[0] else {
            panic!()
        };
        let expected = Expr::Field {
            base: Box::new(Expr::Ident("self".into())),
            name: "threshold".into(),
        };
        let Some(Expr::Binary { lhs, rhs, .. }) = &arms[0].guard else {
            panic!()
        };
        assert_eq!(lhs.as_ref(), &Expr::Ident("n".into()));
        assert_eq!(rhs.as_ref(), &expected);
        let Stmt::Return(Some(Expr::Binary { lhs, rhs, .. })) = &arms[0].body[0] else {
            panic!()
        };
        assert_eq!(lhs.as_ref(), &Expr::Ident("n".into()));
        assert_eq!(rhs.as_ref(), &expected);
    }

    #[test]
    fn statement_match_guard_lifts_nested_closure_values() {
        let mut module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Match {
                scrutinee: Expr::IntLit(42),
                arms: vec![crate::core_ir::MatchArm {
                    pattern: "_".into(),
                    guard: Some(Expr::Call {
                        callee: Box::new(Expr::Closure {
                            params: vec![],
                            ret: Typ::Bool,
                            captures: vec![],
                            body: vec![Stmt::Return(Some(Expr::BoolLit(true)))],
                        }),
                        args: vec![],
                    }),
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                }],
            }],
        }]);
        lower_closures(&mut module);
        assert_eq!(module.decls.iter().filter(|decl| matches!(decl, Decl::Function { name, .. } if name.starts_with("__closure_"))).count(), 1);
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            assert!(
                !matches!(expr, Expr::Closure { .. }),
                "guard closure must be lifted"
            );
        });
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!()
        };
        let Stmt::Match { arms, .. } = &body[0] else {
            panic!()
        };
        assert!(
            matches!(&arms[0].guard, Some(Expr::Call { callee, .. }) if matches!(callee.as_ref(),Expr::Ident(name) if name.starts_with("__closure_")))
        );
    }

    #[test]
    fn lower_struct_match_pattern() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("p".into(), Typ::Named("Point".into()))],
                ret: Typ::Int,
                body: vec![Stmt::Match {
                    scrutinee: Expr::Ident("p".into()),
                    arms: vec![
                        crate::core_ir::MatchArm {
                            guard: None,
                            pattern: "Point { x, y: 0 }".into(),
                            body: vec![Stmt::Return(Some(Expr::Ident("x".into())))],
                        },
                        crate::core_ir::MatchArm {
                            guard: None,
                            pattern: "_".into(),
                            body: vec![Stmt::Return(Some(Expr::IntLit(-1)))],
                        },
                    ],
                }],
                type_params: vec![],
            }],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(
            sil.contains("field_access"),
            "should emit field_access for struct pattern"
        );
        assert!(
            sil.contains("builtin_binop"),
            "should emit comparison for field"
        );
        assert!(sil.contains("store_var x"), "should store x binding");
        assert!(sil.contains("bb_match_arm_"), "should have arm labels");
        assert!(sil.contains("bb_match_end_"), "should have end label");
    }

    #[test]
    fn lower_array_match_pattern() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("arr".into(), Typ::Array(Box::new(Typ::Int)))],
                ret: Typ::Int,
                body: vec![Stmt::Match {
                    scrutinee: Expr::Ident("arr".into()),
                    arms: vec![
                        crate::core_ir::MatchArm {
                            guard: None,
                            pattern: "[1, 2, 3]".into(),
                            body: vec![Stmt::Return(Some(Expr::IntLit(100)))],
                        },
                        crate::core_ir::MatchArm {
                            guard: None,
                            pattern: "[a, b, ..]".into(),
                            body: vec![Stmt::Return(Some(Expr::Ident("a".into())))],
                        },
                        crate::core_ir::MatchArm {
                            guard: None,
                            pattern: "_".into(),
                            body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                        },
                    ],
                }],
                type_params: vec![],
            }],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(
            sil.contains("index_access"),
            "should emit index_access for array pattern"
        );
        assert!(sil.contains("builtin_binop"), "should emit comparisons");
        assert!(sil.contains("store_var a"), "should store a binding");
        assert!(sil.contains("store_var b"), "should store b binding");
    }

    #[test]
    fn lower_tuple_match_pattern() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![("pair".into(), Typ::Named("Tuple".into()))],
                ret: Typ::Int,
                body: vec![Stmt::Match {
                    scrutinee: Expr::Ident("pair".into()),
                    arms: vec![
                        crate::core_ir::MatchArm {
                            guard: None,
                            pattern: "(0, 0)".into(),
                            body: vec![Stmt::Return(Some(Expr::IntLit(10)))],
                        },
                        crate::core_ir::MatchArm {
                            guard: None,
                            pattern: "(x, _)".into(),
                            body: vec![Stmt::Return(Some(Expr::Ident("x".into())))],
                        },
                    ],
                }],
                type_params: vec![],
            }],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(
            sil.contains("index_access"),
            "should emit index_access for tuple pattern"
        );
        assert!(sil.contains("store_var x"), "should store x binding");
        assert!(sil.contains("builtin_binop"), "should emit comparisons");
    }

    #[test]
    fn desugar_class_implements_interface() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Interface {
                    name: "Drawable".into(),
                    methods: vec![MethodSig {
                        name: "draw".into(),
                        params: vec![],
                        ret: Typ::Int,
                    }],
                    visibility: crate::core_ir::Visibility::Pub,
                    type_params: vec![],
                },
                Decl::Class {
                    name: "Circle".into(),
                    fields: vec![("radius".into(), Typ::Int)],
                    methods: vec![Decl::Function {
                        name: "draw".into(),
                        params: vec![],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                        type_params: vec![],
                    }],
                    visibility: crate::core_ir::Visibility::Pub,
                    extends: None,
                    implements: vec!["Drawable".into()],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(
            sil.contains("sil @Circle_draw"),
            "should emit Circle_draw for interface method"
        );
        assert!(
            !sil.lines().any(|l| l.trim() == "sil @Drawable"),
            "interface should not appear as SIL function"
        );
    }

    #[test]
    fn desugar_interface_method_call() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Interface {
                    name: "Drawable".into(),
                    methods: vec![MethodSig {
                        name: "draw".into(),
                        params: vec![],
                        ret: Typ::Int,
                    }],
                    visibility: crate::core_ir::Visibility::Pub,
                    type_params: vec![],
                },
                Decl::Class {
                    name: "Circle".into(),
                    fields: vec![("radius".into(), Typ::Int)],
                    methods: vec![Decl::Function {
                        name: "draw".into(),
                        params: vec![],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                        type_params: vec![],
                    }],
                    visibility: crate::core_ir::Visibility::Pub,
                    extends: None,
                    implements: vec!["Drawable".into()],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Void,
                    body: vec![
                        Stmt::Let(
                            "d".into(),
                            Some(Typ::Named("Drawable".into())),
                            Expr::StructInit {
                                name: "Circle".into(),
                                fields: vec![("radius".into(), Expr::IntLit(5))],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Field {
                                base: Box::new(Expr::Ident("d".into())),
                                name: "draw".into(),
                            }),
                            args: vec![],
                        }),
                    ],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        assert!(
            sil.contains("function_ref @Circle_draw"),
            "should dispatch to Circle_draw for interface method call"
        );
        assert!(sil.contains("sil @Circle_draw"), "should emit Circle_draw");
        assert!(sil.contains("sil @main"), "should emit main");
    }

    #[test]
    fn desugar_interface_param_not_rewritten() {
        let module = UnifiedModule {
            identity: Default::default(),
            decls: vec![
                Decl::Interface {
                    name: "Drawable".into(),
                    methods: vec![MethodSig {
                        name: "draw".into(),
                        params: vec![],
                        ret: Typ::Int,
                    }],
                    visibility: crate::core_ir::Visibility::Pub,
                    type_params: vec![],
                },
                Decl::Class {
                    name: "Circle".into(),
                    fields: vec![("radius".into(), Typ::Int)],
                    methods: vec![Decl::Function {
                        name: "draw".into(),
                        params: vec![],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                        type_params: vec![],
                    }],
                    visibility: crate::core_ir::Visibility::Pub,
                    extends: None,
                    implements: vec!["Drawable".into()],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "handle".into(),
                    params: vec![("d".into(), Typ::Named("Drawable".into()))],
                    ret: Typ::Void,
                    body: vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Field {
                            base: Box::new(Expr::Ident("d".into())),
                            name: "draw".into(),
                        }),
                        args: vec![],
                    })],
                    type_params: vec![],
                },
            ],
        };
        let sil = lower_to_textual_sil(module.clone(), "App");
        // For now, interface methods are NOT dispatched (would require full type inference)
        // The test verifies we don't incorrectly rewrite unknown interface calls
        assert!(
            !sil.contains("apply @Circle_draw"),
            "interface method on unknown concrete type should not be rewritten to class method"
        );
    }

    #[test]
    fn extra_range_arguments_are_not_discarded() {
        let iterator = Expr::Call {
            callee: Box::new(Expr::Ident("range".into())),
            args: vec![
                Expr::IntLit(0),
                Expr::IntLit(6),
                Expr::Call {
                    callee: Box::new(Expr::Ident("step".into())),
                    args: Vec::new(),
                },
            ],
        };
        let expanded = expand_for_loop("item".into(), iterator.clone(), Vec::new(), &mut 0);
        assert!(matches!(&expanded[0], Stmt::Let(_, _, expr) if expr == &iterator));
    }

    #[test]
    fn desugar_range_for_to_while() {
        let mut module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Loop {
                    kind: LoopKind::For {
                        binding: "i".into(),
                    },
                    cond: Some(Expr::Call {
                        callee: Box::new(Expr::Ident("range".into())),
                        args: vec![Expr::IntLit(0), Expr::IntLit(3)],
                    }),
                    body: vec![],
                }],
                type_params: vec![],
            }],
        };
        desugar_module(&mut module);
        let Decl::Function { body, .. } = &module.decls[0] else {
            panic!("expected function");
        };
        assert!(
            body.iter().any(|s| matches!(
                s,
                Stmt::Loop {
                    kind: LoopKind::While,
                    ..
                }
            )),
            "range for should become while: {body:?}"
        );
        assert!(
            !body.iter().any(|s| matches!(
                s,
                Stmt::Loop {
                    kind: LoopKind::For { .. },
                    ..
                }
            )),
            "For should be gone: {body:?}"
        );
    }
}
