//! Fold bounded, pure global initializer calls before native data layout.
use crate::core_ir::{Decl, Expr, Stmt, Typ};
use crate::integer_semantics::IntegerSemantics;
use std::collections::{HashMap, HashSet};

fn value_nodes(value: &Expr) -> usize {
    match value {
        Expr::ArrayLit(items) => 1 + items.iter().map(value_nodes).sum::<usize>(),
        Expr::StructInit { fields, .. } => {
            1 + fields
                .iter()
                .map(|(_, value)| value_nodes(value))
                .sum::<usize>()
        }
        _ => 1,
    }
}

fn value_bytes(value: &Expr) -> usize {
    match value {
        Expr::ArrayLit(items) => 64 + items.iter().map(value_bytes).sum::<usize>(),
        Expr::StringLit(value) => 64 + value.len(),
        Expr::StructInit { fields, .. } => {
            64 + fields
                .iter()
                .map(|(name, value)| name.len() + value_bytes(value))
                .sum::<usize>()
        }
        _ => 64,
    }
}
fn depth_bounded(value: &Expr, depth: usize) -> bool {
    depth < 128
        && match value {
            Expr::ArrayLit(items) => items.iter().all(|item| depth_bounded(item, depth + 1)),
            Expr::StructInit { fields, .. } => fields
                .iter()
                .all(|(_, value)| depth_bounded(value, depth + 1)),
            _ => true,
        }
}
fn bounded(value: &Expr) -> bool {
    depth_bounded(value, 0) && value_nodes(value) <= 4096 && value_bytes(value) <= 262144
}

fn value_type(value: &Expr) -> Option<Typ> {
    Some(match value {
        Expr::UnitLit => Typ::Void,
        Expr::IntLit(_) => Typ::Int,
        Expr::BoolLit(_) => Typ::Bool,
        Expr::FloatLit(_) => Typ::Float,
        Expr::StringLit(_) => Typ::String,
        Expr::StructInit { name, .. } => Typ::Named(name.clone()),
        Expr::ArrayLit(items) => Typ::Array(Box::new(
            items
                .iter()
                .filter(|item| !crate::core_ir::empty_sequence_literal(item))
                .find_map(value_type)
                .unwrap_or(Typ::Void),
        )),
        _ => return None,
    })
}

fn matches_value(typ: &Typ, value: &Expr) -> bool {
    if typ.is_any() {
        return true;
    }
    match (typ.canonical(), value) {
        (Typ::Array(elem) | Typ::Vector(elem), Expr::ArrayLit(items)) => {
            items.iter().all(|item| matches_value(&elem, item))
        }
        (expected, value) => {
            value_type(value).is_some_and(|actual| expected.compatible_with(&actual))
        }
    }
}

fn const_syntax(
    value: &Expr,
    globals: &HashMap<String, Option<Expr>>,
    functions: &HashSet<String>,
    locals: &HashSet<String>,
    depth: usize,
    fuel: &mut usize,
) -> bool {
    if depth >= 128 || *fuel == 0 {
        return false;
    }
    *fuel -= 1;
    let visit = |value: &Expr, scope: &HashSet<String>, fuel: &mut usize| {
        const_syntax(value, globals, functions, scope, depth + 1, fuel)
    };
    match value {
        Expr::Ident(name) => {
            locals.contains(name)
                || globals
                    .get(name)
                    .and_then(Option::as_ref)
                    .is_some_and(|init| visit(init, &HashSet::new(), fuel))
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::BoolLit(_)
        | Expr::StringLit(_) => true,
        Expr::Closure { .. } => false,
        Expr::Call { callee, args } => {
            let Expr::Ident(name) = callee.as_ref() else {
                return false;
            };
            if name == crate::core_ir::INLINE_CONST {
                return args.len() == 1 && visit(&args[0], &HashSet::new(), fuel);
            }
            name == crate::core_ir::ARRAY_REPEAT
                && !locals.contains(name)
                && !globals.contains_key(name)
                && !functions.contains(name)
                && args.len() == 2
                && args.iter().all(|arg| visit(arg, locals, fuel))
        }
        Expr::Block { body, result } => {
            let mut scope = locals.clone();
            if !const_body_syntax(body, globals, functions, &mut scope, depth + 1, fuel) {
                return false;
            }
            result
                .as_deref()
                .is_none_or(|value| visit(value, &scope, fuel))
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => [cond, then_expr, else_expr]
            .into_iter()
            .all(|value| visit(value, locals, fuel)),
        Expr::Unary { expr, .. } => visit(expr, locals, fuel),
        Expr::Binary { lhs, rhs, .. } => visit(lhs, locals, fuel) && visit(rhs, locals, fuel),
        Expr::ArrayLit(items) => items.iter().all(|value| visit(value, locals, fuel)),
        Expr::StructInit { fields, .. } => {
            fields.iter().all(|(_, value)| visit(value, locals, fuel))
        }
        Expr::Field { base, .. } => visit(base, locals, fuel),
        Expr::Index { base, index } => visit(base, locals, fuel) && visit(index, locals, fuel),
        _ => false,
    }
}
fn const_body_syntax(
    body: &[Stmt],
    globals: &HashMap<String, Option<Expr>>,
    functions: &HashSet<String>,
    locals: &mut HashSet<String>,
    depth: usize,
    fuel: &mut usize,
) -> bool {
    if depth >= 128 {
        return false;
    }
    for stmt in body {
        if *fuel == 0 {
            return false;
        }
        *fuel -= 1;
        let visit = |value: &Expr, scope: &HashSet<String>, fuel: &mut usize| {
            const_syntax(value, globals, functions, scope, depth + 1, fuel)
        };
        let valid = match stmt {
            Stmt::Let(name, _, value) => {
                let valid = visit(value, locals, fuel);
                locals.insert(name.clone());
                valid
            }
            Stmt::Assign(name, value) => locals.contains(name) && visit(value, locals, fuel),
            Stmt::IndexAssign { base, index, value } => [base, index, value]
                .into_iter()
                .all(|value| visit(value, locals, fuel)),
            Stmt::Expr(Expr::Call { callee, args }) if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::INDEX_ASSIGN_VALUE_FIRST && !locals.contains(name) && !globals.contains_key(name) && !functions.contains(name)) => {
                matches!(args.as_slice(),[Expr::Ident(name),_,_] if locals.contains(name))
                    && args.iter().all(|arg| visit(arg, locals, fuel))
            }
            Stmt::Expr(value) => visit(value, locals, fuel),
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                visit(cond, locals, fuel)
                    && const_body_syntax(
                        then_body,
                        globals,
                        functions,
                        &mut locals.clone(),
                        depth + 1,
                        fuel,
                    )
                    && const_body_syntax(
                        else_body,
                        globals,
                        functions,
                        &mut locals.clone(),
                        depth + 1,
                        fuel,
                    )
            }
            Stmt::Loop {
                cond: Some(cond),
                body,
                kind: crate::core_ir::LoopKind::While,
            } => {
                visit(cond, locals, fuel)
                    && const_body_syntax(
                        body,
                        globals,
                        functions,
                        &mut locals.clone(),
                        depth + 1,
                        fuel,
                    )
            }
            _ => false,
        };
        if !valid {
            return false;
        }
    }
    true
}

struct Eval<'a> {
    globals: &'a HashMap<String, Option<Expr>>,
    functions: &'a HashSet<String>,
    active: HashSet<String>,
    fuel: usize,
    depth: usize,
    strict: bool,
    invalid_arithmetic: bool,
    semantics: IntegerSemantics,
}
impl Eval<'_> {
    fn step(&mut self) -> Option<()> {
        self.fuel = self.fuel.checked_sub(1)?;
        Some(())
    }
    fn expr(&mut self, expr: &Expr, env: &mut HashMap<String, (Expr, Typ)>) -> Option<Expr> {
        if self.depth >= 128 {
            return None;
        }
        self.depth += 1;
        let result = self.expr_inner(expr, env);
        self.depth -= 1;
        result
    }
    fn expr_inner(&mut self, expr: &Expr, env: &mut HashMap<String, (Expr, Typ)>) -> Option<Expr> {
        self.step()?;
        let value = match expr {
            Expr::Block { body, result } => {
                let mut inner = env.clone();
                let value = match self.body(body, &mut inner)? {
                    Some(value) => value,
                    None => match result {
                        Some(result) => self.expr(result, &mut inner)?,
                        None => Expr::UnitLit,
                    },
                };
                for (name, value) in env.iter_mut() {
                    *value = inner.get(name)?.clone();
                }
                return Some(value);
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => {
                let Expr::BoolLit(cond) = self.expr(cond, env)? else {
                    return None;
                };
                return self.expr(if cond { then_expr } else { else_expr }, env);
            }
            Expr::StructInit { name, fields } => {
                let mut values = Vec::new();
                let mut bytes = 64usize;
                let mut nodes = 1usize;
                for (name, value) in fields {
                    let value = self.expr(value, env)?;
                    bytes = bytes
                        .checked_add(name.len())?
                        .checked_add(value_bytes(&value))?;
                    nodes = nodes.checked_add(value_nodes(&value))?;
                    if bytes > 262144 || nodes > 4096 {
                        return None;
                    }
                    values.push((name.clone(), value));
                }
                Expr::StructInit {
                    name: name.clone(),
                    fields: values,
                }
            }
            Expr::Field { base, name } => {
                let Expr::StructInit { fields, .. } = self.expr(base, env)? else {
                    return None;
                };
                return fields
                    .into_iter()
                    .find(|(field, _)| field == name)
                    .map(|(_, value)| value);
            }
            Expr::IntLit(value) => Expr::IntLit(self.semantics.normalize_word(*value)?),
            Expr::UnitLit | Expr::BoolLit(_) | Expr::FloatLit(_) | Expr::StringLit(_) => {
                expr.clone()
            }
            Expr::Ident(name) => {
                if let Some(value) = env.get(name) {
                    return bounded(&value.0).then(|| value.0.clone());
                }
                if !self.active.insert(name.clone()) {
                    return None;
                }
                let init = self.globals.get(name)?.as_ref()?;
                if self.strict
                    && !const_syntax(
                        init,
                        self.globals,
                        self.functions,
                        &HashSet::new(),
                        0,
                        &mut 65536,
                    )
                {
                    self.active.remove(name);
                    return None;
                }
                let value = self.expr(init, &mut HashMap::new());
                self.active.remove(name);
                return value;
            }
            Expr::ArrayLit(items) => {
                let mut values = Vec::new();
                let mut nodes = 1usize;
                let mut bytes = 64usize;
                for item in items {
                    let value = self.expr(item, env)?;
                    nodes = nodes.checked_add(value_nodes(&value))?;
                    bytes = bytes.checked_add(value_bytes(&value))?;
                    if nodes > 4096 || bytes > 262144 {
                        return None;
                    }
                    values.push(value);
                }
                if let Some(expected) = values
                    .iter()
                    .filter(|value| !crate::core_ir::empty_sequence_literal(value))
                    .find_map(value_type)
                {
                    if !values.iter().all(|value| matches_value(&expected, value)) {
                        return None;
                    }
                }
                Expr::ArrayLit(values)
            }
            Expr::Index { base, index } => {
                let Expr::ArrayLit(items) = self.expr(base, env)? else {
                    return None;
                };
                let Expr::IntLit(index) = self.expr(index, env)? else {
                    return None;
                };
                return items.get(usize::try_from(index).ok()?).cloned();
            }
            Expr::Binary { op, lhs, rhs } => {
                let lhs = self.expr(lhs, env)?;
                if matches!(
                    (op.as_str(), &lhs),
                    ("&&", Expr::BoolLit(false)) | ("||", Expr::BoolLit(true))
                ) {
                    return Some(lhs);
                }
                let rhs = self.expr(rhs, env)?;
                if let (Expr::BoolLit(lhs), Expr::BoolLit(rhs)) = (&lhs, &rhs) {
                    if op == "&&" {
                        return Some(Expr::BoolLit(*lhs && *rhs));
                    }
                    if op == "||" {
                        return Some(Expr::BoolLit(*lhs || *rhs));
                    }
                }
                if self.strict {
                    if let (Expr::IntLit(lhs), Expr::IntLit(rhs)) = (&lhs, &rhs) {
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
                                | "<<"
                                | "shl"
                                | ">>"
                                | "shr"
                        ) {
                            let result = self.semantics.fold_binary(op, *lhs, *rhs);
                            self.invalid_arithmetic |= result.is_none();
                            return result;
                        }
                    }
                }
                if let (Expr::StringLit(lhs), Expr::StringLit(rhs)) = (&lhs, &rhs) {
                    if lhs.len().checked_add(rhs.len())? > 262080 {
                        return None;
                    }
                }
                Expr::Binary {
                    op: op.clone(),
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            }
            Expr::Unary { op, expr } => {
                if let Some(kind) = op
                    .strip_prefix("rust-checked-integer-")
                    .or_else(|| op.strip_prefix("rust-wrapping-integer-"))
                {
                    let (operator, lhs, rhs) = match expr.as_ref() {
                        Expr::Binary { op, lhs, rhs } => {
                            let Expr::IntLit(left) = self.expr(lhs, env)? else {
                                return None;
                            };
                            let Expr::IntLit(right) = self.expr(rhs, env)? else {
                                return None;
                            };
                            (op.as_str(), left, Some(right))
                        }
                        Expr::Unary { op, expr } => {
                            let Expr::IntLit(value) = self.expr(expr, env)? else {
                                return None;
                            };
                            (op.as_str(), value, None)
                        }
                        _ => return None,
                    };
                    let evaluate = if op.starts_with("rust-wrapping-integer-") {
                        crate::typecheck::rust_integers::fold_wrapping_integer
                    } else {
                        crate::typecheck::rust_integers::fold_checked_integer
                    };
                    let result = evaluate(kind, operator, lhs, rhs, self.semantics);
                    if crate::core_ir::integer_spec(kind).is_some_and(|(bits, _)| {
                        bits <= if self.semantics == IntegerSemantics::Word32 {
                            32
                        } else {
                            64
                        }
                    }) && matches!(
                        operator,
                        "+" | "-" | "*" | "/" | "%" | "udiv" | "urem" | "<<" | ">>" | "lshr"
                    ) {
                        self.invalid_arithmetic |= result.is_none();
                    }
                    return result;
                }
                let value = self.expr(expr, env)?;
                if self.strict && matches!(op.as_str(), "-" | "neg") {
                    if let Expr::IntLit(value) = value {
                        let result = self.semantics.fold_unary(op, value);
                        self.invalid_arithmetic |= result.is_none();
                        return result;
                    }
                }
                Expr::Unary {
                    op: op.clone(),
                    expr: Box::new(value),
                }
            }
            Expr::Call { callee, args } => {
                if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::INLINE_CONST)
                {
                    if !self.strict {
                        return None;
                    }
                    let [value] = args.as_slice() else {
                        return None;
                    };
                    if !const_syntax(
                        value,
                        self.globals,
                        self.functions,
                        &HashSet::new(),
                        0,
                        &mut 65536,
                    ) {
                        return None;
                    }
                    return self.expr(value, &mut HashMap::new());
                }
                if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::ARRAY_REPEAT && !self.functions.contains(name) && !self.globals.contains_key(name))
                    && args.len() == 2
                {
                    let value = self.expr(&args[0], env)?;
                    // Repeat counts are checked allocation sizes, not machine-word
                    // arithmetic. Restore the value context even if count evaluation fails.
                    let semantics = self.semantics;
                    self.semantics = IntegerSemantics::Word64;
                    let count = self.expr(&args[1], env);
                    self.semantics = semantics;
                    let Expr::IntLit(count) = count? else {
                        return None;
                    };
                    let count = usize::try_from(count).ok()?;
                    if count > 4096
                        || value_nodes(&value).checked_mul(count)? > 4095
                        || value_bytes(&value).checked_mul(count)? > 262080
                    {
                        return None;
                    }
                    Expr::ArrayLit(vec![value; count])
                } else {
                    if self.strict {
                        return None;
                    }
                    let Expr::Closure {
                        params,
                        ret,
                        body,
                        captures,
                        ..
                    } = callee.as_ref()
                    else {
                        return None;
                    };
                    if !args.is_empty() || !params.is_empty() || !captures.is_empty() {
                        return None;
                    }
                    let value = self.body(body, &mut HashMap::new())??;
                    if !matches_value(ret, &value) {
                        return None;
                    }
                    return Some(value);
                }
            }
            _ => return None,
        };
        let value = crate::core_opt::fold_expr_with_semantics(value, self.semantics);
        if !bounded(&value) {
            return None;
        }
        matches!(
            value,
            Expr::UnitLit
                | Expr::IntLit(_)
                | Expr::BoolLit(_)
                | Expr::FloatLit(_)
                | Expr::StringLit(_)
                | Expr::ArrayLit(_)
                | Expr::StructInit { .. }
        )
        .then_some(value)
    }
    fn scope(
        &mut self,
        body: &[Stmt],
        env: &mut HashMap<String, (Expr, Typ)>,
    ) -> Option<Option<Expr>> {
        let mut inner = env.clone();
        let result = self.body(body, &mut inner)?;
        for (name, value) in env.iter_mut() {
            *value = inner.get(name)?.clone();
        }
        Some(result)
    }
    fn body(
        &mut self,
        body: &[Stmt],
        env: &mut HashMap<String, (Expr, Typ)>,
    ) -> Option<Option<Expr>> {
        if self.depth >= 128 {
            return None;
        }
        self.depth += 1;
        let result = self.body_inner(body, env);
        self.depth -= 1;
        result
    }
    fn body_inner(
        &mut self,
        body: &[Stmt],
        env: &mut HashMap<String, (Expr, Typ)>,
    ) -> Option<Option<Expr>> {
        for stmt in body {
            self.step()?;
            match stmt {
                Stmt::Let(name, typ, expr) => {
                    let value = self.expr(expr, env)?;
                    let typ = typ.clone().unwrap_or(value_type(&value)?);
                    if !matches_value(&typ, &value) {
                        return None;
                    }
                    env.insert(name.clone(), (value, typ));
                }
                Stmt::Assign(name, expr) => {
                    let value = self.expr(expr, env)?;
                    let slot = env.get_mut(name)?;
                    if !matches_value(&slot.1, &value) {
                        return None;
                    }
                    slot.0 = value;
                }
                Stmt::IndexAssign { base, index, value } => {
                    let Expr::Ident(name) = base else {
                        return None;
                    };
                    let Expr::IntLit(index) = self.expr(index, env)? else {
                        return None;
                    };
                    let value = self.expr(value, env)?;
                    let slot = env.get_mut(name)?;
                    let Typ::Array(elem) = slot.1.canonical() else {
                        return None;
                    };
                    if !matches_value(&elem, &value) {
                        return None;
                    }
                    let Expr::ArrayLit(items) = &mut slot.0 else {
                        return None;
                    };
                    *items.get_mut(usize::try_from(index).ok()?)? = value;
                }
                Stmt::Return(Some(expr)) if !self.strict => {
                    return Some(Some(self.expr(expr, env)?));
                }
                Stmt::Expr(Expr::Call { callee, args })
                    if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::INDEX_ASSIGN_VALUE_FIRST && !self.functions.contains(name) && !self.globals.contains_key(name))
                        && args.len() == 3 =>
                {
                    let value = self.expr(&args[2], env)?;
                    let index = self.expr(&args[1], env)?;
                    self.body(
                        &[Stmt::IndexAssign {
                            base: args[0].clone(),
                            index,
                            value,
                        }],
                        env,
                    )?;
                }
                Stmt::Expr(expr) => {
                    self.expr(expr, env)?;
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    let Expr::BoolLit(cond) = self.expr(cond, env)? else {
                        return None;
                    };
                    if let Some(value) =
                        self.scope(if cond { then_body } else { else_body }, env)?
                    {
                        return Some(Some(value));
                    }
                }
                Stmt::Loop {
                    cond: Some(cond),
                    body,
                    ..
                } => loop {
                    let Expr::BoolLit(cond) = self.expr(cond, env)? else {
                        return None;
                    };
                    if !cond {
                        break;
                    }
                    if let Some(value) = self.scope(body, env)? {
                        return Some(Some(value));
                    }
                },
                _ => return None,
            }
            if env.values().any(|(value, _)| !bounded(value))
                || env
                    .iter()
                    .map(|(name, (value, _))| name.len() + value_bytes(value))
                    .sum::<usize>()
                    > 262144
                || env
                    .values()
                    .map(|(value, _)| value_nodes(value))
                    .sum::<usize>()
                    > 4096
            {
                return None;
            }
        }
        Some(None)
    }
}

#[cfg(test)]
fn lower_inline_constants(decls: &mut [Decl]) {
    lower_inline_constants_with_semantics(decls, IntegerSemantics::Word64);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConstantEvalError {
    Arithmetic,
}

/// Evaluate a closed value, classifying arithmetic failures only on reached paths.
pub(crate) fn evaluate_pure_constant(
    expr: &Expr,
    semantics: IntegerSemantics,
) -> Result<Option<Expr>, ConstantEvalError> {
    let globals = HashMap::new();
    let functions = HashSet::new();
    if !const_syntax(expr, &globals, &functions, &HashSet::new(), 0, &mut 65536) {
        return Ok(None);
    }
    let mut evaluator = Eval {
        globals: &globals,
        functions: &functions,
        active: HashSet::new(),
        fuel: 65536,
        depth: 0,
        strict: true,
        invalid_arithmetic: false,
        semantics,
    };
    let value = evaluator.expr(expr, &mut HashMap::new());
    if evaluator.invalid_arithmetic {
        Err(ConstantEvalError::Arithmetic)
    } else {
        Ok(value)
    }
}

pub(crate) fn lower_inline_constants_with_semantics(
    decls: &mut [Decl],
    semantics: IntegerSemantics,
) {
    let mut present = false;
    crate::core_opt::map_decl_exprs_mut(decls, &mut |expr| {
        present |= matches!(expr,Expr::Call {callee,..} if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::INLINE_CONST));
    });
    if !present {
        return;
    }
    crate::core_bindings::normalize_decls(decls);
    let checker = crate::typecheck::TypeChecker::new();
    let facts = checker.collect_decl_facts(decls);
    let constants = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Global {
                name,
                typ,
                constant_value: true,
                ..
            } => Some((name.clone(), typ.clone())),
            _ => None,
        })
        .collect();
    let mut globals = HashMap::new();
    let functions = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    for decl in decls.iter() {
        if let Decl::Global {
            name,
            init,
            constant_value: true,
            ..
        } = decl
        {
            if globals.contains_key(name) {
                globals.insert(name.clone(), None);
            } else {
                globals.insert(name.clone(), init.as_deref().cloned());
            }
        }
    }
    crate::core_opt::map_decl_exprs_mut(decls, &mut |expr| {
        if matches!(expr,Expr::Call {callee,..} if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::INLINE_CONST))
        {
            let Expr::Call { args, .. } = expr else {
                unreachable!()
            };
            let [value] = args.as_slice() else {
                return;
            };
            if !checker.constant_value_is_typed(value, &facts, &constants) {
                return;
            }
            let mut eval = Eval {
                globals: &globals,
                functions: &functions,
                active: HashSet::new(),
                fuel: 65536,
                depth: 0,
                strict: true,
                invalid_arithmetic: false,
                semantics,
            };
            if let Some(value) = eval.expr(expr, &mut HashMap::new()) {
                *expr = value;
            }
        }
    });
}

#[cfg(test)]
pub(super) fn fold_static_initializers(decls: &mut [Decl]) {
    fold_static_initializers_with_semantics(decls, IntegerSemantics::Word64);
}

pub(crate) fn fold_static_initializers_with_semantics(
    decls: &mut [Decl],
    semantics: IntegerSemantics,
) {
    let names = crate::core_bindings::module_names(decls);
    let references = crate::core_bindings::function_references(decls);
    for decl in decls.iter_mut() {
        if let Decl::Global {
            init: Some(init), ..
        } = decl
        {
            crate::core_bindings::normalize_initializer_with_references(init, &names, &references);
        }
    }
    let functions = decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let mut globals = HashMap::new();
    for decl in decls.iter() {
        if let Decl::Global {
            name,
            init: Some(init),
            mutable: false,
            ..
        } = decl
        {
            if globals.contains_key(name) {
                globals.insert(name.clone(), None);
            } else {
                globals.insert(name.clone(), Some(init.as_ref().clone()));
            }
        }
    }
    for decl in decls {
        if let Decl::Global {
            init: Some(init), ..
        } = decl
        {
            let mut eval = Eval {
                globals: &globals,
                functions: &functions,
                active: HashSet::new(),
                fuel: 65536,
                depth: 0,
                strict: false,
                invalid_arithmetic: false,
                semantics,
            };
            if let Some(value) = eval.expr(init, &mut HashMap::new()) {
                **init = value;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_word_constants_reach_named_global_and_inline_values() {
        use IntegerSemantics::{Word32, Word64};
        for (semantics, expected_word, expected_branch) in
            [(Word32, i64::from(i32::MIN), 42), (Word64, 2147483648, 0)]
        {
            let mut module = crate::in_lang_parse::parse_in_source(
                "const named:Int=2147483647+1;\nvar selected:Int=if named<0{42}else{0};\nfn main()->Int{return named;}",
            ).unwrap();
            crate::lower_core::desugar_module_with_semantics(&mut module, semantics);
            assert!(
                module.decls.iter().any(|decl| matches!(decl,
                    Decl::Global { name, init: Some(value), .. }
                    if name == "selected" && value.as_ref() == &Expr::IntLit(expected_branch)
                )),
                "{semantics:?}: {:?}",
                module.decls
            );
            crate::core_opt::optimize_with_linkage_and_semantics(
                &mut module.decls,
                Some("main"),
                crate::emit_profile::EmitProfile::Default,
                false,
                semantics,
            );
            let main = module
                .decls
                .iter()
                .find_map(|decl| match decl {
                    Decl::Function { name, body, .. } if name == "main" => Some(body),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                main.as_slice(),
                &[Stmt::Return(Some(Expr::IntLit(expected_word)))]
            );

            let expression = inline(
                vec![Stmt::Let(
                    "local".into(),
                    Some(Typ::Int),
                    Expr::Binary {
                        op: "+".into(),
                        lhs: Box::new(Expr::IntLit(2147483647)),
                        rhs: Box::new(Expr::IntLit(1)),
                    },
                )],
                Expr::Conditional {
                    cond: Box::new(Expr::Binary {
                        op: "<".into(),
                        lhs: Box::new(Expr::Ident("local".into())),
                        rhs: Box::new(Expr::IntLit(0)),
                    }),
                    then_expr: Box::new(Expr::IntLit(42)),
                    else_expr: Box::new(Expr::IntLit(0)),
                },
            );
            let globals = HashMap::new();
            let functions = HashSet::new();
            let result = Eval {
                globals: &globals,
                functions: &functions,
                active: HashSet::new(),
                fuel: 65536,
                depth: 0,
                strict: true,
                invalid_arithmetic: false,
                semantics,
            }
            .expr(&expression, &mut HashMap::new());
            assert_eq!(result, Some(Expr::IntLit(expected_branch)), "{semantics:?}");
        }
    }

    #[test]
    fn repeat_counts_do_not_wrap_as_machine_words() {
        let globals = HashMap::new();
        let functions = HashSet::new();
        for strict in [false, true] {
            let mut eval = Eval {
                globals: &globals,
                functions: &functions,
                active: HashSet::new(),
                fuel: 65536,
                depth: 0,
                strict,
                invalid_arithmetic: false,
                semantics: IntegerSemantics::Word32,
            };
            let repeat = |count| Expr::Call {
                callee: Box::new(Expr::Ident(crate::core_ir::ARRAY_REPEAT.into())),
                args: vec![Expr::IntLit(42), count],
            };
            let huge = Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::IntLit(0xFFFFFFFF)),
                rhs: Box::new(Expr::IntLit(1)),
            };
            assert_eq!(eval.expr(&repeat(huge), &mut HashMap::new()), None);
            assert_eq!(eval.semantics, IntegerSemantics::Word32);
            assert_eq!(
                eval.expr(&repeat(Expr::IntLit(2)), &mut HashMap::new()),
                Some(Expr::ArrayLit(vec![Expr::IntLit(42); 2]))
            );
            assert_eq!(
                eval.expr(&repeat(Expr::IntLit(-1)), &mut HashMap::new()),
                None
            );
        }
    }

    #[test]
    fn target_word_bit_patterns_compare_signed_without_accepting_wide_literals() {
        let globals = HashMap::new();
        let functions = HashSet::new();
        for strict in [false, true] {
            let mut eval = Eval {
                globals: &globals,
                functions: &functions,
                active: HashSet::new(),
                fuel: 65536,
                depth: 0,
                strict,
                invalid_arithmetic: false,
                semantics: IntegerSemantics::Word32,
            };
            let expr = Expr::Binary {
                op: "<".into(),
                lhs: Box::new(Expr::IntLit(0xFFFFFFFF)),
                rhs: Box::new(Expr::IntLit(0)),
            };
            assert_eq!(
                eval.expr(&expr, &mut HashMap::new()),
                Some(Expr::BoolLit(true))
            );
            assert_eq!(
                eval.expr(&Expr::IntLit(0x100000000), &mut HashMap::new()),
                None
            );
        }
    }

    fn inline(body: Vec<Stmt>, result: Expr) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::INLINE_CONST.into())),
            args: vec![Expr::Block {
                body,
                result: Some(Box::new(result)),
            }],
        }
    }
    fn evaluate_inline(expr: &Expr, fuel: usize) -> Option<Expr> {
        let globals = HashMap::new();
        let functions = HashSet::new();
        Eval {
            globals: &globals,
            functions: &functions,
            active: HashSet::new(),
            fuel,
            depth: 0,
            strict: true,
            invalid_arithmetic: false,
            semantics: IntegerSemantics::Word64,
        }
        .expr(expr, &mut HashMap::new())
    }

    #[test]
    fn inline_constant_unit_values_keep_void_type() {
        assert_eq!(
            evaluate_inline(&inline(vec![], Expr::UnitLit), 65536),
            Some(Expr::UnitLit)
        );
        let marker = Expr::Call {
            callee: Box::new(Expr::Ident(crate::core_ir::INLINE_CONST.into())),
            args: vec![Expr::Block {
                body: vec![Stmt::Expr(Expr::IntLit(42))],
                result: None,
            }],
        };
        assert_eq!(evaluate_inline(&marker, 65536), Some(Expr::UnitLit));
        assert!(matches_value(&Typ::Void, &Expr::UnitLit));
        assert!(!matches_value(&Typ::Int, &Expr::UnitLit));
        let mut decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Void,
            type_params: vec![],
            body: vec![Stmt::Return(Some(marker))],
        }];
        lower_inline_constants(&mut decls);
        let Decl::Function { body, .. } = &decls[0] else {
            panic!()
        };
        assert_eq!(body, &vec![Stmt::Return(Some(Expr::UnitLit))]);
    }

    #[test]
    fn inline_constant_ordered_array_store_uses_shared_type_checking() {
        for stored in [Expr::IntLit(22), Expr::BoolLit(false)] {
            let valid = matches!(stored, Expr::IntLit(_));
            let expression = inline(
                vec![
                    Stmt::Let(
                        "a".into(),
                        Some(Typ::Array(Box::new(Typ::Int))),
                        Expr::ArrayLit(vec![Expr::IntLit(20), Expr::IntLit(0)]),
                    ),
                    Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident(
                            crate::core_ir::INDEX_ASSIGN_VALUE_FIRST.into(),
                        )),
                        args: vec![Expr::Ident("a".into()), Expr::IntLit(1), stored],
                    }),
                ],
                Expr::Ident("a".into()),
            );
            let mut decls = vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Array(Box::new(Typ::Int)),
                type_params: vec![],
                body: vec![Stmt::Return(Some(expression))],
            }];
            lower_inline_constants(&mut decls);
            let Decl::Function { body, .. } = &decls[0] else {
                panic!()
            };
            if valid {
                assert!(
                    matches!(&body[0],Stmt::Return(Some(Expr::ArrayLit(values))) if values==&vec![Expr::IntLit(20),Expr::IntLit(22)])
                );
            } else {
                assert!(matches!(&body[0], Stmt::Return(Some(Expr::Call { .. }))));
            }
        }
    }

    #[test]
    fn inline_constant_array_store_evaluates_value_before_index() {
        let value = inline(
            vec![
                Stmt::Let(
                    "a".into(),
                    Some(Typ::Array(Box::new(Typ::Int))),
                    Expr::ArrayLit(vec![Expr::IntLit(20), Expr::IntLit(0)]),
                ),
                Stmt::Let("index".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident(crate::core_ir::INDEX_ASSIGN_VALUE_FIRST.into())),
                    args: vec![
                        Expr::Ident("a".into()),
                        Expr::Ident("index".into()),
                        Expr::Block {
                            body: vec![Stmt::Assign("index".into(), Expr::IntLit(1))],
                            result: Some(Box::new(Expr::IntLit(22))),
                        },
                    ],
                }),
            ],
            Expr::ArrayLit(vec![
                Expr::Index {
                    base: Box::new(Expr::Ident("a".into())),
                    index: Box::new(Expr::IntLit(0)),
                },
                Expr::Index {
                    base: Box::new(Expr::Ident("a".into())),
                    index: Box::new(Expr::IntLit(1)),
                },
            ]),
        );
        assert_eq!(
            evaluate_inline(&value, 65536),
            Some(Expr::ArrayLit(vec![Expr::IntLit(20), Expr::IntLit(22)]))
        );
    }

    #[test]
    fn inline_constant_unused_values_still_require_valid_types() {
        for invalid in [
            Stmt::Let("invalid".into(), Some(Typ::Int), Expr::BoolLit(false)),
            Stmt::Let(
                "invalid".into(),
                None,
                Expr::StructInit {
                    name: "Pair".into(),
                    fields: vec![("x".into(), Expr::BoolLit(false))],
                },
            ),
        ] {
            let mut decls = vec![
                Decl::Struct {
                    name: "Pair".into(),
                    fields: vec![("x".into(), Typ::Int)],
                    type_params: vec![],
                },
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Int,
                    type_params: vec![],
                    body: vec![Stmt::Return(Some(inline(vec![invalid], Expr::IntLit(42))))],
                },
            ];
            lower_inline_constants(&mut decls);
            let Decl::Function { body, .. } = decls.last().unwrap() else {
                panic!()
            };
            assert!(matches!(&body[0], Stmt::Return(Some(Expr::Call { .. }))));
        }
    }

    #[test]
    fn inline_constant_expression_blocks_commit_outer_local_mutation() {
        let mutate = Expr::Block {
            body: vec![Stmt::Assign(
                "x".into(),
                Expr::Binary {
                    op: "+".into(),
                    lhs: Box::new(Expr::Ident("x".into())),
                    rhs: Box::new(Expr::IntLit(1)),
                },
            )],
            result: Some(Box::new(Expr::Ident("x".into()))),
        };
        for value in [
            mutate.clone(),
            Expr::Conditional {
                cond: Box::new(Expr::BoolLit(true)),
                then_expr: Box::new(mutate),
                else_expr: Box::new(Expr::IntLit(0)),
            },
        ] {
            let expression = inline(
                vec![
                    Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(20)),
                    Stmt::Let("y".into(), Some(Typ::Int), value),
                ],
                Expr::Binary {
                    op: "+".into(),
                    lhs: Box::new(Expr::Ident("x".into())),
                    rhs: Box::new(Expr::Ident("y".into())),
                },
            );
            assert_eq!(evaluate_inline(&expression, 65536), Some(Expr::IntLit(42)));
        }
    }

    #[test]
    fn pure_constant_evaluation_preserves_local_state_and_refuses_open_values() {
        let value = Expr::Block {
            body: vec![
                Stmt::Let("i".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Loop {
                    kind: crate::core_ir::LoopKind::While,
                    cond: Some(Expr::Binary {
                        op: "<".into(),
                        lhs: Box::new(Expr::Ident("i".into())),
                        rhs: Box::new(Expr::IntLit(3)),
                    }),
                    body: vec![Stmt::Assign(
                        "i".into(),
                        Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("i".into())),
                            rhs: Box::new(Expr::IntLit(1)),
                        },
                    )],
                },
            ],
            result: Some(Box::new(Expr::Ident("i".into()))),
        };
        assert_eq!(
            evaluate_pure_constant(&value, IntegerSemantics::Word64),
            Ok(Some(Expr::IntLit(3)))
        );
        assert_eq!(
            evaluate_pure_constant(&Expr::Ident("external".into()), IntegerSemantics::Word64),
            Ok(None)
        );
        let unbounded = Expr::Block {
            body: vec![Stmt::Loop {
                kind: crate::core_ir::LoopKind::While,
                cond: Some(Expr::BoolLit(true)),
                body: vec![],
            }],
            result: None,
        };
        assert_eq!(
            evaluate_pure_constant(&unbounded, IntegerSemantics::Word64),
            Ok(None)
        );
        let bad = Expr::Binary {
            op: "/".into(),
            lhs: Box::new(Expr::IntLit(1)),
            rhs: Box::new(Expr::IntLit(0)),
        };
        assert_eq!(
            evaluate_pure_constant(&bad, IntegerSemantics::Word64),
            Err(ConstantEvalError::Arithmetic)
        );
        let selected = Expr::Conditional {
            cond: Box::new(Expr::BoolLit(true)),
            then_expr: Box::new(Expr::IntLit(3)),
            else_expr: Box::new(bad),
        };
        assert_eq!(
            evaluate_pure_constant(&selected, IntegerSemantics::Word64),
            Ok(Some(Expr::IntLit(3)))
        );
    }

    #[test]
    fn inline_constants_evaluate_local_control_flow_and_aggregates() {
        let value = inline(
            vec![
                Stmt::Let("base".into(), Some(Typ::Int), Expr::IntLit(20)),
                Stmt::If {
                    cond: Expr::BoolLit(true),
                    then_body: vec![Stmt::Assign("base".into(), Expr::IntLit(42))],
                    else_body: vec![],
                },
            ],
            Expr::ArrayLit(vec![Expr::Ident("base".into())]),
        );
        assert_eq!(
            evaluate_inline(&value, 65536),
            Some(Expr::ArrayLit(vec![Expr::IntLit(42)]))
        );
        let value = inline(
            vec![],
            Expr::StructInit {
                name: "Pair".into(),
                fields: vec![(
                    "value".into(),
                    Expr::Binary {
                        op: "+".into(),
                        lhs: Box::new(Expr::IntLit(20)),
                        rhs: Box::new(Expr::IntLit(22)),
                    },
                )],
            },
        );
        assert_eq!(
            evaluate_inline(&value, 65536),
            Some(Expr::StructInit {
                name: "Pair".into(),
                fields: vec![("value".into(), Expr::IntLit(42))]
            })
        );
    }

    #[test]
    fn inline_constants_refuse_captures_calls_returns_and_exhausted_budgets() {
        for value in [
            Expr::Ident("runtime".into()),
            Expr::Call {
                callee: Box::new(Expr::Ident("runtime_call".into())),
                args: vec![],
            },
            Expr::Call {
                callee: Box::new(Expr::Closure {
                    params: vec![],
                    ret: Typ::Int,
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    captures: vec![],
                }),
                args: vec![],
            },
        ] {
            assert!(evaluate_inline(&inline(vec![], value), 65536).is_none());
        }
        assert!(
            evaluate_inline(
                &inline(vec![Stmt::Return(Some(Expr::IntLit(42)))], Expr::IntLit(0)),
                65536
            )
            .is_none()
        );
        assert!(evaluate_inline(&inline(vec![], Expr::IntLit(42)), 0).is_none());
        assert!(
            evaluate_inline(
                &inline(
                    vec![Stmt::Loop {
                        kind: crate::core_ir::LoopKind::While,
                        cond: Some(Expr::BoolLit(true)),
                        body: vec![]
                    }],
                    Expr::IntLit(42)
                ),
                32
            )
            .is_none()
        );
        let captured = inline(
            vec![Stmt::Let("outer".into(), Some(Typ::Int), Expr::IntLit(42))],
            inline(vec![], Expr::Ident("outer".into())),
        );
        assert!(evaluate_inline(&captured, 65536).is_none());
    }

    #[test]
    fn wrapping_integer_constants_share_bounded_operand_evaluation() {
        let operation = |name: &str, op: &str, left: Expr, right: Expr| Expr::Unary {
            op: format!("rust-wrapping-integer-{name}"),
            expr: Box::new(Expr::Binary {
                op: op.into(),
                lhs: Box::new(left),
                rhs: Box::new(right),
            }),
        };
        for (name, op, left, right, expected) in [
            ("u8", "+", 255, 43, 42),
            ("u16", "-", 0, 65494, 42),
            ("u64", "*", -1, -42, 42),
            ("i64", "+", i64::MAX, 1, i64::MIN),
        ] {
            let value = inline(
                vec![Stmt::Let("left".into(), Some(Typ::Int), Expr::IntLit(left))],
                operation(name, op, Expr::Ident("left".into()), Expr::IntLit(right)),
            );
            assert_eq!(
                evaluate_inline(&value, 65536),
                Some(Expr::IntLit(expected)),
                "{name}/{op}"
            );
        }
        let value = inline(
            vec![
                Stmt::Let("value".into(), Some(Typ::Int), Expr::IntLit(254)),
                Stmt::Let("count".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Loop {
                    kind: crate::core_ir::LoopKind::While,
                    cond: Some(Expr::Binary {
                        op: "<".into(),
                        lhs: Box::new(Expr::Ident("count".into())),
                        rhs: Box::new(Expr::IntLit(44)),
                    }),
                    body: vec![
                        Stmt::Assign(
                            "value".into(),
                            operation("u8", "+", Expr::Ident("value".into()), Expr::IntLit(1)),
                        ),
                        Stmt::Assign(
                            "count".into(),
                            Expr::Binary {
                                op: "+".into(),
                                lhs: Box::new(Expr::Ident("count".into())),
                                rhs: Box::new(Expr::IntLit(1)),
                            },
                        ),
                    ],
                },
            ],
            Expr::Ident("value".into()),
        );
        assert_eq!(evaluate_inline(&value, 65536), Some(Expr::IntLit(42)));
        assert!(evaluate_inline(&value, 1).is_none());
    }

    #[test]
    fn strict_constants_validate_dead_branches_and_checked_arithmetic() {
        for invalid in [
            Expr::Ident("runtime".into()),
            Expr::Call {
                callee: Box::new(Expr::Ident("runtime_call".into())),
                args: vec![],
            },
            Expr::Block {
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                result: Some(Box::new(Expr::IntLit(0))),
            },
        ] {
            let value = inline(
                vec![],
                Expr::Conditional {
                    cond: Box::new(Expr::BoolLit(true)),
                    then_expr: Box::new(Expr::IntLit(42)),
                    else_expr: Box::new(invalid),
                },
            );
            assert!(evaluate_inline(&value, 65536).is_none());
        }
        for (op, lhs, rhs) in [
            ("+", i64::MAX, 1),
            ("-", i64::MIN, 1),
            ("*", i64::MAX, 2),
            ("/", i64::MIN, -1),
            ("/", 1, 0),
            ("%", 1, 0),
            ("<<", 1, -1),
            (">>", 1, 64),
        ] {
            let value = inline(
                vec![],
                Expr::Binary {
                    op: op.into(),
                    lhs: Box::new(Expr::IntLit(lhs)),
                    rhs: Box::new(Expr::IntLit(rhs)),
                },
            );
            assert!(evaluate_inline(&value, 65536).is_none(), "{op} {lhs} {rhs}");
        }
    }

    #[test]
    fn unselected_constants_still_require_valid_initializers() {
        let globals = HashMap::from([(
            "INVALID".into(),
            Some(Expr::Call {
                callee: Box::new(Expr::Ident("runtime_call".into())),
                args: vec![],
            }),
        )]);
        let expr = Expr::Conditional {
            cond: Box::new(Expr::BoolLit(true)),
            then_expr: Box::new(Expr::IntLit(42)),
            else_expr: Box::new(Expr::Ident("INVALID".into())),
        };
        assert!(!const_syntax(
            &expr,
            &globals,
            &HashSet::new(),
            &HashSet::new(),
            0,
            &mut 65536
        ));
        let cycle = HashMap::from([("CYCLE".into(), Some(Expr::Ident("CYCLE".into())))]);
        assert!(!const_syntax(
            &Expr::Ident("CYCLE".into()),
            &cycle,
            &HashSet::new(),
            &HashSet::new(),
            0,
            &mut 65536
        ));
    }

    #[test]
    fn inline_constants_do_not_read_runtime_statics() {
        let marker = inline(vec![], Expr::Ident("STATIC".into()));
        let mut module = crate::in_lang_parse::parse_in_source(
            "var STATIC: Int = 42;\nfn main() -> Int { return 0; }",
        )
        .unwrap();
        let Decl::Function { body, .. } = module.decls.last_mut().unwrap() else {
            panic!()
        };
        *body = vec![Stmt::Return(Some(marker.clone()))];
        lower_inline_constants(&mut module.decls);
        let Decl::Function { body, .. } = module.decls.last().unwrap() else {
            panic!()
        };
        assert!(matches!(&body[0], Stmt::Return(Some(Expr::Call { .. }))));
    }

    #[test]
    fn direct_global_repeat_folding_uses_scoped_legacy_resolution() {
        let mut module = crate::in_lang_parse::parse_in_source(
            "var values:[Int]=array-repeat(21,2);fn main()->Int{return 42;}",
        )
        .unwrap();
        fold_static_initializers(&mut module.decls);
        assert!(module.decls.iter().any(|decl| matches!(decl,
            Decl::Global {name,init:Some(value),..} if name=="values"
                && value.as_ref()==&Expr::ArrayLit(vec![Expr::IntLit(21),Expr::IntLit(21)]))));

        let mut module=crate::in_lang_parse::parse_in_source(
            "var values:[Int]={let array-repeat=fn(value:Int,count:Int)->[Int]{return [42];};array-repeat(21,2)};fn main()->Int{return 42;}"
        ).unwrap();
        fold_static_initializers(&mut module.decls);
        let init = module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Global {
                    name,
                    init: Some(value),
                    ..
                } if name == "values" => Some(value),
                _ => None,
            })
            .unwrap();
        let Expr::Block {
            body,
            result: Some(result),
        } = init.as_ref()
        else {
            panic!("{init:?}")
        };
        let Stmt::Let(local, _, _) = &body[0] else {
            panic!("{body:?}")
        };
        assert!(matches!(result.as_ref(),Expr::Call {callee,..}
            if matches!(callee.as_ref(),Expr::Ident(name) if name==local)));
        assert_ne!(local, crate::core_ir::ARRAY_REPEAT);
    }

    #[test]
    fn declared_repeat_function_is_not_evaluated_as_an_intrinsic() {
        let mut module = crate::in_lang_parse::parse_in_source("var values: [Int] = array-repeat(20,2);\nfn array-repeat(value: Int, count: Int) -> [Int] { return [value,count]; }\nfn main() -> Int { return 42; }").unwrap();
        fold_static_initializers(&mut module.decls);
        assert!(module.decls.iter().any(|decl| matches!(decl, Decl::Global { init: Some(init), .. } if matches!(init.as_ref(), Expr::Call { .. }))));
    }

    #[test]
    fn refuses_effects_cycles_and_unbounded_initializers() {
        let mut empty = HashMap::new();
        for source in [
            "static VALUE: i64 = { print(42); 42 }; fn main() -> i64 { return 42; }",
            "static VALUE: f64 = { let value:i64=42.0; value }; fn main() -> i64 { return 42; }",
            "static VALUE: i64 = { [20,true][0] }; fn main() -> i64 { return 42; }",
            "static VALUE: i64 = { let mut value=20; value=42.0; 42 }; fn main() -> i64 { return 42; }",
            "static VALUE: i64 = { let mut text=\"x\"; while true { text=text+text; } 42 }; fn main() -> i64 { return 42; }",
            "static VALUE: i64 = { while true {} 42 }; fn main() -> i64 { return 42; }",
            "static VALUE: [[i64;4096];4096] = { [[0;4096];4096] }; fn main() -> i64 { return 42; }",
            "static VALUE: i64 = VALUE; fn main() -> i64 { return 42; }",
            "static mut OTHER: i64 = 42; static VALUE: i64 = OTHER; fn main() -> i64 { return 42; }",
        ] {
            let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let globals = module
                .decls
                .iter()
                .filter_map(|decl| match decl {
                    Decl::Global {
                        name,
                        init: Some(init),
                        mutable: false,
                        ..
                    } => Some((name.clone(), Some(init.as_ref().clone()))),
                    _ => None,
                })
                .collect();
            let init = module
                .decls
                .iter()
                .find_map(|decl| match decl {
                    Decl::Global { name, init, .. } if name == "VALUE" => init.as_deref(),
                    _ => None,
                })
                .unwrap();
            let mut eval = Eval {
                globals: &globals,
                functions: &HashSet::new(),
                active: HashSet::new(),
                fuel: 65536,
                depth: 0,
                strict: false,
                invalid_arithmetic: false,
                semantics: IntegerSemantics::Word64,
            };
            assert!(eval.expr(init, &mut empty).is_none(), "{source}");
        }
    }
}
