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
    matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if name == "field-modify-value-first" || name == crate::core_ir::FIELD_MODIFY_PLACE_FIRST))
}

fn is_variable_modify(expr: &Expr) -> bool {
    matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if name == "modify-value-first"))
}

fn is_addressable(expr: &Expr) -> bool {
    match expr {
        Expr::Ident(_) => true,
        Expr::Field { base, .. } | Expr::Index { base, .. } => is_addressable(base),
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
    if matches!(expr, Expr::Field { base, .. } if !is_addressable(base)) {
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

// Distinguish an invalid checked constant from a genuinely runtime count.
// constant_int alone returns None for both arithmetic overflow and unknown values.
fn numeric_constant_shape(expr: &Expr, constants: &HashMap<String, i64>) -> bool {
    match expr {
        Expr::IntLit(_) => true,
        Expr::Ident(name) => constants.contains_key(name),
        Expr::Unary { op, expr } if matches!(op.as_str(), "-" | "neg" | "~" | "bitnot") => {
            numeric_constant_shape(expr, constants)
        }
        Expr::Binary { op, lhs, rhs }
            if matches!(
                op.as_str(),
                "+" | "add"
                    | "-"
                    | "sub"
                    | "*"
                    | "mul"
                    | "/"
                    | "div"
                    | "%"
                    | "mod"
                    | "&"
                    | "band"
                    | "|"
                    | "bor"
                    | "^"
                    | "xor"
                    | "<<"
                    | "shl"
                    | ">>"
                    | "shr"
            ) =>
        {
            numeric_constant_shape(lhs, constants) && numeric_constant_shape(rhs, constants)
        }
        _ => false,
    }
}

fn constant_int(expr: &Expr, constants: &HashMap<String, i64>) -> Option<i64> {
    match expr {
        Expr::Ident(name) => constants.get(name).copied(),
        Expr::Unary { op, expr } => {
            let value = constant_int(expr, constants)?;
            match op.as_str() {
                "-" | "neg" => value.checked_neg(),
                "~" | "bitnot" => Some(!value),
                _ => None,
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let (lhs, rhs) = (constant_int(lhs, constants)?, constant_int(rhs, constants)?);
            match op.as_str() {
                "+" | "add" => lhs.checked_add(rhs),
                "-" | "sub" => lhs.checked_sub(rhs),
                "*" | "mul" => lhs.checked_mul(rhs),
                "/" | "div" => lhs.checked_div(rhs),
                "%" | "mod" => lhs.checked_rem(rhs),
                "&" | "band" => Some(lhs & rhs),
                "|" | "bor" => Some(lhs | rhs),
                "^" | "xor" => Some(lhs ^ rhs),
                "<<" | "shl" => lhs.checked_shl(u32::try_from(rhs).ok()?),
                ">>" | "shr" => lhs.checked_shr(u32::try_from(rhs).ok()?),
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

    fn stage_receiver_place(&mut self, expr: &mut Expr, out: &mut Vec<Stmt>) {
        match expr {
            Expr::Index { base, index } => {
                self.expr(base, out);
                **base = self.save((**base).clone(), out);
                self.expr(index, out);
                **index = self.save((**index).clone(), out);
            }
            Expr::Field { base, .. } => self.stage_receiver_place(base, out),
            Expr::Call { callee, args } if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::BORROW) => {
                if let [value] = args.as_mut_slice() {
                    self.stage_receiver_place(value, out);
                }
            }
            _ => self.expr(expr, out),
        }
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
                    if self.reject_invalid_repeat(expr) {
                        continue;
                    }
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

    fn reject_invalid_repeat(&self, expr: &mut Expr) -> bool {
        let Expr::Call { callee, args } = expr else {
            return false;
        };
        if !matches!(callee.as_ref(), Expr::Ident(name) if name == "array-repeat")
            || self.facts.function_signature("array-repeat").is_some()
            || args.len() != 2
        {
            return false;
        }
        if numeric_constant_shape(&args[1], &self.constants) && self.count(&args[1]).is_none() {
            // Keep this refusal stable through later machine-word optimization:
            // a huge structural size must never wrap into a small/empty array.
            **callee = Expr::Ident("__unsupported_array_repeat_count".into());
            return true;
        }
        false
    }

    fn count(&self, expr: &Expr) -> Option<usize> {
        let count = usize::try_from(constant_int(expr, &self.constants)?).ok()?;
        (count <= MAX_EXPANDED_ITEMS).then_some(count)
    }

    fn expr(&mut self, expr: &mut Expr, out: &mut Vec<Stmt>) {
        if self.reject_invalid_repeat(expr) {
            return;
        }
        // Failed compile-time evaluation must remain a refusal, never runtime work.
        if matches!(expr,Expr::Call {callee,..} if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::INLINE_CONST))
        {
            return;
        }

        if let Expr::MatchValue { scrutinee, arms } = expr {
            self.expr(scrutinee, out);
            let typ = self.expr_type(scrutinee);
            for arm in arms {
                let types = self.types.clone();
                let constants = self.constants.clone();
                if let Some(typ) = &typ {
                    if let Ok(pattern) = crate::core_ir::MatchPattern::parse(&arm.pattern) {
                        if let Ok(bindings) =
                            pattern.typed_bindings(typ, &|name| self.facts.pattern_fields(name))
                        {
                            for (name, typ) in bindings {
                                self.constants.remove(&name);
                                self.types.insert(name, typ);
                            }
                        }
                    }
                }
                for value in arm.guard.iter_mut().chain(std::iter::once(&mut arm.value)) {
                    let mut body = Vec::new();
                    self.expr(value, &mut body);
                    if !body.is_empty() {
                        *value = Expr::Block {
                            body,
                            result: Some(Box::new(value.clone())),
                        };
                    }
                }
                self.types = types;
                self.constants = constants;
            }
            return;
        }
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
                if receiver == Some(index) {
                    // Freeze indexed places before later argument prefixes, without copying the receiver.
                    self.stage_receiver_place(arg, out);
                } else {
                    self.expr(arg, out);
                }
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
                && let Expr::Call { callee, args } = expr
                && args.len() == 4
                && let (Expr::StringLit(field), Expr::StringLit(op)) = (&args[1], &args[3])
                && matches!(
                    op.as_str(),
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "<<" | ">>"
                )
            {
                let place_first = matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::FIELD_MODIFY_PLACE_FIRST);
                let spill_rhs = !place_first || expr_needs_spills(&args[2], &self.facts);
                let mut base = args[0].clone();
                // An indexed place must retain its array alias and index across
                // RHS prefixes; saving the Named receiver would mutate a copy.
                let previous = if place_first {
                    let mut capture_place =
                        !is_addressable(&base) || crate::core_opt::expr_has_effects(&args[2]);
                    crate::core_ir::for_each_expr_child(&base, &mut |child| {
                        capture_place |= crate::core_opt::expr_has_effects(child);
                    });
                    // Pure repeated updates need no extra capture slots.
                    if capture_place {
                        self.stage_receiver_place(&mut base, &mut prefix);
                    }
                    if !is_addressable(&base) {
                        base = self.save(base, &mut prefix);
                    }
                    spill_rhs.then(|| {
                        self.save(
                            Expr::Field {
                                base: Box::new(base.clone()),
                                name: field.clone(),
                            },
                            &mut prefix,
                        )
                    })
                } else {
                    None
                };
                let mut value = args[2].clone();
                // Ordinary binary evaluation already reads the field first;
                // materialize it only when RHS prefixes would move ahead of it.
                if spill_rhs {
                    self.expr(&mut value, &mut prefix);
                    value = self.save(value, &mut prefix);
                }
                // Rust's value-first marker retains the original delayed read.
                let previous = previous.unwrap_or_else(|| Expr::Field {
                    base: Box::new(base.clone()),
                    name: field.clone(),
                });
                stmt = Stmt::FieldAssign {
                    base,
                    name: field.clone(),
                    value: Expr::Binary {
                        op: op.clone(),
                        lhs: Box::new(previous),
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
    #[test]
    fn structural_repeat_checks_core_ir_operator_aliases() {
        let constants = HashMap::new();
        for (op, lhs, rhs, expected) in [
            ("add", 1, 1, Some(2)),
            ("mul", i64::MAX, 2, None),
            ("div", 1, 0, None),
            ("shl", 1, 64, None),
        ] {
            let expression = Expr::Binary {
                op: op.into(),
                lhs: Box::new(Expr::IntLit(lhs)),
                rhs: Box::new(Expr::IntLit(rhs)),
            };
            assert!(numeric_constant_shape(&expression, &constants));
            assert_eq!(constant_int(&expression, &constants), expected);
        }
    }

    #[test]
    fn structural_repeat_refusal_survives_target_word_optimization() {
        use crate::emit_profile::EmitProfile::{Default, Harden, Lean};
        use crate::integer_semantics::IntegerSemantics::{Word32, Word64};
        for semantics in [Word32, Word64] {
            for profile in [Default, Lean, Harden] {
                for count in [
                    "0xffffffff+1",
                    "4097",
                    "-1",
                    "(2147483647*2147483647)*4",
                    "1/0",
                ] {
                    for source in [
                        format!(
                            "fn main()->Int{{let values=array-repeat(42,{count});return array_len(values);}}"
                        ),
                        format!(
                            "var values:[Int]=array-repeat(42,{count});\nfn main()->Int{{return array_len(values);}}"
                        ),
                    ] {
                        let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                        crate::lower_core::desugar_module_with_semantics(&mut module, semantics);
                        crate::core_opt::optimize_with_linkage_and_semantics(
                            &mut module.decls,
                            Some("main"),
                            profile,
                            false,
                            semantics,
                        );
                        let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
                        assert!(!report.ok, "{semantics:?}/{profile:?}: {source}");
                    }
                }
                let mut module = crate::in_lang_parse::parse_in_source(
                    "fn main()->Int{let values=array-repeat(21,2);return values[0]+values[1];}",
                )
                .unwrap();
                crate::lower_core::desugar_module_with_semantics(&mut module, semantics);
                crate::core_opt::optimize_with_linkage_and_semantics(
                    &mut module.decls,
                    Some("main"),
                    profile,
                    false,
                    semantics,
                );
                let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
                assert!(report.ok, "{semantics:?}/{profile:?}: {report:?}");
            }
        }
    }

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

#[cfg(test)]
mod indexed_receiver_place_tests {
    use super::*;

    #[test]
    fn indexed_receiver_stages_base_and_index_before_later_argument_spills() {
        for form in ["direct", "borrow", "field"] {
            let (schemas, storage, receiver) = if form == "field" {
                (
                    "struct Row{Int value}\nstruct Holder{Row inner}",
                    "var rows:[Holder]=[Holder{inner:Row{value:42}}]\nfn get()->[Holder]{return rows;}",
                    "get()[at()].inner",
                )
            } else {
                (
                    "struct Row{Int value}",
                    "var rows:[Row]=[Row{value:42}]\nfn get()->[Row]{return rows;}",
                    "get()[at()]",
                )
            };
            let source = format!(
                "{schemas}\n{storage}\nfn at()->Int{{return 0;}}\nfn next()->Int{{return 1;}}\nfn change(self:Row,values:[Int])->Void{{return;}}\nfn main()->Int{{change({receiver},array-repeat(next(),2));return 42;}}"
            );
            let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            if form == "borrow" {
                let Decl::Function { body, .. } = module.decls.last_mut().unwrap() else {
                    panic!("missing main")
                };
                let Stmt::Expr(Expr::Call { args, .. }) = &mut body[0] else {
                    panic!("missing call")
                };
                args[0] = Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::BORROW.into())),
                    args: vec![args[0].clone()],
                };
            }
            lower_value_spills(&mut module.decls);
            let body = module
                .decls
                .iter()
                .find_map(|decl| match decl {
                    Decl::Function { name, body, .. } if name == "main" => Some(body),
                    _ => None,
                })
                .unwrap();
            let calls: Vec<_> = body
                .iter()
                .filter_map(|stmt| match stmt {
                    Stmt::Let(_, _, Expr::Call { callee, .. }) => match callee.as_ref() {
                        Expr::Ident(name) => Some(name.as_str()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            assert_eq!(calls, ["get", "at", "next"], "{form}");
            let args=body.iter().find_map(|stmt|match stmt {
                Stmt::Expr(Expr::Call{callee,args}) if matches!(callee.as_ref(),Expr::Ident(name) if name=="change")=>Some(args),
                _=>None,
            }).unwrap();
            let place = match (form, &args[0]) {
                ("borrow", Expr::Call { callee, args }) if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::BORROW) => {
                    &args[0]
                }
                ("field", Expr::Field { base, name }) if name == "inner" => base.as_ref(),
                ("direct", place) => place,
                _ => panic!("receiver wrapper changed: {form}"),
            };
            let Expr::Index { base, index } = place else {
                panic!("receiver was copied into a value: {form}")
            };
            let (Expr::Ident(base), Expr::Ident(index)) = (base.as_ref(), index.as_ref()) else {
                panic!("place operands were not staged: {form}")
            };
            for (name, call) in [(base, "get"), (index, "at")] {
                assert!(body.iter().any(|stmt|matches!(stmt,Stmt::Let(saved,_,Expr::Call{callee,..}) if saved==name && matches!(callee.as_ref(),Expr::Ident(name) if name==call))),"{form}");
            }
            assert!(
                !body.iter().any(
                    |stmt| matches!(stmt,Stmt::Let(_,Some(Typ::Named(name)),_) if name=="Row")
                ),
                "{form}"
            );
            let once = module.decls.clone();
            lower_value_spills(&mut module.decls);
            assert_eq!(module.decls, once, "{form}");
        }
    }

    #[test]
    fn pure_indexed_receiver_needs_no_value_copy_spill() {
        let source = "struct Row{Int value}\nfn change(self:Row)->Void{return;}\nfn main()->Int{let rows:[Row]=[Row{value:41}];change(rows[0]);return rows[0].value;}";
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        let original = module.decls.clone();
        lower_value_spills(&mut module.decls);
        assert_eq!(module.decls, original);
    }
}

#[cfg(test)]
mod compound_field_place_tests {
    use super::*;

    fn main_body(decls: &[Decl]) -> &[Stmt] {
        decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function { name, body, .. } if name == "main" => Some(body.as_slice()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn place_first_saves_array_index_and_old_field_before_rhs_once() {
        let source = "struct Row{Int x}\nvar rows:[Row]=[Row{x:40}]\nfn get()->[Row]{return rows;}\nfn at()->Int{return 0;}\nfn effect()->Int{return 2;}\nfn next(values:[Int])->Int{return values[0];}\nfn main()->Int{get()[at()].x+=next(array-repeat(effect(),1));return rows[0].x;}";
        let canonical = crate::in_canonical::canonicalize_in_source(source).unwrap();
        for source in [source, canonical.as_str()] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            lower_value_spills(&mut module.decls);
            let body = main_body(&module.decls);
            let calls: Vec<_> = body
                .iter()
                .filter_map(|stmt| match stmt {
                    Stmt::Let(_, _, Expr::Call { callee, .. }) => match callee.as_ref() {
                        Expr::Ident(name) => Some(name.as_str()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            assert_eq!(calls, ["get", "at", "effect", "next"]);
            let old = body.iter().position(|stmt| matches!(stmt,
                Stmt::Let(_, _, Expr::Field {base, name}) if name == "x" && matches!(base.as_ref(), Expr::Index {base,index} if matches!(base.as_ref(),Expr::Ident(_)) && matches!(index.as_ref(),Expr::Ident(_)))
            )).unwrap();
            let rhs = body.iter().position(|stmt| matches!(stmt,
                Stmt::Let(_,_,Expr::Call {callee,..}) if matches!(callee.as_ref(),Expr::Ident(name) if name=="next")
            )).unwrap();
            assert!(old < rhs);
            assert!(body.iter().any(|stmt| matches!(stmt,
                Stmt::FieldAssign {base:Expr::Index {base,index},value:Expr::Binary {lhs,rhs,..},..}
                if matches!(base.as_ref(),Expr::Ident(_)) && matches!(index.as_ref(),Expr::Ident(_)) && matches!(lhs.as_ref(),Expr::Ident(_)) && matches!(rhs.as_ref(),Expr::Ident(_))
            )));
            assert!(
                !body.iter().any(
                    |stmt| matches!(stmt,Stmt::Let(_,Some(Typ::Named(name)),_) if name=="Row")
                )
            );
            let once = module.decls.clone();
            lower_value_spills(&mut module.decls);
            assert_eq!(module.decls, once);
        }
    }

    #[test]
    fn rust_value_first_marker_keeps_rhs_before_old_field_read() {
        let source = "struct Row{Int x}\nvar row:Row=Row{x:20}\nfn next()->Int{return 22;}\nfn main()->Int{field-modify-value-first(row,\"x\",next(),\"+\");return row.x;}";
        let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
        lower_value_spills(&mut module.decls);
        let body = main_body(&module.decls);
        assert!(
            matches!(&body[0],Stmt::Let(_,_,Expr::Call {callee,..}) if matches!(callee.as_ref(),Expr::Ident(name) if name=="next"))
        );
        assert!(
            matches!(&body[1],Stmt::FieldAssign {value:Expr::Binary {lhs,..},..} if matches!(lhs.as_ref(),Expr::Field {..}))
        );
    }
}
