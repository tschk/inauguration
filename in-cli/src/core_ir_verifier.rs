mod definite_initialization;
pub(crate) use definite_initialization::{
    check_definite_initialization, initialization_errors_by_function,
    initialization_errors_by_global,
};

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

/// Runtime/source emission requires all retained source contracts to be resolved.
pub(crate) fn check_execution_contracts(module: &UnifiedModule) -> Result<(), String> {
    let proofs = crate::typecheck::TypeChecker::new().typed_proof_nodes(&module.decls);
    if let Some((name, proof)) = proofs.iter().min_by_key(|(name, _)| *name) {
        return Err(format!(
            "source-type-proof: unresolved contract in `{name}`: {}",
            proof.message()
        ));
    }
    check_definite_initialization(module)
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

    if let Err(reason) = check_definite_initialization(module) {
        return fail_report(
            parsed_function_count,
            call_edges,
            "uninitialized-local",
            reason,
        );
    }
    let completion = CompletionContext::new(module, &facts);

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
                && !typ.accepts_sequence_literal(init, &actual)
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

            let mut env: HashMap<String, Typ> = params.iter().cloned().collect();
            if let Err((code, reason)) =
                check_stmts(name, body, &facts, ret, &mut env, &mut call_edges, 0)
            {
                return fail_report(parsed_function_count, call_edges, &code, reason);
            }
            if let Some(reason) = completion.missing_return(decl) {
                return fail_report(parsed_function_count, call_edges, "missing-return", reason);
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
    declared_functions: HashSet<&'a str>,
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
    IntVector,
    FloatVector,
    BoolVector,
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
    ("outw", IntrinsicRet::Void),
    ("inw", IntrinsicRet::Int),
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
    ("lgdt", IntrinsicRet::Void),
    ("ltr", IntrinsicRet::Void),
    ("invlpg", IntrinsicRet::Void),
    ("read-cr2", IntrinsicRet::Int),
    ("read-cr3", IntrinsicRet::Int),
    ("cr3-read", IntrinsicRet::Int),
    ("write-cr3", IntrinsicRet::Void),
    ("cr3-write", IntrinsicRet::Void),
    ("rdmsr", IntrinsicRet::Int),
    ("wrmsr", IntrinsicRet::Void),
    ("wrmsr64", IntrinsicRet::Void),
    ("invoke", IntrinsicRet::Int),
    ("invoke1", IntrinsicRet::Int),
    ("invoke2", IntrinsicRet::Int),
    ("invoke-native", IntrinsicRet::Int),
    ("invoke-native1", IntrinsicRet::Int),
    ("invoke-native2", IntrinsicRet::Int),
    ("invoke_native", IntrinsicRet::Int),
    ("invoke_native1", IntrinsicRet::Int),
    ("invoke_native2", IntrinsicRet::Int),
    (crate::core_ir::COMPILER_PRINT, IntrinsicRet::Void),
    (crate::core_ir::COMPILER_PRINT_INT, IntrinsicRet::Void),
    (crate::core_ir::COMPILER_UNREACHABLE, IntrinsicRet::Void),
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
    ("env-remove", IntrinsicRet::Void),
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
    (
        crate::compiler::rust_front::integer_parse::STR_BYTE,
        IntrinsicRet::Int,
    ),
    ("str-table-has", IntrinsicRet::Bool),
    ("str-table-get-int", IntrinsicRet::Int),
    ("array-len", IntrinsicRet::Int),
    ("array-push", IntrinsicRet::Void),
    ("vector-push", IntrinsicRet::Void),
    ("vector-extend", IntrinsicRet::Void),
    ("vector-clear", IntrinsicRet::Void),
    ("vector-capacity", IntrinsicRet::Int),
    ("vector-with-capacity-int", IntrinsicRet::IntVector),
    ("vector-with-capacity-float", IntrinsicRet::FloatVector),
    ("vector-with-capacity-bool", IntrinsicRet::BoolVector),
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
    if crate::core_ir::compiler_print_callee(name).is_some() {
        return Some(IntrinsicRet::Void);
    }
    if crate::core_atomic::signature(name).is_some() {
        return Some(IntrinsicRet::Int);
    }
    let public_name = name.replace('_', "-");
    if let Some((_, ret)) = INTRINSICS
        .iter()
        .find(|(intrinsic, _)| *intrinsic == public_name)
    {
        return Some(*ret);
    }
    match name {
        crate::inrt::INRT_STR_CONCAT | crate::inrt::INRT_INT_TO_STRING | "__inrt_str_substr" => {
            Some(IntrinsicRet::String)
        }
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
    if let Some(sig) = crate::core_atomic::signature(name) {
        return Some(&sig.ret);
    }
    static VOID_RET: Typ = Typ::Void;
    static INT_RET: Typ = Typ::Int;
    static STRING_RET: Typ = Typ::String;
    static BOOL_RET: Typ = Typ::Bool;
    static STRING_ARRAY_RET: std::sync::LazyLock<Typ> =
        std::sync::LazyLock::new(|| Typ::Array(Box::new(Typ::String)));
    static INT_VECTOR_RET: std::sync::LazyLock<Typ> =
        std::sync::LazyLock::new(|| Typ::Vector(Box::new(Typ::Int)));
    static FLOAT_VECTOR_RET: std::sync::LazyLock<Typ> =
        std::sync::LazyLock::new(|| Typ::Vector(Box::new(Typ::Float)));
    static BOOL_VECTOR_RET: std::sync::LazyLock<Typ> =
        std::sync::LazyLock::new(|| Typ::Vector(Box::new(Typ::Bool)));
    Some(match intrinsic_ret(name)? {
        IntrinsicRet::Void => &VOID_RET,
        IntrinsicRet::Int => &INT_RET,
        IntrinsicRet::String => &STRING_RET,
        IntrinsicRet::Bool => &BOOL_RET,
        IntrinsicRet::StringArray => &STRING_ARRAY_RET,
        IntrinsicRet::IntVector => &INT_VECTOR_RET,
        IntrinsicRet::FloatVector => &FLOAT_VECTOR_RET,
        IntrinsicRet::BoolVector => &BOOL_VECTOR_RET,
    })
}

pub(crate) fn is_native_invoke(name: &str) -> bool {
    matches!(
        name,
        "invoke-native"
            | "invoke-native1"
            | "invoke-native2"
            | "invoke_native"
            | "invoke_native1"
            | "invoke_native2"
    )
}

pub(crate) fn invoke_param_types(name: &str) -> Option<&'static [Typ]> {
    match name {
        "invoke" | "invoke-native" | "invoke_native" => Some(&[Typ::Int]),
        "invoke1" | "invoke-native1" | "invoke_native1" => Some(&[Typ::Int, Typ::Int]),
        "invoke2" | "invoke-native2" | "invoke_native2" => Some(&[Typ::Int, Typ::Int, Typ::Int]),
        _ => None,
    }
}

pub(crate) fn invoke_signature_error(
    name: &str,
    params: &[(String, Typ)],
    ret: &Typ,
) -> Option<String> {
    let carriers = invoke_param_types(name)?;
    if params.len() + 1 != carriers.len()
        || ret.canonical() != Typ::Int
        || params.iter().any(|(_, typ)| typ.canonical() != Typ::Int)
    {
        Some(format!(
            "`{name}` requires a function with {} Int parameters and an Int return",
            carriers.len() - 1
        ))
    } else {
        None
    }
}

pub(crate) fn intrinsic_param_types(name: &str) -> Option<&'static [Typ]> {
    if let Some(public) = crate::core_ir::compiler_print_callee(name) {
        return intrinsic_param_types(public);
    }
    match name {
        crate::core_ir::COMPILER_UNREACHABLE => return Some(&[]),
        crate::compiler::rust_front::integer_parse::STR_BYTE => {
            return Some(&[Typ::String, Typ::Int]);
        }
        "__inrt_array_len" => return Some(&[Typ::Int]),
        crate::inrt::INRT_VECTOR_CLONE => return Some(&[Typ::Int, Typ::Int, Typ::Int]),
        crate::inrt::INRT_VECTOR_EXTEND => return Some(&[Typ::Int, Typ::Int, Typ::Int, Typ::Int]),
        _ => {}
    }
    if let Some(params) = invoke_param_types(name) {
        return Some(params);
    }
    if let Some(sig) = crate::core_atomic::signature(name) {
        return Some(&sig.params);
    }
    let name = name
        .strip_prefix("__inrt_")
        .unwrap_or(name)
        .replace('_', "-");
    Some(match name.as_str() {
        "load8" | "load16" | "load32" | "load64" => &[Typ::Int],
        "store8" | "store16" | "store32" | "store64" => &[Typ::Int, Typ::Int],
        "inb" | "inw" | "inl" => &[Typ::Int],
        "outb" | "outw" | "outl" => &[Typ::Int, Typ::Int],
        "read-cr3" | "cr3-read" => &[],
        "write-cr3" | "cr3-write" | "lgdt" | "ltr" | "rdmsr" => &[Typ::Int],
        "wrmsr64" => &[Typ::Int, Typ::Int],
        "wrmsr" | "mem-fill64" | "mem-copy64" => &[Typ::Int, Typ::Int, Typ::Int],
        "str-concat" | "str-eq" | "str-contains" | "str-starts-with" | "str-ends-with"
        | "str-index-of" | "path-join" | "write-file" | "env-set" | "str-table-has" => {
            &[Typ::String, Typ::String]
        }
        "str-len" | "str-trim" | "str-split-lines" | "str-split-spaces" | "str-tokenize-expr"
        | "str-to-int" | "str-is-int" | "print" | "process-run" | "read-file" | "fs-exists"
        | "create-dir" | "remove-file" | "env-get" | "env-has" | "path-dirname"
        | "path-basename" | "path-extname" | "path-normalize" | "json-stringify" | "env-remove" => {
            &[Typ::String]
        }
        "env-temp-dir" | "env-current-dir" => &[],
        "str-table-get-int" => &[Typ::String, Typ::String, Typ::String],
        "str-slice" | "str-substr" => &[Typ::String, Typ::Int, Typ::Int],
        "print-int"
        | "to-string"
        | "vector-with-capacity-int"
        | "vector-with-capacity-float"
        | "vector-with-capacity-bool"
        | "vector-alloc" => &[Typ::Int],
        _ => return None,
    })
}

fn add_intrinsics<'a>(functions: &mut HashMap<&'a str, FunctionSig<'a>>) {
    for name in INTRINSICS
        .iter()
        .map(|(name, _)| *name)
        .chain(crate::inrt::INRT_BUILTINS.iter().copied())
        .chain(crate::core_atomic::names())
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
    if crate::core_ir::is_compiler_callee(name) {
        return Err((
            "reserved-intrinsic-name".into(),
            format!("private compiler intrinsic `{name}` cannot be declared"),
        ));
    }
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
    for declaration in &module.decls {
        match declaration {
            Decl::Struct { name, .. }
            | Decl::Class { name, .. }
            | Decl::Function { name, .. }
            | Decl::Global { name, .. } => check_and_insert_top_level(&mut top_level, name)?,
            _ => {}
        }
    }
    Ok(module_type_facts(module))
}

// Completion consumes type facts, not full namespace validation. Raw backend
// compile-only paths deliberately retain Rust's separate type/value namespaces.
fn module_type_facts(module: &UnifiedModule) -> ModuleFacts<'_> {
    let mut functions = HashMap::new();
    let mut declared_functions = HashSet::new();
    let mut structs = HashMap::new();
    let mut globals = HashMap::new();

    add_intrinsics(&mut functions);

    for decl in &module.decls {
        match decl {
            Decl::Struct { name, fields, .. } => {
                structs.insert(name.as_str(), fields.as_slice());
            }
            Decl::Class { name, fields, .. } => {
                // Register class fields as struct schema for verification
                structs.insert(name.as_str(), fields.as_slice());
            }
            Decl::Function {
                name, params, ret, ..
            } => {
                functions.insert(name.as_str(), FunctionSig { params, ret });
                declared_functions.insert(name.as_str());
            }
            Decl::Interface { .. } => {}
            Decl::Component { .. } => {}
            Decl::Global {
                name, typ, mutable, ..
            } => {
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

    ModuleFacts {
        functions,
        declared_functions,
        structs,
        globals,
    }
}

// Completion outcomes are compositional: a loop jump is not a function return,
// and a return hidden in an unexecuted closure is not an enclosing return.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Completion(u8);
impl Completion {
    const NORMAL: Self = Self(1);
    const RETURN: Self = Self(2);
    const THROW: Self = Self(4);
    const BREAK: Self = Self(8);
    const CONTINUE: Self = Self(16);
    const DIVERGE: Self = Self(0);
    fn has(self, outcome: Self) -> bool {
        self.0 & outcome.0 != 0
    }
    fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    fn without(self, outcome: Self) -> Self {
        Self(self.0 & !outcome.0)
    }
    fn then(self, next: Self) -> Self {
        if self.has(Self::NORMAL) {
            self.without(Self::NORMAL).union(next)
        } else {
            self
        }
    }
}

struct CompletionContext<'a, 'm> {
    facts: &'a ModuleFacts<'m>,
    fallible: HashSet<String>,
    calls: HashMap<String, Completion>,
}

impl<'a, 'm> CompletionContext<'a, 'm> {
    fn new(module: &UnifiedModule, facts: &'a ModuleFacts<'m>) -> Self {
        let fallible = crate::core_effects::fallible_functions(module.decls.iter().filter_map(
            |decl| match decl {
                Decl::Function {
                    name,
                    body,
                    ret,
                    params,
                    ..
                } => Some((name.as_str(), body.as_slice(), ret, params.as_slice())),
                _ => None,
            },
        ));
        let mut context = Self {
            facts,
            fallible,
            calls: HashMap::new(),
        };
        for declaration in &module.decls {
            if let Decl::Function { name, body, .. } = declaration
                && !body.is_empty()
            {
                context
                    .calls
                    .insert(name.clone(), Completion::NORMAL.union(Completion::THROW));
            }
        }
        // Start with both possible exits. Refine only from actual defined
        // bodies, leaving unknown recursion/extern calls conservative.
        loop {
            let before = context.calls.clone();
            for declaration in &module.decls {
                if let Decl::Function {
                    name, params, body, ..
                } = declaration
                    && !body.is_empty()
                {
                    let flow =
                        completion_body(body, &mut params.iter().cloned().collect(), &context);
                    let mut call = flow
                        .without(Completion::NORMAL)
                        .without(Completion::RETURN)
                        .without(Completion::BREAK)
                        .without(Completion::CONTINUE);
                    if flow.has(Completion::NORMAL)
                        || flow.has(Completion::RETURN)
                        || flow.has(Completion::BREAK)
                        || flow.has(Completion::CONTINUE)
                    {
                        call = call.union(Completion::NORMAL);
                    }
                    // Prologue/ABI/payload checks can fail before a syntactically
                    // returning body. Refine Normal only; retain known fallibility.
                    if context.fallible.contains(name) {
                        call = call.union(Completion::THROW);
                    }
                    context.calls.insert(name.clone(), call);
                }
            }
            if context.calls == before {
                break;
            }
        }
        context
    }

    fn missing_return(&self, decl: &Decl) -> Option<String> {
        let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        else {
            return None;
        };
        self.missing_return_parts(name, params, ret, body)
    }

    fn missing_return_parts(
        &self,
        name: &str,
        params: &[(String, Typ)],
        ret: &Typ,
        body: &[Stmt],
    ) -> Option<String> {
        // [] is the established external-declaration representation.
        if canonical_type(ret) != Typ::Void
            && !body.is_empty()
            && completion_body(body, &mut params.iter().cloned().collect(), self)
                .has(Completion::NORMAL)
        {
            Some(format!(
                "non-void function `{name}` can reach its end without a return value"
            ))
        } else {
            None
        }
    }
}

/// Preserve a proven non-fallthrough boundary before control-flow lowering
/// replaces exhaustive matches with independent linear gates.
pub(crate) fn preserve_nonvoid_completion(decls: &mut [Decl]) {
    let module = UnifiedModule::new(decls.to_vec());
    let facts = module_type_facts(&module);
    let context = CompletionContext::new(&module, &facts);
    fn preserve(decls: &mut [Decl], context: &CompletionContext<'_, '_>) {
        for decl in decls {
            match decl {
                Decl::Function {
                    params, ret, body, ..
                } if canonical_type(ret) != Typ::Void && !body.is_empty() => {
                    let mut env = params.iter().cloned().collect();
                    if !completion_body(body, &mut env, context).has(Completion::NORMAL)
                        && !matches!(body.last(), Some(Stmt::Expr(Expr::Call {callee, args}))
                            if args.is_empty() && matches!(callee.as_ref(), Expr::Ident(name)
                                if name == crate::core_ir::COMPILER_UNREACHABLE))
                    {
                        body.push(Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident(
                                crate::core_ir::COMPILER_UNREACHABLE.into(),
                            )),
                            args: vec![],
                        }));
                    }
                }
                Decl::Class { methods, .. } => preserve(methods, context),
                _ => {}
            }
        }
    }
    preserve(decls, &context);
}

/// Retain a source completion proof only where lowering actually loses it.
/// Declaration order and signatures are unchanged by match preparation.
pub(crate) fn preserve_nonvoid_completion_after(original: &[Decl], lowered: &mut [Decl]) {
    // Unchanged declarations cannot lose a completion proof.
    if original == lowered {
        return;
    }
    let source = UnifiedModule::new(original.to_vec());
    let target = UnifiedModule::new(lowered.to_vec());
    let source_facts = module_type_facts(&source);
    let target_facts = module_type_facts(&target);
    let source_context = CompletionContext::new(&source, &source_facts);
    let target_context = CompletionContext::new(&target, &target_facts);
    fn preserve(
        original: &[Decl],
        lowered: &mut [Decl],
        source: &CompletionContext<'_, '_>,
        target: &CompletionContext<'_, '_>,
    ) {
        for (original, lowered) in original.iter().zip(lowered) {
            match (original, lowered) {
                (
                    Decl::Function {
                        name: old_name,
                        params: old_params,
                        ret: old_ret,
                        body: old_body,
                        ..
                    },
                    Decl::Function {
                        name,
                        params,
                        ret,
                        body,
                        ..
                    },
                ) if old_name.as_str() == name.as_str()
                    && old_params.as_slice() == params.as_slice()
                    && old_ret == &*ret
                    && canonical_type(ret) != Typ::Void
                    && !old_body.is_empty()
                    && old_body.as_slice() != body.as_slice() =>
                {
                    let mut source_env = old_params.iter().cloned().collect();
                    let mut target_env = params.iter().cloned().collect();
                    if !completion_body(old_body, &mut source_env, source).has(Completion::NORMAL)
                        && completion_body(body, &mut target_env, target).has(Completion::NORMAL)
                    {
                        body.push(Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident(
                                crate::core_ir::COMPILER_UNREACHABLE.into(),
                            )),
                            args: vec![],
                        }));
                    }
                }
                (
                    Decl::Class {
                        name: old_name,
                        methods: old_methods,
                        ..
                    },
                    Decl::Class { name, methods, .. },
                ) if old_name.as_str() == name.as_str() => {
                    preserve(old_methods, methods, source, target)
                }
                _ => {}
            }
        }
    }
    preserve(original, lowered, &source_context, &target_context);
}

/// Raw native APIs share the normal-completion invariant with verified callers.
pub(crate) fn check_nonvoid_completion(module: &UnifiedModule) -> Result<(), String> {
    check_nonvoid_completion_for(
        module,
        module.decls.iter().filter_map(|decl| match decl {
            Decl::Function {
                name,
                params,
                ret,
                body,
                ..
            } => Some((name.as_str(), params.as_slice(), ret, body.as_slice())),
            _ => None,
        }),
    )
}

/// Validate the exact definitions retained by a backend after unsupported-body
/// refusal. Omitted definitions must use that backend's existing trap/refusal path.
pub(crate) fn check_nonvoid_completion_for<'a>(
    module: &UnifiedModule,
    functions: impl IntoIterator<Item = (&'a str, &'a [(String, Typ)], &'a Typ, &'a [Stmt])>,
) -> Result<(), String> {
    let facts = module_type_facts(module);
    let completion = CompletionContext::new(module, &facts);
    for (name, params, ret, body) in functions {
        if let Some(reason) = completion.missing_return_parts(name, params, ret, body) {
            return Err(format!("missing-return: {reason}"));
        }
    }
    Ok(())
}

fn completion_body(
    body: &[Stmt],
    env: &mut HashMap<String, Typ>,
    context: &CompletionContext<'_, '_>,
) -> Completion {
    let mut flow = Completion::NORMAL;
    for statement in body {
        if !flow.has(Completion::NORMAL) {
            break;
        }
        let next = completion_stmt(statement, env, context);
        flow = flow.then(next);
    }
    flow
}

// Keep every intermediate representable at both supported word widths.
fn known_integer(value: &Expr) -> Option<i64> {
    let value = match value {
        Expr::IntLit(value) => *value,
        Expr::Unary { op, expr } if op == "-" => known_integer(expr)?.checked_neg()?,
        Expr::Binary { op, lhs, rhs } => {
            let left = known_integer(lhs)?;
            let right = known_integer(rhs)?;
            match op.as_str() {
                "+" | "add" => left.checked_add(right)?,
                "-" | "sub" => left.checked_sub(right)?,
                "*" | "mul" => left.checked_mul(right)?,
                _ => return None,
            }
        }
        _ => return None,
    };
    i32::try_from(value).ok().map(i64::from)
}

fn known_boolean(value: &Expr) -> Option<bool> {
    match value {
        Expr::BoolLit(value) => Some(*value),
        Expr::Unary { op, expr } if op == "!" => known_boolean(expr).map(|value| !value),
        Expr::Binary { op, lhs, rhs } => {
            if let (Some(left), Some(right)) = (known_integer(lhs), known_integer(rhs)) {
                return match op.as_str() {
                    "==" | "eq" => Some(left == right),
                    "!=" | "neq" => Some(left != right),
                    "<" | "lt" => Some(left < right),
                    "<=" | "le" => Some(left <= right),
                    ">" | "gt" => Some(left > right),
                    ">=" | "ge" => Some(left >= right),
                    _ => None,
                };
            }
            let left = known_boolean(lhs)?;
            match op.as_str() {
                "&&" | "and" => Some(left && known_boolean(rhs)?),
                "||" | "or" => Some(left || known_boolean(rhs)?),
                "==" | "eq" => Some(left == known_boolean(rhs)?),
                "!=" | "neq" => Some(left != known_boolean(rhs)?),
                _ => None,
            }
        }
        _ => None,
    }
}

fn completion_branch(
    cond: &Expr,
    then_flow: impl FnOnce() -> Completion,
    else_flow: impl FnOnce() -> Completion,
) -> Completion {
    match known_boolean(cond) {
        Some(true) => then_flow(),
        Some(false) => else_flow(),
        _ => then_flow().union(else_flow()),
    }
}

fn completion_stmt(
    statement: &Stmt,
    env: &mut HashMap<String, Typ>,
    context: &CompletionContext<'_, '_>,
) -> Completion {
    match statement {
        Stmt::Declare(name, typ) => {
            env.insert(name.clone(), typ.clone());
            Completion::NORMAL
        }
        Stmt::Let(name, annotation, value) => {
            let flow = completion_expr(value, env, context);
            if let Some(typ) = annotation
                .clone()
                .or_else(|| expr_type(value, context.facts, env))
            {
                env.insert(name.clone(), typ);
            }
            flow
        }
        Stmt::Assign(_, value) | Stmt::Expr(value) => completion_expr(value, env, context),
        Stmt::FieldAssign { base, value, .. } => completion_place(base, env, context)
            .then(completion_expr(value, env, context))
            .then(Completion::NORMAL.union(Completion::THROW)),
        Stmt::IndexAssign { base, index, value } => completion_expr(base, env, context)
            .then(completion_expr(index, env, context))
            .then(completion_expr(value, env, context))
            .then(Completion::NORMAL.union(Completion::THROW)),
        Stmt::Return(value) => value
            .as_ref()
            .map_or(Completion::NORMAL, |value| {
                completion_expr(value, env, context)
            })
            .then(Completion::RETURN),
        Stmt::Throw(value) => completion_expr(value, env, context).then(Completion::THROW),
        Stmt::Propagate => Completion::NORMAL.union(Completion::THROW),
        Stmt::Break => Completion::BREAK,
        Stmt::Continue => Completion::CONTINUE,
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => completion_expr(cond, env, context).then(completion_branch(
            cond,
            || completion_body(then_body, &mut env.clone(), context),
            || completion_body(else_body, &mut env.clone(), context),
        )),
        Stmt::Loop { kind, cond, body } => {
            // For loops may run zero times. While/Infinite lowering uses the
            // actual optional condition, not the LoopKind label, for its exit.
            let condition = cond.as_ref().map_or(Completion::NORMAL, |value| {
                completion_expr(value, env, context)
            });
            if cond.as_ref().and_then(known_boolean) == Some(false) {
                return condition;
            }
            let body = completion_body(body, &mut env.clone(), context);
            let can_exit = matches!(kind, crate::core_ir::LoopKind::For { .. })
                || cond
                    .as_ref()
                    .is_some_and(|value| known_boolean(value) != Some(true))
                || body.has(Completion::BREAK);
            let iterations = body
                .without(Completion::NORMAL)
                .without(Completion::BREAK)
                .without(Completion::CONTINUE);
            condition.then(if can_exit {
                iterations.union(Completion::NORMAL)
            } else {
                iterations
            })
        }
        Stmt::Match { scrutinee, arms } => {
            completion_expr(scrutinee, env, context).then(completion_match(
                scrutinee,
                arms.iter().map(|arm| {
                    (
                        arm.pattern.as_str(),
                        arm.guard.as_ref(),
                        CompletionArm::Body(&arm.body),
                    )
                }),
                env,
                context,
            ))
        }
        Stmt::Try { body, catches } => {
            let body = completion_body(body, &mut env.clone(), context);
            if !body.has(Completion::THROW) {
                return body;
            }
            let mut flow = body.without(Completion::THROW);
            let mut uncaught = true;
            for catch in catches {
                if !uncaught {
                    break;
                }
                let mut catch_env = env.clone();
                if let Ok(pattern) = crate::core_ir::MatchPattern::parse(&catch.pattern) {
                    if let crate::core_ir::MatchPattern::IdentPat(name) = &pattern {
                        catch_env.insert(name.clone(), Typ::Named("Any".into()));
                    }
                    uncaught = !matches!(
                        pattern,
                        crate::core_ir::MatchPattern::WildPat
                            | crate::core_ir::MatchPattern::IdentPat(_)
                    );
                }
                flow = flow.union(completion_body(&catch.body, &mut catch_env, context));
            }
            if uncaught {
                flow = flow.union(Completion::THROW);
            }
            flow
        }
    }
}

// Field stores stage an addressable place, not a whole record read. In
// particular an indexed receiver's bounds/payload check follows its RHS;
// source RHS-first operations have already staged their value in prefixes.
fn completion_place(
    value: &Expr,
    env: &HashMap<String, Typ>,
    context: &CompletionContext<'_, '_>,
) -> Completion {
    match value {
        Expr::Ident(_) => Completion::NORMAL,
        Expr::Field { base, .. } => completion_place(base, env, context),
        Expr::Index { base, index } => {
            completion_expr(base, env, context).then(completion_expr(index, env, context))
        }
        _ => completion_expr(value, env, context),
    }
}

enum CompletionArm<'a> {
    Body(&'a [Stmt]),
    Value(&'a Expr),
}

fn completion_match<'a>(
    scrutinee: &Expr,
    arms: impl IntoIterator<Item = (&'a str, Option<&'a Expr>, CompletionArm<'a>)>,
    env: &HashMap<String, Typ>,
    context: &CompletionContext<'_, '_>,
) -> Completion {
    completion_match_with_fallthrough(scrutinee, arms, env, context).0
}
fn completion_match_with_fallthrough<'a>(
    scrutinee: &Expr,
    arms: impl IntoIterator<Item = (&'a str, Option<&'a Expr>, CompletionArm<'a>)>,
    env: &HashMap<String, Typ>,
    context: &CompletionContext<'_, '_>,
) -> (Completion, bool) {
    let typ = expr_type(scrutinee, context.facts, env).unwrap_or(Typ::Named("Any".into()));
    let schema = |name: &str| context.facts.structs.get(name).copied();
    let mut flow = Completion::DIVERGE;
    let mut pending = true;
    let mut bools = [false; 2];
    for (source_pattern, guard, body) in arms {
        if !pending {
            break;
        }
        // Shape/type validation already ran in check_stmt/check_expr. A failed
        // lookup here only makes completion conservative, never exhaustive.
        let pattern = crate::core_ir::MatchPattern::parse(source_pattern).ok();
        let mut arm_env = env.clone();
        if let Some(pattern) = &pattern
            && let Ok(bindings) = pattern.typed_bindings(&typ, &schema)
        {
            arm_env.extend(bindings);
        }
        let guard_flow = guard.map_or(Completion::NORMAL, |guard| {
            completion_expr(guard, &arm_env, context)
        });
        let selected = if guard.and_then(known_boolean) == Some(false) {
            Completion::DIVERGE
        } else {
            match body {
                CompletionArm::Body(body) => completion_body(body, &mut arm_env, context),
                CompletionArm::Value(value) => completion_expr(value, &arm_env, context),
            }
        };
        flow = flow.union(guard_flow.without(Completion::NORMAL));
        if guard_flow.has(Completion::NORMAL) {
            flow = flow.union(selected);
        }
        if guard.is_none()
            || guard.and_then(known_boolean) == Some(true)
            || !guard_flow.has(Completion::NORMAL)
        {
            if let Some(pattern) = &pattern {
                if pattern.irrefutable(&typ, &schema)
                    || matches!(pattern, crate::core_ir::MatchPattern::ArrayPat(items)
                        if matches!(typ.canonical(), Typ::Array(_) | Typ::Vector(_))
                            && matches!(items.as_slice(), [crate::core_ir::MatchPattern::RestPat]))
                {
                    pending = false;
                }
                if typ.canonical() == Typ::Bool
                    && let crate::core_ir::MatchPattern::BoolPat(value) = pattern
                {
                    bools[usize::from(*value)] = true;
                    if bools.into_iter().all(|covered| covered) {
                        pending = false;
                    }
                }
            }
        }
    }
    if pending {
        flow = flow.union(Completion::NORMAL);
    }
    (flow, pending)
}

fn completion_expr(
    value: &Expr,
    env: &HashMap<String, Typ>,
    context: &CompletionContext<'_, '_>,
) -> Completion {
    match value {
        Expr::Closure { .. } => Completion::NORMAL,
        Expr::Block { body, result } => {
            let mut local = env.clone();
            completion_body(body, &mut local, context).then(
                result.as_ref().map_or(Completion::NORMAL, |value| {
                    completion_expr(value, &local, context)
                }),
            )
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => completion_expr(cond, env, context).then(completion_branch(
            cond,
            || completion_expr(then_expr, env, context),
            || completion_expr(else_expr, env, context),
        )),
        Expr::MatchValue { scrutinee, arms } => {
            completion_expr(scrutinee, env, context).then(completion_match(
                scrutinee,
                arms.iter().map(|arm| {
                    (
                        arm.pattern.as_str(),
                        arm.guard.as_ref(),
                        CompletionArm::Value(&arm.value),
                    )
                }),
                env,
                context,
            ))
        }
        Expr::Binary { op, lhs, rhs } if matches!(op.trim(), "&&" | "||" | "and" | "or") => {
            let right = || completion_expr(rhs, env, context);
            let skipped = || Completion::NORMAL;
            completion_expr(lhs, env, context).then(if matches!(op.trim(), "&&" | "and") {
                completion_branch(lhs, right, skipped)
            } else {
                completion_branch(lhs, skipped, right)
            })
        }
        _ => {
            let mut flow = Completion::NORMAL;
            crate::core_ir::for_each_expr_child(value, &mut |child| {
                flow = flow.then(completion_expr(child, env, context));
            });
            if matches!(value, Expr::Call { callee, .. }
                if matches!(callee.as_ref(), Expr::Ident(name)
                    if name == crate::core_ir::COMPILER_UNREACHABLE
                        || name == crate::inrt::INRT_TRAP_BUILTIN
                        && !env.contains_key(name)
                        && !context.facts.globals.contains_key(name.as_str())
                        && !context.facts.declared_functions.contains(name.as_str())))
            {
                return flow.then(Completion::DIVERGE);
            }
            if let Expr::Call { callee, .. } = value
                && let Expr::Ident(name) = callee.as_ref()
                && !env.contains_key(name)
                && !context.facts.globals.contains_key(name.as_str())
                && let Some(completion) = context.calls.get(name).or_else(|| {
                    // An exact bodyless declaration still owns this spelling;
                    // it must not inherit another function's no-return proof.
                    (!context.facts.declared_functions.contains(name.as_str()))
                        .then(|| context.calls.get(&name.replace('_', "-")))
                        .flatten()
                })
            {
                return flow.then(*completion);
            }
            let may_throw = match value {
                Expr::Call { callee, .. } => match callee.as_ref() {
                    Expr::Ident(name) => {
                        env.contains_key(name)
                            || context.fallible.contains(name)
                            || context.fallible.contains(&name.replace('_', "-"))
                            || (matches!(name.as_str(), "invoke" | "invoke1" | "invoke2")
                                && !context.facts.declared_functions.contains(name.as_str()))
                    }
                    _ => true,
                },
                // Checked operations may exit through the language error path;
                // retain catch fallthrough even when the success path returns.
                Expr::Index { .. } | Expr::Field { .. } | Expr::StructInit { .. } => true,
                Expr::Binary { op, lhs, rhs } => {
                    crate::core_effects::checked_binary_operation(op)
                        && ![lhs, rhs].into_iter().any(|operand| {
                            expr_type(operand, context.facts, env).map(|typ| typ.canonical())
                                == Some(Typ::Float)
                        })
                }
                _ => false,
            };
            flow.then(if may_throw {
                Completion::NORMAL.union(Completion::THROW)
            } else {
                Completion::NORMAL
            })
        }
    }
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
        Stmt::Declare(name, typ) => {
            env.insert(name.clone(), typ.clone());
            Ok(())
        }
        Stmt::Let(name, typ, expr) => {
            if matches!(typ, Some(Typ::Vector(item)) if matches!(item.as_ref(), Typ::Generic(name) if name == "_"))
            {
                return Err((
                    "unresolved-type".into(),
                    format!("vector element type for `{name}` in `{fn_name}` remains unresolved"),
                ));
            }
            // Check the initializer before the new binding shadows the previous one.
            check_expr_in_loop(fn_name, expr, facts, env, call_edges, loop_depth)?;
            let expr_typ = expr_type(expr, facts, env);
            if let (Some(expected), Some(actual)) = (typ, expr_typ.as_ref())
                && !expected.compatible_with(actual)
                && !expected.accepts_sequence_literal(expr, actual)
                && !crate::core_ir::expr_diverges(expr)
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
            check_expr_in_loop(fn_name, expr, facts, env, call_edges, loop_depth)?;
            if let Some(expr_typ) = expr_type(expr, facts, env) {
                let expected_typ = env
                    .get(name)
                    .or_else(|| facts.globals.get(name.as_str()).map(|global| global.typ))
                    .expect("assignment target was resolved")
                    .clone();
                if !expected_typ.compatible_with(&expr_typ)
                    && !expected_typ.accepts_sequence_literal(expr, &expr_typ)
                {
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
        Stmt::FieldAssign { base, name, value } => {
            let destination = Expr::Field {
                base: Box::new(base.clone()),
                name: name.clone(),
            };
            check_expr_in_loop(fn_name, &destination, facts, env, call_edges, loop_depth)?;
            check_expr_in_loop(fn_name, value, facts, env, call_edges, loop_depth)?;
            if let (Some(expected), Some(actual)) = (
                expr_type(&destination, facts, env),
                expr_type(value, facts, env),
            ) && !expected.compatible_with(&actual)
                && !expected.accepts_sequence_literal(value, &actual)
                && !crate::core_ir::expr_diverges(value)
            {
                return Err((
                    "type-mismatch".into(),
                    format!(
                        "field assignment `{name}` in `{fn_name}` expected {}, got {}",
                        type_name(&expected),
                        type_name(&actual)
                    ),
                ));
            }
            Ok(())
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => {
            if let Some(name) = immutable_index_root(base, facts, env) {
                return Err((
                    "immutable-global".into(),
                    format!("cannot mutate constant global array `{name}` in `{fn_name}`"),
                ));
            }
            check_expr_in_loop(fn_name, base, facts, env, call_edges, loop_depth)?;
            check_expr_in_loop(fn_name, index, facts, env, call_edges, loop_depth)?;
            check_expr_in_loop(fn_name, value, facts, env, call_edges, loop_depth)?;
            require_type(fn_name, "array index", &Typ::Int, index, facts, env)?;
            match expr_type(base, facts, env).map(|typ| typ.canonical()) {
                Some(Typ::Array(item) | Typ::Vector(item)) => {
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
        Stmt::Expr(expr) => check_expr_in_loop(fn_name, expr, facts, env, call_edges, loop_depth),
        Stmt::Break | Stmt::Continue if loop_depth == 0 => Err((
            "invalid-control-flow".into(),
            format!("loop jump outside a loop in `{fn_name}`"),
        )),
        Stmt::Break | Stmt::Continue => Ok(()),
        Stmt::Return(Some(expr)) => {
            check_expr_in_loop(fn_name, expr, facts, env, call_edges, loop_depth)?;
            if canonical_type(ret) == Typ::Void
                && expr_type(expr, facts, env).is_none_or(|typ| canonical_type(&typ) != Typ::Void)
            {
                return Err((
                    "return-type-mismatch".to_string(),
                    format!("return value in void function `{fn_name}`"),
                ));
            }
            if let Some(expr_typ) = expr_type(expr, facts, env)
                && !ret.compatible_with(&expr_typ)
                && !ret.accepts_sequence_literal(expr, &expr_typ)
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
            check_expr_in_loop(fn_name, cond, facts, env, call_edges, loop_depth)?;
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
                check_expr_in_loop(fn_name, cond, facts, env, call_edges, loop_depth)?;
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
            check_expr_in_loop(fn_name, scrutinee, facts, env, call_edges, loop_depth)?;
            for arm in arms {
                let mut arm_env = env.clone();
                let pattern = crate::core_ir::MatchPattern::parse(&arm.pattern)
                    .map_err(|error| ("invalid-match-pattern".into(), error))?;
                let typ = expr_type(scrutinee, facts, env).unwrap_or(Typ::Named("Any".into()));
                let bindings = pattern
                    .typed_bindings(&typ, &|name| facts.structs.get(name).copied())
                    .map_err(|error| ("invalid-match-pattern".into(), error))?;
                arm_env.extend(bindings);
                if let Some(guard) = &arm.guard {
                    check_expr_in_loop(fn_name, guard, facts, &arm_env, call_edges, loop_depth)?;
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
        Stmt::Throw(expr) => check_expr_in_loop(fn_name, expr, facts, env, call_edges, loop_depth),
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
    check_expr_in_loop(fn_name, expr, facts, env, call_edges, 0)
}

fn check_expr_in_loop(
    fn_name: &str,
    expr: &Expr,
    facts: &ModuleFacts<'_>,
    env: &HashMap<String, Typ>,
    call_edges: &mut Vec<(String, String)>,
    loop_depth: usize,
) -> Result<(), (String, String)> {
    check_expr_with_array_borrow(fn_name, expr, facts, env, call_edges, false, loop_depth)
}

fn immutable_index_root<'a>(
    expr: &'a Expr,
    facts: &ModuleFacts<'_>,
    env: &HashMap<String, Typ>,
) -> Option<&'a str> {
    let mut root = expr;
    while let Expr::Index { base, .. } = root {
        root = base;
    }
    let Expr::Ident(name) = root else {
        return None;
    };
    (!env.contains_key(name)
        && facts
            .globals
            .get(name.as_str())
            .is_some_and(|global| !global.mutable))
    .then_some(name.as_str())
}

fn check_expr_with_array_borrow(
    fn_name: &str,
    expr: &Expr,
    facts: &ModuleFacts<'_>,
    env: &HashMap<String, Typ>,
    call_edges: &mut Vec<(String, String)>,
    array_borrow: bool,
    loop_depth: usize,
) -> Result<(), (String, String)> {
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            check_expr_in_loop(fn_name, scrutinee, facts, env, call_edges, loop_depth)?;
            let typ = expr_type(scrutinee, facts, env).ok_or_else(|| {
                (
                    "unresolved-type".into(),
                    "match scrutinee type must resolve".into(),
                )
            })?;
            if !crate::core_ir::match_value_exhaustive(&typ, arms, &|name| {
                facts.structs.get(name).copied()
            }) {
                return Err((
                    "non-exhaustive-match".into(),
                    "match value requires exhaustive unguarded patterns".into(),
                ));
            }
            for arm in arms {
                let mut inner = env.clone();
                let pattern = crate::core_ir::MatchPattern::parse(&arm.pattern)
                    .map_err(|error| ("invalid-match-pattern".into(), error))?;
                let bindings = pattern
                    .typed_bindings(&typ, &|name| facts.structs.get(name).copied())
                    .map_err(|error| ("invalid-match-pattern".into(), error))?;
                inner.extend(bindings);
                if let Some(guard) = &arm.guard {
                    check_expr_in_loop(fn_name, guard, facts, &inner, call_edges, loop_depth)?;
                    require_type(fn_name, "match guard", &Typ::Bool, guard, facts, &inner)?;
                }
                check_expr_in_loop(fn_name, &arm.value, facts, &inner, call_edges, loop_depth)?;
            }
            if expr_type(expr, facts, env).is_none() {
                return Err((
                    "type-mismatch".into(),
                    "match values must have compatible resolved types".into(),
                ));
            }
            Ok(())
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            check_expr_in_loop(fn_name, cond, facts, env, call_edges, loop_depth)?;
            require_type(
                fn_name,
                "conditional condition",
                &Typ::Bool,
                cond,
                facts,
                env,
            )?;
            for branch in [then_expr, else_expr] {
                check_expr_in_loop(fn_name, branch, facts, env, call_edges, loop_depth)?;
            }
            if expr_type(expr, facts, env).is_none() {
                return Err((
                    "type-mismatch".into(),
                    format!(
                        "conditional branches in `{fn_name}` must have compatible resolved types"
                    ),
                ));
            }
            Ok(())
        }
        Expr::Block { body, result } => {
            let mut inner = env.clone();
            let ret = function_sig(facts, fn_name)
                .map(|sig| sig.ret.clone())
                .unwrap_or(Typ::Void);
            check_stmts(
                fn_name, body, facts, &ret, &mut inner, call_edges, loop_depth,
            )?;
            if let Some(result) = result {
                check_expr_in_loop(fn_name, result, facts, &inner, call_edges, loop_depth)?;
            }
            Ok(())
        }
        Expr::UnitLit
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::StringLit(_)
        | Expr::BoolLit(_) => Ok(()),
        Expr::Closure { .. } => Ok(()),
        Expr::Ident(name) => {
            if !array_borrow
                && !env.contains_key(name)
                && facts.globals.get(name.as_str()).is_some_and(|global| {
                    !global.mutable && matches!(global.typ.canonical(), Typ::Array(_))
                })
            {
                return Err((
                    "immutable-global-reference".into(),
                    format!(
                        "constant global array `{name}` cannot escape as a mutable reference in `{fn_name}`"
                    ),
                ));
            }

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
            check_expr_in_loop(fn_name, expr, facts, env, call_edges, loop_depth)?;
            if matches!(
                op.as_str(),
                "-" | crate::core_ir::rust_references::NEGATE_VALUE
            ) && expr_type(expr, facts, env) != Some(Typ::Void)
            {
                let operand = expr_type(expr, facts, env);
                let known_nominal = matches!(&operand, Some(Typ::Named(name)) if facts.structs.contains_key(name.as_str()));
                if crate::core_ir::numeric_negation_domain(operand.as_ref(), known_nominal)
                    == crate::core_ir::NumericNegationDomain::Rejected
                {
                    return Err((
                        "type-mismatch".into(),
                        "numeric negation requires an Int, Bool, or Float operand".into(),
                    ));
                }
            }
            if matches!(
                op.as_str(),
                "-" | crate::core_ir::rust_references::NEGATE_VALUE | "neg" | "!" | "not"
            ) && expr_type(expr, facts, env) == Some(Typ::Void)
            {
                return Err((
                    "type-mismatch".into(),
                    "unit is not a numeric or Bool operand".into(),
                ));
            }
            if op.starts_with("array-copy")
                && !expr_type(expr, facts, env)
                    .is_some_and(|typ| crate::core_ir::array_copy::valid_operand(op, &typ))
            {
                return Err((
                    "type-mismatch".into(),
                    format!(
                        "array copy in `{fn_name}` requires a scalar array and canonical count"
                    ),
                ));
            }
            if op == "vector-clone"
                && !matches!(expr_type(expr, facts, env), Some(Typ::Vector(item)) if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool))
            {
                return Err((
                    "type-mismatch".into(),
                    format!("vector clone in `{fn_name}` requires scalar elements"),
                ));
            }
            if op == "complement" {
                return Err((
                    "unsupported-complement".into(),
                    format!(
                        "complement operand in `{fn_name}` must have a resolved Int or Bool type"
                    ),
                ));
            }
            if op.starts_with("numeric-cast-") {
                return Err((
                    "unresolved-numeric-cast".into(),
                    format!("numeric cast `{op}` in `{fn_name}` needs a supported source type"),
                ));
            }
            if op.starts_with("float-saturate-")
                && crate::core_ir::saturating_float_cast_spec(op).is_none()
            {
                return Err((
                    "unsupported-floating-cast".into(),
                    format!("unsupported saturating cast `{op}` in `{fn_name}`"),
                ));
            }
            if op.starts_with("int-cast-") && crate::core_ir::integer_cast_spec(op).is_none() {
                return Err((
                    "unsupported-integer-cast".into(),
                    format!("unsupported integer cast `{op}` in `{fn_name}`"),
                ));
            }
            if crate::core_ir::integer_cast_spec(op).is_some()
                && expr_type(expr, facts, env) != Some(Typ::Bool)
            {
                require_type(fn_name, "integer cast", &Typ::Int, expr, facts, env)?;
            }
            if crate::core_ir::float_to_bits_width(op).is_some() {
                require_type(
                    fn_name,
                    "float bit extraction",
                    &Typ::Float,
                    expr,
                    facts,
                    env,
                )?;
            }
            if crate::core_ir::bits_to_float_width(op).is_some() {
                require_type(
                    fn_name,
                    "float bit construction",
                    &Typ::Int,
                    expr,
                    facts,
                    env,
                )?;
            }
            if (op.starts_with("float-to-bits-")
                && crate::core_ir::float_to_bits_width(op).is_none())
                || (op.starts_with("bits-to-float-")
                    && crate::core_ir::bits_to_float_width(op).is_none())
            {
                return Err((
                    "unsupported-floating-cast".into(),
                    format!("unsupported float bit operation `{op}` in `{fn_name}`"),
                ));
            }
            if op.starts_with("float-cast-") {
                if !matches!(op.as_str(), "float-cast-f32" | "float-cast-f64") {
                    return Err((
                        "unsupported-floating-cast".into(),
                        format!("unsupported floating cast `{op}` in `{fn_name}`"),
                    ));
                }
                if expr_type(expr, facts, env) != Some(Typ::Float) {
                    return Err((
                        "type-mismatch".into(),
                        format!("floating width cast in `{fn_name}` needs canonical Float"),
                    ));
                }
            }
            if (crate::core_ir::integer_float_cast_spec(op).is_some() || op == "float-to-int")
                || crate::core_ir::saturating_float_cast_spec(op).is_some()
            {
                let expected = if crate::core_ir::integer_float_cast_spec(op).is_some() {
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
            check_expr_in_loop(fn_name, lhs, facts, env, call_edges, loop_depth)?;
            check_expr_in_loop(fn_name, rhs, facts, env, call_edges, loop_depth)?;
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
                "-" | "*" | "/" => check_numeric_binop_operands(fn_name, lhs, rhs, facts, env),
                "&" | "|" | "^" | "xor" => {
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
                "<<" | ">>" | "%" | "udiv" | "urem" | "lshr" => {
                    require_type(fn_name, "binary operand", &Typ::Int, lhs, facts, env)?;
                    require_type(fn_name, "binary operand", &Typ::Int, rhs, facts, env)
                }
                "==" | "!=" | "<" | ">" | "<=" | ">=" => {
                    let left = expr_type(lhs, facts, env);
                    let right = expr_type(rhs, facts, env);
                    if let Some(mismatch) = crate::core_ir::comparison_operand_mismatch(
                        op,
                        left.as_ref(),
                        right.as_ref(),
                        crate::core_ir::expr_diverges(lhs),
                        crate::core_ir::expr_diverges(rhs),
                    ) {
                        let message = if mismatch.side.is_none() {
                            format!(
                                "type mismatch for binary `{op}` in `{fn_name}`: left {}, right {}",
                                type_name(&mismatch.expected),
                                type_name(&mismatch.actual)
                            )
                        } else {
                            format!(
                                "binary operand in `{fn_name}` expected {}, got {}",
                                type_name(&mismatch.expected),
                                type_name(&mismatch.actual)
                            )
                        };
                        return Err(("type-mismatch".into(), message));
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
            check_struct_init(fn_name, name, fields, facts, env, call_edges, loop_depth)
        }
        Expr::Field { base, name, .. } => {
            check_expr_in_loop(fn_name, base, facts, env, call_edges, loop_depth)?;
            if let Some(typ) = expr_type(base, facts, env)
                && matches!(
                    typ.canonical(),
                    Typ::Int | Typ::Float | Typ::Bool | Typ::String | Typ::Void | Typ::Array(_)
                )
            {
                return Err((
                    "invalid-field-base".into(),
                    format!(
                        "field `{name}` in `{fn_name}` requires an object, got {}",
                        type_name(&typ)
                    ),
                ));
            }
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
                check_expr_in_loop(fn_name, item, facts, env, call_edges, loop_depth)?;
                if crate::core_ir::empty_sequence_literal(item) {
                    continue;
                }
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
            if let Some(expected) = expected {
                for item in items
                    .iter()
                    .filter(|item| crate::core_ir::empty_sequence_literal(item))
                {
                    let actual = expr_type(item, facts, env).unwrap_or(Typ::Void);
                    if !expected.accepts_sequence_literal(item, &actual) {
                        return Err((
                            "array-element-type-mismatch".into(),
                            format!(
                                "empty array literal in `{fn_name}` does not match {}",
                                type_name(&expected)
                            ),
                        ));
                    }
                }
            }
            Ok(())
        }
        Expr::Index { base, index, .. } => {
            if !array_borrow
                && matches!(
                    expr_type(expr, facts, env).map(|typ| typ.canonical()),
                    Some(Typ::Array(_) | Typ::Vector(_))
                )
                && let Some(name) = immutable_index_root(expr, facts, env)
            {
                return Err((
                    "immutable-global-reference".into(),
                    format!(
                        "constant global array `{name}` row cannot escape as a mutable reference in `{fn_name}`"
                    ),
                ));
            }

            check_expr_with_array_borrow(fn_name, base, facts, env, call_edges, true, loop_depth)?;
            check_expr_in_loop(fn_name, index, facts, env, call_edges, loop_depth)?;
            require_type(fn_name, "array index", &Typ::Int, index, facts, env)?;
            if let Some(base_typ) = expr_type(base, facts, env).map(|typ| typ.canonical())
                && !matches!(base_typ, Typ::Array(_) | Typ::Vector(_))
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
                if env.contains_key(name) && invoke_param_types(name).is_some() {
                    return Err((
                        "unresolved-symbol".into(),
                        format!("call target `{name}` is shadowed by a value"),
                    ));
                }
                let Some(sig) = function_sig(facts, name) else {
                    return Err((
                        "unresolved-symbol".to_string(),
                        format!("unresolved function call `{name}` in `{fn_name}`"),
                    ));
                };
                call_edges.push((fn_name.to_string(), name.clone()));
                if !facts.declared_functions.contains(name.as_str())
                    && is_native_invoke(name)
                    && matches!(args.first(), Some(Expr::IntLit(0)))
                {
                    return Err((
                        "invalid-native-address".into(),
                        "raw native call target must be nonzero".into(),
                    ));
                }
                if !facts.declared_functions.contains(name.as_str())
                    && !env.contains_key(name)
                    && invoke_param_types(name).is_some()
                    && let Some(Expr::Ident(target)) = args.first()
                    && !env.contains_key(target)
                    && let Some(target_sig) = function_sig(facts, target)
                    && let Some(error) =
                        invoke_signature_error(name, target_sig.params, target_sig.ret)
                {
                    return Err(("type-mismatch".into(), error));
                }
                // Scalar runtime intrinsics require their declared argument types.
                if !is_intrinsic(name) || facts.declared_functions.contains(name.as_str()) {
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
                        check_expr_in_loop(fn_name, arg, facts, env, call_edges, loop_depth)?;
                        if let Some(arg_typ) = expr_type(arg, facts, env)
                            && !param_typ.compatible_with(&arg_typ)
                            && !param_typ.accepts_sequence_literal(arg, &arg_typ)
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
                    for (index, arg) in args.iter().enumerate() {
                        check_expr_with_array_borrow(
                            fn_name,
                            arg,
                            facts,
                            env,
                            call_edges,
                            index == 0 && matches!(name.as_str(), "array-len" | "array_len"),
                            loop_depth,
                        )?;
                    }
                    if matches!(name.as_str(), "vector-push" | "vector-extend") {
                        if args.len() != 2 {
                            return Err((
                                "call-arity-mismatch".into(),
                                "vector push expects two arguments".into(),
                            ));
                        }
                        let Some(Typ::Vector(item)) = expr_type(&args[0], facts, env) else {
                            return Err((
                                "type-mismatch".into(),
                                "vector push requires vector storage".into(),
                            ));
                        };
                        if !matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool)
                            || !expr_type(&args[1], facts, env).is_some_and(|actual| {
                                if name == "vector-extend" {
                                    Typ::Vector(item.clone())
                                        .accepts_vector_source(&args[1], &actual)
                                } else {
                                    actual.canonical() == item.canonical()
                                }
                            })
                        {
                            return Err((
                                "type-mismatch".into(),
                                "vector push requires matching scalar elements".into(),
                            ));
                        }
                    }
                    if name == "vector-clear" {
                        if args.len() != 1 {
                            return Err((
                                "call-arity-mismatch".into(),
                                "vector clear expects one argument".into(),
                            ));
                        }
                        if !matches!(expr_type(&args[0], facts, env), Some(Typ::Vector(item))
                            if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool))
                        {
                            return Err((
                                "type-mismatch".into(),
                                "vector clear requires scalar vector storage".into(),
                            ));
                        }
                    }
                    if matches!(name.as_str(), "array-len" | "array_len" | "vector-capacity") {
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
                        ) || (name == "vector-capacity"
                            && !matches!(actual, Some(Typ::Vector(_))))
                        {
                            return Err((
                                "type-mismatch".into(),
                                format!(
                                    "argument 1 for `{name}` in `{fn_name}` expected {}, got {}",
                                    if name == "vector-capacity" {
                                        "vector"
                                    } else {
                                        "array"
                                    },
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
                check_expr_in_loop(fn_name, callee, facts, env, call_edges, loop_depth)?;
            }

            for arg in args {
                check_expr_in_loop(fn_name, arg, facts, env, call_edges, loop_depth)?;
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
    loop_depth: usize,
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
        check_expr_in_loop(fn_name, expr, facts, env, call_edges, loop_depth)?;
        if let Some((_, expected)) = schema
            .iter()
            .find(|(schema_field, _)| schema_field == field)
            && let Some(actual) = expr_type(expr, facts, env)
            && !expected.compatible_with(&actual)
            && !expected.accepts_sequence_literal(expr, &actual)
            && !crate::core_ir::expr_diverges(expr)
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
        Expr::MatchValue { scrutinee, arms } => {
            let typ = expr_type(scrutinee, facts, env)?;
            crate::core_ir::match_value_type(
                arms,
                arms.iter().map(|arm| {
                    let pattern = crate::core_ir::MatchPattern::parse(&arm.pattern).ok()?;
                    let bindings = pattern
                        .typed_bindings(&typ, &|name| facts.structs.get(name).copied())
                        .ok()?;
                    let mut inner = env.clone();
                    inner.extend(bindings);
                    expr_type(&arm.value, facts, &inner)
                }),
            )
        }
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => crate::core_ir::conditional_type(
            then_expr,
            expr_type(then_expr, facts, env),
            else_expr,
            expr_type(else_expr, facts, env),
        ),
        Expr::Block { body, result } => {
            let mut inner = env.clone();
            for stmt in body {
                match stmt {
                    Stmt::Declare(name, typ) => {
                        inner.insert(name.clone(), typ.clone());
                    }
                    Stmt::Let(name, typ, value) => {
                        if let Some(typ) = typ.clone().or_else(|| expr_type(value, facts, &inner)) {
                            inner.insert(name.clone(), typ);
                        } else {
                            inner.remove(name);
                        }
                    }
                    _ => {}
                }
            }
            result
                .as_ref()
                .map(|value| expr_type(value, facts, &inner))
                .unwrap_or(Some(Typ::Void))
        }
        Expr::UnitLit => Some(Typ::Void),
        Expr::IntLit(_) => Some(Typ::Int),
        Expr::FloatLit(_) => Some(Typ::Float),
        Expr::StringLit(_) => Some(Typ::String),
        Expr::BoolLit(_) => Some(Typ::Bool),
        Expr::Ident(name) => env
            .get(name)
            .cloned()
            .or_else(|| function_sig(facts, name).map(|_| Typ::Int))
            .or_else(|| {
                facts
                    .globals
                    .get(name.as_str())
                    .map(|global| global.typ.clone())
            }),
        Expr::StructInit { name, .. } => Some(Typ::Named(name.clone())),
        Expr::Field { base, name, .. } => {
            if let Some(Typ::Named(struct_name)) = expr_type(base, facts, env)
                .map(|typ| crate::core_ir::rust_references::dereferenced(&typ))
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
                .filter(|item| !crate::core_ir::empty_sequence_literal(item))
                .find_map(|item| expr_type(item, facts, env))
                .unwrap_or(Typ::Void);
            Some(Typ::Array(Box::new(item_typ)))
        }
        Expr::Index { base, .. } => {
            if let Some(Typ::Array(item) | Typ::Vector(item)) =
                expr_type(base, facts, env).map(|typ| typ.canonical())
            {
                Some(*item)
            } else if expr_type(base, facts, env) == Some(Typ::Int) {
                // Indexing an Int pointer returns Int
                Some(Typ::Int)
            } else {
                None
            }
        }
        Expr::Unary { op, expr, .. }
            if crate::core_ir::rust_references::value_kind(op).is_some() =>
        {
            crate::core_ir::rust_references::value_type(op, expr_type(expr, facts, env)?)
        }
        Expr::Unary { op, expr, .. } => match op.as_str() {
            "!" => Some(Typ::Bool),
            "bool-to-int" | "float-to-int" => Some(Typ::Int),
            op if crate::core_ir::integer_float_cast_spec(op).is_some()
                || crate::core_ir::bits_to_float_width(op).is_some() =>
            {
                Some(Typ::Float)
            }
            op if crate::core_ir::float_to_bits_width(op).is_some() => Some(Typ::Int),
            "float-cast-f32" | "float-cast-f64" => Some(Typ::Float),
            op if crate::core_ir::integer_cast_spec(op).is_some()
                || crate::core_ir::saturating_float_cast_spec(op).is_some() =>
            {
                Some(Typ::Int)
            }
            "-" | crate::core_ir::rust_references::NEGATE_VALUE => {
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
                "&" | "|" | "^" | "xor" => crate::core_ir::bitwise_result_type(lhs_typ, rhs_typ),
                "+" | "-" | "*" | "/" | "%" | "<<" | ">>" | "udiv" | "urem" | "lshr" => {
                    Some(Typ::Int)
                }
                "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => Some(Typ::Bool),
                _ => None,
            }
        }
        Expr::Call { callee, args } => {
            if let Expr::Ident(name) = callee.as_ref()
                && name == crate::core_ir::BORROW
                && let [value] = args.as_slice()
            {
                return expr_type(value, facts, env);
            }
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
        && !expected.accepts_sequence_literal(expr, &actual)
        && !crate::core_ir::expr_diverges(expr)
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
    #[test]
    fn private_unreachable_is_a_reserved_zero_argument_terminator() {
        let module = crate::in_lang_parse::parse_in_source(
            "fn __inrt_unsupported_trap()->Void{return;}fn main()->Int{#unreachable();}",
        )
        .unwrap();
        assert!(verify_for_entry(&module, "main").ok);
        for source in [
            "fn main()->Int{#unreachable(42);}",
            "fn #unreachable()->Void{return;}fn main()->Int{return 42;}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            assert!(!verify_for_entry(&module, "main").ok, "{source}");
        }
    }

    #[test]
    fn nonvoid_definitions_require_no_normal_completion_but_externs_and_void_do_not() {
        for ret in [
            Typ::Int,
            Typ::Float,
            Typ::Bool,
            Typ::String,
            Typ::Array(Box::new(Typ::Int)),
        ] {
            let report = verify_for_entry(
                &module(vec![function_with_ret(
                    "main",
                    ret.clone(),
                    vec![Stmt::Expr(Expr::IntLit(42))],
                )]),
                "main",
            );
            assert_eq!(
                report.reason_code.as_deref(),
                Some("missing-return"),
                "{ret:?}"
            );
            assert!(
                verify_for_entry(
                    &module(vec![function_with_ret("main", ret, vec![])]),
                    "main"
                )
                .ok
            );
        }
        assert!(
            verify_for_entry(
                &module(vec![function("main", vec![Stmt::Expr(Expr::IntLit(42))])]),
                "main"
            )
            .ok
        );
        let returned = Stmt::Return(Some(Expr::IntLit(42)));
        let hidden = Expr::Closure {
            params: vec![],
            ret: Typ::Int,
            body: vec![returned],
            captures: vec![],
        };
        let report = verify_for_entry(
            &module(vec![function_with_ret(
                "main",
                Typ::Int,
                vec![Stmt::Let("hidden".into(), None, hidden)],
            )]),
            "main",
        );
        assert_eq!(report.reason_code.as_deref(), Some("missing-return"));
    }

    #[test]
    fn in_front_preserves_empty_definition_vs_bodyless_external_before_completion() {
        let empty = crate::in_lang_parse::parse_in_source("fn main()->Int{}").unwrap();
        let report = verify_for_entry(&empty, "main");
        assert_eq!(report.reason_code.as_deref(), Some("return-type-mismatch"));
        let external = crate::in_lang_parse::parse_in_source(
            "extern c fn external()->Int;\nfn main()->Int{return external();}",
        )
        .unwrap();
        assert!(verify_for_entry(&external, "main").ok);
    }

    #[test]
    fn completion_tracks_branches_and_loop_scopes_instead_of_any_return() {
        let returned = || Stmt::Return(Some(Expr::IntLit(42)));
        let flag = || Expr::Ident("flag".into());
        let looped = |cond, body| Stmt::Loop {
            kind: crate::core_ir::LoopKind::While,
            cond,
            body,
        };
        let cases = [
            (
                vec![Stmt::If {
                    cond: flag(),
                    then_body: vec![returned()],
                    else_body: vec![],
                }],
                false,
            ),
            (
                vec![Stmt::If {
                    cond: flag(),
                    then_body: vec![returned()],
                    else_body: vec![returned()],
                }],
                true,
            ),
            (
                vec![Stmt::If {
                    cond: Expr::BoolLit(true),
                    then_body: vec![returned()],
                    else_body: vec![],
                }],
                true,
            ),
            (vec![looped(Some(Expr::BoolLit(true)), vec![])], true),
            (vec![looped(None, vec![Stmt::Continue])], true),
            (
                vec![looped(Some(Expr::BoolLit(true)), vec![Stmt::Break])],
                false,
            ),
            (vec![looped(Some(flag()), vec![returned()])], false),
            (
                vec![looped(Some(Expr::BoolLit(false)), vec![returned()])],
                false,
            ),
            (
                vec![looped(
                    None,
                    vec![Stmt::If {
                        cond: flag(),
                        then_body: vec![returned()],
                        else_body: vec![Stmt::Continue],
                    }],
                )],
                true,
            ),
            // The inner break exits only its own loop; the outer one still diverges.
            (
                vec![looped(None, vec![looped(None, vec![Stmt::Break])])],
                true,
            ),
            (
                vec![looped(
                    None,
                    vec![Stmt::If {
                        cond: Expr::BoolLit(false),
                        then_body: vec![Stmt::Break],
                        else_body: vec![],
                    }],
                )],
                true,
            ),
            (
                vec![looped(Some(flag()), vec![Stmt::Break]), returned()],
                true,
            ),
        ];
        for (body, expected) in cases {
            let report = verify_for_entry(
                &module(vec![function_with_params(
                    "main",
                    vec![("flag".into(), Typ::Bool)],
                    Typ::Int,
                    body.clone(),
                )]),
                "main",
            );
            assert_eq!(report.ok, expected, "{body:?}: {report:?}");
            if !expected {
                assert_eq!(report.reason_code.as_deref(), Some("missing-return"));
            }
        }
    }

    #[test]
    fn completion_tracks_ordered_exhaustive_match_guards_and_expression_exits() {
        use crate::core_ir::{MatchArm, MatchValueArm};
        let arm = |pattern: &str, guard, body| MatchArm {
            pattern: pattern.into(),
            guard,
            body,
        };
        let returned = || Stmt::Return(Some(Expr::IntLit(42)));
        let matched = |arms| Stmt::Match {
            scrutinee: Expr::Ident("flag".into()),
            arms,
        };
        for (body, expected) in [
            (
                vec![matched(vec![
                    arm("true", None, vec![returned()]),
                    arm("false", None, vec![returned()]),
                ])],
                true,
            ),
            (
                vec![matched(vec![arm("true", None, vec![returned()])])],
                false,
            ),
            (
                vec![matched(vec![arm(
                    "_",
                    Some(Expr::Block {
                        body: vec![returned()],
                        result: Some(Box::new(Expr::BoolLit(false))),
                    }),
                    vec![],
                )])],
                true,
            ),
            (
                vec![matched(vec![arm(
                    "_",
                    Some(Expr::Ident("flag".into())),
                    vec![returned()],
                )])],
                false,
            ),
            (
                vec![matched(vec![
                    arm("_", None, vec![returned()]),
                    arm("_", None, vec![]),
                ])],
                true,
            ),
            (
                vec![Stmt::Expr(Expr::Block {
                    body: vec![returned()],
                    result: None,
                })],
                true,
            ),
            (
                vec![Stmt::Expr(Expr::Conditional {
                    cond: Box::new(Expr::Ident("flag".into())),
                    then_expr: Box::new(Expr::Block {
                        body: vec![returned()],
                        result: None,
                    }),
                    else_expr: Box::new(Expr::Block {
                        body: vec![returned()],
                        result: None,
                    }),
                })],
                true,
            ),
        ] {
            let report = verify_for_entry(
                &module(vec![function_with_params(
                    "main",
                    vec![("flag".into(), Typ::Bool)],
                    Typ::Int,
                    body.clone(),
                )]),
                "main",
            );
            assert_eq!(report.ok, expected, "{body:?}: {report:?}");
        }
        let value = Expr::MatchValue {
            scrutinee: Box::new(Expr::BoolLit(true)),
            arms: vec![MatchValueArm {
                pattern: "_".into(),
                guard: None,
                value: Expr::Block {
                    body: vec![returned()],
                    result: None,
                },
            }],
        };
        let report = verify_for_entry(
            &module(vec![function_with_ret(
                "main",
                Typ::Int,
                vec![Stmt::Expr(value)],
            )]),
            "main",
        );
        assert!(report.ok, "{report:?}");
    }

    #[test]
    fn completion_respects_short_circuit_reachability_and_still_checks_dead_code() {
        for (op, lhs, expected) in [
            ("&&", true, true),
            ("&&", false, false),
            ("and", true, true),
            ("and", false, false),
            (" && ", false, false),
            (" and ", false, false),
            ("||", true, false),
            ("||", false, true),
            ("or", true, false),
            ("or", false, true),
            (" || ", true, false),
            (" or ", true, false),
        ] {
            let expression = Expr::Binary {
                op: op.into(),
                lhs: Box::new(Expr::BoolLit(lhs)),
                rhs: Box::new(Expr::Block {
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    result: Some(Box::new(Expr::BoolLit(true))),
                }),
            };
            let report = verify_for_entry(
                &module(vec![function_with_ret(
                    "main",
                    Typ::Int,
                    vec![Stmt::Expr(expression)],
                )]),
                "main",
            );
            assert_eq!(report.ok, expected, "{report:?}");
        }
        let report = verify_for_entry(
            &module(vec![function_with_ret(
                "main",
                Typ::Int,
                vec![Stmt::Return(Some(Expr::IntLit(42))), Stmt::Return(None)],
            )]),
            "main",
        );
        assert_eq!(report.reason_code.as_deref(), Some("return-type-mismatch"));
    }

    #[test]
    fn completion_evaluates_loop_header_before_body_and_store_checks_after_rhs() {
        use crate::core_ir::{CatchArm, LoopKind};
        for kind in [
            LoopKind::While,
            LoopKind::Infinite,
            LoopKind::For {
                binding: "item".into(),
            },
        ] {
            let statement = Stmt::Try {
                body: vec![Stmt::Loop {
                    kind,
                    cond: Some(Expr::Block {
                        body: vec![Stmt::Throw(Expr::IntLit(7))],
                        result: Some(Box::new(Expr::BoolLit(true))),
                    }),
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                }],
                catches: vec![CatchArm {
                    pattern: "_".into(),
                    body: vec![],
                }],
            };
            let report = verify_for_entry(
                &module(vec![function_with_ret("main", Typ::Int, vec![statement])]),
                "main",
            );
            assert_eq!(
                report.reason_code.as_deref(),
                Some("missing-return"),
                "{report:?}"
            );
        }
        // Raw indexed stores evaluate their base/index, then RHS, and only then
        // check the destination. An enclosing return in RHS skips that check.
        let rows = Expr::Ident("rows".into());
        let index = Expr::Index {
            base: Box::new(rows),
            index: Box::new(Expr::IntLit(0)),
        };
        let statement = Stmt::Try {
            body: vec![Stmt::FieldAssign {
                base: index,
                name: "x".into(),
                value: Expr::Block {
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    result: Some(Box::new(Expr::IntLit(0))),
                },
            }],
            catches: vec![CatchArm {
                pattern: "_".into(),
                body: vec![],
            }],
        };
        let report = verify_for_entry(
            &module(vec![
                point_struct(),
                function_with_params(
                    "main",
                    vec![(
                        "rows".into(),
                        Typ::Array(Box::new(Typ::Named("Point".into()))),
                    )],
                    Typ::Int,
                    vec![statement],
                ),
            ]),
            "main",
        );
        assert!(report.ok, "{report:?}");
    }

    #[test]
    fn completion_never_proves_builtin_trap_through_value_shadowing() {
        let name = crate::inrt::INRT_TRAP_BUILTIN;
        let call = || {
            Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(name.into())),
                args: vec![Expr::IntLit(0), Expr::IntLit(0)],
            })
        };
        let report = verify_for_entry(
            &module(vec![function_with_ret("main", Typ::Int, vec![call()])]),
            "main",
        );
        assert!(report.ok, "{report:?}");
        let report = verify_for_entry(
            &module(vec![function_with_ret(
                "main",
                Typ::Int,
                vec![
                    Stmt::Let(name.into(), Some(Typ::Int), Expr::IntLit(0)),
                    call(),
                ],
            )]),
            "main",
        );
        assert_eq!(report.reason_code.as_deref(), Some("missing-return"));
        let global = Decl::Global {
            name: name.into(),
            typ: Typ::Int,
            init: Some(Box::new(Expr::IntLit(0))),
            mutable: true,
            constant_value: false,
        };
        let report = verify_for_entry(
            &module(vec![
                global,
                function_with_ret("main", Typ::Int, vec![call()]),
            ]),
            "main",
        );
        assert_eq!(report.reason_code.as_deref(), Some("missing-return"));
    }

    #[test]
    fn completion_checked_division_aliases_cannot_hide_catch_fallthrough() {
        use crate::core_ir::CatchArm;
        for op in [
            "/", "%", "/=", "%=", "udiv", "urem", " / ", " /= ", " %= ", " udiv ",
        ] {
            let statement = Stmt::Try {
                body: vec![Stmt::Return(Some(Expr::Binary {
                    op: op.into(),
                    lhs: Box::new(Expr::IntLit(1)),
                    rhs: Box::new(Expr::IntLit(0)),
                }))],
                catches: vec![CatchArm {
                    pattern: "_".into(),
                    body: vec![],
                }],
            };
            let report = verify_for_entry(
                &module(vec![function_with_ret("main", Typ::Int, vec![statement])]),
                "main",
            );
            assert_eq!(
                report.reason_code.as_deref(),
                Some("missing-return"),
                "{op}: {report:?}"
            );
        }
        let statement = Stmt::Try {
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: "/".into(),
                lhs: Box::new(Expr::FloatLit(crate::core_ir::FloatVal(1.0))),
                rhs: Box::new(Expr::FloatLit(crate::core_ir::FloatVal(0.0))),
            }))],
            catches: vec![CatchArm {
                pattern: "_".into(),
                body: vec![],
            }],
        };
        let report = verify_for_entry(
            &module(vec![function_with_ret("main", Typ::Float, vec![statement])]),
            "main",
        );
        assert!(report.ok, "{report:?}");
    }

    #[test]
    fn completion_distinguishes_caught_failure_from_return_and_nothrow_calls() {
        use crate::core_ir::CatchArm;
        let returned = || Stmt::Return(Some(Expr::IntLit(42)));
        let caught = |body, catch_body| Stmt::Try {
            body,
            catches: vec![CatchArm {
                pattern: "_".into(),
                body: catch_body,
            }],
        };
        let call = |name: &str| Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args: vec![],
        };
        let helpers = || {
            vec![
                function_with_ret("safe", Typ::Int, vec![returned()]),
                function_with_ret("fallible", Typ::Int, vec![Stmt::Throw(Expr::IntLit(7))]),
            ]
        };
        for (body, expected) in [
            (
                vec![caught(vec![Stmt::Throw(Expr::IntLit(7))], vec![returned()])],
                true,
            ),
            (
                vec![caught(vec![Stmt::Throw(Expr::IntLit(7))], vec![])],
                false,
            ),
            (vec![caught(vec![returned()], vec![])], true),
            (
                vec![caught(vec![Stmt::Return(Some(call("safe")))], vec![])],
                true,
            ),
            (
                vec![caught(vec![Stmt::Return(Some(call("fallible")))], vec![])],
                false,
            ),
            (vec![Stmt::Throw(Expr::IntLit(7))], true),
            (vec![Stmt::Propagate], false),
        ] {
            let mut decls = helpers();
            decls.push(function_with_ret("main", Typ::Int, body.clone()));
            let report = verify_for_entry(&module(decls), "main");
            assert_eq!(report.ok, expected, "{body:?}: {report:?}");
        }
    }

    #[test]
    fn private_print_identity_keeps_source_print_override_and_exact_signature() {
        use crate::core_ir::{COMPILER_PRINT, COMPILER_PRINT_INT};
        let source_print = function_with_params(
            "print",
            vec![("value".into(), Typ::String)],
            Typ::Int,
            vec![Stmt::Return(Some(Expr::IntLit(42)))],
        );
        let call = |name: &str, args| Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args,
        };
        let main = function(
            "main",
            vec![
                Stmt::Expr(call(COMPILER_PRINT, vec![Expr::StringLit("hi".into())])),
                Stmt::Expr(call(COMPILER_PRINT_INT, vec![Expr::IntLit(42)])),
                Stmt::Return(None),
            ],
        );
        assert!(verify_for_entry(&module(vec![source_print, main]), "main").ok);
        for (name, args) in [
            (COMPILER_PRINT, vec![Expr::IntLit(42)]),
            (COMPILER_PRINT_INT, vec![Expr::StringLit("hi".into())]),
            (COMPILER_PRINT, vec![]),
        ] {
            let main = function(
                "main",
                vec![Stmt::Expr(call(name, args)), Stmt::Return(None)],
            );
            assert!(!verify_for_entry(&module(vec![main]), "main").ok);
        }
        for name in [COMPILER_PRINT, COMPILER_PRINT_INT] {
            let report = verify_for_entry(
                &module(vec![
                    function(name, vec![Stmt::Return(None)]),
                    function("main", vec![Stmt::Return(None)]),
                ]),
                "main",
            );
            assert_eq!(
                report.reason_code.as_deref(),
                Some("reserved-intrinsic-name")
            );
        }
    }

    #[test]
    fn completion_preflight_preserves_compile_only_type_value_namespaces() {
        let complete = Decl::Function {
            name: "Result".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            type_params: vec![],
        };
        let schema = Decl::Struct {
            name: "Result".into(),
            fields: vec![],
            type_params: vec![],
        };
        let mut value = UnifiedModule::new(vec![schema, complete]);
        assert!(check_nonvoid_completion(&value).is_ok());
        let report = verify_module(
            &value,
            &VerifyOptions {
                entry: None,
                require_entry: false,
            },
        );
        assert_eq!(
            report.reason_code.as_deref(),
            Some("duplicate-top-level-name")
        );
        let Decl::Function { body, .. } = &mut value.decls[1] else {
            unreachable!()
        };
        *body = vec![Stmt::Expr(Expr::IntLit(7))];
        assert!(
            check_nonvoid_completion(&value)
                .unwrap_err()
                .starts_with("missing-return:")
        );
    }
    #[test]
    #[ignore = "manual timing for unchanged completion-proof preparation"]
    fn unchanged_completion_proof_timing() {
        let original = module(
            (0..1000)
                .map(|index| {
                    function_with_ret(
                        &format!("benchmark-{index}"),
                        Typ::Int,
                        vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    )
                })
                .collect(),
        );
        let mut lowered = original.clone();
        let start = std::time::Instant::now();
        for _ in 0..30 {
            preserve_nonvoid_completion_after(
                std::hint::black_box(&original.decls),
                std::hint::black_box(&mut lowered.decls),
            );
        }
        eprintln!("unchanged-proof-1000x30-us={}", start.elapsed().as_micros());
        assert_eq!(original, lowered);
    }

    #[test]
    fn always_throwing_defined_calls_do_not_invent_successful_fallthrough() {
        for source in [
            "fn inner()->Void{throw \"nope\";}fn main()->String{try{inner();}catch(e){return \"caught: nope\";}}",
            "fn relay()->Void{inner();}fn inner()->Void{throw 7;}fn main()->Int{try{relay();}catch(e){return 42;}}",
            "fn main()->Int{try{relay();}catch(e){return 42;}}fn relay()->Void{inner();}fn inner()->Void{throw 7;}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            let report = verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}");
            assert!(check_nonvoid_completion(&module).is_ok());
        }
    }
    #[test]
    fn possibly_returning_calls_and_fallthrough_catches_still_require_a_value() {
        for source in [
            "fn inner(flag:Bool)->Void{if flag{throw 7;}return;}fn main()->Int{try{inner(true);}catch(e){return 42;}}",
            "fn inner()->Void{throw 7;}fn main()->Int{try{inner();}catch(e){let ignored=42;}}",
            "fn recurse()->Void{recurse();}fn main()->Int{try{recurse();}catch(e){return 42;}}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            let report = verify_for_entry(&module, "main");
            assert_eq!(
                report.reason_code.as_deref(),
                Some("missing-return"),
                "{source}: {report:?}"
            );
        }
    }
    #[test]
    fn call_completion_keeps_void_fallthrough_and_declared_trap_overrides_normal() {
        let source = "fn noop()->Void{let value=7;}fn main()->Int{noop();return 42;}";
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        assert!(verify_for_entry(&module, "main").ok);
        let source = format!(
            "fn {}()->Void{{return;}}fn main()->Int{{try{{{}();}}catch(e){{return 42;}}}}",
            crate::inrt::INRT_TRAP_BUILTIN,
            crate::inrt::INRT_TRAP_BUILTIN
        );
        let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
        assert!(
            check_nonvoid_completion(&module)
                .unwrap_err()
                .starts_with("missing-return:")
        );
    }
    #[test]
    fn syntactic_return_does_not_erase_fallible_aggregate_prologue() {
        let module = crate::in_lang_parse::parse_in_source(
            "struct Row{Int value}fn copy(row:Row)->Row{return row;}fn main()->Void{return;}",
        )
        .unwrap();
        let facts = module_type_facts(&module);
        let context = CompletionContext::new(&module, &facts);
        assert!(context.fallible.contains("copy"));
        let completion = context.calls["copy"];
        assert!(completion.has(Completion::NORMAL));
        assert!(completion.has(Completion::THROW));
    }
    #[test]
    fn local_callee_shadow_does_not_inherit_always_throw_summary() {
        let source =
            "fn fail()->Void{throw 7;}fn main(fail:Int)->Int{try{fail();}catch(e){return 42;}}";
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        assert!(
            check_nonvoid_completion(&module)
                .unwrap_err()
                .starts_with("missing-return:")
        );
    }
    #[test]
    fn exact_bodyless_function_shadows_always_throwing_underscore_alias() {
        let exact = function_with_params(
            "print_int",
            vec![("value".into(), Typ::Int)],
            Typ::Void,
            vec![],
        );
        let alias = function_with_params(
            "print-int",
            vec![("value".into(), Typ::Int)],
            Typ::Void,
            vec![Stmt::Throw(Expr::IntLit(7))],
        );
        let main = function_with_ret(
            "main",
            Typ::Int,
            vec![Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident("print_int".into())),
                args: vec![Expr::IntLit(42)],
            })],
        );
        let module = UnifiedModule::new(vec![exact, alias, main]);
        let report = verify_for_entry(&module, "main");
        assert_eq!(
            report.reason_code.as_deref(),
            Some("missing-return"),
            "{report:?}"
        );
        assert!(
            check_nonvoid_completion(&module)
                .unwrap_err()
                .starts_with("missing-return:")
        );
    }
    #[test]
    fn no_exact_declaration_keeps_defined_function_alias_completion() {
        let alias = function_with_params(
            "throw-value",
            vec![],
            Typ::Void,
            vec![Stmt::Throw(Expr::IntLit(7))],
        );
        let main = function_with_ret(
            "main",
            Typ::Int,
            vec![Stmt::Try {
                body: vec![Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("throw_value".into())),
                    args: vec![],
                })],
                catches: vec![crate::core_ir::CatchArm {
                    pattern: "_".into(),
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                }],
            }],
        );
        let module = UnifiedModule::new(vec![main, alias]);
        assert!(verify_for_entry(&module, "main").ok);
        assert!(check_nonvoid_completion(&module).is_ok());
    }
    #[test]
    fn global_callee_shadow_cannot_inherit_function_alias_no_return_proof() {
        let alias = function_with_params(
            "throw-value",
            vec![],
            Typ::Void,
            vec![Stmt::Throw(Expr::IntLit(7))],
        );
        let global = Decl::Global {
            name: "throw_value".into(),
            typ: Typ::Int,
            mutable: true,
            constant_value: false,
            init: Some(Box::new(Expr::IntLit(0))),
        };
        let main = function_with_ret(
            "main",
            Typ::Int,
            vec![Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident("throw_value".into())),
                args: vec![],
            })],
        );
        // Compile-only flow analysis: do not execute this unknown pointer.
        let module = UnifiedModule::new(vec![global, alias, main]);
        assert!(
            check_nonvoid_completion(&module)
                .unwrap_err()
                .starts_with("missing-return:")
        );
    }
    use crate::core_ir::{Decl, UnifiedModule};
    use crate::core_ir::{Expr, Stmt};

    #[test]
    fn unsigned_integer_operators_require_integer_operands_and_return_int() {
        for op in ["udiv", "urem", "lshr"] {
            for value in [
                Expr::IntLit(-1),
                Expr::BoolLit(true),
                Expr::FloatLit(crate::core_ir::FloatVal(1.0)),
            ] {
                let valid = matches!(value, Expr::IntLit(_));
                let module = UnifiedModule::new(vec![Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Int,
                    body: vec![Stmt::Return(Some(Expr::Binary {
                        op: op.into(),
                        lhs: Box::new(value),
                        rhs: Box::new(Expr::IntLit(1)),
                    }))],
                    type_params: vec![],
                }]);
                assert_eq!(
                    verify_for_entry(&module, "main").ok,
                    valid,
                    "{op}: {module:?}"
                );
            }
        }
    }

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
    fn primitive_words_cannot_be_field_bases() {
        for (typ, value) in [
            (Typ::Int, Expr::IntLit(0)),
            (Typ::Named("i64".into()), Expr::IntLit(0)),
            (Typ::Float, Expr::FloatLit(crate::core_ir::FloatVal(1.0))),
            (Typ::Bool, Expr::BoolLit(false)),
            (Typ::String, Expr::StringLit("text".into())),
        ] {
            for write in [false, true] {
                let access = if write {
                    Stmt::FieldAssign {
                        base: Expr::Ident("value".into()),
                        name: "missing".into(),
                        value: Expr::IntLit(42),
                    }
                } else {
                    Stmt::Expr(Expr::Field {
                        base: Box::new(Expr::Ident("value".into())),
                        name: "missing".into(),
                    })
                };
                let module = module(vec![function(
                    "main",
                    vec![
                        Stmt::Let("value".into(), Some(typ.clone()), value.clone()),
                        access,
                    ],
                )]);
                let report = verify_for_entry(&module, "main");
                assert_eq!(
                    report.reason_code.as_deref(),
                    Some("invalid-field-base"),
                    "{typ:?} write={write}: {report:?}"
                );
                assert!(
                    crate::typecheck::TypeChecker::new()
                        .check_module(&module)
                        .is_err()
                );
            }
        }
        let module = module(vec![function_with_params(
            "main",
            vec![("handle".into(), Typ::Named("ForeignHandle".into()))],
            Typ::Void,
            vec![Stmt::Expr(Expr::Field {
                base: Box::new(Expr::Ident("handle".into())),
                name: "member".into(),
            })],
        )]);
        assert!(verify_for_entry(&module, "main").ok);
        assert!(
            crate::typecheck::TypeChecker::new()
                .check_module(&module)
                .is_ok()
        );
    }

    #[test]
    fn field_assignments_validate_destination_names() {
        for source in [
            "struct Point { x: i64 } fn main() -> i64 { let mut point: Point = Point { x: 0 }; point.missing = 42; return 0; }",
            "struct Point { x: i64 } struct Wrapper { point: Point } fn main() -> i64 { let mut wrapper: Wrapper = Wrapper { point: Point { x: 0 } }; wrapper.point.missing = 42; return 0; }",
            "struct Bag { items: Vec<i64> } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; bag.missing = vec![42]; return 0; }",
            "struct Point { x: i64 } fn make() -> Point { return Point { x: 0 }; } fn main() -> i64 { make().missing = 42; return 0; }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            let report = verify_for_entry(&module, "main");
            assert!(!report.ok, "{source}");
            assert_eq!(
                report.reason_code.as_deref(),
                Some("unknown-struct-field"),
                "{report:?}: {source}"
            );
            assert!(
                crate::typecheck::TypeChecker::new()
                    .check_module(&module)
                    .is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn vector_field_replacement_checks_destination_elements() {
        for (source, expected) in [
            (
                "struct Bag { items: Vec<i64> } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; bag.items = vec![20, 22]; return 0; }",
                true,
            ),
            (
                "struct Bag { items: Vec<i64> } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; bag.items = vec![]; return 0; }",
                true,
            ),
            (
                "struct Bag { items: Vec<i64> } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; bag.items = vec![true]; return 0; }",
                false,
            ),
            (
                "struct Bag { items: Vec<i64> } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; let other: Vec<f64> = vec![20.0]; bag.items = other; return 0; }",
                false,
            ),
            (
                "struct Bag { items: Vec<i64> } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; let other: [i64; 1] = [20]; bag.items = other; return 0; }",
                false,
            ),
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            let report = verify_for_entry(&module, "main");
            assert_eq!(report.ok, expected, "{report:?}: {source}");
            assert_eq!(
                crate::typecheck::TypeChecker::new()
                    .check_module(&module)
                    .is_ok(),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn saturating_casts_require_float_and_known_widths() {
        for operand in [
            Expr::IntLit(1),
            Expr::BoolLit(true),
            Expr::StringLit("bad".into()),
        ] {
            let module = module(vec![function_with_ret(
                "main",
                Typ::Int,
                vec![Stmt::Return(Some(Expr::Unary {
                    op: "float-saturate-i8".into(),
                    expr: Box::new(operand),
                }))],
            )]);
            let report = verify_for_entry(&module, "main");
            assert!(!report.ok);
            assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
        }
        let module = module(vec![function_with_ret(
            "main",
            Typ::Int,
            vec![Stmt::Return(Some(Expr::Unary {
                op: "float-saturate-i128".into(),
                expr: Box::new(Expr::FloatLit(crate::core_ir::FloatVal(42.0))),
            }))],
        )]);
        assert_eq!(
            verify_for_entry(&module, "main").reason_code.as_deref(),
            Some("unsupported-floating-cast")
        );
    }

    #[test]
    fn sequence_literals_use_destination_types_without_coercing_variables() {
        for source in [
            "struct Bag { items: Vec<i64> } fn accept(items: Vec<i64>) -> i64 { return 42; } fn make() -> Vec<i64> { return vec![20, 22]; } fn main() -> i64 { let mut items: Vec<i64> = vec![1]; items = vec![20, 22]; let bag = Bag { items: vec![20, 22] }; return accept(vec![20, 22]); }",
            "fn accept(items: Vec<i64>) -> i64 { return 42; } fn make() -> Vec<i64> { return vec![]; } fn main() -> i64 { let items: Vec<i64> = vec![]; return accept(vec![]); }",
        ] {
            let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let report = verify_for_entry(&module, "main");
            assert!(report.ok, "{report:?}: {source}");
            assert!(
                crate::typecheck::TypeChecker::new()
                    .check_module(&module)
                    .is_ok(),
                "{source}"
            );
        }
        for source in [
            "fn main() -> i64 { let items: Vec<i64> = vec![true]; return 0; }",
            "fn main() -> i64 { let items: Vec<i64> = vec![1, true]; return 0; }",
            "fn main() -> i64 { let array: [i64; 1] = [20]; let items: Vec<i64> = array; return 0; }",
            "fn accept(items: Vec<i64>) -> i64 { return 42; } fn main() -> i64 { let array: [i64; 1] = [20]; return accept(array); }",
            "fn make() -> Vec<i64> { return vec![false]; } fn main() -> i64 { return 0; }",
            "struct Bag { items: Vec<i64> } fn main() -> i64 { let bag = Bag { items: vec![false] }; return 0; }",
        ] {
            let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            assert!(!verify_for_entry(&module, "main").ok, "{source}");
            assert!(
                crate::typecheck::TypeChecker::new()
                    .check_module(&module)
                    .is_err(),
                "{source}"
            );
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
    fn immutable_nested_array_rows_cannot_escape_or_be_written() {
        for body in [
            "let row = items[0]; row[0]=0; return 42;",
            "items[0][0]=0; return 42;",
            "return accept(items[0]);",
        ] {
            let source = format!(
                "var items: [[Int]] = [[20,22]];\nfn accept(row: [Int]) -> Int {{ return row[0]; }}\nfn main() -> Int {{ {body} }}"
            );
            let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            for decl in &mut module.decls {
                if let Decl::Global { mutable, .. } = decl {
                    *mutable = false;
                }
            }
            let report = verify_for_entry(&module, "main");
            assert!(!report.ok, "{source}: {report:?}");
            assert!(
                matches!(
                    report.reason_code.as_deref(),
                    Some("immutable-global" | "immutable-global-reference")
                ),
                "{source}: {report:?}"
            );
        }
    }

    #[test]
    fn nested_static_array_reads_preserve_borrow_and_copy_boundaries() {
        for (body, valid) in [
            ("return items[0][0]+items[0][1];", true),
            ("return array_len(items[0]);", true),
            (
                "let items = [[42]]; items[0][0]=42; return items[0][0];",
                true,
            ),
            ("let row = items[0]; row[0]=42; return row[0];", false),
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(&format!(
                "var items: [[Int]] = [[20,22]];\nfn main() -> Int {{ {body} }}"
            ))
            .unwrap();
            crate::lower_core::desugar_module(&mut module);
            for decl in &mut module.decls {
                if let Decl::Global { mutable, .. } = decl {
                    *mutable = false;
                }
            }
            assert_eq!(verify_for_entry(&module, "main").ok, valid, "{body}");
        }
        for (body, valid) in [
            ("unsafe { return ITEMS[0][0]+ITEMS[0][1]; }", true),
            ("unsafe { return ITEMS[0].len() as i64; }", true),
            (
                "unsafe { let mut row = ITEMS[0]; row[0]=0; return ITEMS[0][0]; }",
                false,
            ),
        ] {
            let module = crate::compiler::rust_front::parse_rust_source(&format!(
                "static mut ITEMS: [[i64;2];1]=[[20,22]]; fn main() -> i64 {{ {body} }}"
            ))
            .unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
            crate::lower_core::desugar_module(&mut module);
            assert_eq!(
                verify_for_entry(&module, "main").ok,
                valid,
                "{body}: {module:?}"
            );
        }
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
    fn conditional_blocks_reject_invalid_types_scopes_and_loop_jumps() {
        for source in [
            "fn main()->i64{let x=if 1{42}else{0};x}",
            "fn main()->i64{let x=if true{42}else{false};42}",
            "fn main()->i64{let x={let inner=42;inner};inner}",
            "fn main()->i64{let x=if true{return false;}else{42};x}",
            "fn main()->i64{let x=if true{break;}else{42};x}",
            "fn main()->i64{let x={continue;};42}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = verify_for_entry(&module, "main");
            assert!(!report.ok, "{source}: {module:?}");
        }
    }

    #[test]
    fn checks_scalar_intrinsic_argument_types_and_arity() {
        for (expression, reason) in [
            ("str-eq(42, 0)", Some("type-mismatch")),
            ("vector-push()", Some("call-arity-mismatch")),
            ("vector-extend()", Some("call-arity-mismatch")),
            ("vector-clear()", Some("call-arity-mismatch")),
            ("vector-clear([], [])", Some("call-arity-mismatch")),
            ("vector-clear(42)", Some("type-mismatch")),
            ("vector-clear([1])", Some("type-mismatch")),
            ("vector-capacity()", Some("call-arity-mismatch")),
            ("vector-with-capacity-int()", Some("call-arity-mismatch")),
            ("__inrt_vector_alloc()", Some("call-arity-mismatch")),
            ("__inrt_vector_alloc(true)", Some("type-mismatch")),
            ("vector-with-capacity-float(true)", Some("type-mismatch")),
            ("vector-capacity(42)", Some("type-mismatch")),
            ("vector-capacity([1])", Some("type-mismatch")),
            ("vector-extend(42, [1])", Some("type-mismatch")),
            ("vector-extend([1], [2])", Some("type-mismatch")),
            ("vector-push(42, 1)", Some("type-mismatch")),
            ("vector-push([1], 2)", Some("type-mismatch")),
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
                "int-to-float-f32",
                Typ::Float,
                Expr::IntLit(42),
                Expr::BoolLit(true),
            ),
            (
                "uint-to-float",
                Typ::Float,
                Expr::IntLit(42),
                Expr::BoolLit(true),
            ),
            (
                "uint-to-float-f32",
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
    #[test]
    fn ieee_bit_operations_require_exact_canonical_operand_domains() {
        for width in [32, 64] {
            for to_bits in [false, true] {
                let op = format!(
                    "{}-f{width}",
                    if to_bits {
                        "float-to-bits"
                    } else {
                        "bits-to-float"
                    }
                );
                let ret = if to_bits { Typ::Int } else { Typ::Float };
                for operand in [
                    Expr::IntLit(42),
                    Expr::FloatLit(crate::core_ir::FloatVal(1.0)),
                    Expr::BoolLit(true),
                    Expr::StringLit("bad".into()),
                ] {
                    let accepted = if to_bits {
                        matches!(operand, Expr::FloatLit(_))
                    } else {
                        matches!(operand, Expr::IntLit(_))
                    };
                    let report = verify_module(
                        &module(vec![function_with_ret(
                            "main",
                            ret.clone(),
                            vec![Stmt::Return(Some(Expr::Unary {
                                op: op.clone(),
                                expr: Box::new(operand),
                            }))],
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
        for op in ["float-to-bits-f16", "bits-to-float-f128"] {
            let report = verify_module(
                &module(vec![function_with_ret(
                    "main",
                    Typ::Int,
                    vec![Stmt::Return(Some(Expr::Unary {
                        op: op.into(),
                        expr: Box::new(Expr::IntLit(42)),
                    }))],
                )]),
                &default_options(),
            );
            assert!(!report.ok);
            assert_eq!(
                report.reason_code.as_deref(),
                Some("unsupported-floating-cast")
            );
        }
    }
    #[test]
    fn integer_casts_require_integral_operands_and_known_widths() {
        for (op, operand, reason) in [
            (
                "int-cast-u8",
                Expr::FloatLit(crate::core_ir::FloatVal(42.0)),
                "type-mismatch",
            ),
            (
                "int-cast-i8",
                Expr::StringLit("text".into()),
                "type-mismatch",
            ),
            (
                "int-cast-u128",
                Expr::IntLit(42),
                "unsupported-integer-cast",
            ),
        ] {
            let expr = Expr::Unary {
                op: op.into(),
                expr: Box::new(operand),
            };
            let report = verify_module(
                &module(vec![function_with_ret(
                    "main",
                    Typ::Int,
                    vec![Stmt::Return(Some(expr))],
                )]),
                &default_options(),
            );
            assert!(!report.ok, "{report:?}");
            assert_eq!(report.reason_code.as_deref(), Some(reason));
        }
    }
    #[test]
    fn floating_width_casts_accept_canonical_floats_only() {
        for (op, operand, accepted) in [
            (
                "float-cast-f32",
                Expr::FloatLit(crate::core_ir::FloatVal(42.0)),
                true,
            ),
            (
                "float-cast-f64",
                Expr::FloatLit(crate::core_ir::FloatVal(42.0)),
                true,
            ),
            ("float-cast-f32", Expr::IntLit(42), false),
            ("float-cast-f32", Expr::BoolLit(true), false),
            (
                "float-cast-f128",
                Expr::FloatLit(crate::core_ir::FloatVal(42.0)),
                false,
            ),
        ] {
            let module = module(vec![function_with_ret(
                "main",
                Typ::Float,
                vec![Stmt::Return(Some(Expr::Unary {
                    op: op.into(),
                    expr: Box::new(operand),
                }))],
            )]);
            let report = verify_for_entry(&module, "main");
            assert_eq!(report.ok, accepted, "{report:?}");
        }
    }

    #[test]
    fn raw_vector_copy_intrinsics_preserve_word_abi_parameters() {
        assert_eq!(
            intrinsic_param_types(crate::inrt::INRT_VECTOR_CLONE),
            Some([Typ::Int, Typ::Int, Typ::Int].as_slice())
        );
        assert_eq!(
            intrinsic_param_types(crate::inrt::INRT_VECTOR_EXTEND),
            Some([Typ::Int, Typ::Int, Typ::Int, Typ::Int].as_slice())
        );
        assert_eq!(intrinsic_param_types("vector-extend"), None);
    }

    #[test]
    fn mmio_intrinsics_require_exact_int_arguments() {
        for name in [
            "load8", "load16", "load32", "load64", "store8", "store16", "store32", "store64",
        ] {
            let count = if name.starts_with("store") { 2 } else { 1 };
            assert_eq!(
                intrinsic_param_types(name),
                Some(vec![Typ::Int; count].as_slice())
            );
            let valid = vec![Expr::IntLit(0xffff_ffff); count];
            let verify = |args| {
                verify_for_entry(
                    &module(vec![function(
                        "main",
                        vec![Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident(name.into())),
                            args,
                        })],
                    )]),
                    "main",
                )
            };
            assert!(
                verify(valid.clone()).ok,
                "{name}: Int carrier must admit high-bit addresses"
            );
            for position in 0..count {
                for invalid in [
                    Expr::BoolLit(true),
                    Expr::StringLit("not an address".into()),
                ] {
                    let mut args = valid.clone();
                    args[position] = invalid;
                    assert!(!verify(args).ok, "{name}: invalid argument {position}");
                }
            }
            assert!(
                !verify(valid[..count - 1].to_vec()).ok,
                "{name}: missing argument"
            );
            let mut extra = valid;
            extra.push(Expr::IntLit(0));
            assert!(!verify(extra).ok, "{name}: extra argument");
        }
    }

    #[test]
    fn mmio_named_declarations_keep_their_declared_verifier_abi() {
        for name in [
            "load8", "load16", "load32", "load64", "store8", "store16", "store32", "store64",
        ] {
            for body in [vec![], vec![Stmt::Return(Some(Expr::BoolLit(true)))]] {
                let declaration =
                    function_with_params(name, vec![("flag".into(), Typ::Bool)], Typ::Bool, body);
                let call = |arg| {
                    function_with_ret(
                        "main",
                        Typ::Bool,
                        vec![Stmt::Return(Some(Expr::Call {
                            callee: Box::new(Expr::Ident(name.into())),
                            args: vec![arg],
                        }))],
                    )
                };
                let valid = module(vec![declaration.clone(), call(Expr::BoolLit(true))]);
                assert!(
                    verify_for_entry(&valid, "main").ok,
                    "{name}: declared ABI must take precedence"
                );
                let invalid = module(vec![declaration, call(Expr::IntLit(42))]);
                assert!(
                    !verify_for_entry(&invalid, "main").ok,
                    "{name}: declared ABI must be checked"
                );
            }
        }
    }
    #[test]
    fn comparison_operand_contract_is_used_by_whole_module_verification() {
        for op in ["<", "<=", ">", ">="] {
            for side in [0usize, 1] {
                let divergent = Expr::Block {
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    // Deliberately not numeric: numeric require_type exempts a
                    // divergent operand even when its unreachable result has a type.
                    result: Some(Box::new(Expr::StringLit("unreachable".into()))),
                };
                let mut operands = [Expr::IntLit(1), Expr::IntLit(2)];
                operands[side] = divergent;
                let report = verify_for_entry(
                    &module(vec![function_with_ret(
                        "main",
                        Typ::Int,
                        vec![
                            Stmt::Expr(Expr::Binary {
                                op: op.into(),
                                lhs: Box::new(operands[0].clone()),
                                rhs: Box::new(operands[1].clone()),
                            }),
                            Stmt::Return(Some(Expr::IntLit(42))),
                        ],
                    )]),
                    "main",
                );
                assert!(report.ok, "{op} divergent side{side}: {report:?}");
            }
            for alias in ["i32", "u32", "#integer-i32", "#rust-integer-u32"] {
                for side in [0usize, 1] {
                    let mut params = vec![("left".into(), Typ::Int), ("right".into(), Typ::Int)];
                    params[side].1 = Typ::Named(alias.into());
                    let report = verify_for_entry(
                        &module(vec![function_with_params(
                            "main",
                            params,
                            Typ::Bool,
                            vec![Stmt::Return(Some(Expr::Binary {
                                op: op.into(),
                                lhs: Box::new(Expr::Ident("left".into())),
                                rhs: Box::new(Expr::Ident("right".into())),
                            }))],
                        )]),
                        "main",
                    );
                    assert!(report.ok, "{op} {alias} side{side}: {report:?}");
                }
            }
        }
        // Equality keeps raw identity rather than the ordering compatibility rule.
        for op in ["==", "!="] {
            let report = verify_for_entry(
                &module(vec![function_with_params(
                    "main",
                    vec![
                        ("left".into(), Typ::Named("i32".into())),
                        ("right".into(), Typ::Int),
                    ],
                    Typ::Bool,
                    vec![Stmt::Return(Some(Expr::Binary {
                        op: op.into(),
                        lhs: Box::new(Expr::Ident("left".into())),
                        rhs: Box::new(Expr::Ident("right".into())),
                    }))],
                )]),
                "main",
            );
            assert!(!report.ok);
            assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
            assert_eq!(
                report.reason,
                Some(format!(
                    "type mismatch for binary `{op}` in `main`: left i32, right Int"
                ))
            );
            let divergent = Expr::Block {
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                result: Some(Box::new(Expr::StringLit("unreachable".into()))),
            };
            let report = verify_for_entry(
                &module(vec![function_with_ret(
                    "main",
                    Typ::Int,
                    vec![
                        Stmt::Expr(Expr::Binary {
                            op: op.into(),
                            lhs: Box::new(divergent),
                            rhs: Box::new(Expr::IntLit(1)),
                        }),
                        Stmt::Return(Some(Expr::IntLit(42))),
                    ],
                )]),
                "main",
            );
            assert!(!report.ok);
            assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
            assert_eq!(
                report.reason,
                Some(format!(
                    "type mismatch for binary `{op}` in `main`: left String, right Int"
                ))
            );
        }
    }
}

#[cfg(test)]
mod match_value_tests {
    #[test]
    fn verifies_exhaustive_typed_match_values_and_refuses_unsafe_fallthrough() {
        for (source, valid) in [
            (
                "fn main()->i64{let value=match true{true=>42,false=>0};value}",
                true,
            ),
            (
                "fn main()->i64{let value=match (20,22){(a,b)=>a+b};value}",
                true,
            ),
            (
                "fn main()->i64{let value=match 20{x if x==20=>x+22,_=>0};value}",
                true,
            ),
            (
                "fn main()->i64{let value=match 0{0=>return 42,_=>0};value}",
                true,
            ),
            (
                "fn main()->i64{let value=if let (a,b)=(20,22){a+b}else{0};value}",
                true,
            ),
            ("fn main()->i64{let value=match 0{0=>42};value}", false),
            (
                "fn main()->i64{let value=match true{true if true=>42,false=>0};value}",
                false,
            ),
            (
                "fn main()->i64{let value=match true{true=>42,false=>0.0};value}",
                false,
            ),
            (
                "fn main()->i64{let value=match true{true if 1=>42,_=>0};value}",
                false,
            ),
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            let report = super::verify_for_entry(&module, "main");
            assert_eq!(report.ok, valid, "{source}: {report:?}; {module:?}");
        }
    }
}

#[cfg(test)]
mod unit_value_tests {
    #[test]
    fn unit_values_preserve_void_types_without_numeric_coercion() {
        for (source, valid) in [
            (
                "fn done()->(){return ();} fn main()->i64{let value:()=done();match value{()=>42}}",
                true,
            ),
            (
                "fn main()->i64{let value=();let answer={let value=42;value};match value{()=>answer}}",
                true,
            ),
            (
                "fn main()->i64{let pair=((),42);let ()=pair.0;pair.1}",
                true,
            ),
            ("fn done()->(){return 1;} fn main()->i64{done();42}", false),
            ("fn main()->i64{let value=();value}", false),
            ("fn main()->i64{let value:i64=();42}", false),
            ("fn main()->i64{if (){42}else{0}}", false),
            ("fn main()->i64{let value=()+1;42}", false),
            ("fn main()->i64{let value=-();42}", false),
            ("fn main()->i64{let value=!();42}", false),
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            let report = super::verify_for_entry(&module, "main");
            assert_eq!(report.ok, valid, "{source}: {report:?}; {module:?}");
        }
    }
}

#[cfg(test)]
mod invoke_value_tests {
    #[test]
    fn function_addresses_and_invocation_use_the_int_carrier_contract() {
        for (source, valid) in [
            ("fn main()->i64{let invoke:i64=1;invoke(1)}", false),
            (
                "fn invoke(value:bool)->i64{42} fn main()->i64{invoke(true)}",
                true,
            ),
            (
                "fn invoke1(value:bool)->bool{value} fn main()->i64{if invoke1(true){42}else{0}}",
                true,
            ),
            (
                "fn invoke2(value:bool)->i64{42} fn main()->i64{invoke2(true)}",
                true,
            ),
            (
                "fn invoke(value:bool)->i64{42} fn main()->i64{invoke(1)}",
                false,
            ),
            ("fn target()->i64{42} fn main()->i64{invoke(target)}", true),
            (
                "fn target(x:i64)->i64{x} fn main()->i64{invoke1(target,42)}",
                true,
            ),
            (
                "fn target(x:i64,y:i64)->i64{x+y} fn main()->i64{invoke2(target,20,22)}",
                true,
            ),
            (
                "fn target()->f64{42.0} fn main()->i64{let pointer:fn()->f64=target;42}",
                true,
            ),
            (
                "fn target()->f64{42.0} fn main()->i64{let target:i64=1;invoke(target)}",
                true,
            ),
            ("fn main()->i64{invoke1()}", false),
            ("fn main()->i64{invoke2(1)}", false),
            ("fn main()->i64{invoke(1,2,3,4)}", false),
            ("fn main()->i64{invoke(true)}", false),
            ("fn main()->i64{invoke1(1,true)}", false),
            (
                "fn target()->f64{42.0} fn main()->i64{invoke(target)}",
                false,
            ),
            (
                "fn target(x:f64)->i64{42} fn main()->i64{invoke1(target,42)}",
                false,
            ),
            (
                "fn target(x:i64)->i64{x} fn main()->i64{invoke(target)}",
                false,
            ),
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            let checked = crate::typecheck::TypeChecker::new().check_module(&module);
            let report = super::verify_for_entry(&module, "main");
            assert_eq!(checked.is_ok(), valid, "checker {source}: {checked:?}");
            assert_eq!(
                report.ok, valid,
                "verifier {source}: {report:?}; {module:?}"
            );
        }
    }
}

#[cfg(test)]
mod numeric_negation_domain_regressions {
    use super::*;
    #[test]
    fn whole_module_rejects_known_non_numeric_but_defers_declared_generic() {
        for source in [
            r#"fn main()->Int{return -"hello";}"#,
            "fn main()->Int{let values=[1];return -values;}",
            "struct Row{Int x}fn main()->Int{let value=Row{x:42};return -value;}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            let report = verify_for_entry(&module, "main");
            assert!(!report.ok, "{source}: {report:?}");
            assert_eq!(report.reason_code.as_deref(), Some("type-mismatch"));
        }
        for typ in [Typ::Int, Typ::Bool, Typ::Float, Typ::Generic("T".into())] {
            let mut module =
                crate::in_lang_parse::parse_in_source("fn main(value:Int)->Int{return -value;}")
                    .unwrap();
            if let Decl::Function { params, ret, .. } = &mut module.decls[0] {
                params[0].1 = typ.clone();
                if typ == Typ::Float {
                    *ret = Typ::Float;
                }
            }
            let report = verify_for_entry(&module, "main");
            assert!(report.ok, "{typ:?}: {report:?}");
        }
    }
}
