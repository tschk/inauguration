//! Fold bounded, pure global initializer calls before native data layout.
use crate::core_ir::{Decl, Expr, Stmt, Typ};
use std::collections::{HashMap, HashSet};

fn value_nodes(value: &Expr) -> usize {
    match value {
        Expr::ArrayLit(items) => 1 + items.iter().map(value_nodes).sum::<usize>(),
        _ => 1,
    }
}

fn value_bytes(value: &Expr) -> usize {
    match value {
        Expr::ArrayLit(items) => 64 + items.iter().map(value_bytes).sum::<usize>(),
        Expr::StringLit(value) => 64 + value.len(),
        _ => 64,
    }
}
fn depth_bounded(value: &Expr, depth: usize) -> bool {
    depth < 128
        && match value {
            Expr::ArrayLit(items) => items.iter().all(|item| depth_bounded(item, depth + 1)),
            _ => true,
        }
}
fn bounded(value: &Expr) -> bool {
    depth_bounded(value, 0) && value_nodes(value) <= 4096 && value_bytes(value) <= 262144
}

fn value_type(value: &Expr) -> Option<Typ> {
    Some(match value {
        Expr::IntLit(_) => Typ::Int,
        Expr::BoolLit(_) => Typ::Bool,
        Expr::FloatLit(_) => Typ::Float,
        Expr::StringLit(_) => Typ::String,
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

struct Eval<'a> {
    globals: &'a HashMap<String, Option<Expr>>,
    functions: &'a HashSet<String>,
    active: HashSet<String>,
    fuel: usize,
    depth: usize,
}
impl Eval<'_> {
    fn step(&mut self) -> Option<()> {
        self.fuel = self.fuel.checked_sub(1)?;
        Some(())
    }
    fn expr(&mut self, expr: &Expr, env: &HashMap<String, (Expr, Typ)>) -> Option<Expr> {
        if self.depth >= 128 {
            return None;
        }
        self.depth += 1;
        let result = self.expr_inner(expr, env);
        self.depth -= 1;
        result
    }
    fn expr_inner(&mut self, expr: &Expr, env: &HashMap<String, (Expr, Typ)>) -> Option<Expr> {
        self.step()?;
        let value = match expr {
            Expr::IntLit(_) | Expr::BoolLit(_) | Expr::FloatLit(_) | Expr::StringLit(_) => {
                expr.clone()
            }
            Expr::Ident(name) => {
                if let Some(value) = env.get(name) {
                    return bounded(&value.0).then(|| value.0.clone());
                }
                if !self.active.insert(name.clone()) {
                    return None;
                }
                let value = self.expr(self.globals.get(name)?.as_ref()?, &HashMap::new());
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
            Expr::Unary { op, expr } => Expr::Unary {
                op: op.clone(),
                expr: Box::new(self.expr(expr, env)?),
            },
            Expr::Call { callee, args } => {
                if matches!(callee.as_ref(), Expr::Ident(name) if name == "array-repeat" && !self.functions.contains(name) && !self.globals.contains_key(name))
                    && args.len() == 2
                {
                    let value = self.expr(&args[0], env)?;
                    let Expr::IntLit(count) = self.expr(&args[1], env)? else {
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
        let value = crate::core_opt::fold_expr(value);
        if !bounded(&value) {
            return None;
        }
        matches!(
            value,
            Expr::IntLit(_)
                | Expr::BoolLit(_)
                | Expr::FloatLit(_)
                | Expr::StringLit(_)
                | Expr::ArrayLit(_)
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
                Stmt::Return(Some(expr)) => return Some(Some(self.expr(expr, env)?)),
                Stmt::Expr(Expr::Call { callee, args })
                    if matches!(callee.as_ref(), Expr::Ident(name) if name == "index-assign-value-first" && !self.functions.contains(name) && !self.globals.contains_key(name))
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

pub(super) fn fold_static_initializers(decls: &mut [Decl]) {
    let names = crate::core_bindings::module_names(decls);
    for decl in decls.iter_mut() {
        if let Decl::Global {
            init: Some(init), ..
        } = decl
        {
            crate::core_bindings::normalize_initializer_with_references(
                init,
                &names,
                &HashMap::new(),
            );
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
            };
            if let Some(value) = eval.expr(init, &HashMap::new()) {
                *init = Box::new(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn declared_repeat_function_is_not_evaluated_as_an_intrinsic() {
        let mut module = crate::in_lang_parse::parse_in_source("var values: [Int] = array-repeat(20,2);\nfn array-repeat(value: Int, count: Int) -> [Int] { return [value,count]; }\nfn main() -> Int { return 42; }").unwrap();
        fold_static_initializers(&mut module.decls);
        assert!(module.decls.iter().any(|decl| matches!(decl, Decl::Global { init: Some(init), .. } if matches!(init.as_ref(), Expr::Call { .. }))));
    }

    #[test]
    fn refuses_effects_cycles_and_unbounded_initializers() {
        let empty = HashMap::new();
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
            };
            assert!(eval.expr(init, &empty).is_none(), "{source}");
        }
    }
}
