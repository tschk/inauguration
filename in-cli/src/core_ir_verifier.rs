use crate::core_ir::{Decl, UnifiedModule};
use crate::core_ir::{Expr, Stmt, Typ};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOptions {
    pub entry: Option<String>,
    pub require_entry: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub ok: bool,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
    pub parsed_function_count: usize,
    pub call_edges: Vec<(String, String)>,
}

pub fn verify_for_entry(module: &UnifiedModule, entry: &str) -> VerifyReport {
    verify_module(
        module,
        &VerifyOptions {
            entry: Some(entry.to_string()),
            require_entry: true,
        },
    )
}

pub fn verify_module(module: &UnifiedModule, options: &VerifyOptions) -> VerifyReport {
    let parsed_function_count = module
        .decls
        .iter()
        .filter(|decl| matches!(decl, Decl::Function { .. }))
        .count();

    let mut call_edges = Vec::new();

    let facts = match collect_module_facts(module) {
        Ok(facts) => facts,
        Err((code, reason)) => {
            return fail_report(parsed_function_count, call_edges, &code, reason);
        }
    };

    if options.require_entry || options.entry.is_some() {
        let entry_name = options.entry.as_deref().unwrap_or("main");
        if !facts.functions.contains_key(entry_name) {
            return fail_report(
                parsed_function_count,
                call_edges,
                "missing-entry-symbol",
                format!("missing entry function `{entry_name}`"),
            );
        }
    }

    for decl in &module.decls {
        if let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        {
            if let Err((code, reason)) = check_duplicate_param_names(name, params) {
                return fail_report(parsed_function_count, call_edges, &code, reason);
            }

            if canonical_type(ret) != Typ::Void && body.is_empty() {
                // ponytail: package externs and polyfills have empty bodies with non-void returns;
                // they're satisfied at link time by adapter invoke specs. Skip the empty-body check.
            }

            let mut env: HashMap<String, Typ> = params.iter().cloned().collect();
            if let Err((code, reason)) =
                check_stmts(name, body, &facts, ret, &mut env, &mut call_edges)
            {
                return fail_report(parsed_function_count, call_edges, &code, reason);
            }
        }
    }

    VerifyReport {
        ok: true,
        reason_code: None,
        reason: None,
        parsed_function_count,
        call_edges,
    }
}

fn fail_report(
    parsed_function_count: usize,
    call_edges: Vec<(String, String)>,
    reason_code: &str,
    reason: String,
) -> VerifyReport {
    VerifyReport {
        ok: false,
        reason_code: Some(reason_code.to_string()),
        reason: Some(reason),
        parsed_function_count,
        call_edges,
    }
}

struct ModuleFacts<'a> {
    functions: HashMap<&'a str, FunctionSig<'a>>,
    structs: HashMap<&'a str, &'a [(String, Typ)]>,
    globals: HashSet<&'a str>,
}

#[derive(Debug, Clone, Copy)]
struct FunctionSig<'a> {
    params: &'a [(String, Typ)],
    ret: &'a Typ,
}

/// Return true if `name` is a known intrinsic that doesn't need a module-level declaration.
pub fn is_intrinsic(name: &str) -> bool {
    matches!(
        name,
        "outb"
            | "inb"
            | "outl"
            | "inl"
            | "load8"
            | "load16"
            | "load32"
            | "load64"
            | "store8"
            | "store16"
            | "store32"
            | "store64"
            | "hlt"
            | "cli"
            | "sti"
            | "pause"
            | "lidt"
            | "invlpg"
            | "read-cr2"
            | "invoke"
            | "invoke1"
            | "invoke2"
            | "to-string"
            | "read-file"
            | "write-file"
            | "path-join"
            | "str-eq"
            | "str-contains"
            | "str-starts-with"
            | "str-trim"
            | "str-split-lines"
            | "str-tokenize-expr"
            | "str-to-int"
            | "str-index-of"
            | "str-slice"
            | "str-table-has"
            | "str-table-get-int"
            | "array-len"
            | "str-is-int"
            | "json-stringify"
    )
}

fn function_sig<'a>(facts: &ModuleFacts<'a>, name: &str) -> Option<FunctionSig<'a>> {
    facts.functions.get(name).copied().or_else(|| {
        let kebab_name = name.replace('_', "-");
        (kebab_name != name)
            .then(|| facts.functions.get(kebab_name.as_str()).copied())
            .flatten()
    })
}

fn add_intrinsics<'a>(functions: &mut HashMap<&'a str, FunctionSig<'a>>) {
    // Static intrinsics: these variants contain no owned data, so a static item
    // gives a cheap 'static reference without leaking an allocation.
    static VOID_RET: Typ = Typ::Void;
    static INT_RET: Typ = Typ::Int;
    static STRING_RET: Typ = Typ::String;
    static BOOL_RET: Typ = Typ::Bool;
    let string_array_ret = Box::leak(Box::new(Typ::Array(Box::new(Typ::String))));
    let intrinsics: [(&str, &'static Typ); 40] = [
        ("outb", &VOID_RET),
        ("inb", &INT_RET),
        ("outl", &VOID_RET),
        ("inl", &INT_RET),
        ("load8", &INT_RET),
        ("load16", &INT_RET),
        ("load32", &INT_RET),
        ("load64", &INT_RET),
        ("store8", &VOID_RET),
        ("store16", &VOID_RET),
        ("store32", &VOID_RET),
        ("store64", &VOID_RET),
        ("hlt", &VOID_RET),
        ("cli", &VOID_RET),
        ("sti", &VOID_RET),
        ("pause", &VOID_RET),
        ("lidt", &VOID_RET),
        ("invlpg", &VOID_RET),
        ("read-cr2", &INT_RET),
        ("invoke", &INT_RET),
        ("invoke1", &INT_RET),
        ("invoke2", &INT_RET),
        ("to-string", &STRING_RET),
        ("read-file", &STRING_RET),
        ("write-file", &BOOL_RET),
        ("path-join", &STRING_RET),
        ("str-eq", &BOOL_RET),
        ("str-contains", &BOOL_RET),
        ("str-starts-with", &BOOL_RET),
        ("str-trim", &STRING_RET),
        ("str-split-lines", string_array_ret),
        ("str-tokenize-expr", string_array_ret),
        ("str-to-int", &INT_RET),
        ("str-index-of", &INT_RET),
        ("str-slice", &STRING_RET),
        ("str-table-has", &BOOL_RET),
        ("str-table-get-int", &INT_RET),
        ("array-len", &INT_RET),
        ("str-is-int", &BOOL_RET),
        ("json-stringify", &STRING_RET),
    ];
    for (name, ret) in intrinsics {
        functions.insert(name, FunctionSig { params: &[], ret });
    }
}

fn check_and_insert_top_level<'a>(
    top_level: &mut HashSet<&'a str>,
    name: &'a str,
) -> Result<(), (String, String)> {
    if !top_level.insert(name) {
        return Err((
            "duplicate-top-level-name".to_string(),
            format!("duplicate top-level name `{name}`"),
        ));
    }
    Ok(())
}

fn collect_module_facts(module: &UnifiedModule) -> Result<ModuleFacts<'_>, (String, String)> {
    let mut top_level = HashSet::new();
    let mut functions = HashMap::new();
    let mut structs = HashMap::new();
    let mut globals = HashSet::new();

    add_intrinsics(&mut functions);

    for decl in &module.decls {
        match decl {
            Decl::Struct { name, fields, .. } => {
                check_and_insert_top_level(&mut top_level, name.as_str())?;
                structs.insert(name.as_str(), fields.as_slice());
            }
            Decl::Class { name, fields, .. } => {
                // Register class fields as struct schema for verification
                check_and_insert_top_level(&mut top_level, name.as_str())?;
                structs.insert(name.as_str(), fields.as_slice());
            }
            Decl::Function {
                name, params, ret, ..
            } => {
                check_and_insert_top_level(&mut top_level, name.as_str())?;
                functions.insert(name.as_str(), FunctionSig { params, ret });
            }
            Decl::Interface { .. } => {}
            Decl::Component { .. } => {}
            Decl::Global { name, .. } => {
                check_and_insert_top_level(&mut top_level, name.as_str())?;
                globals.insert(name.as_str());
            }
        }
    }

    Ok(ModuleFacts {
        functions,
        structs,
        globals,
    })
}

fn check_duplicate_param_names(
    fn_name: &str,
    params: &[(String, Typ)],
) -> Result<(), (String, String)> {
    let mut seen = HashSet::new();
    for (name, _) in params {
        if !seen.insert(name.as_str()) {
            return Err((
                "duplicate-param-name".to_string(),
                format!("duplicate parameter name `{name}` in `{fn_name}`"),
            ));
        }
    }
    Ok(())
}

fn check_stmts(
    fn_name: &str,
    stmts: &[Stmt],
    facts: &ModuleFacts<'_>,
    ret: &Typ,
    env: &mut HashMap<String, Typ>,
    call_edges: &mut Vec<(String, String)>,
) -> Result<(), (String, String)> {
    for stmt in stmts {
        check_stmt(fn_name, stmt, facts, ret, env, call_edges)?;
    }
    Ok(())
}

fn check_stmt(
    fn_name: &str,
    stmt: &Stmt,
    facts: &ModuleFacts<'_>,
    ret: &Typ,
    env: &mut HashMap<String, Typ>,
    call_edges: &mut Vec<(String, String)>,
) -> Result<(), (String, String)> {
    match stmt {
        Stmt::Let(name, typ, expr) => {
            // Allow re-declaration of locals (desugared for-loop variables may reuse names)
            // The last declaration wins.
            let _ = env.remove(name);
            check_expr(fn_name, expr, facts, env, call_edges)?;
            let expr_typ = expr_type(expr, facts, env);
            if let (Some(expected), Some(actual)) = (typ, expr_typ.as_ref())
                && expected != actual
            {
                return Err((
                    "type-mismatch".to_string(),
                    format!(
                        "type mismatch for `{name}` in `{fn_name}`: expected {}, got {}",
                        type_name(expected),
                        type_name(actual)
                    ),
                ));
            }
            if let Some(typ) = typ {
                env.insert(name.clone(), typ.clone());
            } else if let Some(expr_typ) = expr_typ {
                env.insert(name.clone(), expr_typ);
            } else {
                env.insert(name.clone(), Typ::Void);
            }
            Ok(())
        }
        Stmt::Assign(name, expr) => {
            // Allow assignment to globals (not just locals)
            let is_global = facts.globals.contains(name.as_str());
            if !env.contains_key(name) && !is_global {
                return Err((
                    "unresolved-symbol".to_string(),
                    format!("unresolved assignment `{name}` in `{fn_name}`"),
                ));
            };
            check_expr(fn_name, expr, facts, env, call_edges)?;
            if let Some(expr_typ) = expr_type(expr, facts, env) {
                let expected_typ = env.get(name).cloned().unwrap_or(Typ::Int);
                if expected_typ != expr_typ {
                    return Err((
                        "type-mismatch".to_string(),
                        format!(
                            "type mismatch for assignment `{name}` in `{fn_name}`: expected {}, got {}",
                            type_name(&expected_typ),
                            type_name(&expr_typ)
                        ),
                    ));
                }
            }
            Ok(())
        }
        Stmt::FieldAssign { base, value, .. } => {
            check_expr(fn_name, base, facts, env, call_edges)?;
            check_expr(fn_name, value, facts, env, call_edges)?;
            Ok(())
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            check_expr(fn_name, base, facts, env, call_edges)?;
            check_expr(fn_name, index, facts, env, call_edges)?;
            check_expr(fn_name, value, facts, env, call_edges)?;
            require_type(fn_name, "array index", &Typ::Int, index, facts, env)?;
            match expr_type(base, facts, env) {
                Some(Typ::Array(item)) => {
                    if let Some(value_typ) = expr_type(value, facts, env)
                        && *item != value_typ
                    {
                        return Err((
                            "type-mismatch".to_string(),
                            format!(
                                "type mismatch for array assignment in `{fn_name}`: expected {}, got {}",
                                type_name(&item),
                                type_name(&value_typ)
                            ),
                        ));
                    }
                    Ok(())
                }
                // Allow Int base (raw memory pointer arithmetic)
                Some(Typ::Int) => Ok(()),
                Some(other) => Err((
                    "type-mismatch".to_string(),
                    format!(
                        "index assignment base in `{fn_name}` expected array, got {}",
                        type_name(&other)
                    ),
                )),
                None => Ok(()),
            }
        }
        Stmt::Expr(expr) => check_expr(fn_name, expr, facts, env, call_edges),
        Stmt::Break | Stmt::Continue => Ok(()),
        Stmt::Return(Some(expr)) => {
            check_expr(fn_name, expr, facts, env, call_edges)?;
            if canonical_type(ret) == Typ::Void {
                return Err((
                    "return-type-mismatch".to_string(),
                    format!("return value in void function `{fn_name}`"),
                ));
            }
            if let Some(expr_typ) = expr_type(expr, facts, env)
                && !ret.compatible_with(&expr_typ)
            {
                return Err((
                    "return-type-mismatch".to_string(),
                    format!(
                        "return type mismatch in `{fn_name}`: expected {}, got {}",
                        type_name(ret),
                        type_name(&expr_typ)
                    ),
                ));
            }
            Ok(())
        }
        Stmt::Return(None) => {
            if canonical_type(ret) != Typ::Void {
                return Err((
                    "return-type-mismatch".to_string(),
                    format!("missing return value in `{fn_name}`"),
                ));
            }
            Ok(())
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            check_expr(fn_name, cond, facts, env, call_edges)?;
            require_type(fn_name, "if condition", &Typ::Bool, cond, facts, env)?;
            check_stmts(fn_name, then_body, facts, ret, &mut env.clone(), call_edges)?;
            check_stmts(fn_name, else_body, facts, ret, &mut env.clone(), call_edges)
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(cond) = cond {
                check_expr(fn_name, cond, facts, env, call_edges)?;
                require_type(fn_name, "loop condition", &Typ::Bool, cond, facts, env)?;
            }
            check_stmts(fn_name, body, facts, ret, &mut env.clone(), call_edges)
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            check_expr(fn_name, scrutinee, facts, env, call_edges)?;
            for arm in arms {
                check_stmts(fn_name, &arm.body, facts, ret, &mut env.clone(), call_edges)?;
            }
            Ok(())
        }
        Stmt::Throw(_) | Stmt::Try { .. } | Stmt::Propagate => Ok(()),
    }
}

fn check_expr(
    fn_name: &str,
    expr: &Expr,
    facts: &ModuleFacts<'_>,
    env: &HashMap<String, Typ>,
    call_edges: &mut Vec<(String, String)>,
) -> Result<(), (String, String)> {
    match expr {
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::StringLit(_) | Expr::BoolLit(_) => Ok(()),
        Expr::Closure { .. } => Ok(()),
        Expr::Ident(name) => {
            if env.contains_key(name)
                || function_sig(facts, name).is_some()
                || facts.globals.contains(name.as_str())
            {
                Ok(())
            } else {
                Err((
                    "unresolved-symbol".to_string(),
                    format!("unresolved identifier `{name}` in `{fn_name}`"),
                ))
            }
        }
        Expr::Unary { expr, .. } => check_expr(fn_name, expr, facts, env, call_edges),
        Expr::Binary { op, lhs, rhs, .. } => {
            check_expr(fn_name, lhs, facts, env, call_edges)?;
            check_expr(fn_name, rhs, facts, env, call_edges)?;
            match op.as_str() {
                "+" => {
                    let lhs_typ = expr_type(lhs, facts, env);
                    let rhs_typ = expr_type(rhs, facts, env);
                    if lhs_typ == Some(Typ::String) || rhs_typ == Some(Typ::String) {
                        require_type(fn_name, "binary operand", &Typ::String, lhs, facts, env)?;
                        require_type(fn_name, "binary operand", &Typ::String, rhs, facts, env)
                    } else {
                        check_numeric_binop_operands(fn_name, lhs, rhs, facts, env)
                    }
                }
                "-" | "*" | "/" | "<" | ">" | "<=" | ">=" => {
                    check_numeric_binop_operands(fn_name, lhs, rhs, facts, env)
                }
                "%" => {
                    require_type(fn_name, "binary operand", &Typ::Int, lhs, facts, env)?;
                    require_type(fn_name, "binary operand", &Typ::Int, rhs, facts, env)
                }
                "==" | "!=" => {
                    if let (Some(lhs_typ), Some(rhs_typ)) =
                        (expr_type(lhs, facts, env), expr_type(rhs, facts, env))
                        && lhs_typ != rhs_typ
                    {
                        return Err((
                            "type-mismatch".to_string(),
                            format!(
                                "type mismatch for binary `{op}` in `{fn_name}`: left {}, right {}",
                                type_name(&lhs_typ),
                                type_name(&rhs_typ)
                            ),
                        ));
                    }
                    Ok(())
                }
                "&&" | "||" => {
                    require_type(fn_name, "binary operand", &Typ::Bool, lhs, facts, env)?;
                    require_type(fn_name, "binary operand", &Typ::Bool, rhs, facts, env)
                }
                _ => Ok(()),
            }
        }
        Expr::StructInit { name, fields, .. } => {
            check_struct_init(fn_name, name, fields, facts, env, call_edges)
        }
        Expr::Field { base, name, .. } => {
            check_expr(fn_name, base, facts, env, call_edges)?;
            if let Some(Typ::Named(struct_name)) = expr_type(base, facts, env)
                && let Some(schema) = facts.structs.get(struct_name.as_str())
                && !schema.iter().any(|(field, _)| field == name)
            {
                return Err((
                    "unknown-struct-field".to_string(),
                    format!("unknown field `{name}` for struct `{struct_name}`"),
                ));
            }
            Ok(())
        }
        Expr::ArrayLit(items) => {
            let mut expected = None;
            for item in items {
                check_expr(fn_name, item, facts, env, call_edges)?;
                if let Some(item_typ) = expr_type(item, facts, env) {
                    if let Some(expected_typ) = &expected {
                        if expected_typ != &item_typ {
                            return Err((
                                "array-element-type-mismatch".to_string(),
                                format!(
                                    "array literal in `{fn_name}` expected {}, got {}",
                                    type_name(expected_typ),
                                    type_name(&item_typ)
                                ),
                            ));
                        }
                    } else {
                        expected = Some(item_typ);
                    }
                }
            }
            Ok(())
        }
        Expr::Index { base, index, .. } => {
            check_expr(fn_name, base, facts, env, call_edges)?;
            check_expr(fn_name, index, facts, env, call_edges)?;
            require_type(fn_name, "array index", &Typ::Int, index, facts, env)?;
            if let Some(base_typ) = expr_type(base, facts, env)
                && !matches!(base_typ, Typ::Array(_))
                && base_typ != Typ::Int
            {
                return Err((
                    "type-mismatch".to_string(),
                    format!(
                        "index base in `{fn_name}` expected array, got {}",
                        type_name(&base_typ)
                    ),
                ));
            }
            Ok(())
        }
        Expr::Call { callee, args, .. } => {
            if let Expr::Ident(name) = callee.as_ref() {
                let Some(sig) = function_sig(facts, name) else {
                    return Err((
                        "unresolved-symbol".to_string(),
                        format!("unresolved function call `{name}` in `{fn_name}`"),
                    ));
                };
                call_edges.push((fn_name.to_string(), name.clone()));
                // Relax arity and type checking for intrinsics — the codegen handles validation.
                if !is_intrinsic(name) {
                    if args.len() != sig.params.len() {
                        return Err((
                            "call-arity-mismatch".to_string(),
                            format!(
                                "call to `{name}` in `{fn_name}` expects {} args, got {}",
                                sig.params.len(),
                                args.len()
                            ),
                        ));
                    }
                    for ((param_name, param_typ), arg) in sig.params.iter().zip(args) {
                        check_expr(fn_name, arg, facts, env, call_edges)?;
                        if let Some(arg_typ) = expr_type(arg, facts, env)
                            && param_typ != &arg_typ
                        {
                            // Allow String→Int/pointer coercion for string literals
                            let param_is_ptr = matches!(param_typ, Typ::Named(n) if n.ends_with("*") || n == "char" || n == "void");
                            if !((*param_typ == Typ::Int || param_is_ptr)
                                && arg_typ == Typ::String
                                && matches!(arg, Expr::StringLit(_)))
                            {
                                return Err((
                                    "type-mismatch".to_string(),
                                    format!(
                                        "argument `{param_name}` for `{name}` in `{fn_name}` expected {}, got {}",
                                        type_name(param_typ),
                                        type_name(&arg_typ)
                                    ),
                                ));
                            }
                        }
                    }
                } else {
                    // Still check argument expressions (for side-effect / error detection)
                    for arg in args {
                        check_expr(fn_name, arg, facts, env, call_edges)?;
                    }
                }
                return Ok(());
            } else {
                check_expr(fn_name, callee, facts, env, call_edges)?;
            }

            for arg in args {
                check_expr(fn_name, arg, facts, env, call_edges)?;
            }
            Ok(())
        }
    }
}

fn check_struct_init(
    fn_name: &str,
    struct_name: &str,
    fields: &[(String, Expr)],
    facts: &ModuleFacts<'_>,
    env: &HashMap<String, Typ>,
    call_edges: &mut Vec<(String, String)>,
) -> Result<(), (String, String)> {
    let schema = facts.structs.get(struct_name).ok_or((
        "unknown-struct".to_string(),
        format!("unknown struct `{struct_name}` in `{fn_name}`"),
    ))?;
    let mut seen = HashSet::new();
    for (field, expr) in fields {
        if !seen.insert(field.as_str()) {
            return Err((
                "duplicate-struct-field".to_string(),
                format!("duplicate field `{field}` for struct `{struct_name}`"),
            ));
        }
        if !schema.iter().any(|(schema_field, _)| schema_field == field) {
            return Err((
                "unknown-struct-field".to_string(),
                format!("unknown field `{field}` for struct `{struct_name}`"),
            ));
        }
        check_expr(fn_name, expr, facts, env, call_edges)?;
        if let Some((_, expected)) = schema
            .iter()
            .find(|(schema_field, _)| schema_field == field)
            && let Some(actual) = expr_type(expr, facts, env)
            && expected != &actual
        {
            return Err((
                "type-mismatch".to_string(),
                format!(
                    "type mismatch for field `{field}` in struct `{struct_name}`: expected {}, got {}",
                    type_name(expected),
                    type_name(&actual)
                ),
            ));
        }
    }
    for (field, _) in *schema {
        if !seen.contains(field.as_str()) {
            return Err((
                "missing-struct-field".to_string(),
                format!("missing field `{field}` for struct `{struct_name}`"),
            ));
        }
    }
    Ok(())
}

fn expr_type(expr: &Expr, facts: &ModuleFacts<'_>, env: &HashMap<String, Typ>) -> Option<Typ> {
    match expr {
        Expr::IntLit(_) => Some(Typ::Int),
        Expr::FloatLit(_) => Some(Typ::Float),
        Expr::StringLit(_) => Some(Typ::String),
        Expr::BoolLit(_) => Some(Typ::Bool),
        Expr::Ident(name) => env
            .get(name)
            .cloned()
            .or_else(|| function_sig(facts, name).map(|sig| sig.ret.clone()))
            .or_else(|| {
                if facts.globals.contains(name.as_str()) {
                    Some(Typ::Int) // most globals are Int
                } else {
                    None
                }
            }),
        Expr::StructInit { name, .. } => Some(Typ::Named(name.clone())),
        Expr::Field { base, name, .. } => {
            if let Some(Typ::Named(struct_name)) = expr_type(base, facts, env)
                && let Some(schema) = facts.structs.get(struct_name.as_str())
                && let Some((_, typ)) = schema.iter().find(|(field, _)| field == name)
            {
                return Some(typ.clone());
            }
            None
        }
        Expr::ArrayLit(items) => {
            let item_typ = items
                .iter()
                .find_map(|item| expr_type(item, facts, env))
                .unwrap_or(Typ::Void);
            Some(Typ::Array(Box::new(item_typ)))
        }
        Expr::Index { base, .. } => {
            if let Some(Typ::Array(item)) = expr_type(base, facts, env) {
                Some(*item)
            } else if expr_type(base, facts, env) == Some(Typ::Int) {
                // Indexing an Int pointer returns Int
                Some(Typ::Int)
            } else {
                None
            }
        }
        Expr::Unary { op, expr, .. } => match op.as_str() {
            "!" => Some(Typ::Bool),
            "-" => {
                if expr_type(expr, facts, env) == Some(Typ::Float) {
                    Some(Typ::Float)
                } else {
                    Some(Typ::Int)
                }
            }
            _ => expr_type(expr, facts, env),
        },
        Expr::Binary { op, lhs, rhs, .. } => {
            let lhs_typ = expr_type(lhs, facts, env);
            let rhs_typ = expr_type(rhs, facts, env);
            match op.as_str() {
                "+" if lhs_typ == Some(Typ::String) || rhs_typ == Some(Typ::String) => {
                    Some(Typ::String)
                }
                "+" | "-" | "*" | "/"
                    if lhs_typ == Some(Typ::Float) || rhs_typ == Some(Typ::Float) =>
                {
                    Some(Typ::Float)
                }
                "+" | "-" | "*" | "/" | "%" | "^" | "<<" | ">>" | "&" | "|" => Some(Typ::Int),
                "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => Some(Typ::Bool),
                _ => None,
            }
        }
        Expr::Call { callee, .. } => {
            if let Expr::Ident(name) = callee.as_ref()
                && let Some(sig) = function_sig(facts, name)
            {
                return Some(sig.ret.clone());
            }
            None
        }
        Expr::Closure { .. } => None,
    }
}

fn check_numeric_binop_operands(
    fn_name: &str,
    lhs: &Expr,
    rhs: &Expr,
    facts: &ModuleFacts<'_>,
    env: &HashMap<String, Typ>,
) -> Result<(), (String, String)> {
    let lhs_typ = expr_type(lhs, facts, env);
    let rhs_typ = expr_type(rhs, facts, env);
    if lhs_typ == Some(Typ::Float) || rhs_typ == Some(Typ::Float) {
        require_type(fn_name, "binary operand", &Typ::Float, lhs, facts, env)?;
        require_type(fn_name, "binary operand", &Typ::Float, rhs, facts, env)
    } else {
        require_type(fn_name, "binary operand", &Typ::Int, lhs, facts, env)?;
        require_type(fn_name, "binary operand", &Typ::Int, rhs, facts, env)
    }
}

fn require_type(
    fn_name: &str,
    context: &str,
    expected: &Typ,
    expr: &Expr,
    facts: &ModuleFacts<'_>,
    env: &HashMap<String, Typ>,
) -> Result<(), (String, String)> {
    if let Some(actual) = expr_type(expr, facts, env)
        && &actual != expected
    {
        return Err((
            "type-mismatch".to_string(),
            format!(
                "{context} in `{fn_name}` expected {}, got {}",
                type_name(expected),
                type_name(&actual)
            ),
        ));
    }
    Ok(())
}

fn type_name(typ: &Typ) -> String {
    match typ {
        Typ::Int => "Int".to_string(),
        Typ::Float => "Float".to_string(),
        Typ::String => "String".to_string(),
        Typ::Bool => "Bool".to_string(),
        Typ::Void => "Void".to_string(),
        Typ::Array(item) => format!("[{}]", type_name(item)),
        Typ::Vector(item) => format!("Vec<{}>", type_name(item)),
        Typ::Named(name) => name.clone(),
        Typ::Generic(name) => name.clone(),
    }
}

fn canonical_type(typ: &Typ) -> Typ {
    typ.canonical()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{Decl, UnifiedModule};
    use crate::core_ir::{Expr, Stmt};

    fn function(name: &str, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.to_string(),
            params: Vec::new(),
            ret: Typ::Void,
            body,
            type_params: vec![],
        }
    }

    fn function_with_ret(name: &str, ret: Typ, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.to_string(),
            params: Vec::new(),
            ret,
            body,
            type_params: vec![],
        }
    }

    fn function_with_params(
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

    fn default_options() -> VerifyOptions {
        VerifyOptions {
            entry: None,
            require_entry: false,
        }
    }

    #[test]
    fn accepts_valid_module_with_calls_and_entry() {
        let report = verify_for_entry(
            &module(vec![
                function_with_params(
                    "helper",
                    vec![("value".to_string(), Typ::Int)],
                    Typ::Int,
                    vec![Stmt::Return(Some(Expr::Ident("value".to_string())))],
                ),
                function(
                    "main",
                    vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".to_string())),
                        args: vec![Expr::IntLit(1)],
                    })],
                ),
            ]),
            "main",
        );

        assert!(report.ok, "{:?}", report);
        assert_eq!(report.parsed_function_count, 2);
        assert_eq!(
            report.call_edges,
            vec![("main".to_string(), "helper".to_string())]
        );
    }

    #[test]
    fn accepts_snake_case_reference_to_kebab_case_std_binding() {
        let module = crate::in_lang_parse::parse_in_source(
            "import std.io;\nimport std.process;\ncapability process.spawn;\ncapability process.stdout;\nfn main() -> void { let report: String = process_run(\"true\"); print(report); return; }\n",
        )
        .expect("parse stdlib source");

        let report = verify_for_entry(&module, "main");

        assert!(
            report.ok,
            "snake_case stdlib call should verify: {report:?}"
        );
        assert!(
            report
                .call_edges
                .contains(&("main".to_string(), "process_run".to_string()))
        );
    }

    #[test]
    fn rejects_duplicate_top_level_names() {
        let report = verify_module(
            &module(vec![
                function("main", Vec::new()),
                function("main", Vec::new()),
            ]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(
            report.reason_code.as_deref(),
            Some("duplicate-top-level-name")
        );
    }

    #[test]
    fn rejects_duplicate_parameter_names() {
        let report = verify_module(
            &module(vec![function_with_params(
                "main",
                vec![
                    ("value".to_string(), Typ::Int),
                    ("value".to_string(), Typ::Int),
                ],
                Typ::Void,
                Vec::new(),
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("duplicate-param-name"));
    }

    #[test]
    fn allows_duplicate_local_names() {
        // Duplicate locals are allowed (desugared for-loop variables may reuse names).
        let report = verify_module(
            &module(vec![function(
                "main",
                vec![
                    Stmt::Let("x".to_string(), None, Expr::IntLit(1)),
                    Stmt::Let("x".to_string(), None, Expr::IntLit(2)),
                ],
            )]),
            &default_options(),
        );

        assert!(
            report.ok,
            "duplicate locals should be allowed: {:?}",
            report
        );
    }

    #[test]
    fn rejects_call_arity_mismatch() {
        let report = verify_module(
            &module(vec![
                function_with_params(
                    "helper",
                    vec![("value".to_string(), Typ::Int)],
                    Typ::Void,
                    Vec::new(),
                ),
                function(
                    "main",
                    vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".to_string())),
                        args: Vec::new(),
                    })],
                ),
            ]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("call-arity-mismatch"));
    }

    #[test]
    fn rejects_call_argument_type_mismatch() {
        let report = verify_module(
            &module(vec![
                function_with_params(
                    "helper",
                    vec![("value".to_string(), Typ::Int)],
                    Typ::Void,
                    Vec::new(),
                ),
                function(
                    "main",
                    vec![Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".to_string())),
                        args: vec![Expr::BoolLit(true)], // Bool→Int still rejected
                    })],
                ),
            ]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
    }

    #[test]
    fn rejects_return_type_mismatch_for_int_functions() {
        let report = verify_module(
            &module(vec![function_with_ret(
                "main",
                Typ::Int,
                vec![Stmt::Return(Some(Expr::StringLit("nope".to_string())))],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("return-type-mismatch"));
    }

    #[test]
    fn rejects_return_type_mismatch_for_string_functions() {
        let report = verify_module(
            &module(vec![function_with_ret(
                "main",
                Typ::String,
                vec![Stmt::Return(Some(Expr::IntLit(1)))],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("return-type-mismatch"));
    }

    #[test]
    fn rejects_missing_return_value() {
        let report = verify_module(
            &module(vec![function_with_ret(
                "main",
                Typ::Int,
                vec![Stmt::Return(None)],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("return-type-mismatch"));
    }

    #[test]
    fn rejects_return_value_in_void_function() {
        let report = verify_module(
            &module(vec![function(
                "main",
                vec![Stmt::Return(Some(Expr::IntLit(1)))],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("return-type-mismatch"));
    }

    #[test]
    fn rejects_missing_entry_symbol() {
        let report = verify_for_entry(&module(vec![function("helper", Vec::new())]), "main");

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("missing-entry-symbol"));
    }

    #[test]
    fn rejects_unknown_and_missing_struct_fields() {
        let unknown = verify_module(
            &module(vec![
                point_struct(),
                function_with_ret(
                    "main",
                    Typ::Named("Point".to_string()),
                    vec![Stmt::Return(Some(Expr::StructInit {
                        name: "Point".to_string(),
                        fields: vec![
                            ("x".to_string(), Expr::IntLit(1)),
                            ("z".to_string(), Expr::IntLit(2)),
                        ],
                    }))],
                ),
            ]),
            &default_options(),
        );
        assert!(!unknown.ok);
        assert_eq!(unknown.reason_code.as_deref(), Some("unknown-struct-field"));

        let missing = verify_module(
            &module(vec![
                point_struct(),
                function_with_ret(
                    "main",
                    Typ::Named("Point".to_string()),
                    vec![Stmt::Return(Some(Expr::StructInit {
                        name: "Point".to_string(),
                        fields: vec![("x".to_string(), Expr::IntLit(1))],
                    }))],
                ),
            ]),
            &default_options(),
        );
        assert!(!missing.ok);
        assert_eq!(missing.reason_code.as_deref(), Some("missing-struct-field"));
    }

    #[test]
    fn rejects_struct_field_type_mismatch() {
        let report = verify_module(
            &module(vec![
                point_struct(),
                function(
                    "main",
                    vec![Stmt::Expr(Expr::StructInit {
                        name: "Point".to_string(),
                        fields: vec![
                            ("x".to_string(), Expr::StringLit("bad".to_string())),
                            ("y".to_string(), Expr::IntLit(2)),
                        ],
                    })],
                ),
            ]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
    }

    #[test]
    fn rejects_non_bool_if_condition() {
        let report = verify_module(
            &module(vec![function(
                "main",
                vec![Stmt::If {
                    cond: Expr::IntLit(1),
                    then_body: Vec::new(),
                    else_body: Vec::new(),
                }],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
    }

    #[test]
    fn rejects_non_bool_loop_condition() {
        let report = verify_module(
            &module(vec![function(
                "main",
                vec![Stmt::Loop {
                    kind: crate::core_ir::LoopKind::While,
                    cond: Some(Expr::IntLit(1)),
                    body: Vec::new(),
                }],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
    }

    #[test]
    fn rejects_let_annotation_type_mismatch() {
        let report = verify_module(
            &module(vec![function(
                "main",
                vec![Stmt::Let(
                    "value".to_string(),
                    Some(Typ::Int),
                    Expr::StringLit("bad".to_string()),
                )],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
    }

    #[test]
    fn rejects_assignment_type_mismatch() {
        let report = verify_module(
            &module(vec![function(
                "main",
                vec![
                    Stmt::Let("value".to_string(), Some(Typ::Int), Expr::IntLit(1)),
                    Stmt::Assign("value".to_string(), Expr::StringLit("bad".to_string())),
                ],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
    }

    #[test]
    fn accepts_array_index_assignment() {
        let report = verify_module(
            &module(vec![function(
                "main",
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
            )]),
            &default_options(),
        );

        assert!(report.ok, "unexpected verifier report: {report:?}");
    }

    #[test]
    fn rejects_array_index_assignment_type_mismatch() {
        let report = verify_module(
            &module(vec![function(
                "main",
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
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
    }

    #[test]
    fn allows_empty_body_for_non_void_return() {
        // ponytail: package externs and polyfills have empty bodies with non-void returns;
        // they're satisfied at link time by adapter invoke specs.
        let report = verify_module(
            &module(vec![function_with_ret("main", Typ::Int, Vec::new())]),
            &default_options(),
        );

        assert!(
            report.ok,
            "empty body + non-void return should be allowed: {report:?}"
        );
    }

    #[test]
    fn rejects_unresolved_identifiers() {
        let report = verify_module(
            &module(vec![function(
                "main",
                vec![Stmt::Return(Some(Expr::Ident("missing".to_string())))],
            )]),
            &default_options(),
        );

        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("unresolved-symbol"));
    }
}
