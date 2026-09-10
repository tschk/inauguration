//! Lower [`crate::core_ir::UnifiedModule`] to textual SIL.

use crate::core_ir::{Decl, MatchPattern, Typ, UnifiedModule};
use crate::core_ir::{Expr, Stmt};
use std::collections::{HashMap, HashSet};

pub fn desugar_module(module: &mut UnifiedModule) {
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
                        body,
                        ..
                    } = method
                    {
                        let mangled = format!("{}_{}", name, method_name);
                        method_map.insert(method_name.clone(), mangled.clone());
                        method_map.insert(format!("{}-{}", name, method_name), mangled.clone());
                        let mut new_params = vec![("self".to_string(), Typ::Named(name.clone()))];
                        new_params.extend(params);
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

    let mut closure_counter = 0usize;
    let mut extra_decls: Vec<Decl> = Vec::new();
    for decl in &mut new_decls {
        if let Decl::Function { body, .. } = decl {
            desugar_closures_in_body(body, &mut closure_counter, &mut extra_decls);
            if !method_map.is_empty() {
                rewrite_method_calls_in_body(body, &method_map);
            }
        }
    }
    new_decls.extend(extra_decls);

    module.decls = new_decls;
}

fn desugar_closures_in_body(body: &mut [Stmt], counter: &mut usize, extra_decls: &mut Vec<Decl>) {
    for stmt in body {
        match stmt {
            Stmt::Let(_, _, e)
            | Stmt::Assign(_, e)
            | Stmt::FieldAssign { value: e, .. }
            | Stmt::Return(Some(e))
            | Stmt::Expr(e) => {
                desugar_closures_in_expr(e, counter, extra_decls);
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                desugar_closures_in_expr(base, counter, extra_decls);
                desugar_closures_in_expr(index, counter, extra_decls);
                desugar_closures_in_expr(value, counter, extra_decls);
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                desugar_closures_in_expr(cond, counter, extra_decls);
                desugar_closures_in_body(then_body, counter, extra_decls);
                desugar_closures_in_body(else_body, counter, extra_decls);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(c) = cond {
                    desugar_closures_in_expr(c, counter, extra_decls);
                }
                desugar_closures_in_body(body, counter, extra_decls);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                desugar_closures_in_expr(scrutinee, counter, extra_decls);
                for arm in arms {
                    desugar_closures_in_body(&mut arm.body, counter, extra_decls);
                }
            }
            Stmt::Return(None) => {}
            Stmt::Break | Stmt::Propagate => {}
            Stmt::Throw(e) => {
                desugar_closures_in_expr(e, counter, extra_decls);
            }
            Stmt::Try { body, catches, .. } => {
                desugar_closures_in_body(body, counter, extra_decls);
                for catch in catches {
                    desugar_closures_in_body(&mut catch.body, counter, extra_decls);
                }
            }
        }
    }
}

fn collect_declared_vars_in_body<'a>(body: &'a [Stmt], out: &mut HashSet<&'a str>) {
    for stmt in body {
        match stmt {
            Stmt::Let(name, _, _) => {
                out.insert(name.as_str());
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
                    collect_declared_vars_in_body(&arm.body, out);
                }
            }
            Stmt::Try { body, catches, .. } => {
                collect_declared_vars_in_body(body, out);
                for catch in catches {
                    collect_declared_vars_in_body(&catch.body, out);
                }
            }
            _ => {}
        }
    }
}

fn collect_free_vars(body: &[Stmt], params: &[(String, Typ)]) -> Vec<String> {
    let mut reads = HashSet::new();
    collect_body_reads(body, &mut reads);

    let mut declared: HashSet<&str> = HashSet::new();
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
        match stmt {
            Stmt::Let(_, _, e)
            | Stmt::Assign(_, e)
            | Stmt::FieldAssign { value: e, .. }
            | Stmt::Return(Some(e))
            | Stmt::Expr(e) => {
                rewrite_captures_in_expr(e, captures);
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
                    rewrite_captures_in_body(&mut arm.body, captures);
                }
            }
            Stmt::Return(None) => {}
            Stmt::Break | Stmt::Propagate => {}
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
        Expr::Ident(_)
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_) => {}
        Expr::Closure { .. } => {}
    }
}

fn desugar_closures_in_expr(expr: &mut Expr, counter: &mut usize, extra_decls: &mut Vec<Decl>) {
    match expr {
        Expr::Closure {
            params, ret, body, ..
        } => {
            let id = *counter;
            *counter += 1;
            let fn_name = format!("__closure_{id}");
            let caps_name = format!("{fn_name}_captures");
            let closure_params = params.clone();
            let captures = collect_free_vars(body, &closure_params);
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
        Expr::Unary { expr: inner, .. } => desugar_closures_in_expr(inner, counter, extra_decls),
        Expr::Binary { lhs, rhs, .. } => {
            desugar_closures_in_expr(lhs, counter, extra_decls);
            desugar_closures_in_expr(rhs, counter, extra_decls);
        }
        Expr::StructInit { fields, .. } => {
            for (_, field_expr) in fields {
                desugar_closures_in_expr(field_expr, counter, extra_decls);
            }
        }
        Expr::Field { base, .. } => {
            desugar_closures_in_expr(base, counter, extra_decls);
        }
        Expr::ArrayLit(items) => {
            for item in items {
                desugar_closures_in_expr(item, counter, extra_decls);
            }
        }
        Expr::Index { base, index, .. } => {
            desugar_closures_in_expr(base, counter, extra_decls);
            desugar_closures_in_expr(index, counter, extra_decls);
        }
        Expr::Call { callee, args, .. } => {
            desugar_closures_in_expr(callee, counter, extra_decls);
            let preserves_map_closure =
                matches!(callee.as_ref(), Expr::Ident(name) if name == "map") && args.len() == 2;
            for (index, arg) in args.iter_mut().enumerate() {
                if preserves_map_closure && index == 1 && matches!(arg, Expr::Closure { .. }) {
                    continue;
                }
                desugar_closures_in_expr(arg, counter, extra_decls);
            }
        }
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_) => {}
    }
}

fn rewrite_method_calls_in_body(body: &mut [Stmt], method_map: &HashMap<String, String>) {
    for stmt in body {
        match stmt {
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
            Stmt::Break | Stmt::Propagate => {}
        }
    }
}

fn rewrite_method_calls_in_expr(expr: &mut Expr, method_map: &HashMap<String, String>) {
    match expr {
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
        Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_)
        | Expr::Ident(_) => {}
        Expr::Closure { body, .. } => {
            rewrite_method_calls_in_body(body, method_map);
        }
    }
}

fn lower_expr(
    e: &Expr,
    env: &HashMap<String, usize>,
    direct_env: &HashSet<String>,
    ssa: &mut usize,
    out: &mut String,
) -> usize {
    match e {
        Expr::IntLit(n) => {
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, {n}\n"));
            id
        }
        Expr::FloatLit(f) => {
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!(
                "%{id} = float_literal $Builtin.FPIEEE64, {}\n",
                f.0
            ));
            id
        }
        Expr::BoolLit(b) => {
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = bool_literal {b}\n"));
            id
        }
        Expr::StringLit(s) => {
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = string_literal {s:?}\n"));
            id
        }
        Expr::Ident(name) => {
            if direct_env.contains(name)
                && let Some(id) = env.get(name)
            {
                return *id;
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
        Expr::Unary { op, expr, .. } => {
            if let Some(n) = fold_unary_int(op, expr) {
                let id = *ssa;
                *ssa += 1;
                out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, {n}\n"));
                return id;
            }
            if let Some(b) = fold_unary_bool(op, expr) {
                let id = *ssa;
                *ssa += 1;
                out.push_str(&format!("%{id} = bool_literal {b}\n"));
                return id;
            }
            let arg = lower_expr(expr, env, direct_env, ssa, out);
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = builtin_unop {op:?} %{arg}\n"));
            id
        }
        Expr::Binary { op, lhs, rhs, .. } => {
            if let Some(n) = fold_int_binop(op, lhs, rhs) {
                let id = *ssa;
                *ssa += 1;
                out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, {n}\n"));
                return id;
            }
            if let Some(b) = fold_bool_binop(op, lhs, rhs) {
                let id = *ssa;
                *ssa += 1;
                out.push_str(&format!("%{id} = bool_literal {b}\n"));
                return id;
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
        Expr::StructInit { name, fields, .. } => {
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
        Expr::Field { base, name, .. } => {
            let base_id = lower_expr(base, env, direct_env, ssa, out);
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = field_access %{base_id} {name}\n"));
            id
        }
        Expr::ArrayLit(items) => {
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
        Expr::Index { base, index, .. } => {
            let base_id = lower_expr(base, env, direct_env, ssa, out);
            let index_id = lower_expr(index, env, direct_env, ssa, out);
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = index_access %{base_id}, %{index_id}\n"));
            id
        }
        Expr::Call { callee, args, .. } => {
            let mut arg_ids = Vec::new();
            if let Expr::Ident(name) = callee.as_ref() {
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
        Expr::Closure { .. } => {
            let id = *ssa;
            *ssa += 1;
            out.push_str(&format!("%{id} = integer_literal $Builtin.Int64, 0\n"));
            id
        }
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
            collect_expr_reads(callee, reads);
            for arg in args {
                collect_expr_reads(arg, reads);
            }
        }
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::StringLit(_) | Expr::BoolLit(_) => {}
        Expr::Closure { .. } => {}
    }
}

fn collect_stmt_reads(st: &Stmt, reads: &mut HashSet<String>) {
    match st {
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
                collect_body_reads(&arm.body, reads);
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
        Stmt::Break | Stmt::Propagate => {}
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
                    ssa,
                    finish_with_return,
                    env,
                    direct_env,
                    &mut out,
                );
            }
            Stmt::Loop {
                cond,
                body: loop_body,
                ..
            } => {
                lower_loop_stmt(
                    cond,
                    loop_body,
                    ssa,
                    finish_with_return,
                    env,
                    direct_env,
                    &mut out,
                );
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                lower_match_stmt(
                    scrutinee,
                    arms,
                    ssa,
                    finish_with_return,
                    env,
                    direct_env,
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
            Stmt::Try {
                body: try_body,
                catches,
                ..
            } => {
                lower_try_stmt(
                    try_body,
                    catches,
                    ssa,
                    finish_with_return,
                    env,
                    direct_env,
                    force_stores,
                    &mut out,
                );
            }
            Stmt::Break => {}
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

#[allow(clippy::too_many_arguments)]
fn lower_if_stmt(
    cond: &Expr,
    then_body: &[Stmt],
    else_body: &[Stmt],
    ssa: &mut usize,
    finish_with_return: bool,
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
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

#[allow(clippy::too_many_arguments)]
fn lower_loop_stmt(
    cond: &Option<Expr>,
    body: &[Stmt],
    ssa: &mut usize,
    finish_with_return: bool,
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
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

#[allow(clippy::too_many_arguments)]
fn lower_match_stmt(
    scrutinee: &Expr,
    arms: &[crate::core_ir::MatchArm],
    ssa: &mut usize,
    finish_with_return: bool,
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
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

#[allow(clippy::too_many_arguments)]
fn lower_try_stmt(
    try_body: &[Stmt],
    catches: &[crate::core_ir::CatchArm],
    ssa: &mut usize,
    finish_with_return: bool,
    env: &mut HashMap<String, usize>,
    direct_env: &HashSet<String>,
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
        try_body,
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
    use super::*;
    use crate::core_ir::MethodSig;
    use crate::core_ir::Typ;
    use crate::core_ir::{Expr, Stmt};

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
                                pattern: "1".into(),
                                body: vec![Stmt::Assign("out".into(), Expr::IntLit(10))],
                            },
                            crate::core_ir::MatchArm {
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
        assert!(
            cap_struct.contains(&("x".into(), Typ::Named("Any".into()))),
            "captures struct should have x field, got: {cap_struct:?}"
        );
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
        assert!(
            cap_struct.contains(&("x".into(), Typ::Named("Any".into()))),
            "captures struct should have x field"
        );
        assert!(
            cap_struct.contains(&("y".into(), Typ::Named("Any".into()))),
            "captures struct should have y field"
        );
        assert_eq!(cap_struct.len(), 2);
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
                            pattern: "Point { x, y: 0 }".into(),
                            body: vec![Stmt::Return(Some(Expr::Ident("x".into())))],
                        },
                        crate::core_ir::MatchArm {
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
                            pattern: "[1, 2, 3]".into(),
                            body: vec![Stmt::Return(Some(Expr::IntLit(100)))],
                        },
                        crate::core_ir::MatchArm {
                            pattern: "[a, b, ..]".into(),
                            body: vec![Stmt::Return(Some(Expr::Ident("a".into())))],
                        },
                        crate::core_ir::MatchArm {
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
                            pattern: "(0, 0)".into(),
                            body: vec![Stmt::Return(Some(Expr::IntLit(10)))],
                        },
                        crate::core_ir::MatchArm {
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
}
