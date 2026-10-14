use crate::core_ir::{Decl, Expr, MatchPattern, MethodSig, Stmt, Typ, UnifiedModule};
use crate::parser_registry::{ParserId, ResolvedBuildParser};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone)]
pub enum TypeError {
    ArityMismatch {
        caller: String,
        fn_name: String,
        expected: usize,
        got: usize,
    },
    ReturnTypeMismatch {
        fn_name: String,
        expected: Typ,
        got: Typ,
    },
    ReturnValueInVoid {
        fn_name: String,
    },
    MissingReturnValue {
        fn_name: String,
    },
    UnknownField {
        struct_name: String,
        field: String,
    },
    UndefinedVariable {
        fn_name: String,
        name: String,
    },
    StructNotFound {
        fn_name: String,
        name: String,
    },
    TypeMismatch {
        context: String,
        expected: Typ,
        got: Typ,
    },
    NotArray {
        expr: String,
    },
    IndexNotInt {
        expr: String,
    },
    MissingInterfaceMethod {
        class_name: String,
        interface_name: String,
        method_name: String,
    },
    InterfaceMethodSigMismatch {
        class_name: String,
        interface_name: String,
        method_name: String,
        detail: String,
    },
    InterfaceNotFound {
        class_name: String,
        interface_name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    Library,
    Executable,
}

#[derive(Default)]
pub struct TypeChecker;

pub(crate) struct Facts {
    functions: HashMap<String, (Vec<(String, Typ)>, Typ)>,
    structs: HashMap<String, Vec<(String, Typ)>>,
    closures: HashMap<String, String>,
}

impl Facts {
    fn closure_target(&self, captures: &str) -> Option<&str> {
        if !self.structs.get(captures)?.is_empty() {
            return None;
        }
        let target = self.closures.get(captures)?;
        let (params, ret) = self.functions.get(target)?;
        fn resolved(typ: &Typ) -> bool {
            match typ {
                Typ::Generic(_) => false,
                Typ::Named(name) => !matches!(name.as_str(), "Any" | "_"),
                Typ::Array(item) | Typ::Vector(item) => resolved(item),
                _ => true,
            }
        }
        (resolved(ret) && params.iter().all(|(_, typ)| resolved(typ))).then_some(target.as_str())
    }

    pub(crate) fn value_receiver_index(&self, function: &str) -> Option<usize> {
        self.functions
            .get(function)?
            .0
            .iter()
            .position(|(name, typ)| {
                name == "self" && matches!(typ, Typ::Named(name) if self.structs.contains_key(name))
            })
    }
}

impl TypeChecker {
    pub fn new() -> Self {
        Self
    }

    pub fn check_module(&self, module: &UnifiedModule) -> Result<(), Vec<TypeError>> {
        let mut errors = Vec::new();
        let facts = self.collect_facts(module);
        let globals: HashMap<String, Typ> = module
            .decls
            .iter()
            .filter_map(|decl| {
                if let Decl::Global { name, typ, .. } = decl {
                    Some((name.clone(), typ.clone()))
                } else {
                    None
                }
            })
            .collect();

        self.check_interface_conformance(module, &mut errors);

        for decl in &module.decls {
            match decl {
                Decl::Function {
                    name,
                    params,
                    ret,
                    body,
                    ..
                } => {
                    let mut env = globals.clone();
                    env.extend(params.iter().cloned());
                    self.check_stmts(name, ret, body, &facts, &mut env, &mut errors);
                }
                Decl::Class { .. } => {}
                _ => {}
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Resolve type-dependent complement into the canonical boolean or integer operator.
    pub(crate) fn resolve_complements(&self, module: &mut UnifiedModule) {
        self.resolve_typed_operators(module, false, None);
    }

    fn resolve_c_value_promotions(&self, module: &mut UnifiedModule) {
        self.resolve_typed_operators(module, true, None);
    }

    pub(crate) fn resolve_closure_calls(
        &self,
        module: &mut UnifiedModule,
        closures: &HashMap<String, String>,
    ) {
        self.resolve_typed_operators(module, false, Some(closures));
    }

    fn resolve_typed_operators(
        &self,
        module: &mut UnifiedModule,
        promote_c_bool: bool,
        closures: Option<&HashMap<String, String>>,
    ) {
        let mut facts = self.collect_facts(module);
        if let Some(closures) = closures {
            facts.closures = closures.clone();
        }
        let globals: HashMap<String, Typ> = module
            .decls
            .iter()
            .filter_map(|decl| match decl {
                Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
                _ => None,
            })
            .collect();
        let generated: std::collections::HashSet<_> = facts.closures.values().cloned().collect();
        // Nested closures are appended before their parents during lifting.
        for closure_phase in [true, false] {
            if closure_phase && generated.is_empty() {
                continue;
            }
            for decl in &mut module.decls {
                match decl {
                    Decl::Function {
                        name,
                        params,
                        ret,
                        body,
                        ..
                    } if generated.contains(name) == closure_phase => {
                        let mut env = globals.clone();
                        env.extend(params.iter().cloned());
                        let initial = env.clone();
                        self.resolve_typed_body(body, &facts, &mut env, Some(ret), promote_c_bool);
                        if closure_phase {
                            if matches!(ret, Typ::Generic(_))
                                && let Some(inferred) =
                                    self.infer_closure_return(body, &facts, &initial)
                            {
                                *ret = inferred;
                            }
                            facts
                                .functions
                                .insert(name.clone(), (params.clone(), ret.clone()));
                        }
                    }
                    Decl::Global {
                        typ,
                        init: Some(expr),
                        ..
                    } if !closure_phase => {
                        self.resolve_typed_expr(expr, &facts, &globals, promote_c_bool);
                        if promote_c_bool {
                            self.resolve_c_value(expr, typ, &facts, &globals);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn infer_closure_return(
        &self,
        body: &[Stmt],
        facts: &Facts,
        initial: &HashMap<String, Typ>,
    ) -> Option<Typ> {
        fn walk(
            checker: &TypeChecker,
            body: &[Stmt],
            facts: &Facts,
            env: &mut HashMap<String, Typ>,
            returns: &mut Vec<Typ>,
        ) -> Option<bool> {
            let mut falls = true;
            for stmt in body {
                if !falls {
                    break;
                }
                match stmt {
                    Stmt::Let(name, typ, value) => {
                        if let Some(typ) =
                            typ.clone().or_else(|| checker.expr_type(value, facts, env))
                        {
                            env.insert(name.clone(), typ);
                        }
                    }
                    Stmt::Return(value) => {
                        returns.push(match value {
                            Some(value) => checker.expr_type(value, facts, env)?,
                            None => Typ::Void,
                        });
                        falls = false;
                    }
                    Stmt::If {
                        cond,
                        then_body,
                        else_body,
                    } => {
                        let left = walk(checker, then_body, facts, &mut env.clone(), returns)?;
                        let right = walk(checker, else_body, facts, &mut env.clone(), returns)?;
                        falls = match cond {
                            Expr::BoolLit(true) => left,
                            Expr::BoolLit(false) => right,
                            _ => left || right,
                        };
                    }
                    Stmt::Loop { body, .. } => {
                        walk(checker, body, facts, &mut env.clone(), returns)?;
                    }
                    // shortcut: infer match/catch returns when their binding types are available.
                    Stmt::Match { .. } | Stmt::Try { .. } | Stmt::Propagate => return None,
                    Stmt::Throw(_) => falls = false,
                    Stmt::Break | Stmt::Continue => break,
                    _ => {}
                }
            }
            Some(falls)
        }
        let mut returns = Vec::new();
        if walk(self, body, facts, &mut initial.clone(), &mut returns)? {
            returns.push(Typ::Void);
        }
        let typ = returns.first()?.canonical();
        returns
            .iter()
            .all(|returned| returned.canonical() == typ)
            .then_some(typ)
    }

    fn resolve_c_value(
        &self,
        expr: &mut Expr,
        expected: &Typ,
        facts: &Facts,
        env: &HashMap<String, Typ>,
    ) {
        let actual = self.expr_type(expr, facts, env);
        if expected == &Typ::Bool && matches!(actual, Some(Typ::Named(_))) {
            *expr = Expr::Call {
                callee: Box::new(Expr::Ident("unsupported-c-boolean-conversion".into())),
                args: vec![expr.clone()],
            };
            return;
        }
        let actual = actual.map(|typ| typ.canonical());
        let zero = match (expected.canonical(), actual.clone()) {
            (Typ::Bool, Some(Typ::Int)) => Some(Expr::IntLit(0)),
            (Typ::Bool, Some(Typ::Float)) => Some(Expr::FloatLit(crate::core_ir::FloatVal(0.0))),
            (Typ::Float, Some(Typ::Int | Typ::Bool)) => {
                let value = if actual == Some(Typ::Bool) {
                    Expr::Unary {
                        op: "bool-to-int".into(),
                        expr: Box::new(expr.clone()),
                    }
                } else {
                    expr.clone()
                };
                *expr = Expr::Unary {
                    op: "int-to-float".into(),
                    expr: Box::new(value),
                };
                return;
            }
            (Typ::Int, Some(Typ::Float)) => {
                *expr = Expr::Unary {
                    op: "float-to-int".into(),
                    expr: Box::new(expr.clone()),
                };
                return;
            }
            (Typ::Int, Some(Typ::Bool)) => {
                *expr = Expr::Unary {
                    op: "bool-to-int".into(),
                    expr: Box::new(expr.clone()),
                };
                return;
            }
            _ => None,
        };
        if let Some(zero) = zero {
            *expr = Expr::Binary {
                op: "!=".into(),
                lhs: Box::new(expr.clone()),
                rhs: Box::new(zero),
            };
        }
    }

    fn resolve_typed_expr(
        &self,
        expr: &mut Expr,
        facts: &Facts,
        env: &HashMap<String, Typ>,
        promote_c_bool: bool,
    ) {
        if let Expr::Closure {
            params, ret, body, ..
        } = expr
        {
            let mut inner = env.clone();
            inner.extend(params.iter().cloned());
            let initial = inner.clone();
            self.resolve_typed_body(body, facts, &mut inner, None, promote_c_bool);
            if matches!(ret, Typ::Generic(_))
                && let Some(inferred) = self.infer_closure_return(body, facts, &initial)
            {
                *ret = inferred;
            }
        } else {
            crate::core_ir::for_each_expr_child_mut(expr, &mut |child| {
                self.resolve_typed_expr(child, facts, env, promote_c_bool)
            });
        }
        if let Expr::Call { callee, args } = expr {
            let captures = match callee.as_ref() {
                Expr::Ident(local) => match env.get(local) {
                    Some(Typ::Named(name)) => Some(name.as_str()),
                    _ => None,
                },
                Expr::StructInit { name, fields } if fields.is_empty() => Some(name.as_str()),
                _ => None,
            };
            if let Some(function) = captures.and_then(|captures| facts.closure_target(captures)) {
                args.insert(0, callee.as_ref().clone());
                *callee = Box::new(Expr::Ident(function.to_string()));
            }
            if let Expr::Ident(name) = callee.as_ref() {
                if let Some(op) = name
                    .strip_prefix("__method__")
                    .filter(|op| matches!(*op, "load" | "store" | "fetch_add"))
                {
                    if let Some(kind) = args
                        .first()
                        .and_then(|arg| self.expr_type(arg, facts, env))
                        .as_ref()
                        .filter(|typ| !matches!(typ, Typ::Named(name) if facts.structs.contains_key(name)))
                        .and_then(crate::core_atomic::Kind::from_type)
                    {
                        *callee = Box::new(Expr::Ident(kind.intrinsic(op)));
                    } else if let Some(Typ::Named(owner))=args.first().and_then(|arg|self.expr_type(arg,facts,env)) {
                        for candidate in [format!("{owner}::{op}"),format!("{owner}_{op}")] {
                            if facts.functions.contains_key(&candidate) {*callee=Box::new(Expr::Ident(candidate));break;}
                        }
                    }
                }
            }
        }
        if let Expr::Call { callee, args } = expr
            && let Expr::Ident(name) = callee.as_mut()
            && let Some(short) = name
                .rsplit_once("::")
                .map(|(_, short)| short)
                .or_else(|| name.strip_prefix("__method__"))
            && matches!(short, "clone" | "is_empty" | "len")
            && !facts.functions.contains_key(name)
            && facts.functions.contains_key(short)
            && let [source] = args.as_slice()
            && matches!(
                self.expr_type(source, facts, env),
                Some(Typ::Vector(_) | Typ::Array(_) | Typ::String)
            )
        {
            *name = short.to_owned();
        }
        if let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if (name.rsplit("::").next() == Some("is_empty") || name == "__method__is_empty") && !facts.functions.contains_key(name) && !facts.functions.contains_key("is_empty"))
            && let [source] = args.as_slice()
            && matches!(
                self.expr_type(source, facts, env),
                Some(Typ::Vector(_) | Typ::Array(_) | Typ::String)
            )
        {
            *expr = Expr::Binary {
                op: "==".into(),
                lhs: Box::new(Expr::Call {
                    callee: Box::new(Expr::Ident(
                        if self.expr_type(source, facts, env) == Some(Typ::String) {
                            "str-len"
                        } else {
                            "array-len"
                        }
                        .into(),
                    )),
                    args: vec![source.clone()],
                }),
                rhs: Box::new(Expr::IntLit(0)),
            };
        }
        if let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if (name == "len" || name == "__method__len") && !facts.functions.contains_key(name) && !facts.functions.contains_key("len"))
            && let [source] = args.as_slice()
            && matches!(
                self.expr_type(source, facts, env),
                Some(Typ::Vector(_) | Typ::Array(_) | Typ::String)
            )
        {
            *callee = Box::new(Expr::Ident(
                if self.expr_type(source, facts, env) == Some(Typ::String) {
                    "str-len"
                } else {
                    "array-len"
                }
                .into(),
            ));
        }
        if let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if (name == "clone" || name == "Vec::clone") && !facts.functions.contains_key(name) && !facts.functions.contains_key("clone"))
            && let [source] = args.as_slice()
            && matches!(self.expr_type(source, facts, env), Some(Typ::Vector(item)) if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool))
        {
            *expr = Expr::Unary {
                op: "vector-clone".into(),
                expr: Box::new(source.clone()),
            };
        }
        if let Expr::Unary { op, expr: value } = expr
            && let Some(target) = op.strip_prefix("numeric-cast-")
        {
            let integer = format!("int-cast-{target}");
            match self.expr_type(value, facts, env).map(|typ| typ.canonical()) {
                Some(Typ::Int | Typ::Bool)
                    if crate::core_ir::integer_cast_spec(&integer).is_some() =>
                {
                    *op = integer
                }
                Some(Typ::Float) if crate::core_ir::integer_cast_spec(&integer).is_some() => {
                    *op = format!("float-saturate-{target}")
                }
                Some(Typ::Float) if matches!(target, "f32" | "f64") => {
                    *op = format!("float-cast-{target}")
                }
                _ => {}
            }
        }
        if promote_c_bool
            && let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if name == "c-numeric-cast")
        {
            if let [value, Expr::StringLit(target)] = args.as_slice() {
                let expected = match target.as_str() {
                    "Int" => Some(Typ::Int),
                    "Float" => Some(Typ::Float),
                    "Bool" => Some(Typ::Bool),
                    _ => None,
                };
                if let Some(expected) = expected
                    && matches!(
                        self.expr_type(value, facts, env).map(|typ| typ.canonical()),
                        Some(Typ::Int | Typ::Float | Typ::Bool)
                    )
                {
                    let mut value = value.clone();
                    self.resolve_c_value(&mut value, &expected, facts, env);
                    *expr = value;
                }
            }
            return;
        }
        if promote_c_bool {
            match expr {
                Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "&&" | "||") => {
                    self.resolve_c_value(lhs, &Typ::Bool, facts, env);
                    self.resolve_c_value(rhs, &Typ::Bool, facts, env);
                }
                Expr::Unary { op, expr } if op == "!" => {
                    self.resolve_c_value(expr, &Typ::Bool, facts, env)
                }
                Expr::Unary { op, expr } if matches!(op.as_str(), "-" | "~") => {
                    if self.expr_type(expr, facts, env) == Some(Typ::Bool) {
                        self.resolve_c_value(expr, &Typ::Int, facts, env)
                    }
                }
                Expr::Binary { op, lhs, rhs }
                    if matches!(
                        op.as_str(),
                        "+" | "-"
                            | "*"
                            | "/"
                            | "%"
                            | "&"
                            | "|"
                            | "^"
                            | "<<"
                            | ">>"
                            | "=="
                            | "!="
                            | "<"
                            | ">"
                            | "<="
                            | ">="
                    ) =>
                {
                    let floating = matches!(
                        op.as_str(),
                        "+" | "-" | "*" | "/" | "==" | "!=" | "<" | ">" | "<=" | ">="
                    ) && [lhs.as_ref(), rhs.as_ref()]
                        .iter()
                        .any(|operand| self.expr_type(operand, facts, env) == Some(Typ::Float));
                    for operand in [lhs, rhs] {
                        if floating {
                            self.resolve_c_value(operand, &Typ::Float, facts, env);
                        } else if self.expr_type(operand, facts, env) == Some(Typ::Bool) {
                            self.resolve_c_value(operand, &Typ::Int, facts, env);
                        }
                    }
                }
                Expr::Call { callee, args } => {
                    if let Expr::Ident(name) = callee.as_ref()
                        && let Some((params, _)) = facts.functions.get(name)
                    {
                        for (arg, (_, expected)) in args.iter_mut().zip(params) {
                            self.resolve_c_value(arg, expected, facts, env);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Expr::Unary { op, expr } = expr
            && op == "complement"
        {
            match self.expr_type(expr, facts, env).map(|typ| typ.canonical()) {
                Some(Typ::Int) => *op = "~".into(),
                Some(Typ::Bool) => *op = "!".into(),
                _ => {} // Verification refuses an unresolved operand type.
            }
        }
    }

    fn resolve_typed_body(
        &self,
        body: &mut [Stmt],
        facts: &Facts,
        env: &mut HashMap<String, Typ>,
        return_type: Option<&Typ>,
        promote_c_bool: bool,
    ) {
        for stmt in body {
            match stmt {
                Stmt::Let(name, typ, expr) => {
                    self.resolve_typed_expr(expr, facts, env, promote_c_bool);
                    if promote_c_bool && let Some(typ) = typ.as_ref() {
                        self.resolve_c_value(expr, typ, facts, env);
                    }
                    if let Some(typ) = typ.clone().or_else(|| self.expr_type(expr, facts, env)) {
                        env.insert(name.clone(), typ);
                    }
                }
                Stmt::Assign(name, expr) => {
                    self.resolve_typed_expr(expr, facts, env, promote_c_bool);
                    if promote_c_bool && let Some(typ) = env.get(name) {
                        self.resolve_c_value(expr, typ, facts, env);
                    }
                }
                Stmt::Return(Some(expr)) => {
                    self.resolve_typed_expr(expr, facts, env, promote_c_bool);
                    if promote_c_bool && let Some(typ) = return_type {
                        self.resolve_c_value(expr, typ, facts, env);
                    }
                }
                Stmt::Expr(expr) | Stmt::Throw(expr) => {
                    self.resolve_typed_expr(expr, facts, env, promote_c_bool)
                }
                Stmt::IndexAssign { base, index, value } => {
                    for expr in [&mut *base, &mut *index, &mut *value] {
                        self.resolve_typed_expr(expr, facts, env, promote_c_bool);
                    }
                    if promote_c_bool
                        && let Some(typ) = self.expr_type(
                            &Expr::Index {
                                base: Box::new(base.clone()),
                                index: Box::new(index.clone()),
                            },
                            facts,
                            env,
                        )
                    {
                        self.resolve_c_value(value, &typ, facts, env);
                    }
                }
                Stmt::FieldAssign { base, name, value } => {
                    for expr in [&mut *base, &mut *value] {
                        self.resolve_typed_expr(expr, facts, env, promote_c_bool);
                    }
                    if promote_c_bool
                        && let Some(typ) = self.expr_type(
                            &Expr::Field {
                                base: Box::new(base.clone()),
                                name: name.clone(),
                            },
                            facts,
                            env,
                        )
                    {
                        self.resolve_c_value(value, &typ, facts, env);
                    }
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    self.resolve_typed_expr(cond, facts, env, promote_c_bool);
                    if promote_c_bool {
                        self.resolve_c_value(cond, &Typ::Bool, facts, env);
                    }
                    self.resolve_typed_body(
                        then_body,
                        facts,
                        &mut env.clone(),
                        return_type,
                        promote_c_bool,
                    );
                    self.resolve_typed_body(
                        else_body,
                        facts,
                        &mut env.clone(),
                        return_type,
                        promote_c_bool,
                    );
                }
                Stmt::Loop { cond, body, .. } => {
                    if let Some(cond) = cond {
                        self.resolve_typed_expr(cond, facts, env, promote_c_bool);
                        if promote_c_bool {
                            self.resolve_c_value(cond, &Typ::Bool, facts, env);
                        }
                    }
                    self.resolve_typed_body(
                        body,
                        facts,
                        &mut env.clone(),
                        return_type,
                        promote_c_bool,
                    );
                }
                Stmt::Match { scrutinee, arms } => {
                    self.resolve_typed_expr(scrutinee, facts, env, promote_c_bool);
                    for arm in arms {
                        let mut inner = env.clone();
                        if let Ok(pattern) = MatchPattern::parse(&arm.pattern) {
                            self.check_match_pattern(
                                "",
                                &pattern,
                                facts,
                                &mut inner,
                                &mut Vec::new(),
                            );
                            if let MatchPattern::IdentPat(name) = pattern
                                && let Some(typ) = self.expr_type(scrutinee, facts, env)
                            {
                                inner.insert(name, typ);
                            }
                        }
                        self.resolve_typed_body(
                            &mut arm.body,
                            facts,
                            &mut inner,
                            return_type,
                            promote_c_bool,
                        );
                    }
                }
                Stmt::Try { body, catches } => {
                    self.resolve_typed_body(
                        body,
                        facts,
                        &mut env.clone(),
                        return_type,
                        promote_c_bool,
                    );
                    for catch in catches {
                        let mut inner = env.clone();
                        if let Ok(pattern) = MatchPattern::parse(&catch.pattern) {
                            self.check_match_pattern(
                                "",
                                &pattern,
                                facts,
                                &mut inner,
                                &mut Vec::new(),
                            );
                        }
                        self.resolve_typed_body(
                            &mut catch.body,
                            facts,
                            &mut inner,
                            return_type,
                            promote_c_bool,
                        );
                    }
                }
                Stmt::Return(None) | Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
            }
        }
    }

    fn collect_facts(&self, module: &UnifiedModule) -> Facts {
        self.collect_decl_facts(&module.decls)
    }

    pub(crate) fn collect_decl_facts(&self, decls: &[Decl]) -> Facts {
        let mut functions: HashMap<String, (Vec<(String, Typ)>, Typ)> = HashMap::new();
        let mut structs: HashMap<String, Vec<(String, Typ)>> = HashMap::new();

        for decl in decls {
            match decl {
                Decl::Struct { name, fields, .. } => {
                    structs
                        .entry(name.clone())
                        .or_default()
                        .extend(fields.clone());
                }
                Decl::Function {
                    name, params, ret, ..
                } => {
                    functions.insert(name.clone(), (params.clone(), ret.clone()));
                }
                Decl::Class {
                    name,
                    fields,
                    methods,
                    ..
                } => {
                    structs
                        .entry(name.clone())
                        .or_default()
                        .extend(fields.clone());
                    for method in methods {
                        if let Decl::Function {
                            name: mname,
                            params,
                            ret,
                            ..
                        } = method
                        {
                            functions.insert(mname.clone(), (params.clone(), ret.clone()));
                            let mangled = format!("{}_{}", name, mname);
                            let mut new_params =
                                vec![("self".to_string(), Typ::Named(name.clone()))];
                            new_params.extend(params.iter().cloned());
                            functions.insert(mangled, (new_params, ret.clone()));
                        }
                    }
                }
                Decl::Interface { .. } => {}
                Decl::Component { .. } => {}
                Decl::Global { .. } => {}
            }
        }

        Facts {
            functions,
            structs,
            closures: HashMap::new(),
        }
    }

    fn check_interface_conformance(&self, module: &UnifiedModule, errors: &mut Vec<TypeError>) {
        let interfaces: HashMap<String, Vec<MethodSig>> = module
            .decls
            .iter()
            .filter_map(|decl| match decl {
                Decl::Interface { name, methods, .. } => Some((name.clone(), methods.clone())),
                _ => None,
            })
            .collect();

        for decl in &module.decls {
            if let Decl::Class {
                name: class_name,
                methods,
                extends,
                implements,
                ..
            } = decl
            {
                for iface_name in implements {
                    self.check_class_against_interface(
                        class_name,
                        iface_name,
                        methods,
                        &interfaces,
                        errors,
                    );
                }

                if let Some(parent) = extends
                    && interfaces.contains_key(parent)
                {
                    self.check_class_against_interface(
                        class_name,
                        parent,
                        methods,
                        &interfaces,
                        errors,
                    );
                }
            }
        }
    }

    fn check_class_against_interface(
        &self,
        class_name: &str,
        iface_name: &str,
        class_methods: &[Decl],
        interfaces: &HashMap<String, Vec<MethodSig>>,
        errors: &mut Vec<TypeError>,
    ) {
        let iface_methods = match interfaces.get(iface_name) {
            Some(m) => m,
            None => {
                errors.push(TypeError::InterfaceNotFound {
                    class_name: class_name.to_string(),
                    interface_name: iface_name.to_string(),
                });
                return;
            }
        };

        for iface_method in iface_methods {
            let class_method = class_methods.iter().find(
                |decl| matches!(decl, Decl::Function { name, .. } if name == &iface_method.name),
            );

            match class_method {
                None => {
                    errors.push(TypeError::MissingInterfaceMethod {
                        class_name: class_name.to_string(),
                        interface_name: iface_name.to_string(),
                        method_name: iface_method.name.clone(),
                    });
                }
                Some(Decl::Function { params, ret, .. }) => {
                    if params.len() != iface_method.params.len() {
                        errors.push(TypeError::InterfaceMethodSigMismatch {
                            class_name: class_name.to_string(),
                            interface_name: iface_name.to_string(),
                            method_name: iface_method.name.clone(),
                            detail: format!(
                                "parameter count mismatch: expected {}, got {}",
                                iface_method.params.len(),
                                params.len()
                            ),
                        });
                    }
                    if !is_compatible(&iface_method.ret, ret) {
                        errors.push(TypeError::InterfaceMethodSigMismatch {
                            class_name: class_name.to_string(),
                            interface_name: iface_name.to_string(),
                            method_name: iface_method.name.clone(),
                            detail: format!(
                                "return type mismatch: expected {:?}, got {:?}",
                                iface_method.ret, ret
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn check_stmts(
        &self,
        fn_name: &str,
        fn_ret: &Typ,
        stmts: &[Stmt],
        facts: &Facts,
        env: &mut HashMap<String, Typ>,
        errors: &mut Vec<TypeError>,
    ) {
        for stmt in stmts {
            self.check_stmt(fn_name, fn_ret, stmt, facts, env, errors);
        }
    }

    fn check_stmt(
        &self,
        fn_name: &str,
        fn_ret: &Typ,
        stmt: &Stmt,
        facts: &Facts,
        env: &mut HashMap<String, Typ>,
        errors: &mut Vec<TypeError>,
    ) {
        match stmt {
            Stmt::Let(name, annot, expr) => {
                self.check_expr(fn_name, expr, facts, env, errors);
                let expr_typ = self.expr_type(expr, facts, env);
                if let (Some(expected), Some(actual)) = (annot, &expr_typ)
                    && !is_compatible(expected, actual)
                    && !expected.accepts_sequence_literal(expr, actual)
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("let binding `{name}` in `{fn_name}`"),
                        expected: expected.clone(),
                        got: actual.clone(),
                    });
                }
                if let Some(t) = annot {
                    env.insert(name.clone(), t.clone());
                } else if let Some(t) = expr_typ {
                    env.insert(name.clone(), t);
                }
            }
            Stmt::Assign(name, expr) => {
                self.check_expr(fn_name, expr, facts, env, errors);
                if let Some(existing_typ) = env.get(name).cloned() {
                    if let Some(actual) = self.expr_type(expr, facts, env)
                        && !is_compatible(&existing_typ, &actual)
                        && !existing_typ.accepts_sequence_literal(expr, &actual)
                    {
                        errors.push(TypeError::TypeMismatch {
                            context: format!("assignment to `{name}` in `{fn_name}`"),
                            expected: existing_typ.clone(),
                            got: actual,
                        });
                    }
                } else {
                    errors.push(TypeError::UndefinedVariable {
                        fn_name: fn_name.to_string(),
                        name: name.clone(),
                    });
                }
            }
            Stmt::Return(Some(expr)) => {
                self.check_expr(fn_name, expr, facts, env, errors);
                if fn_ret.canonical() == Typ::Void {
                    errors.push(TypeError::ReturnValueInVoid {
                        fn_name: fn_name.to_string(),
                    });
                } else if let Some(actual) = self.expr_type(expr, facts, env)
                    && !is_compatible(fn_ret, &actual)
                    && !fn_ret.accepts_sequence_literal(expr, &actual)
                {
                    errors.push(TypeError::ReturnTypeMismatch {
                        fn_name: fn_name.to_string(),
                        expected: fn_ret.clone(),
                        got: actual,
                    });
                }
            }
            Stmt::Return(None) => {
                if fn_ret.canonical() != Typ::Void {
                    errors.push(TypeError::MissingReturnValue {
                        fn_name: fn_name.to_string(),
                    });
                }
            }
            Stmt::Break | Stmt::Continue => {}
            Stmt::Expr(expr) => {
                self.check_expr(fn_name, expr, facts, env, errors);
            }
            Stmt::IndexAssign {
                base, index, value, ..
            } => {
                self.check_expr(fn_name, base, facts, env, errors);
                self.check_expr(fn_name, index, facts, env, errors);
                self.check_expr(fn_name, value, facts, env, errors);
                if let Some(index_typ) = self.expr_type(index, facts, env)
                    && index_typ != Typ::Int
                    && is_concrete(&index_typ)
                {
                    errors.push(TypeError::IndexNotInt {
                        expr: format!("index assignment index in `{fn_name}`"),
                    });
                }
                match self.expr_type(base, facts, env) {
                    Some(Typ::Array(item) | Typ::Vector(item)) => {
                        if let Some(value_typ) = self.expr_type(value, facts, env)
                            && !is_compatible(&item, &value_typ)
                        {
                            errors.push(TypeError::TypeMismatch {
                                context: format!("array assignment value in `{fn_name}`"),
                                expected: *item.clone(),
                                got: value_typ,
                            });
                        }
                    }
                    Some(base_typ) if is_concrete(&base_typ) => {
                        errors.push(TypeError::NotArray {
                            expr: format!("index assignment base in `{fn_name}`"),
                        });
                    }
                    _ => {}
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
            } => {
                self.check_expr(fn_name, cond, facts, env, errors);
                if let Some(cond_typ) = self.expr_type(cond, facts, env)
                    && cond_typ != Typ::Bool
                    && is_concrete(&cond_typ)
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("if condition in `{fn_name}`"),
                        expected: Typ::Bool,
                        got: cond_typ,
                    });
                }
                let mut env_then = env.clone();
                self.check_stmts(fn_name, fn_ret, then_body, facts, &mut env_then, errors);
                let mut env_else = env.clone();
                self.check_stmts(fn_name, fn_ret, else_body, facts, &mut env_else, errors);
            }
            Stmt::Loop { cond, body, .. } => {
                if let Some(cond) = cond {
                    self.check_expr(fn_name, cond, facts, env, errors);
                    if let Some(cond_typ) = self.expr_type(cond, facts, env)
                        && cond_typ != Typ::Bool
                        && is_concrete(&cond_typ)
                    {
                        errors.push(TypeError::TypeMismatch {
                            context: format!("loop condition in `{fn_name}`"),
                            expected: Typ::Bool,
                            got: cond_typ,
                        });
                    }
                }
                let mut env_body = env.clone();
                self.check_stmts(fn_name, fn_ret, body, facts, &mut env_body, errors);
            }
            Stmt::Match {
                scrutinee, arms, ..
            } => {
                self.check_expr(fn_name, scrutinee, facts, env, errors);
                if arms.is_empty() {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("match in `{fn_name}` has no arms"),
                        expected: Typ::Named("at-least-one-arm".to_string()),
                        got: Typ::Named("zero-arms".to_string()),
                    });
                }
                for arm in arms {
                    let mut env_arm = env.clone();
                    if let Ok(pattern) = MatchPattern::parse(&arm.pattern) {
                        self.check_match_pattern(fn_name, &pattern, facts, &mut env_arm, errors);
                    }
                    if let Ok(MatchPattern::IdentPat(name)) = MatchPattern::parse(&arm.pattern)
                        && let Some(typ) = self.expr_type(scrutinee, facts, env)
                    {
                        env_arm.insert(name, typ);
                    }
                    if let Some(guard) = &arm.guard {
                        self.check_expr(fn_name, guard, facts, &env_arm, errors);
                        if let Some(actual) = self.expr_type(guard, facts, &env_arm)
                            && actual.canonical() != Typ::Bool
                        {
                            errors.push(TypeError::TypeMismatch {
                                context: format!("match guard in `{fn_name}`"),
                                expected: Typ::Bool,
                                got: actual,
                            });
                        }
                    }
                    self.check_stmts(fn_name, fn_ret, &arm.body, facts, &mut env_arm, errors);
                }
            }
            Stmt::FieldAssign { base, name, value } => {
                let destination = Expr::Field {
                    base: Box::new(base.clone()),
                    name: name.clone(),
                };
                self.check_expr(fn_name, &destination, facts, env, errors);
                self.check_expr(fn_name, value, facts, env, errors);
                if let (Some(expected), Some(actual)) = (
                    self.expr_type(&destination, facts, env),
                    self.expr_type(value, facts, env),
                ) && !is_compatible(&expected, &actual)
                    && !expected.accepts_sequence_literal(value, &actual)
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("field assignment `{name}` in `{fn_name}`"),
                        expected,
                        got: actual,
                    });
                }
            }
            Stmt::Throw(_) | Stmt::Try { .. } | Stmt::Propagate => {}
        }
    }

    fn check_expr(
        &self,
        fn_name: &str,
        expr: &Expr,
        facts: &Facts,
        env: &HashMap<String, Typ>,
        errors: &mut Vec<TypeError>,
    ) {
        match expr {
            Expr::IntLit(_)
            | Expr::FloatLit(_)
            | Expr::StringLit(_)
            | Expr::BoolLit(_)
            | Expr::Closure { .. } => {}
            Expr::Ident(name) => {
                if !env.contains_key(name) && !is_builtin_fn(name) {
                    errors.push(TypeError::UndefinedVariable {
                        fn_name: fn_name.to_string(),
                        name: name.clone(),
                    });
                }
            }
            Expr::Unary { op, expr: inner } => {
                self.check_expr(fn_name, inner, facts, env, errors);
                if op == "vector-clone"
                    && !matches!(self.expr_type(inner, facts, env), Some(Typ::Vector(item)) if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool))
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("vector clone in `{fn_name}`"),
                        expected: Typ::Named("scalar vector".into()),
                        got: self.expr_type(inner, facts, env).unwrap_or(Typ::Void),
                    });
                }
                if crate::core_ir::integer_cast_spec(op).is_some()
                    && let Some(actual) = self.expr_type(inner, facts, env)
                    && !matches!(actual.canonical(), Typ::Int | Typ::Bool)
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("integer cast in `{fn_name}`"),
                        expected: Typ::Int,
                        got: actual,
                    });
                }
                if matches!(op.as_str(), "float-cast-f32" | "float-cast-f64")
                    && let Some(actual) = self.expr_type(inner, facts, env)
                    && actual != Typ::Float
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("floating width cast in `{fn_name}`"),
                        expected: Typ::Float,
                        got: actual,
                    });
                }
                if (matches!(op.as_str(), "int-to-float" | "float-to-int")
                    || crate::core_ir::saturating_float_cast_spec(op).is_some())
                    && let Some(actual) = self.expr_type(inner, facts, env)
                {
                    let expected = if op == "int-to-float" {
                        Typ::Int
                    } else {
                        Typ::Float
                    };
                    if actual.canonical() != expected {
                        errors.push(TypeError::TypeMismatch {
                            context: format!("numeric conversion in `{fn_name}`"),
                            expected,
                            got: actual,
                        });
                    }
                }
                if op == "bool-to-int"
                    && let Some(actual) = self.expr_type(inner, facts, env)
                    && actual.canonical() != Typ::Bool
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("boolean promotion in `{fn_name}`"),
                        expected: Typ::Bool,
                        got: actual,
                    });
                }
                if matches!(op.as_str(), "complement" | "~")
                    && let Some(actual) = self.expr_type(inner, facts, env)
                {
                    let canonical = actual.canonical();
                    let valid =
                        canonical == Typ::Int || (op == "complement" && canonical == Typ::Bool);
                    if !valid {
                        errors.push(TypeError::TypeMismatch {
                            context: format!("complement operand in `{fn_name}`"),
                            expected: if op == "~" {
                                Typ::Int
                            } else {
                                Typ::Named("Int-or-Bool".into())
                            },
                            got: actual,
                        });
                    }
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                self.check_expr(fn_name, lhs, facts, env, errors);
                self.check_expr(fn_name, rhs, facts, env, errors);
                if let (Some(l), Some(r)) = (
                    self.expr_type(lhs, facts, env),
                    self.expr_type(rhs, facts, env),
                ) {
                    match op.as_str() {
                        "+" => {
                            let l_concrete = is_concrete(&l);
                            let r_concrete = is_concrete(&r);
                            let l_str = l == Typ::String;
                            let r_str = r == Typ::String;
                            if l_str || r_str {
                                if l_str && r_str {
                                    // ok
                                } else if l_concrete && r_concrete {
                                    errors.push(TypeError::TypeMismatch {
                                        context: format!("binary `+` in `{fn_name}`"),
                                        expected: Typ::String,
                                        got: if l_str { r } else { l },
                                    });
                                }
                            } else if l_concrete && r_concrete {
                                if !is_numeric(&l) || !is_numeric(&r) {
                                    errors.push(TypeError::TypeMismatch {
                                        context: format!("binary `+` in `{fn_name}`"),
                                        expected: Typ::Int,
                                        got: if !is_numeric(&l) { l } else { r },
                                    });
                                }
                            }
                        }
                        "&" | "|" | "^" => {
                            if is_concrete(&l)
                                && is_concrete(&r)
                                && crate::core_ir::bitwise_result_type(
                                    Some(l.clone()),
                                    Some(r.clone()),
                                )
                                .is_none()
                            {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("binary `{op}` operands in `{fn_name}`"),
                                    expected: if l == Typ::Bool { Typ::Bool } else { Typ::Int },
                                    got: r,
                                });
                            }
                        }
                        "-" | "*" | "/" | "<<" | ">>" => {
                            if l != Typ::Int && l != Typ::Float && is_concrete(&l) {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("binary `{op}` lhs in `{fn_name}`"),
                                    expected: Typ::Int,
                                    got: l,
                                });
                            }
                            if r != Typ::Int && r != Typ::Float && is_concrete(&r) {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("binary `{op}` rhs in `{fn_name}`"),
                                    expected: Typ::Int,
                                    got: r,
                                });
                            }
                        }
                        "%" => {
                            if l != Typ::Int && is_concrete(&l) {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("binary `{op}` lhs in `{fn_name}`"),
                                    expected: Typ::Int,
                                    got: l,
                                });
                            }
                            if r != Typ::Int && is_concrete(&r) {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("binary `{op}` rhs in `{fn_name}`"),
                                    expected: Typ::Int,
                                    got: r,
                                });
                            }
                        }
                        "==" | "!=" | "<" | ">" | "<=" | ">=" => {
                            if is_concrete(&l) && is_concrete(&r) && !is_compatible(&l, &r) {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("binary `{op}` in `{fn_name}`"),
                                    expected: l,
                                    got: r,
                                });
                            }
                        }
                        "&&" | "||" => {
                            if l != Typ::Bool && is_concrete(&l) {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("binary `{op}` lhs in `{fn_name}`"),
                                    expected: Typ::Bool,
                                    got: l,
                                });
                            }
                            if r != Typ::Bool && is_concrete(&r) {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("binary `{op}` rhs in `{fn_name}`"),
                                    expected: Typ::Bool,
                                    got: r,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            Expr::StructInit { name, fields, .. } => match facts.structs.get(name) {
                Some(schema) => {
                    let mut seen = HashSet::new();
                    for (field_name, field_expr) in fields {
                        self.check_expr(fn_name, field_expr, facts, env, errors);
                        if !seen.insert(field_name.clone()) {
                            errors.push(TypeError::UnknownField {
                                struct_name: name.clone(),
                                field: field_name.clone(),
                            });
                        }
                        let expected_typ =
                            schema.iter().find(|(f, _)| f == field_name).map(|(_, t)| t);
                        match expected_typ {
                            Some(expected) => {
                                if let Some(actual) = self.expr_type(field_expr, facts, env)
                                    && !is_compatible(expected, &actual)
                                    && !expected.accepts_sequence_literal(field_expr, &actual)
                                {
                                    errors.push(TypeError::TypeMismatch {
                                        context: format!("field `{field_name}` in struct `{name}`"),
                                        expected: expected.clone(),
                                        got: actual,
                                    });
                                }
                            }
                            None => {
                                errors.push(TypeError::UnknownField {
                                    struct_name: name.clone(),
                                    field: field_name.clone(),
                                });
                            }
                        }
                    }
                    for (field_name, _) in schema {
                        if !seen.contains(field_name) {
                            errors.push(TypeError::UnknownField {
                                struct_name: name.clone(),
                                field: field_name.clone(),
                            });
                        }
                    }
                }
                None => {
                    errors.push(TypeError::StructNotFound {
                        fn_name: fn_name.to_string(),
                        name: name.clone(),
                    });
                }
            },
            Expr::Field { base, name, .. } => {
                self.check_expr(fn_name, base, facts, env, errors);
                if let Some(typ) = self.expr_type(base, facts, env)
                    && matches!(
                        typ.canonical(),
                        Typ::Int | Typ::Float | Typ::Bool | Typ::String | Typ::Void | Typ::Array(_)
                    )
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("field `{name}` base in `{fn_name}`"),
                        expected: Typ::Named("object".into()),
                        got: typ,
                    });
                    return;
                }
                if let Some(base_typ) = self.expr_type(base, facts, env)
                    && let Typ::Named(struct_name) = &base_typ
                    && let Some(schema) = facts.structs.get(struct_name)
                    && !schema.iter().any(|(f, _)| f == name)
                {
                    errors.push(TypeError::UnknownField {
                        struct_name: struct_name.clone(),
                        field: name.clone(),
                    });
                }
            }
            Expr::ArrayLit(items) => {
                let mut item_typ: Option<Typ> = None;
                for item in items {
                    self.check_expr(fn_name, item, facts, env, errors);
                    if crate::core_ir::empty_sequence_literal(item) {
                        continue;
                    }
                    let typ = self.expr_type(item, facts, env);
                    match (&item_typ, typ) {
                        (Some(expected), Some(actual)) => {
                            if !is_compatible(expected, &actual) {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("array literal element in `{fn_name}`"),
                                    expected: expected.clone(),
                                    got: actual,
                                });
                            }
                        }
                        (None, Some(actual)) => item_typ = Some(actual),
                        _ => {}
                    }
                }
                if let Some(expected) = item_typ {
                    for item in items
                        .iter()
                        .filter(|item| crate::core_ir::empty_sequence_literal(item))
                    {
                        let actual = self.expr_type(item, facts, env).unwrap_or(Typ::Void);
                        if !expected.accepts_sequence_literal(item, &actual) {
                            errors.push(TypeError::TypeMismatch {
                                context: format!("empty array literal element in `{fn_name}`"),
                                expected: expected.clone(),
                                got: actual,
                            });
                        }
                    }
                }
            }
            Expr::Index { base, index, .. } => {
                self.check_expr(fn_name, base, facts, env, errors);
                self.check_expr(fn_name, index, facts, env, errors);
                if let Some(index_typ) = self.expr_type(index, facts, env)
                    && index_typ != Typ::Int
                    && is_concrete(&index_typ)
                {
                    errors.push(TypeError::IndexNotInt {
                        expr: format!("array index in `{fn_name}`"),
                    });
                }
                if let Some(base_typ) = self.expr_type(base, facts, env)
                    && !matches!(
                        base_typ,
                        Typ::Array(_) | Typ::Vector(_) | Typ::Named(_) | Typ::Generic(_)
                    )
                    && is_concrete(&base_typ)
                {
                    errors.push(TypeError::NotArray {
                        expr: format!("indexed base in `{fn_name}`"),
                    });
                }
            }
            Expr::Call { callee, args, .. } => {
                if let Expr::Ident(callee_name) = callee.as_ref() {
                    if is_builtin_fn(callee_name) {
                        for arg in args {
                            self.check_expr(fn_name, arg, facts, env, errors);
                        }
                        return;
                    }
                    if let Some((params, _ret)) = facts.functions.get(callee_name) {
                        if params.len() != args.len() {
                            errors.push(TypeError::ArityMismatch {
                                caller: fn_name.to_string(),
                                fn_name: callee_name.clone(),
                                expected: params.len(),
                                got: args.len(),
                            });
                        }
                        for ((_, param_typ), arg) in params.iter().zip(args.iter()) {
                            self.check_expr(fn_name, arg, facts, env, errors);
                            if let Some(arg_typ) = self.expr_type(arg, facts, env)
                                && !is_compatible(param_typ, &arg_typ)
                                && !param_typ.accepts_sequence_literal(arg, &arg_typ)
                            {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("argument for `{callee_name}` in `{fn_name}`"),
                                    expected: param_typ.clone(),
                                    got: arg_typ,
                                });
                            }
                        }
                        for arg in args.iter().skip(params.len()) {
                            self.check_expr(fn_name, arg, facts, env, errors);
                        }
                    } else {
                        errors.push(TypeError::UndefinedVariable {
                            fn_name: fn_name.to_string(),
                            name: callee_name.clone(),
                        });
                        for arg in args {
                            self.check_expr(fn_name, arg, facts, env, errors);
                        }
                    }
                } else {
                    self.check_expr(fn_name, callee, facts, env, errors);
                    for arg in args {
                        self.check_expr(fn_name, arg, facts, env, errors);
                    }
                }
            }
        }
    }

    pub(crate) fn expr_type(
        &self,
        expr: &Expr,
        facts: &Facts,
        env: &HashMap<String, Typ>,
    ) -> Option<Typ> {
        match expr {
            Expr::IntLit(_) => Some(Typ::Int),
            Expr::FloatLit(_) => Some(Typ::Float),
            Expr::StringLit(_) => Some(Typ::String),
            Expr::BoolLit(_) => Some(Typ::Bool),
            Expr::Ident(name) => env
                .get(name)
                .cloned()
                .or_else(|| is_builtin_fn(name).then(|| builtin_return_type(name))),
            Expr::StructInit { name, .. } => Some(Typ::Named(name.clone())),
            Expr::Field { base, name, .. } => {
                if let Some(base_typ) = self.expr_type(base, facts, env)
                    && let Typ::Named(struct_name) = &base_typ
                    && let Some(schema) = facts.structs.get(struct_name)
                {
                    return schema
                        .iter()
                        .find(|(f, _)| f == name)
                        .map(|(_, t)| t.clone());
                }
                None
            }
            Expr::ArrayLit(items) => {
                let item_typ = items
                    .iter()
                    .filter(|item| !crate::core_ir::empty_sequence_literal(item))
                    .find_map(|item| self.expr_type(item, facts, env));
                Some(Typ::Array(Box::new(item_typ.unwrap_or(Typ::Void))))
            }
            Expr::Index { base, .. } => {
                if let Some(Typ::Array(item) | Typ::Vector(item)) = self.expr_type(base, facts, env)
                {
                    Some(*item)
                } else {
                    None
                }
            }
            Expr::Unary { op, expr, .. } => match op.as_str() {
                "!" => Some(Typ::Bool),
                "bool-to-int" | "float-to-int" => Some(Typ::Int),
                "int-to-float" | "float-cast-f32" | "float-cast-f64" => Some(Typ::Float),
                op if crate::core_ir::integer_cast_spec(op).is_some()
                    || crate::core_ir::saturating_float_cast_spec(op).is_some() =>
                {
                    Some(Typ::Int)
                }
                "-" => self.expr_type(expr, facts, env),
                _ => self.expr_type(expr, facts, env),
            },
            Expr::Binary { op, lhs, rhs, .. } => match op.as_str() {
                "+" => {
                    let l = self.expr_type(lhs, facts, env);
                    let r = self.expr_type(rhs, facts, env);
                    match (l, r) {
                        (Some(Typ::String), Some(Typ::String)) => Some(Typ::String),
                        (Some(Typ::Float), _) | (_, Some(Typ::Float)) => Some(Typ::Float),
                        _ => Some(Typ::Int),
                    }
                }
                "&" | "|" | "^" => crate::core_ir::bitwise_result_type(
                    self.expr_type(lhs, facts, env),
                    self.expr_type(rhs, facts, env),
                ),
                "-" | "*" | "/" | "%" | "<<" | ">>" => {
                    let l = self.expr_type(lhs, facts, env);
                    let r = self.expr_type(rhs, facts, env);
                    if l == Some(Typ::Float) || r == Some(Typ::Float) {
                        Some(Typ::Float)
                    } else {
                        Some(Typ::Int)
                    }
                }
                "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => Some(Typ::Bool),
                _ => None,
            },
            Expr::Call { callee, .. } => {
                if let Expr::Ident(name) = callee.as_ref() {
                    if is_builtin_fn(name) {
                        Some(builtin_return_type(name))
                    } else {
                        facts.functions.get(name).map(|(_, ret)| ret.clone())
                    }
                } else {
                    None
                }
            }
            Expr::Closure { .. } => None,
        }
    }
}

impl TypeChecker {
    fn check_match_pattern(
        &self,
        _fn_name: &str,
        pattern: &MatchPattern,
        facts: &Facts,
        env: &mut HashMap<String, Typ>,
        errors: &mut Vec<TypeError>,
    ) {
        match pattern {
            MatchPattern::StructPat { name, fields } => {
                let schema = match facts.structs.get(name) {
                    Some(s) => s,
                    None => {
                        errors.push(TypeError::StructNotFound {
                            fn_name: _fn_name.to_string(),
                            name: name.clone(),
                        });
                        return;
                    }
                };
                for (field_name, subpat) in fields {
                    if !schema.iter().any(|(f, _)| f == field_name) {
                        errors.push(TypeError::UnknownField {
                            struct_name: name.clone(),
                            field: field_name.clone(),
                        });
                    }
                    self.check_match_pattern(_fn_name, subpat, facts, env, errors);
                }
            }
            MatchPattern::IdentPat(var_name) => {
                env.insert(var_name.clone(), Typ::Generic("inferred".into()));
            }
            MatchPattern::TuplePat(pats) => {
                for subpat in pats {
                    self.check_match_pattern(_fn_name, subpat, facts, env, errors);
                }
            }
            MatchPattern::ArrayPat(pats) => {
                for subpat in pats {
                    self.check_match_pattern(_fn_name, subpat, facts, env, errors);
                }
            }
            MatchPattern::IntPat(_)
            | MatchPattern::StringPat(_)
            | MatchPattern::BoolPat(_)
            | MatchPattern::WildPat
            | MatchPattern::RestPat => {}
        }
    }
}

fn is_concrete(typ: &Typ) -> bool {
    !matches!(typ, Typ::Named(_) | Typ::Generic(_))
}

fn is_numeric(typ: &Typ) -> bool {
    matches!(typ, Typ::Int | Typ::Float)
}

fn is_compatible(expected: &Typ, actual: &Typ) -> bool {
    if matches!(expected, Typ::Generic(_)) || matches!(actual, Typ::Generic(_)) {
        return true;
    }
    expected.compatible_with(actual)
}

fn is_builtin_fn(name: &str) -> bool {
    if crate::core_atomic::signature(name).is_some() {
        return true;
    }
    matches!(
        name,
        "print"
            | "print-int"
            | "print-string"
            | "to-int"
            | "to-string"
            | "len"
            | "throw-error"
            | "str-len"
            | "str-concat"
            | "str-eq"
            | "str-contains"
            | "str-trim"
            | "str-to-int"
            | "str-starts-with"
            | "str-index-of"
            | "str-slice"
            | "str-is-int"
            | "str-table-has"
            | "str-table-get-int"
            | "array-push"
            | "array-pop"
            | "array-len" | "array_len"
            | "bool-to-int"
            | "int-to-bool"
            // x86 bare-metal intrinsics
            | "outb" | "inb" | "outl" | "inl"
            | "load8" | "load16" | "load32" | "load64"
            | "store8" | "store16" | "store32" | "store64"
            | "hlt" | "cli" | "sti" | "pause"
            | "lidt" | "invlpg" | "read-cr2" | "read-cr3"
            | "invoke" | "invoke1" | "invoke2"
    )
}

fn builtin_return_type(name: &str) -> Typ {
    if let Some(sig) = crate::core_atomic::signature(name) {
        return sig.ret.clone();
    }
    match name {
        "len" | "str-len" | "array-len" | "array_len" | "bool-to-int" | "to-int" | "str-to-int"
        | "str-index-of" | "str-table-get-int" | "inb" | "inl" | "load8" | "load16" | "load32"
        | "load64" | "read-cr2" | "read-cr3" | "invoke" | "invoke1" | "invoke2" => Typ::Int,
        "str-eq" | "str-contains" | "str-starts-with" | "str-is-int" | "str-table-has"
        | "int-to-bool" => Typ::Bool,
        "str-concat" | "str-trim" | "str-slice" | "to-string" => Typ::String,
        _ => Typ::Void,
    }
}

fn format_typ(typ: &Typ) -> String {
    match typ {
        Typ::Int => "Int".to_string(),
        Typ::Float => "Float".to_string(),
        Typ::String => "String".to_string(),
        Typ::Bool => "Bool".to_string(),
        Typ::Void => "Void".to_string(),
        Typ::Array(item) => format!("[{}]", format_typ(item)),
        Typ::Vector(item) => format!("Vec<{}>", format_typ(item)),
        Typ::Named(name) => name.clone(),
        Typ::Generic(name) => name.clone(),
    }
}

fn format_type_error(error: &TypeError) -> String {
    match error {
        TypeError::ArityMismatch {
            caller,
            fn_name,
            expected,
            got,
        } => format!("function `{fn_name}` expects {expected} args, got {got} in `{caller}`"),
        TypeError::ReturnTypeMismatch {
            fn_name,
            expected,
            got,
        } => format!(
            "return type mismatch in `{fn_name}`: expected {}, got {}",
            format_typ(expected),
            format_typ(got)
        ),
        TypeError::ReturnValueInVoid { fn_name } => {
            format!("return value in void function `{fn_name}`")
        }
        TypeError::MissingReturnValue { fn_name } => {
            format!("missing return value in `{fn_name}`")
        }
        TypeError::UnknownField { struct_name, field } => {
            format!("unknown field `{field}` for struct `{struct_name}`")
        }
        TypeError::UndefinedVariable { fn_name, name } => {
            format!("unresolved identifier `{name}` in `{fn_name}`")
        }
        TypeError::StructNotFound { fn_name, name } => {
            format!("unknown struct `{name}` in `{fn_name}`")
        }
        TypeError::TypeMismatch {
            context,
            expected,
            got,
        } => format!(
            "type mismatch in `{context}`: expected {}, got {}",
            format_typ(expected),
            format_typ(got)
        ),
        TypeError::NotArray { expr } => format!("{expr} expected array"),
        TypeError::IndexNotInt { expr } => format!("{expr} expected int index"),
        TypeError::MissingInterfaceMethod {
            class_name,
            interface_name,
            method_name,
        } => format!(
            "missing interface method `{method_name}` for class `{class_name}` implementing `{interface_name}`"
        ),
        TypeError::InterfaceMethodSigMismatch {
            class_name,
            interface_name,
            method_name,
            detail,
        } => format!(
            "interface method signature mismatch for `{method_name}` in class `{class_name}` implementing `{interface_name}`: {detail}"
        ),
        TypeError::InterfaceNotFound {
            class_name,
            interface_name,
        } => format!("interface `{interface_name}` not found for class `{class_name}`"),
    }
}

fn format_type_errors(errors: &[TypeError]) -> String {
    errors
        .iter()
        .map(format_type_error)
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn typecheck_executable(module: &UnifiedModule) -> Result<(), String> {
    typecheck_module(module, ModuleKind::Executable)
}

pub fn typecheck_module(module: &UnifiedModule, kind: ModuleKind) -> Result<(), String> {
    let mut top_level = HashSet::new();
    for decl in &module.decls {
        match decl {
            Decl::Struct { name, .. } | Decl::Class { name, .. } | Decl::Function { name, .. }
                if !top_level.insert(name.clone()) =>
            {
                return Err(format!("duplicate top-level name `{name}`"));
            }
            _ => {}
        }
        if let Decl::Class { name, methods, .. } = decl {
            for method in methods {
                if let Decl::Function {
                    name: method_name, ..
                } = method
                {
                    let mangled = format!("{}_{}", name, method_name);
                    if !top_level.insert(mangled.clone()) {
                        return Err(format!("duplicate top-level name `{mangled}`"));
                    }
                }
            }
        }
    }

    if kind == ModuleKind::Executable && !top_level.contains("main") {
        return Err("missing main function".to_string());
    }

    match TypeChecker::new().check_module(module) {
        Ok(()) => Ok(()),
        Err(errors) => Err(format_type_errors(&errors)),
    }
}

pub fn typecheck_resolved(
    resolved: &ResolvedBuildParser,
    module: &UnifiedModule,
) -> Result<(), String> {
    if let ResolvedBuildParser::CoreIr(parser_id) = resolved
        && uses_family_typecheck(*parser_id)
    {
        return typecheck_for_parser(*parser_id, module);
    }
    typecheck_executable(module)
}

pub fn uses_family_typecheck(parser_id: ParserId) -> bool {
    if let Some(row) = crate::language_support::language_support_for_parser(parser_id.as_str()) {
        return row.can_typecheck();
    }
    false
}

fn typecheck_for_parser(parser_id: ParserId, module: &UnifiedModule) -> Result<(), String> {
    let normalized = normalize_module(parser_id, module);
    if uses_polyglot_entrypoint_typecheck(parser_id) {
        return typecheck_polyglot_entrypoints(&normalized);
    }
    typecheck_executable(&normalized)
}

fn uses_polyglot_entrypoint_typecheck(parser_id: ParserId) -> bool {
    matches!(
        parser_id,
        ParserId::Lua | ParserId::JavaScript | ParserId::TypeScript
    )
}

fn typecheck_polyglot_entrypoints(module: &UnifiedModule) -> Result<(), String> {
    let mut checked_decls: Vec<Decl> = module
        .decls
        .iter()
        .filter(|decl| {
            matches!(
                decl,
                Decl::Struct { .. } | Decl::Class { .. } | Decl::Global { .. }
            )
        })
        .cloned()
        .collect();
    let functions: Vec<Decl> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function {
                name,
                params,
                ret,
                body,
                type_params,
            } => Some(Decl::Function {
                name: name.clone(),
                params: params.clone(),
                ret: ret.clone(),
                body: if name == "answer" || name == "main" {
                    body.clone()
                } else {
                    Vec::new()
                },
                type_params: type_params.clone(),
            }),
            _ => None,
        })
        .collect();
    if !functions
        .iter()
        .any(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
    {
        return Err("missing main function".to_string());
    }
    checked_decls.extend(functions);
    typecheck_executable(&UnifiedModule::new(checked_decls))
}

pub fn normalize_module(parser_id: ParserId, module: &UnifiedModule) -> UnifiedModule {
    let decls = module
        .decls
        .iter()
        .map(|decl| normalize_decl(parser_id, decl))
        .collect();
    let mut normalized = UnifiedModule::with_identity(decls, module.identity.clone());
    if matches!(
        parser_id,
        ParserId::C | ParserId::Cpp | ParserId::ObjC | ParserId::ObjCpp
    ) {
        TypeChecker::new().resolve_c_value_promotions(&mut normalized);
    }
    if parser_id == ParserId::Rust {
        TypeChecker::new().resolve_complements(&mut normalized);
    }
    normalized
}

fn normalize_decl(parser_id: ParserId, decl: &Decl) -> Decl {
    match decl {
        Decl::Function {
            name,
            params,
            ret,
            body,
            type_params,
        } => {
            let mut body = body.clone();
            let ret = normalize_function_ret(parser_id, ret, &body);
            normalize_function_body(parser_id, &ret, &mut body);
            Decl::Function {
                name: name.clone(),
                params: params
                    .iter()
                    .map(|(n, t)| (n.clone(), normalize_parser_type(parser_id, t)))
                    .collect(),
                ret,
                body,
                type_params: type_params.clone(),
            }
        }
        Decl::Global {
            name,
            typ,
            init,
            mutable,
            constant_value,
        } => Decl::Global {
            name: name.clone(),
            typ: normalize_parser_type(parser_id, typ),
            init: init.clone(),
            mutable: *mutable,
            constant_value: *constant_value,
        },
        Decl::Class {
            name,
            fields,
            methods,
            visibility,
            extends,
            implements,
            type_params,
        } => Decl::Class {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|(n, t)| (n.clone(), normalize_parser_type(parser_id, t)))
                .collect(),
            methods: methods
                .iter()
                .map(|m| normalize_decl(parser_id, m))
                .collect(),
            visibility: *visibility,
            extends: extends.clone(),
            implements: implements.clone(),
            type_params: type_params.clone(),
        },
        Decl::Struct {
            name,
            fields,
            type_params,
        } => Decl::Struct {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|(n, t)| (n.clone(), normalize_parser_type(parser_id, t)))
                .collect(),
            type_params: type_params.clone(),
        },
        other => other.clone(),
    }
}

fn normalize_function_ret(parser_id: ParserId, ret: &Typ, body: &[Stmt]) -> Typ {
    let normalized = normalize_parser_type(parser_id, ret);
    if matches!(
        parser_id,
        ParserId::Lua | ParserId::Perl | ParserId::Python | ParserId::Ruby | ParserId::JavaScript
    ) && normalized == Typ::Void
    {
        if let Some(inferred) = infer_return_type_from_body(body) {
            return normalize_parser_type(parser_id, &inferred);
        }
        if matches!(parser_id, ParserId::JavaScript) && body_returns_expression(body) {
            return normalize_parser_type(parser_id, &Typ::Named("Any".to_string()));
        }
    }
    normalized
}

fn normalize_function_body(parser_id: ParserId, ret: &Typ, body: &mut Vec<Stmt>) {
    if !matches!(
        parser_id,
        ParserId::Php
            | ParserId::Lua
            | ParserId::Zig
            | ParserId::Scala
            | ParserId::Perl
            | ParserId::JavaScript
            | ParserId::TypeScript
    ) {
        return;
    }
    if body.iter().any(|s| matches!(s, Stmt::Return(_))) {
        return;
    }
    if *ret == Typ::Void {
        return;
    }
    if let Some(Stmt::Expr(expr)) = body.last().cloned() {
        body.pop();
        body.push(Stmt::Return(Some(expr)));
    }
}

fn infer_return_type_from_body(body: &[Stmt]) -> Option<Typ> {
    for stmt in body.iter().rev() {
        match stmt {
            Stmt::Return(Some(expr)) => return expr_type_hint(expr),
            Stmt::Expr(expr) => return expr_type_hint(expr),
            _ => {}
        }
    }
    None
}

fn expr_type_hint(expr: &Expr) -> Option<Typ> {
    match expr {
        Expr::IntLit(_) => Some(Typ::Int),
        Expr::FloatLit(_) => Some(Typ::Float),
        Expr::StringLit(_) => Some(Typ::String),
        Expr::BoolLit(_) => Some(Typ::Bool),
        _ => None,
    }
}

fn body_returns_expression(body: &[Stmt]) -> bool {
    body.iter()
        .rev()
        .any(|stmt| matches!(stmt, Stmt::Return(Some(_)) | Stmt::Expr(_) | Stmt::Throw(_)))
}

fn normalize_type(typ: &Typ) -> Typ {
    match typ {
        Typ::Named(name) => {
            let lower = name.to_ascii_lowercase();
            match lower.as_str() {
                "int" | "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32"
                | "u64" | "u128" | "usize" | "integer" | "number" | "int32" | "int64" | "long"
                | "char" | "byte" | "short" | "uintptr" | "size_t" | "ssize_t" => Typ::Int,
                "float" | "f32" | "f64" | "double" => Typ::Float,
                "bool" | "boolean" => Typ::Bool,
                "string" | "str" => Typ::String,
                "void" | "unit" | "nil" | "none" | "()" => Typ::Void,
                _ if name == "Int" => Typ::Int,
                _ if name == "Unit" => Typ::Void,
                _ => typ.clone(),
            }
        }
        other => other.clone(),
    }
}

fn normalize_parser_type(parser_id: ParserId, typ: &Typ) -> Typ {
    if matches!(
        parser_id,
        ParserId::C | ParserId::Cpp | ParserId::ObjC | ParserId::ObjCpp
    ) && let Typ::Named(name) = typ
    {
        return crate::compiler::tree_front::normalize_c_type(name);
    }
    if matches!(
        parser_id,
        ParserId::JavaScript | ParserId::Python | ParserId::Ruby | ParserId::Php
    ) && matches!(typ, Typ::Named(name) if name == "Any")
    {
        return Typ::Int;
    }
    normalize_type(typ)
}

#[cfg(test)]
mod tests {
    use super::uses_family_typecheck;
    use crate::parser_registry::ParserId;

    #[test]
    fn numeric_casts_resolve_from_index_fields_calls_and_arithmetic() {
        for source in [
            "fn main() -> i64 { let items: Vec<f64> = vec![20.0, 22.0]; return (items[0] + items[1]) as i64; }",
            "struct Point { x: f64 } fn main() -> i64 { let point = Point { x: 42.9 }; return point.x as i64; }",
            "fn value() -> f64 { return 42.9; } fn main() -> i64 { return value() as i64; }",
            "fn main() -> i64 { return (20.5 + 21.5) as i64; }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let module = super::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            assert!(
                super::TypeChecker::new().check_module(&module).is_ok(),
                "{source}"
            );
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
        }
    }

    #[test]
    fn family_typecheck_follows_language_matrix_go() {
        assert!(uses_family_typecheck(ParserId::Go));
    }

    #[test]
    fn family_typecheck_in_lang_follows_matrix() {
        assert!(uses_family_typecheck(ParserId::In));
    }
    use super::*;
    use crate::core_ir::{Decl, Expr, MethodSig, Stmt, Typ, UnifiedModule, Visibility};

    #[test]
    fn globals_are_visible_and_locals_shadow_them() {
        for source in [
            "var count: Int = 3;\nfn main() -> Int { count = count + 5; return count; }",
            "var count: Int = 3;\nfn read(count: String) -> String { return count; }\nfn main() -> Bool { let count = true; return count; }",
            "var count: Int = 3;\nfn main() -> Int { return count; }",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            assert!(TypeChecker::new().check_module(&module).is_ok(), "{source}");
        }
    }

    #[test]
    fn global_assignments_check_the_declared_type() {
        let module = crate::in_lang_parse::parse_in_source(
            "var count: Int = 3;\nfn main() { count = true; }",
        )
        .unwrap();
        let errors = TypeChecker::new().check_module(&module).unwrap_err();
        assert!(
            errors.iter().any(|error| matches!(
                error,
                TypeError::TypeMismatch {
                    expected: Typ::Int,
                    got: Typ::Bool,
                    ..
                }
            )),
            "{errors:?}"
        );
    }

    #[test]
    fn polyglot_entrypoint_typechecking_retains_globals() {
        let module = crate::in_lang_parse::parse_in_source(
            "var count: Int = 3;\nfn main() -> Int { return count; }",
        )
        .unwrap();
        assert!(typecheck_for_parser(ParserId::JavaScript, &module).is_ok());
    }

    fn function(name: &str, ret: Typ, params: Vec<(String, Typ)>, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.to_string(),
            params,
            ret,
            body,
            type_params: vec![],
        }
    }

    fn function_with_params(name: &str, params: Vec<(String, Typ)>, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.to_string(),
            params,
            ret: Typ::Void,
            body,
            type_params: vec![],
        }
    }

    fn function_with_ret(name: &str, ret: Typ, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.to_string(),
            params: vec![],
            ret,
            body,
            type_params: vec![],
        }
    }

    fn function_with_params_and_ret(
        name: &str,
        params: Vec<(String, Typ)>,
        ret: Typ,
        body: Vec<Stmt>,
    ) -> Decl {
        Decl::Function {
            name: name.to_string(),
            params,
            ret,
            body,
            type_params: vec![],
        }
    }

    fn module(decls: Vec<Decl>) -> UnifiedModule {
        UnifiedModule::new(decls)
    }

    fn point_struct() -> Decl {
        Decl::Struct {
            name: "Point".to_string(),
            fields: vec![("x".to_string(), Typ::Int), ("y".to_string(), Typ::Int)],
            type_params: vec![],
        }
    }

    #[test]
    fn test_call_arity_mismatch() {
        let m = module(vec![
            function("helper", Typ::Void, vec![("x".into(), Typ::Int)], vec![]),
            function(
                "main",
                Typ::Void,
                vec![],
                vec![Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![Expr::IntLit(1), Expr::IntLit(2)],
                })],
            ),
        ]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("arity mismatch should fail");
        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::ArityMismatch { caller, fn_name, expected: 1, got: 2 }
                if fn_name == "helper" && caller == "main"
            )),
            "expected ArityMismatch, got: {err:?}"
        );
    }

    #[test]
    fn test_valid_call() {
        let m = module(vec![
            function("helper", Typ::Void, vec![("x".into(), Typ::Int)], vec![]),
            function(
                "main",
                Typ::Void,
                vec![],
                vec![Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![Expr::IntLit(1)],
                })],
            ),
        ]);

        TypeChecker::new()
            .check_module(&m)
            .expect("valid call should pass");
    }

    #[test]
    fn test_return_type_mismatch() {
        let m = module(vec![function(
            "main",
            Typ::Int,
            vec![],
            vec![Stmt::Return(Some(Expr::StringLit("hello".into())))],
        )]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("return type mismatch should fail");
        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::ReturnTypeMismatch { fn_name, expected: Typ::Int, got: Typ::String }
                if fn_name == "main"
            )),
            "expected ReturnTypeMismatch, got: {err:?}"
        );
    }

    #[test]
    fn test_undefined_variable() {
        let m = module(vec![function(
            "main",
            Typ::Void,
            vec![],
            vec![Stmt::Expr(Expr::Ident("undeclared".into()))],
        )]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("undefined variable should fail");
        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::UndefinedVariable { fn_name, name } if fn_name == "main" && name == "undeclared"
            )),
            "expected UndefinedVariable, got: {err:?}"
        );
    }

    #[test]
    fn test_struct_field_access_valid() {
        let m = module(vec![
            Decl::Struct {
                name: "Point".into(),
                fields: vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
                type_params: vec![],
            },
            function(
                "main",
                Typ::Void,
                vec![],
                vec![
                    Stmt::Let(
                        "p".into(),
                        Some(Typ::Named("Point".into())),
                        Expr::StructInit {
                            name: "Point".into(),
                            fields: vec![
                                ("x".into(), Expr::IntLit(1)),
                                ("y".into(), Expr::IntLit(2)),
                            ],
                        },
                    ),
                    Stmt::Expr(Expr::Field {
                        base: Box::new(Expr::Ident("p".into())),
                        name: "x".into(),
                    }),
                ],
            ),
        ]);

        TypeChecker::new()
            .check_module(&m)
            .expect("valid field access should pass");
    }

    #[test]
    fn test_struct_field_access_invalid() {
        let m = module(vec![
            Decl::Struct {
                name: "Point".into(),
                fields: vec![("x".into(), Typ::Int), ("y".into(), Typ::Int)],
                type_params: vec![],
            },
            function(
                "main",
                Typ::Void,
                vec![],
                vec![
                    Stmt::Let(
                        "p".into(),
                        Some(Typ::Named("Point".into())),
                        Expr::StructInit {
                            name: "Point".into(),
                            fields: vec![
                                ("x".into(), Expr::IntLit(1)),
                                ("y".into(), Expr::IntLit(2)),
                            ],
                        },
                    ),
                    Stmt::Expr(Expr::Field {
                        base: Box::new(Expr::Ident("p".into())),
                        name: "z".into(),
                    }),
                ],
            ),
        ]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("invalid field access should fail");
        assert!(
            err.iter().any(
                |e| matches!(e, TypeError::UnknownField { struct_name, field } if struct_name == "Point" && field == "z")
            ),
            "expected UnknownField, got: {err:?}"
        );
    }

    #[test]
    fn test_binary_type_mismatch() {
        let m = module(vec![function(
            "main",
            Typ::Void,
            vec![],
            vec![Stmt::Let(
                "x".into(),
                None,
                Expr::Binary {
                    op: "+".into(),
                    lhs: Box::new(Expr::BoolLit(true)),
                    rhs: Box::new(Expr::IntLit(1)),
                },
            )],
        )]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("bool + int should fail");
        assert!(
            err.iter()
                .any(|e| matches!(e, TypeError::TypeMismatch { context, .. } if context.contains("binary `+`"))),
            "expected TypeMismatch for binary +, got: {err:?}"
        );
    }

    #[test]
    fn test_match_has_wildcard() {
        let m = module(vec![function(
            "main",
            Typ::Void,
            vec![],
            vec![Stmt::Match {
                scrutinee: Expr::IntLit(1),
                arms: vec![crate::core_ir::MatchArm {
                    guard: None,
                    pattern: "_".into(),
                    body: vec![Stmt::Return(None)],
                }],
            }],
        )]);

        TypeChecker::new()
            .check_module(&m)
            .expect("match with wildcard should pass");
    }

    #[test]
    fn test_match_no_arms_fails() {
        let m = module(vec![function(
            "main",
            Typ::Void,
            vec![],
            vec![Stmt::Match {
                scrutinee: Expr::IntLit(1),
                arms: vec![],
            }],
        )]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("match with no arms should fail");
        assert!(
            err.iter()
                .any(|e| matches!(e, TypeError::TypeMismatch { context, .. } if context.contains("no arms"))),
            "expected match no-arms error, got: {err:?}"
        );
    }

    #[test]
    fn test_conservative_named_types_pass() {
        let m = module(vec![function(
            "main",
            Typ::Named("Widget".into()),
            vec![],
            vec![
                Stmt::Let(
                    "w".into(),
                    Some(Typ::Named("Widget".into())),
                    Expr::Ident("UNDECLARED_BUT_NAMED_OK".into()),
                ),
                Stmt::Return(Some(Expr::Ident("w".into()))),
            ],
        )]);

        let result = TypeChecker::new().check_module(&m);
        match result {
            Ok(()) => {}
            Err(errors) => {
                assert!(
                    !errors
                        .iter()
                        .any(|e| matches!(e, TypeError::ReturnTypeMismatch { .. })),
                    "Named types should not produce ReturnTypeMismatch"
                );
            }
        }
    }

    #[test]
    fn test_string_concat_is_valid() {
        let m = module(vec![function(
            "main",
            Typ::String,
            vec![],
            vec![Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::StringLit("hello".into())),
                rhs: Box::new(Expr::StringLit("world".into())),
            }))],
        )]);

        let result = TypeChecker::new().check_module(&m);
        match result {
            Ok(()) => {}
            Err(errors) => {
                assert!(
                    !errors
                        .iter()
                        .any(|e| matches!(e, TypeError::TypeMismatch { context, .. } if context.contains("binary `+`"))),
                    "String + String should not produce binary + error: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn test_index_not_int() {
        let m = module(vec![function(
            "main",
            Typ::Void,
            vec![],
            vec![
                Stmt::Let(
                    "xs".into(),
                    Some(Typ::Array(Box::new(Typ::Int))),
                    Expr::ArrayLit(vec![Expr::IntLit(1)]),
                ),
                Stmt::Expr(Expr::Index {
                    base: Box::new(Expr::Ident("xs".into())),
                    index: Box::new(Expr::StringLit("not_int".into())),
                }),
            ],
        )]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("string index should fail");
        assert!(
            err.iter()
                .any(|e| matches!(e, TypeError::IndexNotInt { .. })),
            "expected IndexNotInt, got: {err:?}"
        );
    }

    #[test]
    fn test_not_array() {
        let m = module(vec![function(
            "main",
            Typ::Void,
            vec![],
            vec![
                Stmt::Let("x".into(), None, Expr::IntLit(42)),
                Stmt::Expr(Expr::Index {
                    base: Box::new(Expr::Ident("x".into())),
                    index: Box::new(Expr::IntLit(0)),
                }),
            ],
        )]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("indexing non-array should fail");
        assert!(
            err.iter().any(|e| matches!(e, TypeError::NotArray { .. })),
            "expected NotArray, got: {err:?}"
        );
    }

    #[test]
    fn test_struct_not_found() {
        let m = module(vec![function(
            "main",
            Typ::Void,
            vec![],
            vec![Stmt::Expr(Expr::StructInit {
                name: "Missing".into(),
                fields: vec![],
            })],
        )]);

        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("unknown struct should fail");
        assert!(
            err.iter()
                .any(|e| matches!(e, TypeError::StructNotFound { fn_name, name } if fn_name == "main" && name == "Missing")),
            "expected StructNotFound, got: {err:?}"
        );
    }

    #[test]
    fn test_int_plus_int_is_valid() {
        let m = module(vec![function(
            "main",
            Typ::Int,
            vec![],
            vec![Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::IntLit(1)),
                rhs: Box::new(Expr::IntLit(2)),
            }))],
        )]);

        TypeChecker::new()
            .check_module(&m)
            .expect("int + int should pass");
    }

    #[test]
    fn test_bool_and_bool_is_valid() {
        let m = module(vec![function(
            "main",
            Typ::Bool,
            vec![],
            vec![Stmt::Return(Some(Expr::Binary {
                op: "&&".into(),
                lhs: Box::new(Expr::BoolLit(true)),
                rhs: Box::new(Expr::BoolLit(false)),
            }))],
        )]);

        TypeChecker::new()
            .check_module(&m)
            .expect("bool && bool should pass");
    }

    #[test]
    fn test_class_implements_interface() {
        let m = module(vec![
            Decl::Interface {
                name: "Drawable".into(),
                methods: vec![MethodSig {
                    name: "draw".into(),
                    params: vec![],
                    ret: Typ::Void,
                }],
                visibility: Visibility::Pub,
                type_params: vec![],
            },
            Decl::Class {
                name: "Circle".into(),
                fields: vec![],
                methods: vec![function("draw", Typ::Void, vec![], vec![])],
                visibility: Visibility::Pub,
                extends: None,
                implements: vec!["Drawable".into()],
                type_params: vec![],
            },
        ]);
        TypeChecker::new()
            .check_module(&m)
            .expect("class implementing interface should pass");
    }

    #[test]
    fn test_class_missing_interface_method() {
        let m = module(vec![
            Decl::Interface {
                name: "Drawable".into(),
                methods: vec![MethodSig {
                    name: "draw".into(),
                    params: vec![],
                    ret: Typ::Void,
                }],
                visibility: Visibility::Pub,
                type_params: vec![],
            },
            Decl::Class {
                name: "Circle".into(),
                fields: vec![],
                methods: vec![],
                visibility: Visibility::Pub,
                extends: None,
                implements: vec!["Drawable".into()],
                type_params: vec![],
            },
        ]);
        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("class missing interface method should fail");
        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::MissingInterfaceMethod {
                    class_name,
                    interface_name,
                    method_name,
                } if class_name == "Circle"
                    && interface_name == "Drawable"
                    && method_name == "draw"
            )),
            "expected MissingInterfaceMethod, got: {err:?}"
        );
    }

    #[test]
    fn test_class_wrong_param_count() {
        let m = module(vec![
            Decl::Interface {
                name: "Drawable".into(),
                methods: vec![MethodSig {
                    name: "draw".into(),
                    params: vec![("x".into(), Typ::Int)],
                    ret: Typ::Void,
                }],
                visibility: Visibility::Pub,
                type_params: vec![],
            },
            Decl::Class {
                name: "Circle".into(),
                fields: vec![],
                methods: vec![function("draw", Typ::Void, vec![], vec![])],
                visibility: Visibility::Pub,
                extends: None,
                implements: vec!["Drawable".into()],
                type_params: vec![],
            },
        ]);
        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("class wrong param count should fail");
        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::InterfaceMethodSigMismatch {
                    class_name,
                    interface_name,
                    method_name,
                    detail,
                } if class_name == "Circle"
                    && interface_name == "Drawable"
                    && method_name == "draw"
                    && detail.contains("parameter count")
            )),
            "expected InterfaceMethodSigMismatch for params, got: {err:?}"
        );
    }

    #[test]
    fn test_class_extends_implicit_implements() {
        let m = module(vec![
            Decl::Interface {
                name: "Shape".into(),
                methods: vec![MethodSig {
                    name: "area".into(),
                    params: vec![],
                    ret: Typ::Float,
                }],
                visibility: Visibility::Pub,
                type_params: vec![],
            },
            Decl::Class {
                name: "Circle".into(),
                fields: vec![],
                methods: vec![function("area", Typ::Float, vec![], vec![])],
                visibility: Visibility::Pub,
                extends: Some("Shape".into()),
                implements: vec![],
                type_params: vec![],
            },
        ]);
        TypeChecker::new()
            .check_module(&m)
            .expect("class extending interface should pass");
    }

    #[test]
    fn test_interface_not_found() {
        let m = module(vec![Decl::Class {
            name: "Circle".into(),
            fields: vec![],
            methods: vec![],
            visibility: Visibility::Pub,
            extends: None,
            implements: vec!["UnknownIface".into()],
            type_params: vec![],
        }]);
        let err = TypeChecker::new()
            .check_module(&m)
            .expect_err("class implementing unknown interface should fail");
        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::InterfaceNotFound {
                    class_name,
                    interface_name,
                } if class_name == "Circle" && interface_name == "UnknownIface"
            )),
            "expected InterfaceNotFound, got: {err:?}"
        );
    }

    // Moved from core_typecheck.rs

    #[test]
    fn rejects_duplicate_top_level_function_names() {
        let err = typecheck_executable(&module(vec![
            function("main", Typ::Void, vec![], vec![]),
            function("main", Typ::Void, vec![], vec![]),
        ]))
        .expect_err("duplicate function names should fail");

        assert!(
            err.contains("duplicate top-level name `main`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_top_level_struct_and_function_names() {
        let err = typecheck_executable(&module(vec![
            Decl::Struct {
                name: "Widget".to_string(),
                fields: vec![],
                type_params: vec![],
            },
            function("Widget", Typ::Void, vec![], vec![]),
            function("main", Typ::Void, vec![], vec![]),
        ]))
        .expect_err("duplicate struct/function names should fail");

        assert!(
            err.contains("duplicate top-level name `Widget`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_executable_module_without_main() {
        let err =
            typecheck_executable(&module(vec![function("helper", Typ::Void, vec![], vec![])]))
                .expect_err("executable modules require main");

        assert!(
            err.contains("missing main function"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_unresolved_function_calls_in_bounded_bodies() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Void,
                vec![],
                vec![
                    Stmt::If {
                        cond: Expr::BoolLit(true),
                        then_body: vec![Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("missing".to_string())),
                            args: vec![],
                        })],
                        else_body: vec![],
                    },
                    Stmt::Loop {
                        kind: crate::core_ir::LoopKind::While,
                        cond: Some(Expr::BoolLit(false)),
                        body: vec![],
                    },
                ],
            )]))
            .expect_err("unresolved direct calls should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::UndefinedVariable { fn_name, name } if fn_name == "main" && name == "missing"
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_unresolved_identifiers_in_value_position() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Int,
                vec![],
                vec![Stmt::Return(Some(Expr::Ident("missing".to_string())))],
            )]))
            .expect_err("unresolved identifiers should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::UndefinedVariable { fn_name, name } if fn_name == "main" && name == "missing"
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_assignment_to_unresolved_identifier() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Void,
                vec![],
                vec![Stmt::Assign("missing".to_string(), Expr::IntLit(1))],
            )]))
            .expect_err("assignments require existing bindings");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::UndefinedVariable { fn_name, name } if fn_name == "main" && name == "missing"
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn accepts_array_index_assignment() {
        TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Void,
                vec![],
                vec![
                    Stmt::Let(
                        "xs".to_string(),
                        Some(Typ::Array(Box::new(Typ::Int))),
                        Expr::ArrayLit(vec![Expr::IntLit(1), Expr::IntLit(2)]),
                    ),
                    Stmt::IndexAssign {
                        base: Expr::Ident("xs".to_string()),
                        index: Expr::IntLit(1),
                        value: Expr::IntLit(9),
                    },
                ],
            )]))
            .expect("array index assignment should typecheck");
    }

    #[test]
    fn rejects_array_index_assignment_type_mismatch() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Void,
                vec![],
                vec![
                    Stmt::Let(
                        "xs".to_string(),
                        Some(Typ::Array(Box::new(Typ::Int))),
                        Expr::ArrayLit(vec![Expr::IntLit(1), Expr::IntLit(2)]),
                    ),
                    Stmt::IndexAssign {
                        base: Expr::Ident("xs".to_string()),
                        index: Expr::IntLit(1),
                        value: Expr::StringLit("bad".to_string()),
                    },
                ],
            )]))
            .expect_err("array index assignment value must match item type");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::TypeMismatch { context, .. } if context.contains("array assignment")
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn accepts_function_params_as_bound_identifiers() {
        TypeChecker::new()
            .check_module(&module(vec![
                function_with_params_and_ret(
                    "helper",
                    vec![("value".to_string(), Typ::Int)],
                    Typ::Int,
                    vec![Stmt::Return(Some(Expr::Ident("value".to_string())))],
                ),
                function(
                    "main",
                    Typ::Void,
                    vec![],
                    vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".to_string())),
                        args: vec![Expr::IntLit(7)],
                    })],
                ),
            ]))
            .expect("function parameters should be in scope");
    }

    #[test]
    fn accepts_resolved_calls_in_bounded_bodies() {
        TypeChecker::new()
            .check_module(&module(vec![
                function("helper", Typ::Void, vec![], vec![]),
                function(
                    "main",
                    Typ::Void,
                    vec![],
                    vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".to_string())),
                        args: vec![],
                    })],
                ),
            ]))
            .expect("resolved direct calls should pass");
    }

    #[test]
    fn rejects_call_arity_mismatch() {
        let err = TypeChecker::new()
            .check_module(&module(vec![
                function_with_params("helper", vec![("value".to_string(), Typ::Int)], vec![]),
                function(
                    "main",
                    Typ::Void,
                    vec![],
                    vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".to_string())),
                        args: vec![],
                    })],
                ),
            ]))
            .expect_err("call arity mismatches should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::ArityMismatch { caller, fn_name, expected: 1, got: 0 }
                if caller == "main" && fn_name == "helper"
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_call_argument_type_mismatch() {
        let err = TypeChecker::new()
            .check_module(&module(vec![
                function_with_params("helper", vec![("value".to_string(), Typ::Int)], vec![]),
                function(
                    "main",
                    Typ::Void,
                    vec![],
                    vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".to_string())),
                        args: vec![Expr::StringLit("bad".to_string())],
                    })],
                ),
            ]))
            .expect_err("call argument type mismatches should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::TypeMismatch { context, expected: Typ::Int, got: Typ::String }
                if context.contains("argument")
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn validates_struct_init_and_field_access() {
        TypeChecker::new()
            .check_module(&module(vec![
                point_struct(),
                function(
                    "main",
                    Typ::Void,
                    vec![],
                    vec![
                        Stmt::Let(
                            "p".to_string(),
                            Some(Typ::Named("Point".to_string())),
                            Expr::StructInit {
                                name: "Point".to_string(),
                                fields: vec![
                                    ("x".to_string(), Expr::IntLit(2)),
                                    ("y".to_string(), Expr::IntLit(5)),
                                ],
                            },
                        ),
                        Stmt::Expr(Expr::Field {
                            base: Box::new(Expr::Ident("p".to_string())),
                            name: "y".to_string(),
                        }),
                    ],
                ),
            ]))
            .expect("struct init and field access should pass");
    }

    #[test]
    fn accepts_any_struct_field_init_and_numeric_use() {
        TypeChecker::new()
            .check_module(&module(vec![
                Decl::Struct {
                    name: "Boxed".to_string(),
                    fields: vec![("value".to_string(), Typ::Named("Any".to_string()))],
                    type_params: vec![],
                },
                function_with_ret(
                    "main",
                    Typ::Int,
                    vec![
                        Stmt::Let(
                            "boxed".to_string(),
                            None,
                            Expr::StructInit {
                                name: "Boxed".to_string(),
                                fields: vec![("value".to_string(), Expr::IntLit(41))],
                            },
                        ),
                        Stmt::Return(Some(Expr::Binary {
                            op: "+".to_string(),
                            lhs: Box::new(Expr::Field {
                                base: Box::new(Expr::Ident("boxed".to_string())),
                                name: "value".to_string(),
                            }),
                            rhs: Box::new(Expr::IntLit(1)),
                        })),
                    ],
                ),
            ]))
            .expect("Any fields should accept concrete values and typed use");
    }

    #[test]
    fn rejects_unknown_struct_init_field() {
        let err = TypeChecker::new()
            .check_module(&module(vec![
                point_struct(),
                function(
                    "main",
                    Typ::Void,
                    vec![],
                    vec![Stmt::Expr(Expr::StructInit {
                        name: "Point".to_string(),
                        fields: vec![
                            ("x".to_string(), Expr::IntLit(2)),
                            ("z".to_string(), Expr::IntLit(5)),
                        ],
                    })],
                ),
            ]))
            .expect_err("unknown struct fields should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::UnknownField { struct_name, field } if struct_name == "Point" && field == "z"
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_struct_init_field_type_mismatch() {
        let err = TypeChecker::new()
            .check_module(&module(vec![
                point_struct(),
                function(
                    "main",
                    Typ::Void,
                    vec![],
                    vec![Stmt::Expr(Expr::StructInit {
                        name: "Point".to_string(),
                        fields: vec![
                            ("x".to_string(), Expr::StringLit("bad".to_string())),
                            ("y".to_string(), Expr::IntLit(5)),
                        ],
                    })],
                ),
            ]))
            .expect_err("struct field type mismatches should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::TypeMismatch { context, expected: Typ::Int, got: Typ::String }
                if context.contains("field `x`")
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_return_type_mismatch() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function_with_ret(
                "main",
                Typ::Int,
                vec![Stmt::Return(Some(Expr::StringLit("bad".to_string())))],
            )]))
            .expect_err("return type mismatches should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::ReturnTypeMismatch { fn_name, expected: Typ::Int, got: Typ::String }
                if fn_name == "main"
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn accepts_named_primitive_return_aliases() {
        TypeChecker::new()
            .check_module(&module(vec![function_with_ret(
                "main",
                Typ::Named("Int".into()),
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            )]))
            .expect("named primitive aliases should typecheck");
    }

    #[test]
    fn rejects_missing_return_value() {
        let err = typecheck_executable(&module(vec![function_with_ret(
            "main",
            Typ::Int,
            vec![Stmt::Return(None)],
        )]))
        .expect_err("missing return values should fail");

        assert!(
            err.contains("missing return value in `main`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_return_value_in_void_function() {
        let err = typecheck_executable(&module(vec![function(
            "main",
            Typ::Void,
            vec![],
            vec![Stmt::Return(Some(Expr::IntLit(1)))],
        )]))
        .expect_err("void functions should not return values");

        assert!(
            err.contains("return value in void function `main`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_non_bool_if_condition() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Void,
                vec![],
                vec![Stmt::If {
                    cond: Expr::IntLit(1),
                    then_body: vec![],
                    else_body: vec![],
                }],
            )]))
            .expect_err("if conditions require Bool");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::TypeMismatch { context, expected: Typ::Bool, got: Typ::Int }
                if context.contains("if condition")
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_non_bool_loop_condition() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Void,
                vec![],
                vec![Stmt::Loop {
                    kind: crate::core_ir::LoopKind::While,
                    cond: Some(Expr::IntLit(1)),
                    body: vec![],
                }],
            )]))
            .expect_err("loop conditions require Bool");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::TypeMismatch { context, expected: Typ::Bool, got: Typ::Int }
                if context.contains("loop condition")
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_let_annotation_type_mismatch() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Void,
                vec![],
                vec![Stmt::Let(
                    "value".to_string(),
                    Some(Typ::Int),
                    Expr::StringLit("bad".to_string()),
                )],
            )]))
            .expect_err("let annotation mismatches should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::TypeMismatch { context, expected: Typ::Int, got: Typ::String }
                if context.contains("let binding")
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn rejects_assignment_type_mismatch() {
        let err = TypeChecker::new()
            .check_module(&module(vec![function(
                "main",
                Typ::Void,
                vec![],
                vec![
                    Stmt::Let("value".to_string(), Some(Typ::Int), Expr::IntLit(1)),
                    Stmt::Assign("value".to_string(), Expr::StringLit("bad".to_string())),
                ],
            )]))
            .expect_err("assignment type mismatches should fail");

        assert!(
            err.iter().any(|e| matches!(
                e,
                TypeError::TypeMismatch { context, expected: Typ::Int, got: Typ::String }
                if context.contains("assignment")
            )),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn accepts_library_module_without_main() {
        typecheck_module(
            &module(vec![function("helper", Typ::Void, vec![], vec![])]),
            ModuleKind::Library,
        )
        .expect("library modules should not require main");
    }

    #[test]
    fn accepts_float_and_string_binary_operators() {
        TypeChecker::new()
            .check_module(&module(vec![function_with_ret(
                "main",
                Typ::Float,
                vec![Stmt::Return(Some(Expr::Binary {
                    op: "+".to_string(),
                    lhs: Box::new(Expr::FloatLit(crate::core_ir::FloatVal(2.5))),
                    rhs: Box::new(Expr::FloatLit(crate::core_ir::FloatVal(3.5))),
                }))],
            )]))
            .expect("float add");
        TypeChecker::new()
            .check_module(&module(vec![function_with_ret(
                "main",
                Typ::String,
                vec![Stmt::Return(Some(Expr::Binary {
                    op: "+".to_string(),
                    lhs: Box::new(Expr::StringLit("a".to_string())),
                    rhs: Box::new(Expr::StringLit("b".to_string())),
                }))],
            )]))
            .expect("string concat");
    }

    // Moved from family_typecheck.rs

    #[test]
    fn php_int_return_typechecks_after_normalization() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Named("int".into()),
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Named("void".into()),
                body: vec![],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::Php, &module).is_ok());
    }

    #[test]
    fn zig_i32_return_typechecks_after_normalization() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Named("i32".into()),
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Named("void".into()),
                body: vec![],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::Zig, &module).is_ok());
    }

    #[test]
    fn lua_void_ret_infers_from_return_stmt() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::Lua, &module).is_ok());
    }

    #[test]
    fn javascript_void_ret_infers_from_return_stmt() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::JavaScript, &module).is_ok());
    }

    #[test]
    fn javascript_void_ret_with_call_return_infers_dynamic() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![],
                }))],
                type_params: vec![],
            },
            Decl::Function {
                name: "helper".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Void,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("answer".into())),
                    args: vec![],
                }))],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::JavaScript, &module).is_ok());
    }

    #[test]
    fn typescript_number_return_typechecks_after_normalization() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Named("number".into()),
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Named("void".into()),
                body: vec![],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::TypeScript, &module).is_ok());
    }

    #[test]
    fn javascript_entrypoint_typecheck_keeps_class_context() {
        let module = UnifiedModule::new(vec![
            Decl::Class {
                name: "Counter".into(),
                fields: vec![("value".into(), Typ::Int)],
                methods: vec![],
                visibility: Visibility::Pub,
                extends: None,
                implements: vec![],
                type_params: vec![],
            },
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let(
                        "counter".into(),
                        None,
                        Expr::StructInit {
                            name: "Counter".into(),
                            fields: vec![("value".into(), Expr::IntLit(42))],
                        },
                    ),
                    Stmt::Return(Some(Expr::Field {
                        base: Box::new(Expr::Ident("counter".into())),
                        name: "value".into(),
                    })),
                ],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::JavaScript, &module).is_ok());
    }

    #[test]
    fn javascript_entrypoint_typecheck_keeps_helper_signatures() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "helper".into(),
                params: vec![("value".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Ident("missing".into())))],
                type_params: vec![],
            },
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![Expr::IntLit(42)],
                }))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::JavaScript, &module).is_ok());
    }

    #[test]
    fn javascript_entrypoint_typecheck_still_checks_helper_call_args() {
        let module = UnifiedModule::new(vec![
            Decl::Function {
                name: "helper".into(),
                params: vec![("value".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![],
                type_params: vec![],
            },
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![Expr::StringLit("bad".into())],
                }))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::JavaScript, &module).is_err());
    }

    #[test]
    fn typescript_struct_fields_normalize_before_entrypoint_typecheck() {
        let module = UnifiedModule::new(vec![
            Decl::Struct {
                name: "Counter".into(),
                fields: vec![("value".into(), Typ::Named("number".into()))],
                type_params: vec![],
            },
            Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Named("number".into()),
                body: vec![
                    Stmt::Let(
                        "counter".into(),
                        None,
                        Expr::StructInit {
                            name: "Counter".into(),
                            fields: vec![("value".into(), Expr::IntLit(42))],
                        },
                    ),
                    Stmt::Return(Some(Expr::Field {
                        base: Box::new(Expr::Ident("counter".into())),
                        name: "value".into(),
                    })),
                ],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Named("number".into()),
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            },
        ]);
        assert!(typecheck_for_parser(ParserId::TypeScript, &module).is_ok());
    }
    #[test]
    fn complement_does_not_reuse_an_outer_catch_binding_type() {
        let mut module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![
                Stmt::Let("value".into(), Some(Typ::Int), Expr::IntLit(1)),
                Stmt::Try {
                    body: vec![Stmt::Throw(Expr::BoolLit(true))],
                    catches: vec![crate::core_ir::CatchArm {
                        pattern: "value".into(),
                        body: vec![Stmt::Return(Some(Expr::Unary {
                            op: "complement".into(),
                            expr: Box::new(Expr::Ident("value".into())),
                        }))],
                    }],
                },
            ],
        }]);
        TypeChecker::new().resolve_complements(&mut module);
        let verification = crate::core_ir_verifier::verify_module(
            &module,
            &crate::core_ir_verifier::VerifyOptions {
                entry: Some("main".into()),
                require_entry: true,
            },
        );
        assert!(!verification.ok);
        assert_eq!(
            verification.reason_code.as_deref(),
            Some("unsupported-complement")
        );
    }
    #[test]
    fn complement_requires_integral_operand_types() {
        for source in [
            "fn main() -> f64 { return !1.25; }",
            "fn main() -> String { return !\"text\"; }",
        ] {
            let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let errors = TypeChecker::new().check_module(&module).unwrap_err();
            assert!(errors.iter().any(|error| matches!(error, TypeError::TypeMismatch { context, .. } if context.contains("complement operand"))), "{errors:?}");
        }
    }
    #[test]
    fn malformed_numeric_cast_markers_remain_refused() {
        for expression in [
            "c-numeric-cast()",
            "c-numeric-cast(42, \"Unknown\")",
            "c-numeric-cast(42, 0)",
            "c-numeric-cast(42, \"Int\", 0)",
            "c-numeric-cast(\"text\", \"Int\")",
        ] {
            let source = format!("fn main() -> Int {{ return {expression}; }}");
            let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            let module = super::normalize_module(ParserId::C, &module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(!report.ok, "{expression}: {report:?}");
            assert_eq!(report.reason_code.as_deref(), Some("unresolved-symbol"));
        }
    }
}
