//! Spill effectful Core IR operands for array repetition and ordered stores.
//!
//! Save the value once, then reuse it in an ordinary array literal. Expression
//! spills preserve left-to-right evaluation and remain inside conditional paths.

use crate::core_ir::{Decl, Expr, LoopKind, Stmt, Typ, expr_contains_call};
use std::collections::{HashMap, HashSet};

const MAX_EXPANDED_ITEMS: usize = 4096;

fn is_repeat(expr: &Expr) -> bool {
    matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if name == "array-repeat"))
}

fn is_index_store(expr: &Expr) -> bool {
    matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if name == "index-assign-value-first" || name == "index-modify-value-first" || name == "index-modify-place-first"))
}

fn is_field_modify(expr: &Expr) -> bool {
    matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if name == "field-modify-value-first"))
}

fn is_variable_modify(expr: &Expr) -> bool {
    matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if name == "modify-value-first"))
}

fn is_addressable(expr: &Expr) -> bool {
    match expr {
        Expr::Ident(_) => true,
        Expr::Field { base, .. } => is_addressable(base),
        Expr::Call { callee, args } if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::BORROW) =>
        {
            matches!(args.as_slice(), [value] if is_addressable(value))
        }
        _ => false,
    }
}

fn value_receiver_index(name: &str, facts: &crate::typecheck::Facts) -> Option<usize> {
    if matches!(name, "vector-push" | "vector-extend" | "vector-clear") {
        Some(0)
    } else {
        facts.value_receiver_index(name)
    }
}

fn expr_needs_spills(expr: &Expr, facts: &crate::typecheck::Facts) -> bool {
    if is_repeat(expr) || is_index_store(expr) || is_field_modify(expr) || is_variable_modify(expr)
    {
        return true;
    }
    if matches!(expr, Expr::Field { base, .. } if matches!(base.as_ref(), Expr::Call { .. } | Expr::StructInit { .. }))
    {
        return true;
    }
    if let Expr::Call { callee, args } = expr
        && let Expr::Ident(name) = callee.as_ref()
        && let Some(index) = value_receiver_index(name, facts)
        && args.get(index).is_some_and(|arg| !is_addressable(arg))
    {
        return true;
    }
    if let Expr::Block { body, result } = expr {
        return body.iter().any(|stmt| stmt_needs_spills(stmt, facts))
            || result
                .as_deref()
                .is_some_and(|value| expr_needs_spills(value, facts));
    }
    if let Expr::Closure { body, .. } = expr {
        return body.iter().any(|stmt| stmt_needs_spills(stmt, facts));
    }
    let mut found = false;
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        found |= expr_needs_spills(child, facts)
    });
    found
}

fn stmt_needs_spills(stmt: &Stmt, facts: &crate::typecheck::Facts) -> bool {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Expr(expr)
        | Stmt::Throw(expr)
        | Stmt::Return(Some(expr)) => expr_needs_spills(expr, facts),
        Stmt::IndexAssign { base, index, value } => {
            matches!(base, Expr::Field { .. })
                || [base, index, value]
                    .into_iter()
                    .any(|expr| expr_contains_call(expr) || expr_needs_spills(expr, facts))
        }
        Stmt::FieldAssign { base, value, .. } => [base, value]
            .into_iter()
            .any(|expr| expr_needs_spills(expr, facts)),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            expr_needs_spills(cond, facts)
                || then_body
                    .iter()
                    .chain(else_body)
                    .any(|stmt| stmt_needs_spills(stmt, facts))
        }
        Stmt::Loop { cond, body, .. } => {
            cond.as_ref()
                .is_some_and(|expr| expr_needs_spills(expr, facts))
                || body.iter().any(|stmt| stmt_needs_spills(stmt, facts))
        }
        Stmt::Match { scrutinee, arms } => {
            expr_needs_spills(scrutinee, facts)
                || arms.iter().any(|arm| {
                    arm.guard
                        .as_ref()
                        .is_some_and(|expr| expr_needs_spills(expr, facts))
                        || arm.body.iter().any(|stmt| stmt_needs_spills(stmt, facts))
                })
        }
        Stmt::Try { body, catches } => {
            body.iter().any(|stmt| stmt_needs_spills(stmt, facts))
                || catches
                    .iter()
                    .any(|catch| catch.body.iter().any(|stmt| stmt_needs_spills(stmt, facts)))
        }
        _ => false,
    }
}

pub(super) fn has_value_spills(decls: &[Decl]) -> bool {
    let facts = crate::typecheck::TypeChecker::new().collect_decl_facts(decls);
    has_value_spills_with_facts(decls, &facts)
}

fn has_value_spills_with_facts(decls: &[Decl], facts: &crate::typecheck::Facts) -> bool {
    decls.iter().any(|decl| match decl {
        Decl::Function { body, .. } => body.iter().any(|stmt| stmt_needs_spills(stmt, facts)),
        Decl::Class { methods, .. } => has_value_spills_with_facts(methods, facts),
        Decl::Global {
            init: Some(expr), ..
        } => expr_needs_spills(expr, facts),
        _ => false,
    })
}

pub(crate) fn lower_value_spills(decls: &mut [Decl]) {
    let facts = crate::typecheck::TypeChecker::new().collect_decl_facts(decls);
    if !has_value_spills_with_facts(decls, &facts) {
        return;
    }
    let mut names = crate::core_bindings::module_names(decls);
    fn reserve(decls: &[Decl], names: &mut HashSet<String>) {
        for decl in decls {
            match decl {
                Decl::Function { params, body, .. } => {
                    names.extend(params.iter().map(|(name, _)| name.clone()));
                    crate::core_bindings::reserve_body(body, names);
                }
                Decl::Class { methods, .. } => reserve(methods, names),
                Decl::Global {
                    init: Some(expr), ..
                } => crate::core_bindings::reserve_expr(expr, names),
                _ => {}
            }
        }
    }
    reserve(decls, &mut names);
    let mut constants = HashMap::new();
    // Resolve immutable integer globals to a fixed point, including forward refs.
    for _ in 0..decls.len() {
        let old_len = constants.len();
        for decl in decls.iter() {
            if let Decl::Global {
                name,
                init: Some(expr),
                mutable: false,
                ..
            } = decl
                && let Some(value) = constant_int(expr, &constants)
            {
                constants.insert(name.clone(), value);
            }
        }
        if constants.len() == old_len {
            break;
        }
    }
    let types = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect();
    let mut lower = Lower {
        names,
        constants,
        facts,
        types,
        counter: 0,
    };
    lower.members(decls);
}

fn constant_int(expr: &Expr, constants: &HashMap<String, i64>) -> Option<i64> {
    match expr {
        Expr::Ident(name) => constants.get(name).copied(),
        Expr::Unary { op, expr } => {
            let value = constant_int(expr, constants)?;
            match op.as_str() {
                "-" => value.checked_neg(),
                "~" => Some(!value),
                _ => None,
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let (lhs, rhs) = (constant_int(lhs, constants)?, constant_int(rhs, constants)?);
            match op.as_str() {
                "+" => lhs.checked_add(rhs),
                "-" => lhs.checked_sub(rhs),
                "*" => lhs.checked_mul(rhs),
                "/" => lhs.checked_div(rhs),
                "%" => lhs.checked_rem(rhs),
                "&" => Some(lhs & rhs),
                "|" => Some(lhs | rhs),
                "^" => Some(lhs ^ rhs),
                "<<" => lhs.checked_shl(u32::try_from(rhs).ok()?),
                ">>" => lhs.checked_shr(u32::try_from(rhs).ok()?),
                _ => None,
            }
        }
        _ => super::const_int(expr),
    }
}

struct Lower {
    names: HashSet<String>,
    constants: HashMap<String, i64>,
    counter: usize,
    facts: crate::typecheck::Facts,
    types: HashMap<String, Typ>,
}

impl Lower {
    fn fresh(&mut self) -> String {
        loop {
            let name = format!("__array_repeat_{}", self.counter);
            self.counter += 1;
            if self.names.insert(name.clone()) {
                return name;
            }
        }
    }

    fn save(&mut self, expr: Expr, out: &mut Vec<Stmt>) -> Expr {
        let name = self.fresh();
        let typ = self.expr_type(&expr);
        if let Some(typ) = &typ {
            self.types.insert(name.clone(), typ.clone());
        }
        out.push(Stmt::Let(name.clone(), typ, expr));
        Expr::Ident(name)
    }

    fn expr_type(&self, expr: &Expr) -> Option<Typ> {
        crate::typecheck::TypeChecker::new().expr_type(expr, &self.facts, &self.types)
    }

    fn pattern_body(&mut self, body: &mut Vec<Stmt>, pattern: &str, typ: Option<Typ>) {
        let constants = self.constants.clone();
        let types = self.types.clone();
        let mut names = HashSet::new();
        crate::core_bindings::reserve_pattern(pattern, &mut names);
        for name in names {
            self.constants.remove(&name);
            self.types
                .insert(name, typ.clone().unwrap_or(Typ::Generic("_".into())));
        }
        self.body(body);
        self.constants = constants;
        self.types = types;
    }

    fn members(&mut self, decls: &mut [Decl]) {
        for decl in decls {
            match decl {
                Decl::Function { params, body, .. } => {
                    let constants = self.constants.clone();
                    let types = self.types.clone();
                    for (name, typ) in params {
                        self.constants.remove(name);
                        self.types.insert(name.clone(), typ.clone());
                    }
                    self.body(body);
                    self.constants = constants;
                    self.types = types;
                }
                Decl::Class { methods, .. } => self.members(methods),
                // A global initializer has no statement context. Only expand
                // literal values; preserve other operations for honest refusal.
                Decl::Global {
                    init: Some(expr), ..
                } if is_repeat(expr) => {
                    if let Expr::Call { args, .. } = expr.as_mut()
                        && args.len() == 2
                        && matches!(
                            args[0],
                            Expr::IntLit(_)
                                | Expr::FloatLit(_)
                                | Expr::BoolLit(_)
                                | Expr::StringLit(_)
                        )
                        && let Some(count) = self.count(&args[1])
                    {
                        **expr = Expr::ArrayLit(vec![args[0].clone(); count]);
                    }
                }
                _ => {}
            }
        }
    }

    fn count(&self, expr: &Expr) -> Option<usize> {
        let count = usize::try_from(constant_int(expr, &self.constants)?).ok()?;
        (count <= MAX_EXPANDED_ITEMS).then_some(count)
    }

    fn expr(&mut self, expr: &mut Expr, out: &mut Vec<Stmt>) {
        if let Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } = expr
        {
            self.expr(cond, out);
            for branch in [then_expr, else_expr] {
                let mut body = Vec::new();
                self.expr(branch, &mut body);
                if !body.is_empty() {
                    **branch = Expr::Block {
                        body,
                        result: Some(Box::new((**branch).clone())),
                    };
                }
            }
            return;
        }
        if let Expr::Block { body, result } = expr {
            let constants = self.constants.clone();
            let types = self.types.clone();
            self.body(body);
            for stmt in body.iter() {
                if let Stmt::Let(name, typ, value) = stmt {
                    self.constants.remove(name);
                    if let Some(typ) = typ.clone().or_else(|| self.expr_type(value)) {
                        self.types.insert(name.clone(), typ);
                    }
                }
            }
            if let Some(result) = result {
                self.expr(result, body);
            }
            self.constants = constants;
            self.types = types;
            return;
        }

        if let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::BORROW)
            && let [value] = args.as_mut_slice()
        {
            self.expr(value, out);
            return;
        }

        if let Expr::Closure { params, body, .. } = expr {
            let constants = self.constants.clone();
            let types = self.types.clone();
            for (name, typ) in params {
                self.constants.remove(name);
                self.types.insert(name.clone(), typ.clone());
            }
            self.body(body);
            self.constants = constants;
            self.types = types;
            return;
        }
        if !expr_needs_spills(expr, &self.facts) || is_variable_modify(expr) {
            return;
        }
        if is_repeat(expr) {
            let Expr::Call { args, .. } = expr else {
                unreachable!()
            };
            if args.len() != 2 {
                return;
            }
            let Some(count) = self.count(&args[1]) else {
                return;
            };
            let mut value = args[0].clone();
            self.expr(&mut value, out);
            let typ = self.expr_type(&value);
            let saved = self.save(value, out);
            if count == 0 {
                let name = self.fresh();
                let typ = Typ::Array(Box::new(typ.unwrap_or(Typ::Generic("_".into()))));
                self.types.insert(name.clone(), typ.clone());
                out.push(Stmt::Let(name.clone(), Some(typ), Expr::ArrayLit(vec![])));
                *expr = Expr::Ident(name);
            } else {
                *expr = Expr::ArrayLit(vec![saved; count]);
            }
            return;
        }
        if let Expr::Binary { op, lhs, rhs } = expr
            && matches!(op.as_str(), "&&" | "||")
        {
            self.expr(lhs, out);
            let saved = self.save((**lhs).clone(), out);
            let mut branch = Vec::new();
            self.expr(rhs, &mut branch);
            let Expr::Ident(name) = &saved else {
                unreachable!()
            };
            branch.push(Stmt::Assign(name.clone(), (**rhs).clone()));
            let cond = if op == "&&" {
                saved.clone()
            } else {
                Expr::Unary {
                    op: "!".into(),
                    expr: Box::new(saved.clone()),
                }
            };
            out.push(Stmt::If {
                cond,
                then_body: branch,
                else_body: vec![],
            });
            *expr = saved;
            return;
        }
        // Save each child in order so a later child's spills cannot run before
        // earlier calls, reads, or array allocations. A direct callee is a
        // symbol, not a first-class value to materialize.
        if let Expr::Call { callee, args } = expr {
            if !matches!(callee.as_ref(), Expr::Ident(_)) {
                self.expr(callee, out);
            }
            let receiver = if let Expr::Ident(name) = callee.as_ref() {
                value_receiver_index(name, &self.facts)
            } else {
                None
            };
            let vector_source = if matches!(callee.as_ref(), Expr::Ident(name) if name == "vector-extend")
            {
                args.first().and_then(|arg| self.expr_type(arg))
            } else {
                None
            };
            for (index, arg) in args.iter_mut().enumerate() {
                self.expr(arg, out);
                if receiver != Some(index) || !is_addressable(arg) {
                    let literal_vector = index == 1 && matches!(arg, Expr::ArrayLit(_));
                    *arg = self.save(arg.clone(), out);
                    if literal_vector
                        && let Some(expected) = vector_source.as_ref()
                        && let Some(Stmt::Let(name, typ, _)) = out.last_mut()
                    {
                        *typ = Some(expected.clone());
                        self.types.insert(name.clone(), expected.clone());
                    }
                }
            }
        } else {
            crate::core_ir::for_each_expr_child_mut(expr, &mut |child| {
                self.expr(child, out);
                *child = self.save(child.clone(), out);
            });
        }
    }

    fn body(&mut self, body: &mut Vec<Stmt>) {
        let constants = self.constants.clone();
        let types = self.types.clone();
        let mut out = Vec::with_capacity(body.len());
        for mut stmt in std::mem::take(body) {
            let mut prefix = Vec::new();
            if let Stmt::Expr(expr) = &stmt
                && is_index_store(expr)
                && let Expr::Call { callee, args } = expr
                && ((matches!(callee.as_ref(), Expr::Ident(name) if name == "index-assign-value-first")
                    && args.len() == 3)
                    || (matches!(callee.as_ref(), Expr::Ident(name) if name == "index-modify-value-first" || name == "index-modify-place-first")
                        && args.len() == 4
                        && matches!(&args[3], Expr::StringLit(op) if matches!(op.as_str(), "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>"))))
            {
                let place_first = matches!(callee.as_ref(), Expr::Ident(name) if name == "index-modify-place-first");
                let mut value = args[2].clone();
                if !place_first {
                    self.expr(&mut value, &mut prefix);
                    value = self.save(value, &mut prefix);
                }
                let mut base = args[0].clone();
                self.expr(&mut base, &mut prefix);
                let base = self.save(base, &mut prefix);
                let mut index = args[1].clone();
                self.expr(&mut index, &mut prefix);
                let index = self.save(index, &mut prefix);
                let mut read = Expr::Index {
                    base: Box::new(base.clone()),
                    index: Box::new(index.clone()),
                };
                if place_first {
                    // .in compound updates read the place before RHS effects.
                    read = self.save(read, &mut prefix);
                    self.expr(&mut value, &mut prefix);
                    value = self.save(value, &mut prefix);
                }
                let value = if let Some(Expr::StringLit(op)) = args.get(3) {
                    Expr::Binary {
                        op: op.clone(),
                        lhs: Box::new(read),
                        rhs: Box::new(value),
                    }
                } else {
                    value
                };
                stmt = Stmt::IndexAssign { base, index, value };
            }
            if let Stmt::Expr(expr) = &stmt
                && is_field_modify(expr)
                && let Expr::Call { args, .. } = expr
                && args.len() == 4
                && let (Expr::StringLit(field), Expr::StringLit(op)) = (&args[1], &args[3])
                && matches!(
                    op.as_str(),
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>"
                )
            {
                let mut value = args[2].clone();
                self.expr(&mut value, &mut prefix);
                let value = self.save(value, &mut prefix);
                // Keep the original place. Saving a struct as a value would
                // make a copy and update that copy instead of the source object.
                let base = args[0].clone();
                stmt = Stmt::FieldAssign {
                    base: base.clone(),
                    name: field.clone(),
                    value: Expr::Binary {
                        op: op.clone(),
                        lhs: Box::new(Expr::Field {
                            base: Box::new(base),
                            name: field.clone(),
                        }),
                        rhs: Box::new(value),
                    },
                };
            }
            if let Stmt::Expr(expr) = &stmt
                && is_variable_modify(expr)
                && let Expr::Call { args, .. } = expr
                && args.len() == 3
                && let (Expr::Ident(name), Expr::StringLit(op)) = (&args[0], &args[2])
                && matches!(
                    op.as_str(),
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>"
                )
            {
                let mut value = args[1].clone();
                self.expr(&mut value, &mut prefix);
                let value = self.save(value, &mut prefix);
                stmt = Stmt::Assign(
                    name.clone(),
                    Expr::Binary {
                        op: op.clone(),
                        lhs: Box::new(Expr::Ident(name.clone())),
                        rhs: Box::new(value),
                    },
                );
            }
            match &mut stmt {
                Stmt::Let(_, _, expr)
                | Stmt::Assign(_, expr)
                | Stmt::Expr(expr)
                | Stmt::Return(Some(expr))
                | Stmt::Throw(expr) => self.expr(expr, &mut prefix),
                Stmt::IndexAssign { base, index, value } => {
                    let field_header = matches!(base, Expr::Field { .. })
                        && matches!(
                            self.expr_type(base).map(|typ| typ.canonical()),
                            Some(Typ::Vector(_) | Typ::Int)
                        );
                    if field_header
                        || [&*base, &*index, &*value].into_iter().any(|expr| {
                            expr_contains_call(expr) || expr_needs_spills(expr, &self.facts)
                        })
                    {
                        for expr in [base, index, value] {
                            self.expr(expr, &mut prefix);
                            *expr = self.save(expr.clone(), &mut prefix);
                        }
                    }
                }
                Stmt::FieldAssign { base, value, .. } => {
                    if expr_needs_spills(base, &self.facts) || expr_needs_spills(value, &self.facts)
                    {
                        self.expr(base, &mut prefix);
                        if !is_addressable(base) {
                            *base = self.save(base.clone(), &mut prefix);
                        }
                        self.expr(value, &mut prefix);
                        *value = self.save(value.clone(), &mut prefix);
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    self.expr(cond, &mut prefix);
                    self.body(then_body);
                    self.body(else_body);
                }
                Stmt::Loop { kind, cond, body } => {
                    if let Some(cond) = cond {
                        self.expr(cond, &mut prefix);
                        if let LoopKind::For { binding } = kind {
                            let typ = match self.expr_type(cond) {
                                Some(Typ::Array(item) | Typ::Vector(item)) => Some(*item),
                                _ => None,
                            };
                            self.pattern_body(body, binding, typ);
                        } else {
                            self.body(body);
                        }
                        if !prefix.is_empty() && matches!(kind, LoopKind::While) {
                            prefix.push(Stmt::If {
                                cond: Expr::Unary {
                                    op: "!".into(),
                                    expr: Box::new(cond.clone()),
                                },
                                then_body: vec![Stmt::Break],
                                else_body: vec![],
                            });
                            prefix.append(body);
                            *body = std::mem::take(&mut prefix);
                            *kind = LoopKind::Infinite;
                            // Infinite loops ignore this condition in native lowering.
                            *cond = Expr::BoolLit(true);
                        }
                    } else {
                        self.body(body);
                    }
                }
                Stmt::Match { scrutinee, arms } => {
                    self.expr(scrutinee, &mut prefix);
                    let typ = self.expr_type(scrutinee);
                    for arm in arms {
                        self.pattern_body(&mut arm.body, &arm.pattern, typ.clone());
                    }
                }
                Stmt::Try { body, catches } => {
                    self.body(body);
                    for catch in catches {
                        self.pattern_body(&mut catch.body, &catch.pattern, None);
                    }
                }
                _ => {}
            }
            if let Stmt::Let(name, typ, expr) = &stmt {
                // Initializers still see the previous binding; the new local
                // shadows a compile-time global only after its initialization.
                self.constants.remove(name);
                if let Some(typ) = typ.clone().or_else(|| self.expr_type(expr)) {
                    self.types.insert(name.clone(), typ);
                } else {
                    self.types.remove(name);
                }
            }
            out.extend(prefix);
            out.push(stmt);
        }
        *body = out;
        self.constants = constants;
        self.types = types;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_variable_updates_remain_unresolved() {
        for args in [
            vec![Expr::Ident("value".into()), Expr::IntLit(2)],
            vec![
                Expr::IntLit(1),
                Expr::IntLit(2),
                Expr::StringLit("+".into()),
            ],
            vec![
                Expr::Ident("value".into()),
                Expr::IntLit(2),
                Expr::StringLit("&&".into()),
            ],
            vec![
                Expr::Ident("value".into()),
                Expr::IntLit(2),
                Expr::IntLit(3),
            ],
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(
                "var value: Int = 20;\nfn main() -> Int { return 42; }",
            )
            .unwrap();
            let Decl::Function { body, .. } = module.decls.last_mut().unwrap() else {
                panic!("missing main");
            };
            body.insert(
                0,
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("modify-value-first".into())),
                    args,
                }),
            );
            let original = module.decls.clone();
            lower_value_spills(&mut module.decls);
            assert_eq!(module.decls, original);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(!report.ok, "{report:?}");
            assert_eq!(report.reason_code.as_deref(), Some("unresolved-symbol"));
        }
    }
}
