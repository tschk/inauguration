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
        if let Decl::Global {
            name,
            typ,
            init: Some(init),
            ..
        } = decl
        {
            let env = HashMap::new();
            if let Err((code, reason)) = check_expr(name, init, &facts, &env, &mut call_edges) {
                return fail_report(parsed_function_count, call_edges, &code, reason);
            }
            // Struct globals use zero as a request for zero-filled aggregate storage.
            let zeroed_struct = matches!(init.as_ref(), Expr::IntLit(0))
                && matches!(typ, Typ::Named(name) if facts.structs.contains_key(name.as_str()));
            if let Some(actual) = expr_type(init, &facts, &env)
                && !zeroed_struct
                && canonical_type(typ) != canonical_type(&actual)
            {
                return fail_report(
                    parsed_function_count,
                    call_edges,
                    "type-mismatch",
                    format!(
                        "global `{name}` initializer expected {}, got {}",
                        type_name(typ),
                        type_name(&actual)
                    ),
                );
            }
        }
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
                check_stmts(name, body, &facts, ret, &mut env, &mut call_edges, 0)
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
    globals: HashMap<&'a str, GlobalSig<'a>>,
}

struct GlobalSig<'a> {
    typ: &'a Typ,
    mutable: bool,
}

#[derive(Debug, Clone, Copy)]
struct FunctionSig<'a> {
    params: &'a [(String, Typ)],
    ret: &'a Typ,
}

/// Return type of an intrinsic, as the verifier needs it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IntrinsicRet {
    Void,
    Int,
    Bool,
    String,
    StringArray,
}

/// Every intrinsic the codegen resolves without a module-level declaration.
///
/// This is the single list: the predicate below and the signature table are both
/// derived from it, so a name the codegen supports cannot be rejected for want of
/// an entry. Keeping two lists meant `str-len` and `str-concat` were compiled by
/// the lowering yet failed verification before reaching it.
const INTRINSICS: &[(&str, IntrinsicRet)] = &[
    ("outb", IntrinsicRet::Void),
    ("inb", IntrinsicRet::Int),
    ("outl", IntrinsicRet::Void),
    ("inl", IntrinsicRet::Int),
    ("load8", IntrinsicRet::Int),
    ("load16", IntrinsicRet::Int),
    ("load32", IntrinsicRet::Int),
    ("load64", IntrinsicRet::Int),
    ("store8", IntrinsicRet::Void),
    ("store16", IntrinsicRet::Void),
    ("store32", IntrinsicRet::Void),
    ("store64", IntrinsicRet::Void),
    ("hlt", IntrinsicRet::Void),
    ("cli", IntrinsicRet::Void),
    ("sti", IntrinsicRet::Void),
    ("pause", IntrinsicRet::Void),
    ("lidt", IntrinsicRet::Void),
    ("invlpg", IntrinsicRet::Void),
    ("read-cr2", IntrinsicRet::Int),
    ("invoke", IntrinsicRet::Int),
    ("invoke1", IntrinsicRet::Int),
    ("invoke2", IntrinsicRet::Int),
    ("print", IntrinsicRet::Void),
    ("print-int", IntrinsicRet::Void),
    ("display", IntrinsicRet::String),
    ("to-string", IntrinsicRet::String),
    ("to-string-lossy", IntrinsicRet::String),
    ("from-utf8-lossy", IntrinsicRet::String),
    ("read-file", IntrinsicRet::String),
    ("write-file", IntrinsicRet::Bool),
    ("fs-exists", IntrinsicRet::Bool),
    ("create-dir", IntrinsicRet::Bool),
    ("remove-file", IntrinsicRet::Bool),
    ("env-get", IntrinsicRet::String),
    ("env-has", IntrinsicRet::Bool),
    ("env-set", IntrinsicRet::Void),
    ("env-temp-dir", IntrinsicRet::String),
    ("env-current-dir", IntrinsicRet::String),
    ("process-run", IntrinsicRet::String),
    ("path-join", IntrinsicRet::String),
    ("path-dirname", IntrinsicRet::String),
    ("path-basename", IntrinsicRet::String),
    ("path-extname", IntrinsicRet::String),
    ("path-normalize", IntrinsicRet::String),
    ("str-len", IntrinsicRet::Int),
    ("str-concat", IntrinsicRet::String),
    ("str-eq", IntrinsicRet::Bool),
    ("str-contains", IntrinsicRet::Bool),
    ("str-starts-with", IntrinsicRet::Bool),
    ("str-ends-with", IntrinsicRet::Bool),
    ("str-trim", IntrinsicRet::String),
    ("str-slice", IntrinsicRet::String),
    ("str-split-lines", IntrinsicRet::StringArray),
    ("str-split-spaces", IntrinsicRet::StringArray),
    ("str-tokenize-expr", IntrinsicRet::StringArray),
    ("str-to-int", IntrinsicRet::Int),
    ("str-is-int", IntrinsicRet::Bool),
    ("str-index-of", IntrinsicRet::Int),
    ("str-table-has", IntrinsicRet::Bool),
    ("str-table-get-int", IntrinsicRet::Int),
    ("array-len", IntrinsicRet::Int),
    ("array-push", IntrinsicRet::Void),
    ("json-stringify", IntrinsicRet::String),
];

/// Return true if `name` is a known intrinsic that doesn't need a module-level declaration.
pub fn is_intrinsic(name: &str) -> bool {
    intrinsic_ret(name).is_some()
}

/// Return type of a name the codegen resolves without a module-level
/// declaration, or `None` when the name must be declared.
///
/// Covers both the language-level intrinsics and the runtime-blob builtins the
/// lowering emits directly (`__inrt_str_eq` and friends): the lowering accepts
/// those by name, so verification has to accept them too.
fn intrinsic_ret(name: &str) -> Option<IntrinsicRet> {
    let public_name = name.replace('_', "-");
    if let Some((_, ret)) = INTRINSICS
        .iter()
        .find(|(intrinsic, _)| *intrinsic == public_name)
    {
        return Some(*ret);
    }
    match name {
        crate::inrt::INRT_STR_CONCAT | "__inrt_str_substr" => Some(IntrinsicRet::String),
        crate::inrt::INRT_STR_EQ | crate::inrt::INRT_STR_CONTAINS => Some(IntrinsicRet::Bool),
        crate::inrt::INRT_PRINT | crate::inrt::INRT_PRINT_INT | crate::inrt::INRT_TRAP_BUILTIN => {
            Some(IntrinsicRet::Void)
        }
        _ => crate::inrt::is_inrt_builtin(name).then_some(IntrinsicRet::Int),
    }
}

fn function_sig<'a>(facts: &ModuleFacts<'a>, name: &str) -> Option<FunctionSig<'a>> {
    facts.functions.get(name).copied().or_else(|| {
        let kebab_name = name.replace('_', "-");
        (kebab_name != name)
            .then(|| facts.functions.get(kebab_name.as_str()).copied())
            .flatten()
    })
}

pub(crate) fn intrinsic_return_type(name: &str) -> Option<&'static Typ> {
    static VOID_RET: Typ = Typ::Void;
    static INT_RET: Typ = Typ::Int;
    static STRING_RET: Typ = Typ::String;
    static BOOL_RET: Typ = Typ::Bool;
    static STRING_ARRAY_RET: std::sync::LazyLock<Typ> =
        std::sync::LazyLock::new(|| Typ::Array(Box::new(Typ::String)));
    Some(match intrinsic_ret(name)? {
        IntrinsicRet::Void => &VOID_RET,
        IntrinsicRet::Int => &INT_RET,
        IntrinsicRet::String => &STRING_RET,
        IntrinsicRet::Bool => &BOOL_RET,
        IntrinsicRet::StringArray => &STRING_ARRAY_RET,
    })
}

fn intrinsic_param_types(name: &str) -> Option<&'static [Typ]> {
    let name = name
        .strip_prefix("__inrt_")
        .unwrap_or(name)
        .replace('_', "-");
    Some(match name.as_str() {
        "str-concat" | "str-eq" | "str-contains" | "str-starts-with" | "str-ends-with"
        | "str-index-of" | "path-join" | "write-file" | "env-set" => &[Typ::String, Typ::String],
        "str-len" | "str-trim" | "str-split-lines" | "str-split-spaces" | "str-tokenize-expr"
        | "str-to-int" | "str-is-int" | "print" | "process-run" | "read-file" | "fs-exists"
        | "create-dir" | "remove-file" | "env-get" | "env-has" | "path-dirname"
        | "path-basename" | "path-extname" | "path-normalize" | "json-stringify" => &[Typ::String],
        "env-temp-dir" | "env-current-dir" => &[],
        "str-slice" | "str-substr" => &[Typ::String, Typ::Int, Typ::Int],
        "print-int" | "to-string" => &[Typ::Int],
        _ => return None,
    })
}

fn add_intrinsics<'a>(functions: &mut HashMap<&'a str, FunctionSig<'a>>) {
    for name in INTRINSICS
        .iter()
        .map(|(name, _)| *name)
        .chain(crate::inrt::INRT_BUILTINS.iter().copied())
    {
        if let Some(ret) = intrinsic_return_type(name) {
            functions.insert(name, FunctionSig { params: &[], ret });
        }
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
    let mut globals = HashMap::new();

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
            Decl::Global {
                name, typ, mutable, ..
            } => {
                check_and_insert_top_level(&mut top_level, name.as_str())?;
                globals.insert(
                    name.as_str(),
                    GlobalSig {
                        typ,
                        mutable: *mutable,
                    },
                );
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
    loop_depth: usize,
) -> Result<(), (String, String)> {
    for stmt in stmts {
        check_stmt(fn_name, stmt, facts, ret, env, call_edges, loop_depth)?;
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
    loop_depth: usize,
) -> Result<(), (String, String)> {
    match stmt {
        Stmt::Let(name, typ, expr) => {
            // Check the initializer before the new binding shadows the previous one.
            check_expr(fn_name, expr, facts, env, call_edges)?;
            let expr_typ = expr_type(expr, facts, env);
            if let (Some(expected), Some(actual)) = (typ, expr_typ.as_ref())
                && !expected.compatible_with(actual)
                && !(matches!(expected.canonical(), Typ::Array(_))
                    && matches!(expr, Expr::ArrayLit(items) if items.is_empty()))
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
            let is_global = facts.globals.contains_key(name.as_str());
            if !env.contains_key(name) && !is_global {
                return Err((
                    "unresolved-symbol".to_string(),
                    format!("unresolved assignment `{name}` in `{fn_name}`"),
                ));
            };
            if !env.contains_key(name)
                && let Some(global) = facts.globals.get(name.as_str())
                && !global.mutable
            {
                return Err((
                    "immutable-global".into(),
                    format!("cannot assign to constant global `{name}` in `{fn_name}`"),
                ));
            }
            check_expr(fn_name, expr, facts, env, call_edges)?;
            if let Some(expr_typ) = expr_type(expr, facts, env) {
                let expected_typ = env
                    .get(name)
                    .or_else(|| facts.globals.get(name.as_str()).map(|global| global.typ))
                    .expect("assignment target was resolved")
                    .clone();
                if !expected_typ.compatible_with(&expr_typ) {
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
        Stmt::Break | Stmt::Continue if loop_depth == 0 => Err((
            "invalid-control-flow".into(),
            format!("loop jump outside a loop in `{fn_name}`"),
        )),
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
            check_stmts(
                fn_name,
                then_body,
                facts,
                ret,
                &mut env.clone(),
                call_edges,
                loop_depth,
            )?;
            check_stmts(
                fn_name,
                else_body,
                facts,
                ret,
                &mut env.clone(),
                call_edges,
                loop_depth,
            )
        }
        Stmt::Loop { cond, body, .. } => {
            if let Some(cond) = cond {
                check_expr(fn_name, cond, facts, env, call_edges)?;
                require_type(fn_name, "loop condition", &Typ::Bool, cond, facts, env)?;
            }
            check_stmts(
                fn_name,
                body,
                facts,
                ret,
                &mut env.clone(),
                call_edges,
                loop_depth + 1,
            )
        }
        Stmt::Match {
            scrutinee, arms, ..
        } => {
            check_expr(fn_name, scrutinee, facts, env, call_edges)?;
            for arm in arms {
                let mut arm_env = env.clone();
                if let Ok(crate::core_ir::MatchPattern::IdentPat(name)) =
                    crate::core_ir::MatchPattern::parse(&arm.pattern)
                {
                    arm_env.insert(
                        name,
                        expr_type(scrutinee, facts, env).unwrap_or(Typ::Named("Any".into())),
                    );
                }
                if let Some(guard) = &arm.guard {
                    check_expr(fn_name, guard, facts, &arm_env, call_edges)?;
                    require_type(fn_name, "match guard", &Typ::Bool, guard, facts, &arm_env)?;
                }
                check_stmts(
                    fn_name,
                    &arm.body,
                    facts,
                    ret,
                    &mut arm_env,
                    call_edges,
                    loop_depth,
                )?;
            }
            Ok(())
        }
        Stmt::Throw(expr) => check_expr(fn_name, expr, facts, env, call_edges),
        Stmt::Try { body, catches } => {
            check_stmts(
                fn_name,
                body,
                facts,
                ret,
                &mut env.clone(),
                call_edges,
                loop_depth,
            )?;
            for catch in catches {
                let mut catch_env = env.clone();
                let pattern = crate::core_ir::MatchPattern::parse(&catch.pattern)
                    .map_err(|error| ("invalid-catch-pattern".into(), error))?;
                // CatchArm does not declare the payload type.
                if let crate::core_ir::MatchPattern::IdentPat(name) = pattern {
                    catch_env.insert(name, Typ::Named("Any".into()));
                }
                check_stmts(
                    fn_name,
                    &catch.body,
                    facts,
                    ret,
                    &mut catch_env,
                    call_edges,
                    loop_depth,
                )?;
            }
            Ok(())
        }
        Stmt::Propagate => Ok(()),
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
                || facts.globals.contains_key(name.as_str())
            {
                Ok(())
            } else {
                Err((
                    "unresolved-symbol".to_string(),
                    format!("unresolved identifier `{name}` in `{fn_name}`"),
                ))
            }
        }
        Expr::Unary { op, expr } => {
            check_expr(fn_name, expr, facts, env, call_edges)?;
            if op == "complement" {
                return Err((
                    "unsupported-complement".into(),
                    format!(
                        "complement operand in `{fn_name}` must have a resolved Int or Bool type"
                    ),
                ));
            }
            if matches!(op.as_str(), "int-to-float" | "float-to-int") {
                let expected = if op == "int-to-float" {
                    Typ::Int
                } else {
                    Typ::Float
                };
                require_type(fn_name, "numeric conversion", &expected, expr, facts, env)?;
            }
            if op == "bool-to-int" {
                require_type(fn_name, "boolean promotion", &Typ::Bool, expr, facts, env)?;
            }
            if op == "~" {
                require_type(
                    fn_name,
                    "bitwise complement operand",
                    &Typ::Int,
                    expr,
                    facts,
                    env,
                )?;
            }
            Ok(())
        }
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
                "&" | "|" | "^" => {
                    if crate::core_ir::bitwise_result_type(
                        expr_type(lhs, facts, env),
                        expr_type(rhs, facts, env),
                    )
                    .is_some()
                    {
                        Ok(())
                    } else {
                        Err((
                            "type-mismatch".into(),
                            format!(
                                "binary `{op}` in `{fn_name}` needs matching Int or Bool operands"
                            ),
                        ))
                    }
                }
                "<<" | ">>" | "%" => {
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
            let mut expected: Option<Typ> = None;
            for item in items {
                check_expr(fn_name, item, facts, env, call_edges)?;
                if let Some(item_typ) = expr_type(item, facts, env) {
                    if let Some(expected_typ) = &expected {
                        if !expected_typ.compatible_with(&item_typ) {
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
                // Scalar runtime intrinsics require their declared argument types.
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
                            && !param_typ.compatible_with(&arg_typ)
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
                    for arg in args {
                        check_expr(fn_name, arg, facts, env, call_edges)?;
                    }
                    if matches!(name.as_str(), "array-len" | "array_len") {
                        if args.len() != 1 {
                            return Err((
                                "call-arity-mismatch".into(),
                                format!(
                                    "call to `{name}` in `{fn_name}` expects 1 args, got {}",
                                    args.len()
                                ),
                            ));
                        }
                        let actual = expr_type(&args[0], facts, env);
                        if !matches!(
                            actual.as_ref().map(Typ::canonical),
                            Some(Typ::Array(_) | Typ::Vector(_))
                        ) {
                            return Err((
                                "type-mismatch".into(),
                                format!(
                                    "argument 1 for `{name}` in `{fn_name}` expected array, got {}",
                                    actual
                                        .as_ref()
                                        .map(type_name)
                                        .unwrap_or_else(|| "unknown".into())
                                ),
                            ));
                        }
                    }
                    if let Some(params) = intrinsic_param_types(name) {
                        if args.len() != params.len() {
                            return Err((
                                "call-arity-mismatch".into(),
                                format!(
                                    "call to `{name}` in `{fn_name}` expects {} args, got {}",
                                    params.len(),
                                    args.len()
                                ),
                            ));
                        }
                        for (index, (expected, arg)) in params.iter().zip(args).enumerate() {
                            if let Some(actual) = expr_type(arg, facts, env)
                                && actual.canonical() != expected.canonical()
                            {
                                return Err((
                                    "type-mismatch".into(),
                                    format!(
                                        "argument {} for `{name}` in `{fn_name}` expected {}, got {}",
                                        index + 1,
                                        type_name(expected),
                                        type_name(&actual)
                                    ),
                                ));
                            }
                        }
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
            && !expected.compatible_with(&actual)
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
                facts
                    .globals
                    .get(name.as_str())
                    .map(|global| global.typ.clone())
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
            "bool-to-int" | "float-to-int" => Some(Typ::Int),
            "int-to-float" => Some(Typ::Float),
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
                "&" | "|" | "^" => crate::core_ir::bitwise_result_type(lhs_typ, rhs_typ),
                "+" | "-" | "*" | "/" | "%" | "<<" | ">>" => Some(Typ::Int),
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
        && !expected.compatible_with(&actual)
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
    fn shadow_initializers_use_the_previous_binding() {
        for params in [vec![], vec![("x".into(), Typ::Int)]] {
            let mut body = vec![];
            if params.is_empty() {
                body.push(Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(1)));
            }
            body.push(Stmt::Let(
                "x".into(),
                Some(Typ::Int),
                Expr::Ident("x".into()),
            ));
            body.push(Stmt::Return(Some(Expr::Ident("x".into()))));
            let report = verify_module(
                &module(vec![function_with_params("main", params, Typ::Int, body)]),
                &default_options(),
            );
            assert!(report.ok, "{report:?}");
        }
        let report = verify_module(
            &module(vec![function(
                "main",
                vec![Stmt::Let("x".into(), None, Expr::Ident("x".into()))],
            )]),
            &default_options(),
        );
        assert!(!report.ok, "a first declaration cannot initialize itself");
    }

    #[test]
    fn exception_bodies_preserve_statement_checks() {
        use crate::core_ir::CatchArm;
        for (bad, reason) in [
            (
                Stmt::Assign("value".into(), Expr::IntLit(42)),
                "immutable-global",
            ),
            (
                Stmt::Expr(Expr::Ident("missing".into())),
                "unresolved-symbol",
            ),
            (
                Stmt::Return(Some(Expr::StringLit("wrong".into()))),
                "return-type-mismatch",
            ),
            (
                Stmt::Let(
                    "local".into(),
                    Some(Typ::Int),
                    Expr::StringLit("wrong".into()),
                ),
                "type-mismatch",
            ),
        ] {
            for in_catch in [false, true] {
                let (body, catch_body) = if in_catch {
                    (vec![], vec![bad.clone()])
                } else {
                    (vec![bad.clone()], vec![])
                };
                let report = verify_module(
                    &module(vec![
                        Decl::Global {
                            name: "value".into(),
                            typ: Typ::Int,
                            init: Some(Box::new(Expr::IntLit(0))),
                            mutable: false,
                            constant_value: false,
                        },
                        function_with_params(
                            "main",
                            vec![],
                            Typ::Int,
                            vec![Stmt::Try {
                                body,
                                catches: vec![CatchArm {
                                    pattern: "error".into(),
                                    body: catch_body,
                                }],
                            }],
                        ),
                    ]),
                    &default_options(),
                );
                assert!(!report.ok, "{in_catch}: {bad:?}: {report:?}");
                assert_eq!(report.reason_code.as_deref(), Some(reason));
            }
        }
        let report = verify_module(
            &module(vec![function_with_params(
                "main",
                vec![],
                Typ::Int,
                vec![Stmt::Throw(Expr::Ident("missing".into()))],
            )]),
            &default_options(),
        );
        assert!(!report.ok);
        assert_eq!(report.reason_code.as_deref(), Some("unresolved-symbol"));
    }

    #[test]
    fn caught_payloads_keep_unknown_types_until_consumed() {
        use crate::core_ir::{CatchArm, FloatVal};
        for (typ, value) in [
            (Typ::Int, Expr::IntLit(7)),
            (Typ::Bool, Expr::BoolLit(true)),
            (Typ::Float, Expr::FloatLit(FloatVal(3.75))),
            (Typ::String, Expr::StringLit("value".into())),
        ] {
            let report = verify_module(
                &module(vec![function_with_ret(
                    "main",
                    typ.clone(),
                    vec![Stmt::Try {
                        body: vec![Stmt::Throw(value)],
                        catches: vec![CatchArm {
                            pattern: "error".into(),
                            body: vec![
                                Stmt::Let("alias".into(), Some(typ), Expr::Ident("error".into())),
                                Stmt::Return(Some(Expr::Ident("alias".into()))),
                            ],
                        }],
                    }],
                )]),
                &default_options(),
            );
            assert!(report.ok, "{report:?}");
        }
    }

    #[test]
    fn exception_bindings_and_call_edges_keep_their_scope() {
        use crate::core_ir::CatchArm;
        let caught = Stmt::Try {
            body: vec![
                Stmt::Let("local".into(), Some(Typ::Int), Expr::IntLit(7)),
                Stmt::Throw(Expr::Ident("local".into())),
            ],
            catches: vec![CatchArm {
                pattern: "error".into(),
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![Expr::Ident("error".into())],
                }))],
            }],
        };
        let helper = function_with_params(
            "helper",
            vec![("value".into(), Typ::Int)],
            Typ::Int,
            vec![Stmt::Return(Some(Expr::Ident("value".into())))],
        );
        let report = verify_module(
            &module(vec![
                helper.clone(),
                function_with_params(
                    "main",
                    vec![],
                    Typ::Int,
                    vec![caught.clone(), Stmt::Return(Some(Expr::IntLit(0)))],
                ),
            ]),
            &default_options(),
        );
        assert!(report.ok, "{report:?}");
        assert!(
            report
                .call_edges
                .contains(&("main".into(), "helper".into()))
        );
        for name in ["local", "error"] {
            let report = verify_module(
                &module(vec![
                    helper.clone(),
                    function_with_params(
                        "main",
                        vec![],
                        Typ::Int,
                        vec![caught.clone(), Stmt::Return(Some(Expr::Ident(name.into())))],
                    ),
                ]),
                &default_options(),
            );
            assert!(!report.ok, "binding leaked: {name}: {report:?}");
            assert_eq!(report.reason_code.as_deref(), Some("unresolved-symbol"));
        }
    }

    #[test]
    fn global_assignments_respect_mutability_and_local_shadowing() {
        for mutable in [false, true] {
            for shadowed in [0, 1, 2] {
                let params = if shadowed == 1 {
                    vec![("value".into(), Typ::Int)]
                } else {
                    vec![]
                };
                let mut body = vec![];
                if shadowed == 2 {
                    body.push(Stmt::Let("value".into(), Some(Typ::Int), Expr::IntLit(0)));
                }
                body.push(Stmt::Assign("value".into(), Expr::IntLit(42)));
                body.push(Stmt::Return(Some(Expr::Ident("value".into()))));
                let report = verify_module(
                    &module(vec![
                        Decl::Global {
                            name: "value".into(),
                            typ: Typ::Int,
                            init: Some(Box::new(Expr::IntLit(1))),
                            mutable,
                            constant_value: false,
                        },
                        function_with_params("main", params, Typ::Int, body),
                    ]),
                    &default_options(),
                );
                assert_eq!(report.ok, mutable || shadowed != 0, "{report:?}");
                if !report.ok {
                    assert_eq!(report.reason_code.as_deref(), Some("immutable-global"));
                }
            }
        }
    }

    #[test]
    fn struct_global_zero_initialization_remains_valid() {
        for (value, valid) in [(0, true), (1, false)] {
            let report = verify_module(
                &module(vec![
                    point_struct(),
                    Decl::Global {
                        name: "point".into(),
                        typ: Typ::Named("Point".into()),
                        init: Some(Box::new(Expr::IntLit(value))),
                        mutable: true,
                        constant_value: false,
                    },
                ]),
                &default_options(),
            );
            assert_eq!(report.ok, valid, "{report:?}");
        }
    }

    #[test]
    fn rejects_invalid_global_initializers() {
        for (value, reason) in [
            (Expr::StringLit("wrong".into()), "type-mismatch"),
            (Expr::Ident("missing".into()), "unresolved-symbol"),
        ] {
            let report = verify_module(
                &module(vec![Decl::Global {
                    name: "value".into(),
                    typ: Typ::Float,
                    init: Some(Box::new(value)),
                    mutable: true,
                    constant_value: false,
                }]),
                &default_options(),
            );
            assert!(!report.ok, "{report:?}");
            assert_eq!(report.reason_code.as_deref(), Some(reason));
        }
    }

    #[test]
    fn globals_keep_declared_types_for_reads_and_assignments() {
        for (typ, value) in [
            (Typ::Float, Expr::FloatLit(crate::core_ir::FloatVal(1.25))),
            (Typ::String, Expr::StringLit("value".into())),
            (Typ::Bool, Expr::BoolLit(true)),
        ] {
            let global = Decl::Global {
                name: "value".into(),
                typ: typ.clone(),
                init: Some(Box::new(value.clone())),
                mutable: true,
                constant_value: false,
            };
            let valid = module(vec![
                global.clone(),
                function_with_ret(
                    "main",
                    typ.clone(),
                    vec![
                        Stmt::Assign("value".into(), value),
                        Stmt::Return(Some(Expr::Ident("value".into()))),
                    ],
                ),
            ]);
            let report = verify_for_entry(&valid, "main");
            assert!(report.ok, "{typ:?}: {report:?}");
            let invalid = module(vec![
                global,
                function("main", vec![Stmt::Assign("value".into(), Expr::IntLit(1))]),
            ]);
            let report = verify_module(&invalid, &default_options());
            assert!(!report.ok, "{typ:?}: {report:?}");
            assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
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
    fn rejects_loop_jumps_outside_lexical_loops() {
        for jump in ["break", "continue"] {
            for body in [
                format!("{jump};"),
                format!("if false {{ {jump}; }}"),
                format!("try {{ {jump}; }} catch _ {{ }}"),
                format!("try {{ throw 7; }} catch _ {{ {jump}; }}"),
                format!("while false {{ {jump}; }} {jump};"),
            ] {
                let module = crate::in_lang_parse::parse_in_source(&format!(
                    "fn main() -> Int {{ {body} return 0; }}"
                ))
                .unwrap();
                let report = verify_module(&module, &default_options());
                assert!(!report.ok, "{body}");
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("invalid-control-flow"),
                    "{body}"
                );
            }
            let module = crate::in_lang_parse::parse_in_source(&format!(
                "fn main() -> Int {{ while true {{ try {{ {jump}; }} catch _ {{ }} }} return 0; }}"
            ))
            .unwrap();
            assert!(verify_module(&module, &default_options()).ok);
        }
    }

    #[test]
    fn checks_scalar_intrinsic_argument_types_and_arity() {
        for (expression, reason) in [
            ("str-eq(42, 0)", Some("type-mismatch")),
            ("array-len(\"abc\")", Some("type-mismatch")),
            ("array_len(42)", Some("type-mismatch")),
            ("array-len()", Some("call-arity-mismatch")),
            ("array_len([], [])", Some("call-arity-mismatch")),
            ("array-len([])", None),
            ("array_len([1, 2])", None),
            ("str_eq(42, 0)", Some("type-mismatch")),
            ("process_run(0)", Some("type-mismatch")),
            ("path-join(42, \"x\")", Some("type-mismatch")),
            ("path_join(\"x\", false)", Some("type-mismatch")),
            ("read-file(42)", Some("type-mismatch")),
            ("fs_exists(false)", Some("type-mismatch")),
            ("write_file(\"x\", 42)", Some("type-mismatch")),
            ("env_set(\"KEY\", false)", Some("type-mismatch")),
            ("env-get(42)", Some("type-mismatch")),
            ("env-temp-dir(1)", Some("call-arity-mismatch")),
            ("path-join(\"a\")", Some("call-arity-mismatch")),
            ("path_join(\"a\", \"b\")", None),
            ("write-file(\"a\", \"b\")", None),
            ("env-set(\"KEY\", \"value\")", None),
            ("env_current_dir()", None),
            ("json_stringify(42)", Some("type-mismatch")),
            ("process-run()", Some("call-arity-mismatch")),
            ("str_concat(\"a\")", Some("call-arity-mismatch")),
            ("__inrt_str_eq(false, \"a\")", Some("type-mismatch")),
            ("str-concat(\"a\")", Some("call-arity-mismatch")),
            (
                "__inrt_str_substr(\"abc\", false, 1)",
                Some("type-mismatch"),
            ),
            ("str-eq(\"a\", \"a\")", None),
            ("str_eq(\"a\", \"a\")", None),
            ("str_concat(\"a\", \"b\")", None),
            ("__inrt_str_substr(\"abc\", 0, 1)", None),
        ] {
            let source = format!("fn main() -> Int {{ let value = {expression}; return 0; }}");
            let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            let report = verify_for_entry(&module, "main");
            assert_eq!(report.reason_code.as_deref(), reason, "{report:?}");
            assert_eq!(report.ok, reason.is_none(), "{report:?}");
        }
    }

    #[test]
    fn match_bindings_keep_scrutinee_types_and_arm_scope() {
        for (source, reason) in [
            (
                "fn main() -> Int { match 5 { captured { return captured; } } return 0; }",
                None,
            ),
            (
                "fn main() -> Int { match false { captured { return captured; } } return 0; }",
                Some("return-type-mismatch"),
            ),
            (
                "fn main() -> Int { match 5 { captured { let seen = captured; } } return captured; }",
                Some("unresolved-symbol"),
            ),
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            let report = verify_for_entry(&module, "main");
            assert_eq!(report.reason_code.as_deref(), reason, "{report:?}");
            assert_eq!(report.ok, reason.is_none(), "{report:?}");
        }
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
    #[test]
    fn numeric_conversions_require_matching_operand_types() {
        for (op, ret, good, bad) in [
            (
                "int-to-float",
                Typ::Float,
                Expr::IntLit(42),
                Expr::BoolLit(true),
            ),
            (
                "float-to-int",
                Typ::Int,
                Expr::FloatLit(crate::core_ir::FloatVal(42.9)),
                Expr::IntLit(42),
            ),
        ] {
            for (operand, accepted) in [(good, true), (bad, false)] {
                let expr = Expr::Unary {
                    op: op.into(),
                    expr: Box::new(operand),
                };
                let report = verify_module(
                    &module(vec![function_with_ret(
                        "main",
                        ret.clone(),
                        vec![Stmt::Return(Some(expr))],
                    )]),
                    &default_options(),
                );
                assert_eq!(report.ok, accepted, "{op}: {report:?}");
                if !accepted {
                    assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
                }
            }
        }
    }
}
