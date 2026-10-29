mod rigid_values;
mod rust_arrays;
mod rust_callables;
mod rust_floats;
mod rust_generics;
pub(crate) mod rust_integers;
mod type_proofs;
#[path = "typecheck/vector_inference.rs"]
mod vector_inference;
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

#[path = "typecheck/closure_inference.rs"]
mod closure_inference;

fn resolved_closure_type(typ: &Typ) -> bool {
    match typ {
        Typ::Generic(_) => false,
        Typ::Named(name) => !matches!(name.as_str(), "Any" | "_"),
        Typ::Array(item) | Typ::Vector(item) => {
            item.canonical() != Typ::Void && resolved_closure_type(item)
        }
        _ => true,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolvePolicy {
    Ordinary,
    ClosureValidate,
    CValues,
    RustFixedArrays,
    RustArrayInference,
    RustArrayValidate,
    RustFloatInference,
    RustFloatValidate,
    RustFloatLower,
    RustGenericArguments,
    RustCallables { lower: bool, word64: bool },
}
impl ResolvePolicy {
    fn floats(self) -> bool {
        matches!(
            self,
            Self::RustFloatInference | Self::RustFloatValidate | Self::RustFloatLower
        )
    }
    fn callables(self) -> bool {
        matches!(self, Self::RustCallables { .. })
    }
    fn destinations(self) -> bool {
        self.arrays() || self.floats() || self == Self::RustGenericArguments || self.callables()
    }
    fn arrays(self) -> bool {
        matches!(
            self,
            Self::RustFixedArrays | Self::RustArrayInference | Self::RustArrayValidate
        )
    }
}

pub(crate) struct Facts {
    functions: HashMap<String, (Vec<(String, Typ)>, Typ)>,
    generic_parameters: HashMap<String, Vec<String>>,
    callable_signatures: HashMap<String, crate::core_ir::rust_callables::Signature>,
    rigid_callables: HashMap<String, crate::core_ir::rust_callables::Signature>,
    callable_alias_origins: HashMap<String, Option<String>>,
    neg_implementations: Vec<(String, crate::core_ir::rust_generics::NegImplementation)>,
    structs: HashMap<String, Vec<(String, Typ)>>,
    closures: HashMap<String, String>,
    closure_environments: std::collections::HashSet<String>,
    unary_methods: Option<crate::boundary_ir::UnaryMethodBindings>,
    unary_method_errors: std::cell::RefCell<Vec<String>>,
    require_resolved_inline_values: bool,
    array_constraints: std::cell::RefCell<rust_arrays::Constraints>,
    float_constraints: std::cell::RefCell<rust_floats::Constraints>,
}

impl Facts {
    pub(crate) fn function_signature(&self, name: &str) -> Option<(&[(String, Typ)], &Typ)> {
        self.functions
            .get(name)
            .map(|(params, ret)| (params.as_slice(), ret))
    }

    pub(crate) fn pattern_fields(&self, name: &str) -> Option<&[(String, Typ)]> {
        self.structs.get(name).map(Vec::as_slice)
    }

    // Audit-only bounds permit invocation without making the opaque receiver
    // a Copy function pointer or equating it with a particular function item.
    fn rigid_callable(&self, typ: &Typ) -> Option<crate::core_ir::rust_callables::Signature> {
        let Typ::Named(name) = typ else { return None };
        if let Some(reference) = crate::core_ir::rust_references::decode(name) {
            return reference
                .mutable
                .then(|| self.rigid_callable(&reference.referent))?;
        }
        self.rigid_callables.get(name).cloned()
    }

    fn closure_target(&self, captures: &str) -> Option<&str> {
        if !self.structs.get(captures)?.is_empty() && !self.closure_environments.contains(captures)
        {
            return None;
        }
        let target = self.closures.get(captures)?;
        let (params, ret) = self.functions.get(target)?;
        (resolved_closure_type(ret) && params.iter().all(|(_, typ)| resolved_closure_type(typ)))
            .then_some(target.as_str())
    }

    pub(crate) fn value_receiver_index(&self, function: &str) -> Option<usize> {
        self.functions
            .get(function)?
            .0
            .iter()
            .position(|(name, typ)| {
                name == "self" && matches!(typ.canonical(), Typ::Named(name) if self.structs.contains_key(&name))
            })
    }
}

/// Expression children may carry a planning hint, but only actual value edges
/// may default/validate/materialize a destination. In particular None differs
/// from Destination(None), which is an unconstrained source result edge.
#[derive(Clone, Copy)]
enum ValueContext<'a> {
    Expression(Option<&'a Typ>),
    Destination(Option<&'a Typ>),
}
impl<'a> ValueContext<'a> {
    fn expected(self) -> Option<&'a Typ> {
        match self {
            Self::Expression(value) | Self::Destination(value) => value,
        }
    }
    fn destination(self) -> bool {
        matches!(self, Self::Destination(_))
    }
}
struct ResolvedValue {
    typ: Option<Typ>,
    c_kind: Option<CIntegerKind>,
    values: Option<Vec<Option<Typ>>>,
}
#[derive(Clone, Copy)]
struct ValueScope<'a> {
    context: ValueContext<'a>,
    return_type: Option<&'a Typ>,
}
#[derive(Default)]
pub(super) struct ResolvedInputs<'a> {
    actual: Option<Option<Typ>>,
    arguments: Option<&'a [Option<Typ>]>,
    operands: Option<&'a [Option<Typ>]>,
    values: Option<&'a [Option<Typ>]>,
}
#[cfg(test)]
thread_local! {static TYPED_BODY_VISITS:std::cell::Cell<usize>=const{std::cell::Cell::new(0)};}

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
        if let Err(reason) = crate::core_ir_verifier::check_definite_initialization(module) {
            errors.push(TypeError::TypeMismatch {
                context: reason,
                expected: Typ::Named("initialized local".into()),
                got: Typ::Named("uninitialized or unsupported local".into()),
            });
        }

        for decl in &module.decls {
            errors.extend(self.check_function_with_facts(decl, &facts, &globals));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Check one function against already collected module facts and globals.
    /// Module-level conformance and definite-initialization checks remain the caller's responsibility.
    pub(super) fn check_function_with_facts(
        &self,
        decl: &Decl,
        facts: &Facts,
        globals: &HashMap<String, Typ>,
    ) -> Vec<TypeError> {
        let mut errors = Vec::new();
        if let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        {
            let mut env = globals.clone();
            env.extend(params.iter().cloned());
            self.check_stmts(name, ret, body, facts, &mut env, &mut errors);
        }
        errors
    }

    /// Eligibility only for the new effectful-argument inline path. Reuse the
    /// checker scoped walk; unrelated unsupported functions keep their old path.
    pub(crate) fn checked_inline_functions(&self, decls: &[Decl]) -> HashSet<String> {
        let initialization_errors =
            crate::core_ir_verifier::initialization_errors_by_function(decls);
        let mut facts = self.collect_decl_facts(decls);
        facts.require_resolved_inline_values = true;
        let globals: HashMap<_, _> = decls
            .iter()
            .filter_map(|decl| match decl {
                Decl::Global { name, typ, .. } => Some((name.clone(), typ.clone())),
                _ => None,
            })
            .collect();
        decls
            .iter()
            .filter_map(|decl| {
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
                if initialization_errors.contains_key(name) {
                    return None;
                }
                let mut env = globals.clone();
                env.extend(params.iter().cloned());
                let mut errors = Vec::new();
                self.check_stmts(name, ret, body, &facts, &mut env, &mut errors);
                errors.is_empty().then(|| name.clone())
            })
            .collect()
    }

    /// Resolve type-dependent complement into the canonical boolean or integer operator.
    pub(crate) fn resolve_rust_callables(
        &self,
        module: &mut UnifiedModule,
        lower: bool,
        semantics: crate::integer_semantics::IntegerSemantics,
    ) {
        self.resolve_typed_operators(
            module,
            ResolvePolicy::RustCallables {
                lower,
                word64: semantics == crate::integer_semantics::IntegerSemantics::Word64,
            },
            None,
            None,
        );
    }

    pub(crate) fn resolve_complements(&self, module: &mut UnifiedModule) {
        let _ = self.resolve_typed_operators(module, ResolvePolicy::Ordinary, None, None);
    }

    fn resolve_c_value_promotions(&self, module: &mut UnifiedModule) {
        let _ = self.resolve_typed_operators(module, ResolvePolicy::CValues, None, None);
    }

    pub(crate) fn resolve_closure_calls(
        &self,
        module: &mut UnifiedModule,
        closures: &HashMap<String, String>,
    ) {
        let generated: std::collections::HashSet<_> = closures.values().cloned().collect();
        loop {
            let returns: HashMap<_, _> = module
                .decls
                .iter()
                .filter_map(|decl| match decl {
                    Decl::Function { name, ret, .. } if generated.contains(name) => {
                        Some((name.clone(), ret.clone()))
                    }
                    _ => None,
                })
                .collect();
            let _ =
                self.resolve_typed_operators(module, ResolvePolicy::Ordinary, Some(closures), None);
            let parameters_changed = closure_inference::infer(self, module, closures);
            let returns_changed = module.decls.iter().any(|decl| match decl {
                Decl::Function { name, ret, .. } => {
                    returns.get(name).is_some_and(|previous| previous != ret)
                }
                _ => false,
            });
            if !parameters_changed && !returns_changed {
                break;
            }
        }
        let _ = self.resolve_typed_operators(
            module,
            ResolvePolicy::ClosureValidate,
            Some(closures),
            None,
        );
    }

    /// Resolve only frontend-proven operator bindings. Other normalization
    /// entrypoints never infer a trait from a method basename.
    pub(crate) fn resolve_unary_methods(
        &self,
        module: &mut UnifiedModule,
        bindings: &crate::boundary_ir::UnaryMethodBindings,
    ) -> Result<(), String> {
        let mut has_origin = false;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            has_origin |= matches!(expr, Expr::Unary { op, .. } if op == crate::core_ir::rust_references::NEGATE_VALUE);
        });
        if !has_origin {
            return Ok(());
        }
        let errors =
            self.resolve_typed_operators(module, ResolvePolicy::Ordinary, None, Some(bindings));
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn resolve_rust_floats(&self, module: &mut UnifiedModule, policy: ResolvePolicy) {
        if crate::core_ir::rust_floats::present(&module.decls) {
            let _ = self.resolve_typed_operators(module, policy, None, None);
        }
    }

    fn resolve_typed_operators(
        &self,
        module: &mut UnifiedModule,
        policy: ResolvePolicy,
        closures: Option<&HashMap<String, String>>,
        unary_methods: Option<&crate::boundary_ir::UnaryMethodBindings>,
    ) -> Vec<String> {
        crate::core_bindings::normalize_decls(&mut module.decls);
        crate::core_ir::tuples::register(module);
        let mut facts = self.collect_facts(module);
        facts.unary_methods = unary_methods.cloned();
        if let Some(closures) = closures {
            facts.closures = closures.clone();
            facts.closure_environments =
                closure_inference::proven_environments(module, &facts, closures);
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
        let mut float_invalid = None;
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
                        type_params,
                    } if generated.contains(name) == closure_phase => {
                        // Infer only concrete caller bodies. Templates are audited and
                        // traversed after source substitutions by rust_generics.
                        if (policy == ResolvePolicy::RustGenericArguments || policy.callables())
                            && !type_params.is_empty()
                        {
                            continue;
                        }
                        let mut env = globals.clone();
                        env.extend(params.iter().cloned());
                        let initial = env.clone();
                        if policy.floats() {
                            *facts.float_constraints.borrow_mut() = Default::default();
                            for (name, typ) in &initial {
                                facts.float_constraints.borrow_mut().bind(name, typ);
                            }
                            loop {
                                facts.float_constraints.borrow_mut().reset_changed();
                                self.resolve_typed_body(
                                    body,
                                    &facts,
                                    &mut initial.clone(),
                                    Some(ret),
                                    ResolvePolicy::RustFloatInference,
                                );
                                if !facts.float_constraints.borrow().changed() {
                                    break;
                                }
                            }
                        }
                        if policy.arrays() {
                            *facts.array_constraints.borrow_mut() = Default::default();
                            for (name, typ) in &initial {
                                facts.array_constraints.borrow_mut().bind(name, typ);
                            }
                            loop {
                                facts.array_constraints.borrow_mut().reset_changed();
                                self.resolve_typed_body(
                                    body,
                                    &facts,
                                    &mut initial.clone(),
                                    Some(ret),
                                    ResolvePolicy::RustArrayInference,
                                );
                                if !facts.array_constraints.borrow().changed() {
                                    break;
                                }
                            }
                        }
                        self.resolve_typed_body(body, &facts, &mut env, Some(ret), policy);
                        if policy.floats()
                            && let Some(proof) =
                                facts.float_constraints.borrow().failure().filter(|proof| {
                                    policy == ResolvePolicy::RustFloatLower || proof.is_invalid()
                                })
                        {
                            if proof.is_invalid() {
                                if crate::config::env_config().debug {
                                    eprintln!("[rust-source] in `{name}`: {}", proof.message());
                                }
                                float_invalid = crate::core_ir::type_proofs::prefer_failure(
                                    float_invalid,
                                    Some(crate::core_ir::type_proofs::ResolutionFailure::Invalid(
                                        format!("in `{name}`: {}", proof.message()),
                                    )),
                                );
                            }
                            body.insert(0, Stmt::Expr(Expr::Ident(proof.symbol())));
                        }
                        if policy.arrays() {
                            let proof = facts.array_constraints.borrow().failure();
                            if let Some(proof) = proof.filter(|proof| {
                                policy == ResolvePolicy::RustFixedArrays || proof.is_invalid()
                            }) {
                                body.insert(0, Stmt::Expr(Expr::Ident(proof.symbol())));
                            }
                        }
                        if closure_phase {
                            if !policy.arrays()
                                && matches!(ret, Typ::Generic(name) if name == "_")
                                && params.iter().all(|(_, typ)| resolved_closure_type(typ))
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
                        name,
                        typ,
                        init: Some(expr),
                        ..
                    } if !closure_phase => {
                        if policy.floats() {
                            *facts.float_constraints.borrow_mut() = Default::default();
                            for (name, typ) in &globals {
                                facts.float_constraints.borrow_mut().bind(name, typ);
                            }
                            loop {
                                facts.float_constraints.borrow_mut().reset_changed();
                                self.resolve_typed_value(
                                    expr,
                                    Some(typ),
                                    &facts,
                                    &globals,
                                    ResolvePolicy::RustFloatInference,
                                    None,
                                );
                                if !facts.float_constraints.borrow().changed() {
                                    break;
                                }
                            }
                        }
                        if policy.arrays() {
                            *facts.array_constraints.borrow_mut() = Default::default();
                            for (name, typ) in &globals {
                                facts.array_constraints.borrow_mut().bind(name, typ);
                            }
                            loop {
                                facts.array_constraints.borrow_mut().reset_changed();
                                self.resolve_typed_value(
                                    expr,
                                    Some(typ),
                                    &facts,
                                    &globals,
                                    ResolvePolicy::RustArrayInference,
                                    None,
                                );
                                if !facts.array_constraints.borrow().changed() {
                                    break;
                                }
                            }
                        }
                        // Global initializers are folded as owned constant value graphs before data layout.
                        let initializer_policy = if policy == ResolvePolicy::RustFixedArrays {
                            ResolvePolicy::RustArrayValidate
                        } else {
                            policy
                        };
                        self.resolve_typed_value(
                            expr,
                            Some(typ),
                            &facts,
                            &globals,
                            initializer_policy,
                            None,
                        );
                        if policy.arrays() {
                            let proof = facts.array_constraints.borrow().failure();
                            if let Some(proof) = proof.filter(|proof| proof.is_invalid()) {
                                **expr = Expr::Ident(proof.symbol());
                            }
                        }
                        if policy.floats()
                            && let Some(proof) =
                                facts.float_constraints.borrow().failure().filter(|proof| {
                                    policy == ResolvePolicy::RustFloatLower || proof.is_invalid()
                                })
                        {
                            if proof.is_invalid() {
                                float_invalid = crate::core_ir::type_proofs::prefer_failure(
                                    float_invalid,
                                    Some(crate::core_ir::type_proofs::ResolutionFailure::Invalid(
                                        format!("in `{name}`: {}", proof.message()),
                                    )),
                                );
                            }
                            **expr = Expr::Ident(proof.symbol());
                        }
                        if policy == ResolvePolicy::CValues {
                            self.resolve_c_value(expr, typ, &facts, &globals);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(proof) = float_invalid
            && !module.decls.iter().any(|decl| matches!(decl, Decl::Global { name, .. } if name == "#rust-source-float-validation")) {
            module.decls.push(Decl::Global { name: "#rust-source-float-validation".into(), typ: proof.typ(), init: None, mutable: false, constant_value: false });
        }
        let old_len = module.decls.len();
        crate::core_ir::tuples::register(module);
        let mut errors = facts.unary_method_errors.into_inner();
        if module.decls.len() != old_len {
            errors.extend(self.resolve_typed_operators(module, policy, closures, unary_methods));
        }
        errors
    }

    fn pattern_environment(
        &self,
        pattern: &str,
        typ: &Typ,
        facts: &Facts,
        env: &HashMap<String, Typ>,
    ) -> Option<HashMap<String, Typ>> {
        let pattern = MatchPattern::parse(pattern).ok()?;
        let bindings = pattern
            .typed_bindings(typ, &|name| facts.structs.get(name).map(Vec::as_slice))
            .ok()?;
        let mut inner = env.clone();
        inner.extend(bindings);
        Some(inner)
    }

    fn infer_closure_return(
        &self,
        body: &[Stmt],
        facts: &Facts,
        initial: &HashMap<String, Typ>,
    ) -> Option<Typ> {
        self.infer_return_with(
            body,
            facts,
            initial,
            &|value, env| closure_inference::argument_type(self, value, facts, env),
            &|_, _| Some(()),
        )
    }

    fn infer_return_with(
        &self,
        body: &[Stmt],
        facts: &Facts,
        initial: &HashMap<String, Typ>,
        resolve: &impl Fn(&Expr, &HashMap<String, Typ>) -> Option<Typ>,
        update: &impl Fn(&Stmt, &mut HashMap<String, Typ>) -> Option<()>,
    ) -> Option<Typ> {
        fn walk(
            checker: &TypeChecker,
            body: &[Stmt],
            facts: &Facts,
            env: &mut HashMap<String, Typ>,
            returns: &mut Vec<Typ>,
            resolve: &impl Fn(&Expr, &HashMap<String, Typ>) -> Option<Typ>,
            update: &impl Fn(&Stmt, &mut HashMap<String, Typ>) -> Option<()>,
        ) -> Option<bool> {
            let mut falls = true;
            for stmt in body {
                if !falls {
                    break;
                }
                update(stmt, env)?;
                match stmt {
                    Stmt::Declare(name, typ) => {
                        env.insert(name.clone(), typ.clone());
                    }
                    Stmt::Let(name, typ, value) => {
                        if let Some(typ) = typ.clone().or_else(|| resolve(value, env)) {
                            env.insert(name.clone(), typ);
                        }
                    }
                    Stmt::Return(value) => {
                        returns.push(match value {
                            Some(value) => resolve(value, env)?,
                            None => Typ::Void,
                        });
                        falls = false;
                    }
                    Stmt::If {
                        cond,
                        then_body,
                        else_body,
                    } => {
                        let left = walk(
                            checker,
                            then_body,
                            facts,
                            &mut env.clone(),
                            returns,
                            resolve,
                            update,
                        )?;
                        let right = walk(
                            checker,
                            else_body,
                            facts,
                            &mut env.clone(),
                            returns,
                            resolve,
                            update,
                        )?;
                        falls = match cond {
                            Expr::BoolLit(true) => left,
                            Expr::BoolLit(false) => right,
                            _ => left || right,
                        };
                    }
                    Stmt::Loop { body, .. } => {
                        walk(
                            checker,
                            body,
                            facts,
                            &mut env.clone(),
                            returns,
                            resolve,
                            update,
                        )?;
                    }
                    Stmt::Match { scrutinee, arms } => {
                        let typ = resolve(scrutinee, env)?;
                        let schema = |name: &str| facts.structs.get(name).map(Vec::as_slice);
                        let mut exhaustive = false;
                        let mut arm_falls = false;
                        for arm in arms {
                            let pattern = MatchPattern::parse(&arm.pattern).ok()?;
                            if arm.guard.is_none() && pattern.irrefutable(&typ, &schema) {
                                exhaustive = true;
                            }
                            let mut inner =
                                checker.pattern_environment(&arm.pattern, &typ, facts, env)?;
                            arm_falls |= walk(
                                checker, &arm.body, facts, &mut inner, returns, resolve, update,
                            )?;
                        }
                        if typ == Typ::Bool
                            && [true, false].iter().all(|value| {
                                arms.iter().any(|arm| {
                                    arm.guard.is_none()
                                        && MatchPattern::parse(&arm.pattern).ok()
                                            == Some(MatchPattern::BoolPat(*value))
                                })
                            })
                        {
                            exhaustive = true;
                        }
                        falls = !exhaustive || arm_falls;
                    }
                    Stmt::Try { body, catches } => {
                        falls = walk(
                            checker,
                            body,
                            facts,
                            &mut env.clone(),
                            returns,
                            resolve,
                            update,
                        )?;
                        for catch in catches {
                            let mut inner = checker.pattern_environment(
                                &catch.pattern,
                                &Typ::Named("Any".into()),
                                facts,
                                env,
                            )?;
                            falls |= walk(
                                checker,
                                &catch.body,
                                facts,
                                &mut inner,
                                returns,
                                resolve,
                                update,
                            )?;
                        }
                    }
                    Stmt::Propagate => return None,
                    Stmt::Throw(_) => falls = false,
                    Stmt::Break | Stmt::Continue => break,
                    _ => {}
                }
            }
            Some(falls)
        }
        let mut returns = Vec::new();
        if walk(
            self,
            body,
            facts,
            &mut initial.clone(),
            &mut returns,
            resolve,
            update,
        )? {
            returns.push(Typ::Void);
        }
        if let Some(typ) =
            rust_floats::join_types(&returns.iter().cloned().map(Some).collect::<Vec<_>>())
        {
            return Some(typ);
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
        if let Typ::Named(name) = expected
            && let Some(target) = crate::core_ir::integer_type_name(name)
            && matches!(
                self.expr_type(expr, facts, env).map(|typ| typ.canonical()),
                Some(Typ::Int | Typ::Float | Typ::Bool)
            )
        {
            let mut value = expr.clone();
            self.resolve_c_value(&mut value, &Typ::Int, facts, env);
            if let Some(value) =
                crate::core_ir::integer_cast_expr(value, &format!("int-cast-{target}"))
            {
                *expr = value;
            }
            return;
        }
        let actual = self.expr_type(expr, facts, env);
        if expected == &Typ::Bool
            && matches!(&actual, Some(Typ::Named(name)) if crate::core_ir::integer_type_name(name).is_none())
        {
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

    fn resolve_vector_allocation(&self, expr: &mut Expr, expected: &Typ, facts: &Facts) -> bool {
        match expr {
            Expr::MatchValue { arms, .. } => {
                let mut changed = false;
                for arm in arms {
                    changed |= self.resolve_vector_allocation(&mut arm.value, expected, facts);
                }
                return changed;
            }
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => {
                let left = self.resolve_vector_allocation(then_expr, expected, facts);
                let right = self.resolve_vector_allocation(else_expr, expected, facts);
                return left || right;
            }
            Expr::Block {
                result: Some(result),
                ..
            } => return self.resolve_vector_allocation(result, expected, facts),
            _ => {}
        }
        let Expr::Call { callee, args } = expr else {
            return false;
        };
        let Expr::Ident(name) = callee.as_ref() else {
            return false;
        };
        if name == "#receiver-method:clone"
            && let [source] = args.as_mut_slice()
        {
            return self.resolve_vector_allocation(source, expected, facts);
        }
        if args.len() != 1
            || facts.functions.contains_key(name)
            || (name != crate::core_ir::VECTOR_ALLOCATION
                && !matches!(
                    name.as_str(),
                    "Vec::with_capacity"
                        | "std::vec::Vec::with_capacity"
                        | "alloc::vec::Vec::with_capacity"
                ))
            || (name == "Vec::with_capacity"
                && facts
                    .structs
                    .keys()
                    .any(|owner| owner == "Vec" || owner.ends_with("::Vec")))
        {
            return false;
        }
        let Some(intrinsic) = expected.vector_allocation_intrinsic() else {
            return false;
        };
        **callee = Expr::Ident(intrinsic.into());
        true
    }

    fn resolve_policy_vector_allocation(
        &self,
        policy: ResolvePolicy,
        expr: &mut Expr,
        expected: &Typ,
        facts: &Facts,
    ) -> bool {
        !policy.arrays()
            && policy != ResolvePolicy::RustGenericArguments
            && !policy.callables()
            && self.resolve_vector_allocation(expr, expected, facts)
    }

    fn finish_typed_value(
        &self,
        policy: ResolvePolicy,
        expr: &mut Expr,
        expected: Option<&Typ>,
        facts: &Facts,
        env: &HashMap<String, Typ>,
    ) -> Option<Typ> {
        // Legacy callers lack invocation-local child summaries. Enter the same
        // scoped walker; production summary paths call the shallow helper directly.
        self.resolve_typed_value(expr, expected, facts, env, policy, None)
    }
    fn finish_typed_value_with_actual(
        &self,
        policy: ResolvePolicy,
        expr: &mut Expr,
        expected: Option<&Typ>,
        facts: &Facts,
        env: &HashMap<String, Typ>,
        resolved: &ResolvedInputs<'_>,
    ) -> Option<Typ> {
        if policy.arrays() || policy.floats() {
            // Scope/result traversal belongs to resolve_typed_value. Completion
            // is shallow and must never replay a block's statements.
            if policy.floats() {
                rust_floats::finish_value_with_actual(
                    self, expr, expected, facts, env, policy, resolved,
                )
            } else {
                rust_arrays::finish_value_with_actual(
                    self,
                    expr,
                    expected,
                    facts,
                    env,
                    policy,
                    resolved.actual.clone(),
                )
            }
        } else {
            self.expr_type(expr, facts, env)
        }
    }

    fn resolve_typed_value(
        &self,
        expr: &mut Expr,
        expected: Option<&Typ>,
        facts: &Facts,
        env: &HashMap<String, Typ>,
        policy: ResolvePolicy,
        return_type: Option<&Typ>,
    ) -> Option<Typ> {
        self.resolve_typed_expr_context(
            expr,
            facts,
            env,
            policy,
            ValueScope {
                return_type,
                context: ValueContext::Destination(expected),
            },
        )
        .typ
    }
    fn resolve_typed_expr(
        &self,
        expr: &mut Expr,
        facts: &Facts,
        env: &HashMap<String, Typ>,
        policy: ResolvePolicy,
        return_type: Option<&Typ>,
    ) -> Option<CIntegerKind> {
        self.resolve_typed_expr_context(
            expr,
            facts,
            env,
            policy,
            ValueScope {
                return_type,
                context: ValueContext::Expression(None),
            },
        )
        .c_kind
    }
    fn resolve_typed_expr_context(
        &self,
        expr: &mut Expr,
        facts: &Facts,
        env: &HashMap<String, Typ>,
        policy: ResolvePolicy,
        scope: ValueScope<'_>,
    ) -> ResolvedValue {
        let ValueScope {
            return_type,
            context,
        } = scope;
        let mut value_type = None;
        let expected = context.expected();
        if let Expr::Unary { op, expr: inner } = expr
            && op == "*"
            && self
                .expr_type(inner, facts, env)
                .as_ref()
                .and_then(crate::core_ir::rust_references::scalar_read_type)
                .is_some()
        {
            *op = crate::core_ir::rust_references::SCALAR_READ.into();
        }
        if let Expr::Unary { op, expr: inner } = expr
            && op == "*"
            && self
                .expr_type(inner, facts, env)
                .as_ref()
                .and_then(crate::core_ir::rust_boxes::pointee)
                .is_some()
        {
            *expr = crate::core_ir::rust_boxes::dereference(*std::mem::replace(
                inner,
                Box::new(Expr::UnitLit),
            ));
        }
        if let Expr::Field { base, name } = expr
            && name != crate::core_ir::rust_boxes::DATA
        {
            while self
                .expr_type(base, facts, env)
                .as_ref()
                .and_then(crate::core_ir::rust_boxes::pointee)
                .is_some()
            {
                **base = crate::core_ir::rust_boxes::dereference(std::mem::replace(
                    base.as_mut(),
                    Expr::UnitLit,
                ));
            }
        }
        if let Some(expected) = expected
            && let Some(actual) = self.expr_type(expr, facts, env)
        {
            crate::core_ir::rust_boxes::coerce(expr, expected, &actual);
        }
        let mut scoped_result = None;
        let mut resolved_arguments = None;
        let mut resolved_callee = None;
        let mut resolved_operands = None;
        let mut resolved_values = None;
        let argument_contexts = if policy.floats() {
            rust_floats::argument_contexts(self, expr, expected, facts, env, policy)
        } else {
            None
        };
        let operand_context = if policy.floats() {
            rust_floats::operand_context(self, expr, expected, facts, env)
        } else {
            None
        };
        let mut c_kind = if policy == ResolvePolicy::CValues {
            let typ = match expr {
                Expr::Binary { .. }
                | Expr::Unary { .. }
                | Expr::Conditional { .. }
                | Expr::Block { .. }
                | Expr::MatchValue { .. } => None,
                _ => self.expr_type(expr, facts, env),
            };
            c_expression_leaf_kind(expr, typ.as_ref())
        } else {
            None
        };
        let mut c_operands = [None; 2];
        if let Expr::StructInit { name, fields } = expr
            && let Some(schema) = facts.structs.get(name)
        {
            for (name, value) in fields {
                if let Some((_, expected)) = schema.iter().find(|(field, _)| field == name) {
                    self.resolve_policy_vector_allocation(policy, value, expected, facts);
                }
            }
        }
        if let Expr::Call { callee, args } = expr
            && let Expr::Ident(name) = callee.as_ref()
            && let Some((params, _)) = facts.functions.get(name)
        {
            for (arg, (_, expected)) in args.iter_mut().zip(params) {
                self.resolve_policy_vector_allocation(policy, arg, expected, facts);
            }
        }
        if let Expr::MatchValue { scrutinee, arms } = expr {
            let scrutinee_value = self.resolve_typed_expr_context(
                scrutinee,
                facts,
                env,
                policy,
                ValueScope {
                    return_type,
                    context: ValueContext::Expression(None),
                },
            );
            let typ = scrutinee_value.typ;
            let mut types = Vec::with_capacity(arms.len());
            for arm in arms.iter_mut() {
                if let Some(typ) = &typ {
                    crate::core_ir::rust_options::pattern(&mut arm.pattern, typ, &|name| {
                        facts.structs.get(name).cloned()
                    });
                }
                let inner = typ
                    .as_ref()
                    .and_then(|typ| self.pattern_environment(&arm.pattern, typ, facts, env));
                if let Some(inner) = inner {
                    if let Some(guard) = &mut arm.guard {
                        self.resolve_typed_expr(guard, facts, &inner, policy, return_type);
                    }
                    let value = self.resolve_typed_expr_context(
                        &mut arm.value,
                        facts,
                        &inner,
                        policy,
                        ValueScope {
                            return_type,
                            context: if policy == ResolvePolicy::RustFixedArrays {
                                ValueContext::Expression(expected)
                            } else {
                                context
                            },
                        },
                    );
                    types.push(value.typ);
                } else {
                    types.push(None);
                }
            }
            let live: Vec<_> = arms
                .iter()
                .zip(&types)
                .filter(|(arm, _)| !crate::core_ir::expr_diverges(&arm.value))
                .map(|(_, typ)| typ.clone())
                .collect();
            scoped_result = Some(
                (if policy.callables() {
                    rust_callables::join(&live)
                } else {
                    None
                })
                .or_else(|| rust_arrays::join_types(&live))
                .or_else(|| rust_floats::join_types(&live))
                .or_else(|| crate::core_ir::match_value_type(arms, types.clone())),
            );
            resolved_operands = Some(types);
        } else if let Expr::Block { body, result } = expr {
            let mut inner = env.clone();
            self.resolve_typed_body(body, facts, &mut inner, return_type, policy);
            if let Some(result) = result {
                let tail_context =
                    if context.destination() && policy != ResolvePolicy::RustFixedArrays {
                        context
                    } else {
                        ValueContext::Expression(expected)
                    };
                let resolved = self.resolve_typed_expr_context(
                    result,
                    facts,
                    &inner,
                    policy,
                    ValueScope {
                        return_type,
                        context: tail_context,
                    },
                );
                c_kind = resolved.c_kind;
                scoped_result = Some(resolved.typ);
            } else {
                scoped_result = Some(Some(Typ::Void));
            }
        } else if let Expr::Closure {
            params, ret, body, ..
        } = expr
        {
            let mut inner = env.clone();
            inner.extend(params.iter().cloned());
            if policy.arrays() {
                for (name, typ) in params.iter() {
                    facts.array_constraints.borrow_mut().bind(name, typ);
                }
            }
            let initial = inner.clone();
            self.resolve_typed_body(body, facts, &mut inner, Some(ret), policy);
            if !policy.arrays()
                && policy != ResolvePolicy::RustGenericArguments
                && !policy.callables()
                && matches!(ret, Typ::Generic(name) if name == "_")
                && params.iter().all(|(_, typ)| resolved_closure_type(typ))
                && let Some(inferred) = self.infer_closure_return(body, facts, &initial)
            {
                *ret = inferred;
            }
        } else if let Expr::Binary { op, lhs, rhs } = expr {
            let operand = operand_context
                .as_ref()
                .map_or(ValueContext::Expression(None), |typ| {
                    ValueContext::Destination(Some(typ))
                });
            let left = self.resolve_typed_expr_context(
                lhs,
                facts,
                env,
                policy,
                ValueScope {
                    return_type,
                    context: operand,
                },
            );
            let right = self.resolve_typed_expr_context(
                rhs,
                facts,
                env,
                policy,
                ValueScope {
                    return_type,
                    context: operand,
                },
            );
            c_operands = [left.c_kind, right.c_kind];
            resolved_operands = Some(vec![left.typ, right.typ]);
            if !policy.arrays() {
                if matches!(op.as_str(), "lua-bitand" | "lua-bitor" | "lua-bitxor") {
                    if self.expr_type(lhs, facts, env) != Some(Typ::Int)
                        || self.expr_type(rhs, facts, env) != Some(Typ::Int)
                    {
                        *expr = Expr::Ident("#unsupported-lua-bitwise-operands".into());
                        return ResolvedValue {
                            typ: value_type,
                            c_kind: None,
                            values: None,
                        };
                    }
                    *op = match op.as_str() {
                        "lua-bitand" => "&",
                        "lua-bitor" => "|",
                        _ => "xor",
                    }
                    .into();
                }
                if op == "lua-add" {
                    let left = self.expr_type(lhs, facts, env);
                    let right = self.expr_type(rhs, facts, env);
                    if !matches!(
                        (&left, &right),
                        (Some(Typ::Int), Some(Typ::Int)) | (Some(Typ::Float), Some(Typ::Float))
                    ) {
                        *expr = Expr::Ident("#unsupported-lua-numeric-addition".into());
                        return ResolvedValue {
                            typ: value_type,
                            c_kind: None,
                            values: None,
                        };
                    }
                    *op = "+".into();
                }
                if op == "lua-div" {
                    // Lua integer division produces Float; retain only proven Float operands.
                    if self.expr_type(lhs, facts, env) != Some(Typ::Float)
                        || self.expr_type(rhs, facts, env) != Some(Typ::Float)
                    {
                        *expr = Expr::Ident("#unsupported-lua-integer-division".into());
                        return ResolvedValue {
                            typ: value_type,
                            c_kind: None,
                            values: None,
                        };
                    }
                    *op = "/".into();
                }
                if policy == ResolvePolicy::CValues {
                    c_kind = match op.as_str() {
                        "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => {
                            Some(CIntegerKind::I32)
                        }
                        "<<" | ">>" => c_operands[0],
                        _ => CIntegerKind::common(c_operands[0], c_operands[1]),
                    };
                }
            }
        } else if let Expr::Unary { op, expr } = expr {
            let operand = if let Some(typ) = operand_context.as_ref() {
                ValueContext::Destination(Some(typ))
            } else if crate::core_ir::rust_arrays::value_length(op).is_some() {
                ValueContext::Expression(expected)
            } else {
                ValueContext::Expression(None)
            };
            let resolved = self.resolve_typed_expr_context(
                expr,
                facts,
                env,
                policy,
                ValueScope {
                    return_type,
                    context: operand,
                },
            );
            let inner = resolved.c_kind;
            if crate::core_ir::rust_arrays::value_length(op).is_some() {
                resolved_values = resolved.values;
            }
            resolved_operands = Some(vec![resolved.typ]);
            if policy == ResolvePolicy::CValues && matches!(op.as_str(), "-" | "~" | "+") {
                c_kind = inner;
            }
        } else if let Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } = expr
        {
            self.resolve_typed_expr(cond, facts, env, policy, return_type);
            let branch = if policy == ResolvePolicy::RustFixedArrays {
                ValueContext::Expression(expected)
            } else {
                context
            };
            let left = self.resolve_typed_expr_context(
                then_expr,
                facts,
                env,
                policy,
                ValueScope {
                    return_type,
                    context: branch,
                },
            );
            let right = self.resolve_typed_expr_context(
                else_expr,
                facts,
                env,
                policy,
                ValueScope {
                    return_type,
                    context: branch,
                },
            );
            if policy == ResolvePolicy::CValues {
                c_kind = CIntegerKind::common(left.c_kind, right.c_kind);
            }
            let live: Vec<_> = [
                (&**then_expr, left.typ.clone()),
                (&**else_expr, right.typ.clone()),
            ]
            .into_iter()
            .filter(|(expr, _)| !crate::core_ir::expr_diverges(expr))
            .map(|(_, typ)| typ)
            .collect();
            scoped_result = Some(
                (if policy.callables() {
                    rust_callables::join(&live)
                } else {
                    None
                })
                .or_else(|| rust_arrays::join_types(&live))
                .or_else(|| rust_floats::join_types(&live))
                .or_else(|| {
                    crate::core_ir::conditional_type(
                        then_expr,
                        left.typ.clone(),
                        else_expr,
                        right.typ.clone(),
                    )
                }),
            );
            resolved_operands = Some(vec![left.typ, right.typ]);
        } else if let Expr::Call { callee, args } = expr {
            let inline_const = matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::INLINE_CONST)
                && args.len() == 1;
            let option_payload = if matches!(callee.as_ref(),Expr::Ident(name)if crate::core_ir::rust_boxes::is_constructor(name))
            {
                let explicit = match callee.as_ref() {
                    Expr::Ident(name) => crate::core_ir::rust_boxes::constructor_payload(name),
                    _ => None,
                };
                explicit.or_else(|| {
                    expected.and_then(|typ| match typ {
                        Typ::Named(name) => crate::core_ir::rust_boxes::payload(name),
                        _ => None,
                    })
                })
            } else if matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::rust_options::SOME)
            {
                expected.and_then(|typ| match typ {
                    Typ::Named(name) => crate::core_ir::rust_options::payload(name),
                    _ => None,
                })
            } else {
                None
            };
            let tuple_context = if matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::tuples::VALUE)
            {
                expected.and_then(|typ| match typ {
                    Typ::Named(name) => crate::core_ir::tuples::fields(name),
                    _ => None,
                })
            } else {
                None
            };
            let repeat =
                matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::ARRAY_REPEAT);
            let repeat_element = if repeat {
                expected.and_then(|typ| match typ {
                    Typ::Array(element) | Typ::Vector(element) => Some((**element).clone()),
                    Typ::Named(name) => {
                        crate::core_ir::rust_arrays::decode(name).map(|array| array.element)
                    }
                    _ => None,
                })
            } else {
                None
            };
            let lexical_callable = match callee.as_ref() {
                Expr::Ident(name) => env.get(name).and_then(rust_callables::signature),
                _ => self
                    .expr_type(callee, facts, env)
                    .as_ref()
                    .and_then(rust_callables::signature),
            };
            let params: Option<Vec<(String, Typ)>> =
                if policy.callables() || lexical_callable.is_some() {
                    rust_callables::callee_signature(callee, facts, env)
                        .or_else(|| {
                            self.expr_type(callee, facts, env)
                                .as_ref()
                                .and_then(rust_callables::signature)
                        })
                        .map(|signature| {
                            signature
                                .parameters
                                .into_iter()
                                .enumerate()
                                .map(|(index, typ)| (format!("arg{index}"), typ))
                                .collect()
                        })
                } else {
                    match callee.as_ref() {
                        Expr::Ident(name) => {
                            facts.functions.get(name).map(|(params, _)| params.clone())
                        }
                        _ => None,
                    }
                };
            let params = params.or_else(|| match callee.as_ref() {
                Expr::Ident(name) => crate::core_ir::rust_boxes::allocation_payload(name)
                    .map(|payload| vec![("value".into(), payload)]),
                _ => None,
            });
            let callee_value = self.resolve_typed_expr_context(
                callee,
                facts,
                env,
                policy,
                ValueScope {
                    return_type,
                    context: ValueContext::Expression(None),
                },
            );
            resolved_callee = Some(callee_value.typ);
            let mut argument_types = Vec::with_capacity(args.len());
            for (index, arg) in args.iter_mut().enumerate() {
                let destination = if let Some(planned) = argument_contexts
                    .as_ref()
                    .and_then(|contexts| contexts.get(index))
                {
                    // An explicit None preserves a deliberately unresolved weak
                    // generic slot; do not replace it with a Generic<T> context.
                    planned.clone()
                } else {
                    params
                        .as_ref()
                        .and_then(|params| params.get(index))
                        .map(|(_, typ)| typ.clone())
                };
                let destination = option_payload
                    .clone()
                    .or_else(|| {
                        tuple_context
                            .as_ref()
                            .and_then(|fields| fields.get(index))
                            .map(|(_, typ)| typ.clone())
                    })
                    .or_else(|| {
                        if repeat && index == 0 {
                            repeat_element.clone()
                        } else {
                            None
                        }
                    })
                    .or(destination);
                let context = if inline_const {
                    // Compile-time payloads are not runtime transfer edges.
                    // Copy the resulting value outside the foldable boundary.
                    if policy == ResolvePolicy::RustFixedArrays {
                        ValueContext::Expression(expected)
                    } else {
                        context
                    }
                } else if policy.destinations() {
                    destination
                        .as_ref()
                        .map_or(ValueContext::Expression(None), |typ| {
                            ValueContext::Destination(Some(typ))
                        })
                } else {
                    ValueContext::Expression(None)
                };
                let resolved = self.resolve_typed_expr_context(
                    arg,
                    facts,
                    env,
                    policy,
                    ValueScope {
                        return_type,
                        context,
                    },
                );
                argument_types.push(resolved.typ);
            }
            resolved_arguments = Some(argument_types);
        } else if let Expr::StructInit { name, fields } = expr {
            let schema = expected
                .and_then(|typ| match typ {
                    Typ::Named(name) => crate::core_ir::tuples::fields(name),
                    _ => None,
                })
                .or_else(|| facts.structs.get(name).cloned())
                .or_else(|| crate::core_ir::tuples::fields(name));
            let tuple = crate::core_ir::tuples::fields(name).is_some();
            let mut values = Vec::with_capacity(fields.len());
            let nominal_name = name.clone();
            for (name, value) in fields {
                let destination = schema
                    .as_ref()
                    .and_then(|schema| schema.iter().find(|(field, _)| field == name))
                    .map(|(_, typ)| typ);
                let destination = destination.filter(|typ| {
                    !(policy == ResolvePolicy::RustFloatLower
                        && tuple
                        && rust_floats::has_weak_float(typ))
                });
                let context = if policy.destinations() {
                    destination.map_or(ValueContext::Expression(None), |typ| {
                        ValueContext::Destination(Some(typ))
                    })
                } else {
                    ValueContext::Expression(None)
                };
                let resolved = self.resolve_typed_expr_context(
                    value,
                    facts,
                    env,
                    policy,
                    ValueScope {
                        return_type,
                        context,
                    },
                );
                values.push(resolved.typ);
            }
            scoped_result = Some(if tuple {
                values
                    .iter()
                    .cloned()
                    .collect::<Option<Vec<_>>>()
                    .map(|types| crate::core_ir::tuples::typ(&types))
            } else {
                Some(Typ::Named(nominal_name))
            });
            resolved_values = Some(values);
        } else if let Expr::ArrayLit(items) = expr {
            let element = rust_floats::element_context(self, items, expected, facts, env);
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                let context = if policy == ResolvePolicy::RustFixedArrays {
                    ValueContext::Expression(element.as_ref())
                } else {
                    element
                        .as_ref()
                        .map_or(ValueContext::Expression(None), |typ| {
                            ValueContext::Destination(Some(typ))
                        })
                };
                let resolved = self.resolve_typed_expr_context(
                    item,
                    facts,
                    env,
                    policy,
                    ValueScope {
                        return_type,
                        context,
                    },
                );
                values.push(if matches!(item, Expr::StringLit(_)) {
                    rust_arrays::source_element_type(self, item, facts, env)
                } else {
                    resolved.typ
                });
            }
            scoped_result = Some(Some(Typ::Array(Box::new(
                values
                    .first()
                    .cloned()
                    .unwrap_or(Some(Typ::Void))
                    .unwrap_or(Typ::Generic("_".into())),
            ))));
            resolved_values = Some(values);
        } else {
            crate::core_ir::for_each_expr_child_mut(expr, &mut |child| {
                self.resolve_typed_expr(child, facts, env, policy, return_type);
            });
        }
        let resolved_call_type = resolved_arguments.as_ref().and_then(|types| {
            rust_floats::call_type_with_arguments(self, expr, expected, facts, env, types).map(Some)
        });
        if let Some(typ) = resolved_arguments.as_deref().and_then(|arguments| {
            crate::core_ir::rust_boxes::constructor(expr, expected, arguments).or_else(|| {
                crate::core_ir::rust_options::constructor(expr, expected, arguments, &|name| {
                    facts.structs.get(name).cloned()
                })
            })
        }) {
            return ResolvedValue {
                typ: Some(typ),
                c_kind,
                values: None,
            };
        }
        if let Expr::Unary { op, .. } = expr
            && let Some(actual) = resolved_operands
                .as_ref()
                .and_then(|types| types.first())
                .and_then(Option::as_ref)
            && let Some(typ) = crate::core_ir::rust_boxes::value_type(op, actual)
        {
            return ResolvedValue {
                typ: Some(typ),
                c_kind,
                values: None,
            };
        }
        if let Expr::Call { callee, args } = expr
            && args.len() == 1
            && let Expr::Ident(name) = callee.as_ref()
            && let Some(payload) = crate::core_ir::rust_boxes::allocation_payload(name)
        {
            return ResolvedValue {
                typ: Some(crate::core_ir::rust_boxes::address(payload)),
                c_kind,
                values: None,
            };
        }
        if let ResolvePolicy::RustCallables { lower, word64 } = policy {
            let typ = rust_callables::finish(
                self,
                expr,
                context.destination().then_some(expected).flatten(),
                facts,
                env,
                &ResolvedInputs {
                    actual: scoped_result,
                    arguments: resolved_arguments.as_deref(),
                    operands: resolved_operands.as_deref(),
                    values: resolved_values.as_deref(),
                },
                lower,
                word64,
                resolved_callee.as_ref(),
            )
            .flatten();
            return ResolvedValue {
                typ,
                c_kind,
                values: resolved_values,
            };
        }
        if policy == ResolvePolicy::RustGenericArguments {
            // Affirmative source impl metadata still carries the raw Output here.
            // Resolve before numeric erasure, while ambiguous/unproven operators
            // remain available to the authoritative owned unary resolver.
            if let Expr::Unary { op, expr: operand } = expr
                && op == crate::core_ir::rust_references::NEGATE_VALUE
                && let Some(receiver) = self.expr_type(operand, facts, env)
            {
                let mut matches = facts.neg_implementations.iter().filter(|(_, proof)| {
                    crate::core_ir::rust_references::same_source_type(&receiver, &proof.receiver)
                });
                if let Some((target, proof)) = matches.next()
                    && matches.next().is_none()
                    && let Some((params, ret)) = facts.functions.get(target)
                    && params.len() == 1
                    && params[0].0 == "self"
                    && crate::core_ir::rust_references::same_source_type(
                        &params[0].1,
                        &proof.receiver,
                    )
                    && crate::core_ir::rust_references::same_source_type(ret, &proof.output)
                {
                    let output = proof.output.clone();
                    *expr = Expr::Call {
                        callee: Box::new(Expr::Ident(target.clone())),
                        args: vec![operand.as_ref().clone()],
                    };
                    if context.destination()
                        && let Some(failure) =
                            rust_generics::proven_operator_result_failure(&output, expected)
                    {
                        *expr = Expr::Ident(failure.symbol());
                        return ResolvedValue {
                            typ: Some(failure.typ()),
                            c_kind,
                            values: None,
                        };
                    }
                    return ResolvedValue {
                        typ: Some(output),
                        c_kind,
                        values: None,
                    };
                }
            }
            let inferred = resolved_arguments.as_deref().and_then(|arguments| {
                rust_generics::infer_call(expr, arguments, expected, facts, env)
            });
            let typ = match inferred {
                Some(Ok((arguments, result))) => {
                    if let Expr::Call { callee, .. } = expr {
                        **callee = Expr::Unary {
                            op: crate::core_ir::rust_generics::inferred_arguments(&arguments),
                            expr: Box::new(std::mem::replace(callee.as_mut(), Expr::UnitLit)),
                        };
                    }
                    Some(result)
                }
                Some(Err(failure)) => {
                    if let Expr::Call { callee, .. } = expr {
                        **callee = Expr::Ident(failure.symbol());
                    }
                    Some(failure.typ())
                }
                None => scoped_result
                    .clone()
                    .or(resolved_call_type.clone())
                    .unwrap_or_else(|| self.expr_type(expr, facts, env)),
            };
            return ResolvedValue {
                typ,
                c_kind,
                values: resolved_values,
            };
        }
        if policy.arrays() || policy.floats() {
            let handled_float_call = policy.floats()
                && rust_floats::finish_call_with_types(
                    self,
                    expr,
                    facts,
                    env,
                    policy,
                    resolved_arguments.as_deref(),
                );
            let _ = handled_float_call; // Its non-scoped transfer checks remain authoritative.
            if policy.arrays() {
                rust_arrays::check_join_with_actual(
                    self,
                    expr,
                    facts,
                    env,
                    policy,
                    scoped_result.clone(),
                );
            }
            if policy.floats()
                && scoped_result
                    .as_ref()
                    .and_then(Option::as_ref)
                    .and_then(crate::core_ir::type_proofs::failure_in_type)
                    .is_some()
            {
                rust_floats::finish_value_with_actual(
                    self,
                    expr,
                    None,
                    facts,
                    env,
                    policy,
                    &ResolvedInputs {
                        actual: scoped_result.clone(),
                        ..Default::default()
                    },
                );
            }
        }
        let finished_expr_type = if policy.floats()
            && !matches!(
                expr,
                Expr::Block { .. } | Expr::Conditional { .. } | Expr::MatchValue { .. }
            )
            && resolved_arguments.is_none()
            && resolved_values.is_none()
        {
            // An unhandled float hook must retain ordinary source typing.
            // A genuinely resolved scoped None still suppresses the fallback.
            rust_floats::finish_expr_with_inputs(
                self,
                expr,
                facts,
                env,
                policy,
                resolved_operands.as_deref(),
            )
            .map(Some)
        } else {
            None
        };
        if policy.floats() || policy.arrays() {
            value_type = if let Some(typ) = scoped_result.clone().filter(|_| {
                matches!(
                    expr,
                    Expr::Block { .. } | Expr::Conditional { .. } | Expr::MatchValue { .. }
                ) && (!context.destination() || policy != ResolvePolicy::RustFixedArrays)
            }) {
                typ
            } else if context.destination() {
                if policy != ResolvePolicy::RustFixedArrays && matches!(expr, Expr::Block { .. }) {
                    scoped_result.flatten()
                } else {
                    self.finish_typed_value_with_actual(
                        policy,
                        expr,
                        expected,
                        facts,
                        env,
                        &ResolvedInputs {
                            actual: scoped_result
                                .clone()
                                .or_else(|| resolved_call_type.clone())
                                .or_else(|| finished_expr_type.clone()),
                            arguments: resolved_arguments.as_deref(),
                            operands: resolved_operands.as_deref(),
                            values: resolved_values.as_deref(),
                        },
                    )
                }
            } else if let Some(typ) = scoped_result.or(resolved_call_type).or(finished_expr_type) {
                typ
            } else {
                if policy.floats() {
                    rust_floats::source_type(self, expr, facts, env)
                } else {
                    self.expr_type(expr, facts, env)
                }
            };
            return ResolvedValue {
                typ: value_type,
                c_kind,
                values: resolved_values,
            };
        }
        if let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if matches!(name.as_str(), "Vec::new" | "std::vec::Vec::new" | "alloc::vec::Vec::new") && !facts.functions.contains_key(name))
            && args.is_empty()
            && !matches!(callee.as_ref(), Expr::Ident(name) if name == "Vec::new"
                && facts.structs.keys().any(|owner| owner == "Vec" || owner.ends_with("::Vec")))
        {
            *expr = Expr::ArrayLit(vec![]);
        }
        if let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::tuples::VALUE)
            && !args.is_empty()
            && let Some(types) = args
                .iter()
                .map(|arg| closure_inference::argument_type(self, arg, facts, env))
                .collect::<Option<Vec<_>>>()
            && let Typ::Named(name) = crate::core_ir::tuples::typ(&types)
        {
            *expr = Expr::StructInit {
                name,
                fields: std::mem::take(args)
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| (format!("_{i}"), value))
                    .collect(),
            };
        }
        if let Expr::Call { callee, args } = expr {
            if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::tuples::LET_CHECK)
                && let [base, Expr::IntLit(fixed), Expr::BoolLit(rest)] = args.as_slice()
                && let Some(typ) = self.expr_type(base, facts, env)
                && let Some(len) = match typ {
                    Typ::Void => Some(0),
                    Typ::Named(name) => {
                        crate::core_ir::tuples::fields(&name).map(|fields| fields.len())
                    }
                    _ => None,
                }
                && let Ok(fixed) = usize::try_from(*fixed)
                && ((!rest && len == fixed) || (*rest && len >= fixed))
            {
                *expr = Expr::IntLit(0);
            } else if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::tuples::LET_FIELD)
                && let [base, Expr::IntLit(index), Expr::BoolLit(from_end)] = args.as_slice()
                && let Some(typ) = self.expr_type(base, facts, env)
                && let Some((name, _)) = crate::core_ir::tuples::let_field(&typ, *index, *from_end)
            {
                *expr = Expr::Field {
                    base: Box::new(base.clone()),
                    name,
                };
            }
        }
        if let Expr::Unary { op, expr: operand } = expr
            && op == crate::core_ir::rust_references::NEGATE_VALUE
            && let Some(bindings) = &facts.unary_methods
            && let Some(receiver) = self
                .expr_type(operand, facts, env)
                .as_ref()
                .and_then(crate::core_ir::rust_references::receiver_identity)
        {
            let binding = bindings.get(&(receiver.clone(), "-".into()));
            let resolution = match binding {
                Some(Ok(target)) => match facts.functions.get(target) {
                    Some((params, _))
                        if params.len() == 1
                            && params[0].0 == "self"
                            && crate::core_ir::rust_references::receiver_identity(&params[0].1)
                                .as_deref()
                                == Some(receiver.as_str()) =>
                    {
                        Ok(Some(target.clone()))
                    }
                    _ => Err(format!(
                        "Rust unary method `{target}` lacks the exact receiver signature `{receiver}`"
                    )),
                },
                Some(Err(reason)) => Err(reason.clone()),
                None if facts.structs.contains_key(&receiver) => Err(format!(
                    "Rust unary `-` has no authoritative Neg binding for `{receiver}`"
                )),
                // Unknown/generic provenance remains deferred until specialization.
                None => Ok(None),
            };
            match resolution {
                Ok(Some(target)) => {
                    *expr = Expr::Call {
                        callee: Box::new(Expr::Ident(target)),
                        args: vec![operand.as_ref().clone()],
                    };
                }
                Err(reason) => {
                    facts.unary_method_errors.borrow_mut().push(reason);
                }
                _ => {}
            }
        }
        if let Expr::Unary { op, expr: operand } = expr
            && op == crate::core_ir::rust_references::NEGATE_VALUE
            && self
                .expr_type(operand, facts, env)
                .is_some_and(|typ| matches!(typ.canonical(), Typ::Int | Typ::Float | Typ::Bool))
        {
            *op = "-".into();
        }
        if let Expr::Call { callee, args } = expr
            && facts.unary_methods.is_none()
            && matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::BORROW)
            && let [value] = args.as_slice()
            && let Some(typ) = closure_inference::argument_type(self, value, facts, env)
            && !matches!(typ, Typ::Vector(_))
            && !matches!(typ, Typ::Named(ref name) if crate::core_ir::tuples::fields(name).is_some())
        {
            *expr = value.clone();
        }
        if let Expr::Call { callee, args } = expr
            && facts.unary_methods.is_none()
            && let Expr::Ident(name) = callee.as_ref()
            && let Some(method) = name.strip_prefix(crate::core_ir::METHOD_PREFIX)
            && let Some(receiver) = args.first().and_then(|arg| self.expr_type(arg, facts, env))
        {
            let target = if let Typ::Named(owner) = &receiver {
                [format!("{owner}::{method}"), format!("{owner}_{method}")]
                    .into_iter()
                    .find(|name| facts.value_receiver_index(name) == Some(0))
            } else {
                None
            };
            let receiver = receiver.canonical();
            if let Some(target) = target {
                **callee = Expr::Ident(target);
            } else if matches!(receiver, Typ::Array(_) | Typ::Vector(_) | Typ::String)
                && facts.functions.get(method).is_some_and(|(params, _)| {
                    params.len() == args.len()
                        && params.iter().zip(args.iter()).all(|((_, expected), arg)| {
                            self.expr_type(arg, facts, env).is_some_and(|actual| {
                                is_compatible(expected, &actual)
                                    || expected.accepts_sequence_literal(arg, &actual)
                            })
                        })
                })
            {
                **callee = Expr::Ident(method.into());
            } else if method == "push"
                && args.len() == 2
                && matches!(&receiver, Typ::Vector(item) if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool)
                    && self.expr_type(&args[1], facts, env).is_some_and(|actual| is_compatible(item, &actual)))
            {
                **callee = Expr::Ident("vector-push".into());
            } else if method == "extend"
                && args.len() == 2
                && matches!(&receiver, Typ::Vector(item) if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool)
                    && self.expr_type(&args[1], facts, env).is_some_and(|actual| receiver.accepts_vector_source(&args[1], &actual)))
            {
                **callee = Expr::Ident("vector-extend".into());
            } else if method == "clear"
                && args.len() == 1
                && matches!(&receiver, Typ::Vector(item) if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool))
            {
                **callee = Expr::Ident("vector-clear".into());
            } else if method == "capacity" && args.len() == 1 && matches!(receiver, Typ::Vector(_))
            {
                **callee = Expr::Ident("vector-capacity".into());
            } else if method == "len"
                && matches!(receiver, Typ::Array(_) | Typ::Vector(_) | Typ::String)
            {
                **callee = Expr::Ident(
                    if receiver == Typ::String {
                        "str-len"
                    } else {
                        "array-len"
                    }
                    .into(),
                );
            } else if method == "is_empty"
                && matches!(receiver, Typ::Array(_) | Typ::Vector(_) | Typ::String)
            {
                *expr = Expr::Binary {
                    op: "==".into(),
                    lhs: Box::new(Expr::Call {
                        callee: Box::new(Expr::Ident(
                            if receiver == Typ::String {
                                "str-len"
                            } else {
                                "array-len"
                            }
                            .into(),
                        )),
                        args: vec![args[0].clone()],
                    }),
                    rhs: Box::new(Expr::IntLit(0)),
                };
            } else if matches!(method, "iter" | "iter_mut")
                && matches!(receiver, Typ::Array(_) | Typ::Vector(_))
            {
                let op = if method == "iter_mut" {
                    crate::core_ir::rust_references::ITER_MUTABLE
                } else {
                    crate::core_ir::rust_references::ITER_SHARED
                };
                if args.len() == 1
                    && self.expr_type(&args[0], facts, env).is_some_and(|typ| {
                        crate::core_ir::rust_references::iteration_type(op, typ).is_some()
                    })
                {
                    *expr = Expr::Unary {
                        op: op.into(),
                        expr: Box::new(args[0].clone()),
                    };
                } else {
                    *expr = Expr::Ident(
                        crate::core_ir::type_proofs::ResolutionFailure::Invalid(
                            "sequence iteration requires zero arguments and a compatible borrow"
                                .into(),
                        )
                        .symbol(),
                    );
                }
            } else if method == "clone"
                && matches!(&receiver, Typ::Vector(item) if matches!(item.canonical(), Typ::Int | Typ::Float | Typ::Bool))
            {
                *expr = Expr::Unary {
                    op: "vector-clone".into(),
                    expr: Box::new(args[0].clone()),
                };
            } else if matches!(
                method,
                "iter" | "into_iter" | "cloned" | "copied" | "collect" | "to_vec" | "to_string"
            ) && matches!(receiver, Typ::Array(_) | Typ::Vector(_) | Typ::String)
            {
                // shortcut: owned iterator adapters keep their receiver until iterator values are modeled.
                *expr = args[0].clone();
            } else if crate::core_atomic::Kind::from_type(&receiver).is_some()
                || matches!(receiver, Typ::Array(_) | Typ::Vector(_) | Typ::String)
            {
                **callee = Expr::Ident(if matches!(receiver, Typ::Vector(_)) {
                    format!("Vec::{method}")
                } else {
                    format!("__method__{method}")
                });
            }
        }
        if let Expr::Call { callee, args } = expr {
            let captures = match callee.as_ref() {
                Expr::Ident(local) if !crate::core_ir::is_compiler_callee(local) => {
                    match env.get(local) {
                        Some(Typ::Named(name)) => Some(name.as_str()),
                        _ => None,
                    }
                }
                Expr::StructInit { name, .. } => Some(name.as_str()),
                _ => None,
            };
            if let Some(function) = captures.and_then(|captures| facts.closure_target(captures)) {
                args.insert(0, callee.as_ref().clone());
                **callee = Expr::Ident(function.to_string());
            } else if policy == ResolvePolicy::ClosureValidate
                && captures.is_some_and(|captures| {
                    facts.closures.contains_key(captures)
                        && facts
                            .structs
                            .get(captures)
                            .is_some_and(|fields| !fields.is_empty())
                })
            {
                // Keep an unsupported environment from becoming callable after optimization.
                *expr = Expr::Ident(
                    crate::core_ir::type_proofs::ResolutionFailure::Unsupported(
                        "captured closure requires a stable read-only scalar environment".into(),
                    )
                    .symbol(),
                );
                return ResolvedValue {
                    typ: value_type,
                    c_kind,
                    values: resolved_values,
                };
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
                        **callee = Expr::Ident(kind.intrinsic(op));
                    } else if let Some(Typ::Named(owner))=args.first().and_then(|arg|self.expr_type(arg,facts,env)) {
                        for candidate in [format!("{owner}::{op}"),format!("{owner}_{op}")] {
                            if facts.functions.contains_key(&candidate) {**callee = Expr::Ident(candidate);break;}
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
            **callee = Expr::Ident(
                if self.expr_type(source, facts, env) == Some(Typ::String) {
                    "str-len"
                } else {
                    "array-len"
                }
                .into(),
            );
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
            // Rust literal source inference must precede canonical cast lowering.
            match closure_inference::argument_type(self, value, facts, env)
                .map(|typ| typ.canonical())
            {
                Some(Typ::Int | Typ::Bool)
                    if crate::core_ir::integer_cast_spec(&integer).is_some()
                        && !rust_integers::direct_unsuffixed_cast_source(value) =>
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
        if policy == ResolvePolicy::CValues
            && let Expr::Unary { op, expr: inner } = expr
            && matches!(
                op.as_str(),
                "c-integer-literal-i32" | "c-integer-literal-u32"
            )
        {
            *expr = *std::mem::replace(inner, Box::new(Expr::UnitLit));
            return ResolvedValue {
                typ: value_type,
                c_kind,
                values: resolved_values,
            };
        }
        if policy == ResolvePolicy::CValues
            && let Expr::Call { callee, args } = expr
            && matches!(callee.as_ref(), Expr::Ident(name) if name == "c-numeric-cast")
        {
            if let [value, Expr::StringLit(target)] = args.as_slice() {
                let expected = match target.as_str() {
                    name if c_fixed_integer_name(name) => {
                        Some(Typ::Named(format!("#integer-{name}")))
                    }
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
            return ResolvedValue {
                typ: value_type,
                c_kind,
                values: resolved_values,
            };
        }
        if policy == ResolvePolicy::CValues {
            match expr {
                Expr::StructInit { name, fields } => {
                    if let Some(schema) = facts.structs.get(name) {
                        for (field, value) in fields {
                            if let Some((_, expected)) =
                                schema.iter().find(|(name, _)| name == field)
                            {
                                self.resolve_c_value(value, expected, facts, env);
                            }
                        }
                    }
                }
                Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "&&" | "||") => {
                    self.resolve_c_value(lhs, &Typ::Bool, facts, env);
                    self.resolve_c_value(rhs, &Typ::Bool, facts, env);
                }
                Expr::Unary { op, expr } if op == "!" => {
                    self.resolve_c_value(expr, &Typ::Bool, facts, env)
                }
                Expr::Unary { op, expr } if matches!(op.as_str(), "-" | "~" | "+") => {
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
                            | "xor"
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
                        && !env.contains_key(name)
                    {
                        if let Some((params, _)) = facts.functions.get(name) {
                            for (arg, (_, expected)) in args.iter_mut().zip(params) {
                                self.resolve_c_value(arg, expected, facts, env);
                            }
                        } else if let Some(params) =
                            crate::core_ir_verifier::intrinsic_param_types(name)
                        {
                            // C prototypes may not produce a Core IR declaration.
                            // Apply the same C value promotions to known intrinsic
                            // parameters; definitions still keep their own ABI.
                            for (arg, expected) in args.iter_mut().zip(params) {
                                self.resolve_c_value(arg, expected, facts, env);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if let Expr::Call { callee, args } = expr
            && let Expr::Ident(name) = callee.as_ref()
            && let Some((params, _)) = facts.functions.get(name)
        {
            for (arg, (_, expected)) in args.iter_mut().zip(params) {
                if self.resolve_policy_vector_allocation(policy, arg, expected, facts) {
                    self.resolve_typed_expr(arg, facts, env, policy, return_type);
                }
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
        if policy == ResolvePolicy::CValues
            && let Expr::Unary { op, expr: inner } = expr
            && op == "+"
        {
            let typ = self.expr_type(inner, facts, env);
            if matches!(typ, Some(Typ::Int | Typ::Float | Typ::Bool))
                || matches!(&typ, Some(Typ::Named(name)) if crate::core_ir::integer_type_name(name).is_some())
            {
                *expr = *std::mem::replace(inner, Box::new(Expr::UnitLit));
            }
        }
        if policy == ResolvePolicy::CValues {
            let floating = matches!(&*expr, Expr::Binary { lhs, rhs, .. } if [lhs, rhs].iter().any(|operand| self.expr_type(operand, facts, env) == Some(Typ::Float)));
            if !floating {
                resolve_c_unsigned_expression(expr, c_kind, c_operands);
            }
        }
        value_type = scoped_result
            .or(resolved_call_type)
            .unwrap_or_else(|| self.expr_type(expr, facts, env));
        ResolvedValue {
            typ: value_type,
            c_kind,
            values: resolved_values,
        }
    }

    fn resolve_typed_body(
        &self,
        body: &mut [Stmt],
        facts: &Facts,
        env: &mut HashMap<String, Typ>,
        return_type: Option<&Typ>,
        policy: ResolvePolicy,
    ) {
        #[cfg(test)]
        TYPED_BODY_VISITS.with(|visits| visits.set(visits.get() + 1));
        if !policy.arrays()
            && !policy.floats()
            && policy != ResolvePolicy::RustGenericArguments
            && !policy.callables()
        {
            vector_inference::infer(self, body, facts, env, return_type);
        }
        for stmt in body {
            match stmt {
                Stmt::Declare(name, typ) => {
                    if policy.floats() {
                        facts.float_constraints.borrow_mut().bind(name, typ);
                    }
                    if policy.arrays() {
                        facts.array_constraints.borrow_mut().bind(name, typ);
                    }
                    env.insert(name.clone(), typ.clone());
                }
                Stmt::Let(name, typ, expr) => {
                    if policy == ResolvePolicy::RustGenericArguments || policy.callables() {
                        let inferred = self.resolve_typed_value(
                            expr,
                            typ.as_ref(),
                            facts,
                            env,
                            policy,
                            return_type,
                        );
                        if let Some(selected) = typ.clone().or(inferred) {
                            if policy.callables() && rust_callables::signature(&selected).is_some()
                            {
                                *typ = Some(selected.clone());
                            }
                            env.insert(name.clone(), selected);
                        }
                        continue;
                    }
                    if policy.floats() {
                        if let Some(annotation) = typ.as_ref() {
                            facts.float_constraints.borrow_mut().bind(name, annotation);
                        }
                        let expected = facts
                            .float_constraints
                            .borrow()
                            .get(name)
                            .or_else(|| typ.clone());
                        if let Some(inferred) = self.resolve_typed_value(
                            expr,
                            expected.as_ref(),
                            facts,
                            env,
                            policy,
                            return_type,
                        ) {
                            facts.float_constraints.borrow_mut().bind(name, &inferred);
                            let selected = facts
                                .float_constraints
                                .borrow()
                                .get(name)
                                .unwrap_or(inferred);
                            // Closure lifting needs proven captured-local widths before erasure.
                            if matches!(
                                policy,
                                ResolvePolicy::RustFloatValidate | ResolvePolicy::RustFloatLower
                            ) && crate::core_ir::rust_floats::kind(&selected).is_some()
                            {
                                *typ = Some(selected.clone());
                            }
                            env.insert(name.clone(), selected);
                        }
                        continue;
                    }
                    if policy.arrays() {
                        if let Some(annotation) = typ.as_ref() {
                            facts.array_constraints.borrow_mut().bind(name, annotation);
                        }
                        let expected = facts
                            .array_constraints
                            .borrow()
                            .get(name)
                            .or_else(|| typ.clone());
                        let inferred = self.resolve_typed_value(
                            expr,
                            expected.as_ref(),
                            facts,
                            env,
                            policy,
                            return_type,
                        );
                        if let Some(inferred) = inferred {
                            facts.array_constraints.borrow_mut().bind(name, &inferred);
                            let explicit_other_type = typ.as_ref().filter(|typ| {
                                !matches!(typ, Typ::Named(name) if crate::core_ir::rust_arrays::decode(name).is_some())
                            });
                            let selected = explicit_other_type.cloned().unwrap_or_else(|| {
                                facts
                                    .array_constraints
                                    .borrow()
                                    .get(name)
                                    .unwrap_or(inferred)
                            });
                            if policy != ResolvePolicy::RustArrayInference
                                && crate::core_ir::rust_arrays::decode(match &selected {
                                    Typ::Named(name) => name,
                                    _ => "",
                                })
                                .is_some()
                            {
                                *typ = Some(selected.clone());
                            }
                            env.insert(name.clone(), selected);
                        }
                        continue;
                    }
                    if let Some(expected) = typ.as_ref() {
                        self.resolve_policy_vector_allocation(policy, expr, expected, facts);
                    }
                    self.resolve_typed_expr(expr, facts, env, policy, return_type);
                    if let Some(Typ::Vector(item)) = typ.as_mut()
                        && matches!(item.as_ref(), Typ::Generic(name) if name == "_")
                        && let Some(Typ::Array(element) | Typ::Vector(element)) =
                            closure_inference::argument_type(self, expr, facts, env)
                        && resolved_closure_type(&element)
                    {
                        *item = element;
                    }
                    // Preserve concrete inferred array layouts for all native backends.
                    // Empty and unresolved arrays still need contextual element types;
                    // leave other inferred bindings available to closure inference.
                    if typ.is_none()
                        && let Some(inferred @ Typ::Array(_)) = self
                            .expr_type(expr, facts, env)
                            .map(|typ| typ.inference_type())
                        && resolved_closure_type(&inferred)
                    {
                        *typ = Some(inferred);
                    }
                    if policy == ResolvePolicy::CValues
                        && let Some(typ) = typ.as_ref()
                    {
                        self.resolve_c_value(expr, typ, facts, env);
                    }
                    if let Some(typ) = typ.clone().or_else(|| self.expr_type(expr, facts, env)) {
                        let discard = name.starts_with("#tuple-let-") && typ == Typ::Void;
                        env.insert(name.clone(), typ);
                        if discard {
                            *stmt = Stmt::Expr(expr.clone());
                        }
                    }
                }
                Stmt::Assign(name, expr) => {
                    if policy.floats() {
                        if let Some(original) = env.get(name) {
                            facts.float_constraints.borrow_mut().bind(name, original);
                        }
                        let expected = facts
                            .float_constraints
                            .borrow()
                            .get(name)
                            .or_else(|| env.get(name).cloned());
                        if let Some(inferred) = self.resolve_typed_value(
                            expr,
                            expected.as_ref(),
                            facts,
                            env,
                            policy,
                            return_type,
                        ) {
                            facts.float_constraints.borrow_mut().bind(name, &inferred);
                        }
                        continue;
                    }
                    if policy.arrays() {
                        if let Some(original) = env.get(name) {
                            facts.array_constraints.borrow_mut().bind(name, original);
                        }
                        let expected = facts
                            .array_constraints
                            .borrow()
                            .get(name)
                            .or_else(|| env.get(name).cloned());
                        if let Some(inferred) = self.resolve_typed_value(
                            expr,
                            expected.as_ref(),
                            facts,
                            env,
                            policy,
                            return_type,
                        ) {
                            facts.array_constraints.borrow_mut().bind(name, &inferred);
                        }
                        continue;
                    }
                    if let Some(expected) = env.get(name) {
                        self.resolve_policy_vector_allocation(policy, expr, expected, facts);
                    }
                    self.resolve_typed_value(expr, env.get(name), facts, env, policy, return_type);
                    if policy == ResolvePolicy::CValues
                        && let Some(typ) = env.get(name)
                    {
                        self.resolve_c_value(expr, typ, facts, env);
                    }
                }
                Stmt::Return(Some(expr)) => {
                    if let Some(expected) = return_type {
                        self.resolve_policy_vector_allocation(policy, expr, expected, facts);
                    }
                    self.resolve_typed_value(expr, return_type, facts, env, policy, return_type);
                    if policy == ResolvePolicy::CValues
                        && let Some(typ) = return_type
                    {
                        self.resolve_c_value(expr, typ, facts, env);
                    }
                }
                Stmt::Expr(expr) | Stmt::Throw(expr) => {
                    if policy.floats() {
                        self.resolve_typed_value(expr, None, facts, env, policy, return_type);
                    } else {
                        self.resolve_typed_expr(expr, facts, env, policy, return_type);
                    }
                }
                Stmt::IndexAssign { base, index, value } => {
                    for expr in [&mut *base, &mut *index] {
                        self.resolve_typed_expr(expr, facts, env, policy, return_type);
                    }
                    if policy.destinations() {
                        let expected = self.expr_type(
                            &Expr::Index {
                                base: Box::new(base.clone()),
                                index: Box::new(index.clone()),
                            },
                            facts,
                            env,
                        );
                        self.resolve_typed_value(
                            value,
                            expected.as_ref(),
                            facts,
                            env,
                            policy,
                            return_type,
                        );
                    } else {
                        self.resolve_typed_expr(value, facts, env, policy, return_type);
                    }
                    if policy == ResolvePolicy::CValues
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
                    while self
                        .expr_type(base, facts, env)
                        .as_ref()
                        .and_then(crate::core_ir::rust_boxes::pointee)
                        .is_some()
                    {
                        *base = crate::core_ir::rust_boxes::dereference(std::mem::replace(
                            base,
                            Expr::UnitLit,
                        ));
                    }
                    if let Some(expected) = self.expr_type(
                        &Expr::Field {
                            base: Box::new(base.clone()),
                            name: name.clone(),
                        },
                        facts,
                        env,
                    ) {
                        self.resolve_policy_vector_allocation(policy, value, &expected, facts);
                    }
                    self.resolve_typed_expr(base, facts, env, policy, return_type);
                    if policy.destinations() {
                        let expected = self.expr_type(
                            &Expr::Field {
                                base: Box::new(base.clone()),
                                name: name.clone(),
                            },
                            facts,
                            env,
                        );
                        self.resolve_typed_value(
                            value,
                            expected.as_ref(),
                            facts,
                            env,
                            policy,
                            return_type,
                        );
                    } else {
                        self.resolve_typed_expr(value, facts, env, policy, return_type);
                    }
                    if policy == ResolvePolicy::CValues
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
                    self.resolve_typed_expr(cond, facts, env, policy, return_type);
                    if policy == ResolvePolicy::CValues {
                        self.resolve_c_value(cond, &Typ::Bool, facts, env);
                    }
                    self.resolve_typed_body(
                        then_body,
                        facts,
                        &mut env.clone(),
                        return_type,
                        policy,
                    );
                    self.resolve_typed_body(
                        else_body,
                        facts,
                        &mut env.clone(),
                        return_type,
                        policy,
                    );
                }
                Stmt::Loop { kind, cond, body } => {
                    if let Some(cond) = cond {
                        self.resolve_typed_expr(cond, facts, env, policy, return_type);
                        if policy == ResolvePolicy::CValues
                            && !matches!(kind, crate::core_ir::LoopKind::For { .. })
                        {
                            self.resolve_c_value(cond, &Typ::Bool, facts, env);
                        }
                    }
                    let mut inner = env.clone();
                    if let crate::core_ir::LoopKind::For { binding } = kind {
                        if let Some(iterator) = cond.as_mut()
                            && let Some(source @ Typ::Named(_)) =
                                self.expr_type(iterator, facts, env)
                            && let Typ::Named(name) = &source
                            && let Some(reference) = crate::core_ir::rust_references::decode(name)
                        {
                            let op = if reference.mutable {
                                crate::core_ir::rust_references::ITER_MUTABLE
                            } else {
                                crate::core_ir::rust_references::ITER_SHARED
                            };
                            if crate::core_ir::rust_references::iteration_type(op, source).is_some()
                            {
                                *iterator = Expr::Unary {
                                    op: op.into(),
                                    expr: Box::new(std::mem::replace(iterator, Expr::UnitLit)),
                                };
                            }
                        }
                        let element = cond
                            .as_ref()
                            .and_then(|value| self.expr_type(value, facts, env))
                            .and_then(for_element_type)
                            .unwrap_or_else(|| Typ::Generic("_".into()));
                        inner.insert(binding.clone(), element);
                    }
                    self.resolve_typed_body(body, facts, &mut inner, return_type, policy);
                }
                Stmt::Match { scrutinee, arms } => {
                    self.resolve_typed_expr(scrutinee, facts, env, policy, return_type);
                    let typ = self
                        .expr_type(scrutinee, facts, env)
                        .unwrap_or(Typ::Generic("_".into()));
                    for arm in arms {
                        crate::core_ir::rust_options::pattern(&mut arm.pattern, &typ, &|name| {
                            facts.structs.get(name).cloned()
                        });
                        let mut inner = self
                            .pattern_environment(&arm.pattern, &typ, facts, env)
                            .unwrap_or_else(|| env.clone());
                        if let Some(guard) = &mut arm.guard {
                            self.resolve_typed_expr(guard, facts, &inner, policy, return_type);
                        }
                        self.resolve_typed_body(
                            &mut arm.body,
                            facts,
                            &mut inner,
                            return_type,
                            policy,
                        );
                    }
                }
                Stmt::Try { body, catches } => {
                    self.resolve_typed_body(body, facts, &mut env.clone(), return_type, policy);
                    for catch in catches {
                        let mut inner = self
                            .pattern_environment(
                                &catch.pattern,
                                &Typ::Named("Any".into()),
                                facts,
                                env,
                            )
                            .unwrap_or_else(|| env.clone());
                        self.resolve_typed_body(
                            &mut catch.body,
                            facts,
                            &mut inner,
                            return_type,
                            policy,
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

    pub(crate) fn constant_value_is_typed(
        &self,
        value: &Expr,
        facts: &Facts,
        constants: &HashMap<String, Typ>,
    ) -> bool {
        self.constant_value_type_failure(value, facts, constants)
            .is_none()
    }

    pub(crate) fn constant_value_type_failure(
        &self,
        value: &Expr,
        facts: &Facts,
        constants: &HashMap<String, Typ>,
    ) -> Option<crate::core_ir::type_proofs::ResolutionFailure> {
        use crate::core_ir::type_proofs::ResolutionFailure;
        let mut errors = Vec::new();
        self.check_expr("#inline-constant", value, facts, constants, &mut errors);
        if errors.is_empty() {
            return None;
        }
        let primitive = |typ: &Typ| {
            matches!(
                typ.canonical(),
                Typ::Int | Typ::Bool | Typ::Float | Typ::String | Typ::Void
            )
        };
        let invalid = errors.iter().any(|error| {
            matches!(error,
            TypeError::TypeMismatch { expected, got, .. }
            | TypeError::ReturnTypeMismatch { expected, got, .. }
                if primitive(expected) && primitive(got))
        });
        let reason = "constant expression types could not be validated".into();
        Some(if invalid {
            ResolutionFailure::Invalid(reason)
        } else {
            ResolutionFailure::Unsupported(reason)
        })
    }

    pub(crate) fn collect_decl_facts(&self, decls: &[Decl]) -> Facts {
        let mut functions: HashMap<String, (Vec<(String, Typ)>, Typ)> = HashMap::new();
        let mut structs: HashMap<String, Vec<(String, Typ)>> = HashMap::new();
        let mut generic_parameters = HashMap::new();
        let mut callable_signatures = HashMap::new();
        let mut callable_alias_origins = HashMap::new();
        let mut neg_implementations = Vec::new();

        for decl in decls {
            match decl {
                Decl::Struct { name, fields, .. } => {
                    structs
                        .entry(name.clone())
                        .or_default()
                        .extend(fields.clone());
                }
                Decl::Function {
                    name,
                    params,
                    ret,
                    type_params,
                    body,
                    ..
                } => {
                    for statement in body {
                        if let Stmt::Expr(Expr::Unary { op, expr }) = statement
                            && matches!(expr.as_ref(), Expr::UnitLit)
                            && crate::core_ir::rust_callables::is_alias_origin(op)
                        {
                            let origin = crate::core_ir::rust_callables::decode_alias_origin(op);
                            callable_alias_origins
                                .entry(name.clone())
                                .and_modify(|existing| {
                                    *existing = None;
                                })
                                .or_insert(origin);
                        }
                        if let Stmt::Expr(Expr::Unary { op, expr }) = statement
                            && matches!(expr.as_ref(), Expr::UnitLit)
                            && let Some(signature) =
                                crate::core_ir::rust_callables::decode_declaration(op)
                        {
                            callable_signatures.insert(name.clone(), signature);
                        }
                        if let Stmt::Expr(Expr::Unary { op, expr }) = statement
                            && matches!(expr.as_ref(), Expr::UnitLit)
                            && let Some(proof) =
                                crate::core_ir::rust_generics::decode_neg_implementation(op)
                        {
                            neg_implementations.push((name.clone(), proof));
                        }
                    }
                    generic_parameters.insert(name.clone(), type_params.clone());
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
            generic_parameters,
            callable_signatures,
            rigid_callables: HashMap::new(),
            callable_alias_origins,
            neg_implementations,
            structs,
            closures: HashMap::new(),
            closure_environments: Default::default(),
            unary_methods: None,
            unary_method_errors: std::cell::RefCell::new(Vec::new()),
            require_resolved_inline_values: false,
            array_constraints: Default::default(),
            float_constraints: Default::default(),
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
        if facts.require_resolved_inline_values
            && matches!(stmt, Stmt::Throw(_) | Stmt::Try { .. } | Stmt::Propagate)
        {
            errors.push(TypeError::TypeMismatch {
                context: format!("unchecked exception statement in inline proof for `{fn_name}`"),
                expected: Typ::Named("checked statement".into()),
                got: Typ::Named("exception statement".into()),
            });
            return;
        }
        match stmt {
            Stmt::Declare(name, typ) => {
                env.insert(name.clone(), typ.clone());
            }
            Stmt::Let(name, annot, expr) => {
                self.check_expr(fn_name, expr, facts, env, errors);
                let expr_typ = self.expr_type(expr, facts, env);
                if let (Some(expected), Some(actual)) = (annot, &expr_typ)
                    && !is_compatible(expected, actual)
                    && !expected.accepts_sequence_literal(expr, actual)
                    && !crate::core_ir::expr_diverges(expr)
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
                if fn_ret.canonical() == Typ::Void
                    && self
                        .expr_type(expr, facts, env)
                        .is_none_or(|typ| typ.canonical() != Typ::Void)
                {
                    errors.push(TypeError::ReturnValueInVoid {
                        fn_name: fn_name.to_string(),
                    });
                } else if let Some(actual) = self.expr_type(expr, facts, env)
                    && !is_compatible(fn_ret, &actual)
                    && !fn_ret.accepts_sequence_literal(expr, &actual)
                    && !crate::core_ir::expr_diverges(expr)
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
            Stmt::Loop { kind, cond, body } => {
                let mut env_body = env.clone();
                if let Some(cond) = cond {
                    self.check_expr(fn_name, cond, facts, env, errors);
                    if let crate::core_ir::LoopKind::For { binding } = kind {
                        let element = self
                            .expr_type(cond, facts, env)
                            .and_then(|typ| {
                                let element = for_element_type(typ.clone());
                                if element.is_none() && is_concrete(&typ) {
                                    errors.push(TypeError::NotArray {
                                        expr: format!("for iterator in `{fn_name}`"),
                                    });
                                }
                                element
                            })
                            .unwrap_or_else(|| Typ::Generic("_".into()));
                        env_body.insert(binding.clone(), element);
                    } else if let Some(cond_typ) = self.expr_type(cond, facts, env)
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
                    if let Ok(pattern) = MatchPattern::parse(&arm.pattern)
                        && let Some(typ) = self.expr_type(scrutinee, facts, env)
                    {
                        match pattern.typed_bindings(&typ, &|name| {
                            facts.structs.get(name).map(Vec::as_slice)
                        }) {
                            Ok(bindings) => env_arm.extend(bindings),
                            Err(reason) => errors.push(TypeError::TypeMismatch {
                                context: format!("match pattern in `{fn_name}`: {reason}"),
                                expected: typ,
                                got: Typ::Named(arm.pattern.clone()),
                            }),
                        }
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
                    && !crate::core_ir::expr_diverges(value)
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
        // Ordinary checking preserves deferred frontend behavior. Inline
        // eligibility is stricter: a missing fact is not proof, and closures
        // must first be lifted and checked as their own functions.
        if facts.require_resolved_inline_values {
            if let Expr::Call { callee, .. } = expr
                && matches!(callee.as_ref(), Expr::Ident(name) if env.contains_key(name))
            {
                errors.push(TypeError::TypeMismatch {
                    context: format!("shadowed direct callee in inline proof for `{fn_name}`"),
                    expected: Typ::Named("unshadowed function target".into()),
                    got: Typ::Generic("callable value".into()),
                });
                return;
            }
            if matches!(expr, Expr::Closure { .. }) {
                errors.push(TypeError::TypeMismatch {
                    context: format!("unlifted closure in inline proof for `{fn_name}`"),
                    expected: Typ::Named("checked lifted function".into()),
                    got: Typ::Generic("closure".into()),
                });
                return;
            }
            if self.expr_type(expr, facts, env).is_none() {
                errors.push(TypeError::TypeMismatch {
                    context: format!("unresolved expression in inline proof for `{fn_name}`"),
                    expected: Typ::Named("resolved expression type".into()),
                    got: Typ::Generic("_".into()),
                });
            }
        }
        if let Expr::Call { callee, args } = expr {
            if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::INDEX_ASSIGN_VALUE_FIRST || (name=="index-assign-value-first" && !facts.functions.contains_key(name) && !env.contains_key(name)))
            {
                let [base, index, value] = args.as_slice() else {
                    errors.push(TypeError::ArityMismatch {
                        caller: fn_name.into(),
                        fn_name: "index-assign-value-first".into(),
                        expected: 3,
                        got: args.len(),
                    });
                    return;
                };
                self.check_stmts(
                    fn_name,
                    &Typ::Void,
                    &[Stmt::IndexAssign {
                        base: base.clone(),
                        index: index.clone(),
                        value: value.clone(),
                    }],
                    facts,
                    &mut env.clone(),
                    errors,
                );
                return;
            }
        }
        match expr {
            Expr::MatchValue { scrutinee, arms } => {
                self.check_expr(fn_name, scrutinee, facts, env, errors);
                if let Some(typ) = self.expr_type(scrutinee, facts, env) {
                    if !crate::core_ir::match_value_exhaustive(&typ, arms, &|name| {
                        facts.structs.get(name).map(Vec::as_slice)
                    }) {
                        errors.push(TypeError::TypeMismatch {
                            context: format!("non-exhaustive match value in `{fn_name}`"),
                            expected: Typ::Named("exhaustive-match".into()),
                            got: typ.clone(),
                        });
                    }
                    for arm in arms {
                        if let Some(inner) =
                            self.pattern_environment(&arm.pattern, &typ, facts, env)
                        {
                            if let Some(guard) = &arm.guard {
                                self.check_expr(fn_name, guard, facts, &inner, errors);
                                if let Some(actual) = self.expr_type(guard, facts, &inner)
                                    && actual.canonical() != Typ::Bool
                                    && !crate::core_ir::expr_diverges(guard)
                                {
                                    errors.push(TypeError::TypeMismatch {
                                        context: format!("match guard in `{fn_name}`"),
                                        expected: Typ::Bool,
                                        got: actual,
                                    });
                                }
                            }
                            self.check_expr(fn_name, &arm.value, facts, &inner, errors);
                        } else {
                            errors.push(TypeError::TypeMismatch {
                                context: format!("match pattern `{}` in `{fn_name}`", arm.pattern),
                                expected: typ.clone(),
                                got: Typ::Named("invalid-pattern".into()),
                            });
                        }
                    }
                }
                if self.expr_type(expr, facts, env).is_none() {
                    errors.push(TypeError::TypeMismatch {
                        context: format!(
                            "match values in `{fn_name}` require compatible resolved types"
                        ),
                        expected: Typ::Named("compatible-match-arms".into()),
                        got: Typ::Named("invalid-match-values".into()),
                    });
                }
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => {
                for value in [cond, then_expr, else_expr] {
                    self.check_expr(fn_name, value, facts, env, errors);
                }
                if let Some(actual) = self.expr_type(cond, facts, env)
                    && !crate::core_ir::expr_diverges(cond)
                    && actual.canonical() != Typ::Bool
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("conditional in `{fn_name}`"),
                        expected: Typ::Bool,
                        got: actual,
                    });
                }
                if self.expr_type(expr, facts, env).is_none() {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("conditional branches in `{fn_name}`"),
                        expected: self
                            .expr_type(then_expr, facts, env)
                            .unwrap_or(Typ::Generic("unknown".into())),
                        got: self
                            .expr_type(else_expr, facts, env)
                            .unwrap_or(Typ::Generic("unknown".into())),
                    });
                }
            }
            Expr::Block { body, result } => {
                let mut inner = env.clone();
                let ret = facts
                    .functions
                    .get(fn_name)
                    .map(|(_, ret)| ret.clone())
                    .unwrap_or(Typ::Void);
                self.check_stmts(fn_name, &ret, body, facts, &mut inner, errors);
                if let Some(value) = result {
                    self.check_expr(fn_name, value, facts, &inner, errors);
                }
            }
            Expr::UnitLit
            | Expr::IntLit(_)
            | Expr::FloatLit(_)
            | Expr::StringLit(_)
            | Expr::BoolLit(_)
            | Expr::Closure { .. } => {}
            Expr::Ident(name) => {
                if !env.contains_key(name)
                    && !facts.functions.contains_key(name)
                    && !is_builtin_fn(name)
                {
                    errors.push(TypeError::UndefinedVariable {
                        fn_name: fn_name.to_string(),
                        name: name.clone(),
                    });
                }
            }
            Expr::Unary { op, expr: inner } => {
                self.check_expr(fn_name, inner, facts, env, errors);
                if matches!(
                    op.as_str(),
                    "-" | crate::core_ir::rust_references::NEGATE_VALUE
                ) && self.expr_type(inner, facts, env) != Some(Typ::Void)
                {
                    let operand = self.expr_type(inner, facts, env);
                    let known_nominal = matches!(&operand, Some(Typ::Named(name)) if facts.structs.contains_key(name));
                    if crate::core_ir::numeric_negation_domain(operand.as_ref(), known_nominal)
                        == crate::core_ir::NumericNegationDomain::Rejected
                    {
                        errors.push(TypeError::TypeMismatch {
                            context: format!("numeric negation in `{fn_name}`"),
                            expected: Typ::Named("numeric-or-Bool".into()),
                            got: operand.unwrap_or(Typ::Void),
                        });
                    }
                }
                if matches!(
                    op.as_str(),
                    "-" | crate::core_ir::rust_references::NEGATE_VALUE | "neg" | "!" | "not"
                ) && self.expr_type(inner, facts, env) == Some(Typ::Void)
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("unary `{op}` in `{fn_name}`"),
                        expected: Typ::Named("numeric-or-Bool".into()),
                        got: Typ::Void,
                    });
                }
                if op.starts_with("array-copy")
                    && !self
                        .expr_type(inner, facts, env)
                        .is_some_and(|typ| crate::core_ir::array_copy::valid_operand(op, &typ))
                {
                    errors.push(TypeError::TypeMismatch {
                        context: format!("array copy in `{fn_name}`"),
                        expected: Typ::Named("scalar array with a canonical copy count".into()),
                        got: self.expr_type(inner, facts, env).unwrap_or(Typ::Void),
                    });
                }
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
                if ((crate::core_ir::integer_float_cast_spec(op).is_some() || op == "float-to-int")
                    || crate::core_ir::saturating_float_cast_spec(op).is_some()
                    || crate::core_ir::float_to_bits_width(op).is_some()
                    || crate::core_ir::bits_to_float_width(op).is_some())
                    && let Some(actual) = self.expr_type(inner, facts, env)
                {
                    let expected = if crate::core_ir::integer_float_cast_spec(op).is_some()
                        || crate::core_ir::bits_to_float_width(op).is_some()
                    {
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
                let op = match op.as_str() {
                    "add" => "+",
                    "sub" => "-",
                    "mul" => "*",
                    "div" => "/",
                    "mod" => "%",
                    "shl" => "<<",
                    "shr" => ">>",
                    "eq" => "==",
                    "neq" => "!=",
                    "lt" => "<",
                    "le" => "<=",
                    "gt" => ">",
                    "ge" => ">=",
                    "band" => "&",
                    "bor" => "|",
                    other => other,
                };
                self.check_expr(fn_name, lhs, facts, env, errors);
                self.check_expr(fn_name, rhs, facts, env, errors);
                if matches!(op, "==" | "!=" | "<" | "<=" | ">" | ">=") {
                    let left = self.expr_type(lhs, facts, env);
                    let right = self.expr_type(rhs, facts, env);
                    if left
                        .as_ref()
                        .is_some_and(crate::core_ir::rust_generics::is_rigid_type)
                        || right
                            .as_ref()
                            .is_some_and(crate::core_ir::rust_generics::is_rigid_type)
                    {
                        errors.push(TypeError::TypeMismatch {
                            context: format!(
                                "comparison `{op}` requires a generic trait bound in `{fn_name}`"
                            ),
                            expected: Typ::Named("comparison trait bound".into()),
                            got: left.clone().unwrap_or(Typ::Void),
                        });
                        return;
                    }
                    if let Some(mismatch) = crate::core_ir::comparison_operand_mismatch(
                        op,
                        left.as_ref(),
                        right.as_ref(),
                        crate::core_ir::expr_diverges(lhs),
                        crate::core_ir::expr_diverges(rhs),
                    ) {
                        errors.push(TypeError::TypeMismatch {
                            context: format!("binary `{op}` in `{fn_name}`"),
                            expected: mismatch.expected,
                            got: mismatch.actual,
                        });
                    }
                    return;
                }
                if let (Some(l), Some(r)) = (
                    self.expr_type(lhs, facts, env),
                    self.expr_type(rhs, facts, env),
                ) {
                    let (l, r) = (l.canonical(), r.canonical());
                    match op {
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
                        "&" | "|" | "^" | "xor" => {
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
                        "%" | "udiv" | "urem" | "lshr" => {
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
                                    && !crate::core_ir::expr_diverges(field_expr)
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
                if let Some(base_typ) = self
                    .expr_type(base, facts, env)
                    .map(|typ| crate::core_ir::rust_references::dereferenced(&typ))
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
                    if let Some(signature) = env
                        .get(callee_name)
                        .and_then(|typ| facts.rigid_callable(typ))
                    {
                        if signature.parameters.len() != args.len() {
                            errors.push(TypeError::ArityMismatch {
                                caller: fn_name.into(),
                                fn_name: callee_name.clone(),
                                expected: signature.parameters.len(),
                                got: args.len(),
                            });
                        }
                        for (index, arg) in args.iter().enumerate() {
                            self.check_expr(fn_name, arg, facts, env, errors);
                            if let Some(expected) = signature.parameters.get(index)
                                && let Some(actual) = self.expr_type(arg, facts, env)
                                && !is_compatible(expected, &actual)
                            {
                                errors.push(TypeError::TypeMismatch {
                                    context: format!("bounded callable argument in `{fn_name}`"),
                                    expected: expected.clone(),
                                    got: actual,
                                });
                            }
                        }
                        return;
                    }
                    if let Some(actual) = env.get(callee_name)
                        && crate::core_ir_verifier::invoke_param_types(callee_name).is_some()
                    {
                        errors.push(TypeError::TypeMismatch {
                            context: format!("call target `{callee_name}` is shadowed by a value"),
                            expected: Typ::Named("callable".into()),
                            got: actual.clone(),
                        });
                        for arg in args {
                            self.check_expr(fn_name, arg, facts, env, errors);
                        }
                        return;
                    }
                    if is_builtin_fn(callee_name)
                        && !facts.functions.contains_key(callee_name)
                        && !env.contains_key(callee_name)
                    {
                        if let Some(params) =
                            crate::core_ir_verifier::invoke_param_types(callee_name)
                        {
                            if params.len() != args.len() {
                                errors.push(TypeError::ArityMismatch {
                                    caller: fn_name.into(),
                                    fn_name: callee_name.clone(),
                                    expected: params.len(),
                                    got: args.len(),
                                });
                            }
                            for (expected, arg) in params.iter().zip(args) {
                                if let Some(actual) = self.expr_type(arg, facts, env)
                                    && actual.canonical() != *expected
                                {
                                    errors.push(TypeError::TypeMismatch {
                                        context: format!(
                                            "argument for `{callee_name}` in `{fn_name}`"
                                        ),
                                        expected: expected.clone(),
                                        got: actual,
                                    });
                                }
                            }
                            if let Some(Expr::Ident(target)) = args.first()
                                && !env.contains_key(target)
                                && let Some((params, ret)) = facts.functions.get(target)
                                && let Some(reason) =
                                    crate::core_ir_verifier::invoke_signature_error(
                                        callee_name,
                                        params,
                                        ret,
                                    )
                            {
                                errors.push(TypeError::TypeMismatch {
                                    context: reason,
                                    expected: Typ::Named("Int-function-signature".into()),
                                    got: Typ::Named(target.clone()),
                                });
                            }
                        }
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

    pub(crate) fn checked_expr_type(
        &self,
        expr: &Expr,
        facts: &Facts,
        env: &HashMap<String, Typ>,
    ) -> Option<Typ> {
        let mut errors = Vec::new();
        self.check_expr("expression", expr, facts, env, &mut errors);
        errors
            .is_empty()
            .then(|| self.expr_type(expr, facts, env))
            .flatten()
    }

    pub(crate) fn expr_type(
        &self,
        expr: &Expr,
        facts: &Facts,
        env: &HashMap<String, Typ>,
    ) -> Option<Typ> {
        match expr {
            Expr::MatchValue { scrutinee, arms } => {
                let typ = self.expr_type(scrutinee, facts, env)?;
                let types: Vec<_> = arms
                    .iter()
                    .map(|arm| {
                        let inner = self.pattern_environment(&arm.pattern, &typ, facts, env)?;
                        self.expr_type(&arm.value, facts, &inner)
                    })
                    .collect();
                let live: Vec<_> = arms
                    .iter()
                    .zip(&types)
                    .filter(|(arm, _)| !crate::core_ir::expr_diverges(&arm.value))
                    .map(|(_, typ)| typ.clone())
                    .collect();
                rust_callables::join(&live)
                    .or_else(|| rust_arrays::join_types(&live))
                    .or_else(|| rust_floats::join_types(&live))
                    .or_else(|| crate::core_ir::match_value_type(arms, types))
            }
            Expr::Conditional {
                then_expr,
                else_expr,
                ..
            } => {
                let left = self.expr_type(then_expr, facts, env);
                let right = self.expr_type(else_expr, facts, env);
                let live: Vec<_> = [(then_expr, left.clone()), (else_expr, right.clone())]
                    .into_iter()
                    .filter(|(expr, _)| !crate::core_ir::expr_diverges(expr))
                    .map(|(_, typ)| typ)
                    .collect();
                rust_callables::join(&live)
                    .or_else(|| rust_arrays::join_types(&live))
                    .or_else(|| rust_floats::join_types(&live))
                    .or_else(|| crate::core_ir::conditional_type(then_expr, left, else_expr, right))
            }
            Expr::Block { body, result } => {
                let mut inner = env.clone();
                for stmt in body {
                    match stmt {
                        Stmt::Declare(name, typ) => {
                            inner.insert(name.clone(), typ.clone());
                        }
                        Stmt::Let(name, typ, value) => {
                            if let Some(typ) =
                                typ.clone().or_else(|| self.expr_type(value, facts, &inner))
                            {
                                inner.insert(name.clone(), typ);
                            }
                        }
                        _ => {}
                    }
                }
                result
                    .as_ref()
                    .map(|value| self.expr_type(value, facts, &inner))
                    .unwrap_or(Some(Typ::Void))
            }
            Expr::UnitLit => Some(Typ::Void),
            Expr::IntLit(_) => Some(Typ::Int),
            Expr::FloatLit(_) => Some(Typ::Float),
            Expr::StringLit(_) => Some(Typ::String),
            Expr::BoolLit(_) => Some(Typ::Bool),
            Expr::Ident(name) => env
                .get(name)
                .map(|original| {
                    facts
                        .float_constraints
                        .borrow()
                        .get(name)
                        .or_else(|| facts.array_constraints.borrow().get(name))
                        .unwrap_or_else(|| original.clone())
                })
                .or_else(|| rust_callables::function_value_type(name, facts))
                .or_else(|| facts.functions.contains_key(name).then_some(Typ::Int))
                .or_else(|| is_builtin_fn(name).then(|| builtin_return_type(name))),
            Expr::StructInit { name, .. } => Some(Typ::Named(name.clone())),
            Expr::Field { base, name, .. } => {
                if matches!(name.as_str(), "ptr" | "len" | "cap")
                    && matches!(self.expr_type(base, facts, env), Some(Typ::Vector(_)))
                {
                    return Some(Typ::Int);
                }
                if let Some(base_typ) = self
                    .expr_type(base, facts, env)
                    .map(|typ| crate::core_ir::rust_references::dereferenced(&typ))
                    && let Typ::Named(struct_name) = &base_typ
                    && let Some(schema) = facts.structs.get(struct_name)
                {
                    return schema
                        .iter()
                        .find(|(f, _)| f == name)
                        .map(|(_, t)| t.clone());
                }
                if let Some(Typ::Named(shape)) = self.expr_type(base, facts, env) {
                    return crate::core_ir::tuples::fields(&shape)?
                        .into_iter()
                        .find(|(field, _)| field == name)
                        .map(|(_, typ)| typ);
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
                let mut base = self.expr_type(base, facts, env)?;
                // Auto-deref the sequence receiver without erasing source facts
                // from its elements (for example, the &str in &[&str]).
                while let Typ::Named(name) = &base {
                    let Some(reference) = crate::core_ir::rust_references::decode(name) else {
                        break;
                    };
                    base = reference.referent;
                }
                match base {
                    Typ::Array(item) | Typ::Vector(item) => Some(*item),
                    Typ::Named(name) => {
                        crate::core_ir::rust_arrays::decode(&name).map(|array| array.element)
                    }
                    _ => None,
                }
            }
            Expr::Unary { op, expr } if crate::core_ir::rust_arrays::value_length(op).is_some() => {
                let length = crate::core_ir::rust_arrays::value_length(op)?;
                let element = match expr.as_ref() {
                    Expr::Call { callee, args } if matches!(callee.as_ref(), Expr::Ident(name) if name == crate::core_ir::ARRAY_REPEAT) => {
                        rust_arrays::source_element_type(self, args.first()?, facts, env)?
                    }
                    Expr::ArrayLit(items) => items
                        .first()
                        .and_then(|item| rust_arrays::source_element_type(self, item, facts, env))
                        .unwrap_or(Typ::Void),
                    _ => {
                        let Typ::Array(element) = self.expr_type(expr, facts, env)? else {
                            return None;
                        };
                        *element
                    }
                };
                Some(crate::core_ir::rust_arrays::typ(element, length))
            }
            Expr::Unary { op, expr, .. }
                if crate::core_ir::rust_references::value_kind(op).is_some() =>
            {
                crate::core_ir::rust_references::value_type(op, self.expr_type(expr, facts, env)?)
            }
            Expr::Unary { op, expr }
                if crate::core_ir::rust_references::iteration_reference(op).is_some() =>
            {
                crate::core_ir::rust_references::iteration_type(
                    op,
                    self.expr_type(expr, facts, env)?,
                )
            }
            Expr::Unary { op, expr } if op == crate::core_ir::rust_references::SCALAR_READ => {
                crate::core_ir::rust_references::scalar_read_type(
                    &self.expr_type(expr, facts, env)?,
                )
            }
            Expr::Unary { op, expr }
                if matches!(
                    op.as_str(),
                    "*" | crate::core_ir::rust_boxes::DEREF
                        | crate::core_ir::rust_boxes::SHARED
                        | crate::core_ir::rust_boxes::MUTABLE
                ) =>
            {
                let operand = self.expr_type(expr, facts, env)?;
                if op == "*" {
                    crate::core_ir::rust_references::scalar_read_type(&operand)
                        .or_else(|| crate::core_ir::rust_boxes::pointee(&operand))
                        .or(Some(operand))
                } else {
                    crate::core_ir::rust_boxes::value_type(op, &operand)
                }
            }
            Expr::Unary { op, .. } if crate::core_ir::rust_callables::is_cast(op) => {
                crate::core_ir::rust_callables::decode_cast(op)
                    .map(crate::core_ir::rust_callables::typ)
            }
            Expr::Unary { op, expr, .. }
                if crate::core_ir::rust_generics::decode_arguments(op).is_some() =>
            {
                let arguments = crate::core_ir::rust_generics::decode_arguments(op)?;
                let Expr::Ident(name) = expr.as_ref() else {
                    return None;
                };
                if env.contains_key(name) {
                    return None;
                }
                rust_callables::generic_function_value_type(name, &arguments, facts)
            }
            Expr::Unary { op, .. } if crate::core_ir::rust_floats::inferred_type(op).is_some() => {
                crate::core_ir::rust_floats::inferred_type(op)
            }
            Expr::Unary { op, expr, .. } => match op.as_str() {
                "rust-integer-literal-magnitude" => Some(Typ::Int),
                op if op
                    .strip_prefix("rust-integer-literal-")
                    .or_else(|| op.strip_prefix("rust-integer-value-"))
                    .is_some_and(|kind| {
                        crate::core_ir::rust_integer_type_name(&format!("#rust-integer-{kind}"))
                            .is_some()
                    }) =>
                {
                    let kind = op
                        .strip_prefix("rust-integer-literal-")
                        .or_else(|| op.strip_prefix("rust-integer-value-"))
                        .unwrap();
                    Some(Typ::Named(format!("#rust-integer-{kind}")))
                }
                op if op.starts_with("numeric-cast-") => {
                    // Spills can stage this value before canonical cast lowering.
                    // Project the destination carrier, never the operand width;
                    // Rust source width remains on its private outer value marker.
                    let target = op.strip_prefix("numeric-cast-")?;
                    if crate::core_ir::integer_cast_spec(&format!("int-cast-{target}")).is_some() {
                        Some(Typ::Int)
                    } else if matches!(target, "f32" | "f64") {
                        Some(Typ::Float)
                    } else {
                        None
                    }
                }
                "c-integer-literal-i32" => Some(Typ::Named("#integer-i32".into())),
                "c-integer-literal-u32" => Some(Typ::Named("#integer-u32".into())),
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
                "-" | crate::core_ir::rust_references::NEGATE_VALUE | "~" | "+" => {
                    let typ = self.expr_type(expr, facts, env);
                    if matches!(
                        op.as_str(),
                        "-" | crate::core_ir::rust_references::NEGATE_VALUE
                    ) && typ.as_ref().is_some_and(|typ| typ.canonical() == Typ::Bool)
                    {
                        return Some(Typ::Int);
                    }
                    if matches!(&typ, Some(Typ::Named(name)) if crate::core_ir::integer_type_name(name).is_some())
                    {
                        c_expression_leaf_kind(expr, typ.as_ref()).map(CIntegerKind::typ)
                    } else {
                        typ
                    }
                }
                _ => self.expr_type(expr, facts, env),
            },
            Expr::Binary { op, lhs, rhs, .. } => {
                let op = match op.as_str() {
                    "add" => "+",
                    "sub" => "-",
                    "mul" => "*",
                    "div" => "/",
                    "mod" => "%",
                    "shl" => "<<",
                    "shr" => ">>",
                    "eq" => "==",
                    "neq" => "!=",
                    "lt" => "<",
                    "le" => "<=",
                    "gt" => ">",
                    "ge" => ">=",
                    "band" => "&",
                    "bor" => "|",
                    other => other,
                };
                let left = self.expr_type(lhs, facts, env);
                let right = self.expr_type(rhs, facts, env);
                if matches!(
                    op,
                    "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "xor" | "<<" | ">>"
                ) {
                    if let Some(joined) = rust_floats::join_types(&[left.clone(), right.clone()]) {
                        return Some(joined);
                    }
                    let rust_kind = |typ: &Option<Typ>| matches!(typ, Some(Typ::Named(name)) if crate::core_ir::rust_integer_type_name(name).is_some());
                    if rust_kind(&left) {
                        return left;
                    }
                    if rust_kind(&right) && !matches!(op, "<<" | ">>") {
                        return right;
                    }
                }
                if matches!(op, "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "xor" | "<<" | ">>")
                    && [left.as_ref(), right.as_ref()].iter().any(|typ| matches!(typ, Some(Typ::Named(name)) if crate::core_ir::integer_type_name(name).is_some()))
                {
                    let l = c_expression_leaf_kind(lhs, left.as_ref());
                    let r = c_expression_leaf_kind(rhs, right.as_ref());
                    let kind = if matches!(op, "<<" | ">>") { l } else { CIntegerKind::common(l, r) };
                    if let Some(kind) = kind { return Some(kind.typ()); }
                }
                let left = left.map(|typ| typ.canonical());
                let right = right.map(|typ| typ.canonical());
                match op {
                    "udiv" | "urem" | "lshr" => Some(Typ::Int),
                    "+" => match (left, right) {
                        (Some(Typ::String), Some(Typ::String)) => Some(Typ::String),
                        (Some(Typ::Float), _) | (_, Some(Typ::Float)) => Some(Typ::Float),
                        _ => Some(Typ::Int),
                    },
                    "&" | "|" | "^" | "xor" => crate::core_ir::bitwise_result_type(left, right),
                    "-" | "*" | "/" | "%" | "<<" | ">>" => {
                        if left == Some(Typ::Float) || right == Some(Typ::Float) {
                            Some(Typ::Float)
                        } else {
                            Some(Typ::Int)
                        }
                    }
                    "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => Some(Typ::Bool),
                    _ => None,
                }
            }
            Expr::Call { callee, args } => {
                if let Some(typ) = rust_floats::call_type(self, expr, facts, env) {
                    return Some(typ);
                }
                if let Expr::Ident(name) = callee.as_ref() {
                    if name == "c-numeric-cast"
                        && !facts.functions.contains_key(name)
                        && let [_, Expr::StringLit(target)] = args.as_slice()
                    {
                        return match target.as_str() {
                            name if c_fixed_integer_name(name) => {
                                Some(Typ::Named(format!("#integer-{name}")))
                            }
                            "Int" => Some(Typ::Int),
                            "Float" => Some(Typ::Float),
                            "Bool" => Some(Typ::Bool),
                            _ => None,
                        };
                    }
                    if name == crate::core_ir::tuples::LET_FIELD {
                        let [base, Expr::IntLit(index), Expr::BoolLit(from_end)] = args.as_slice()
                        else {
                            return None;
                        };
                        let typ = self.expr_type(base, facts, env)?;
                        return crate::core_ir::tuples::let_field(&typ, *index, *from_end)
                            .map(|(_, typ)| typ);
                    }
                    if name == crate::core_ir::tuples::VALUE && !args.is_empty() {
                        let types = args
                            .iter()
                            .map(|arg| closure_inference::argument_type(self, arg, facts, env))
                            .collect::<Option<Vec<_>>>()?;
                        return Some(crate::core_ir::tuples::typ(&types));
                    }
                    if name == crate::core_ir::BORROW
                        && let [value] = args.as_slice()
                    {
                        return self.expr_type(value, facts, env);
                    }
                    if let [value] = args.as_slice() {
                        if crate::core_ir::rust_boxes::is_constructor(name) {
                            if let Some(payload) =
                                crate::core_ir::rust_boxes::constructor_payload(name)
                            {
                                return Some(crate::core_ir::rust_boxes::typ(payload));
                            }
                            return self
                                .expr_type(value, facts, env)
                                .map(crate::core_ir::rust_boxes::typ);
                        }
                        if let Some(payload) = crate::core_ir::rust_boxes::allocation_payload(name)
                        {
                            return Some(crate::core_ir::rust_boxes::address(payload));
                        }
                    }
                    if matches!(
                        name.as_str(),
                        "std::hint::black_box" | "core::hint::black_box"
                    ) && !env.contains_key(name)
                        && !facts.functions.contains_key(name)
                        && let [value] = args.as_slice()
                    {
                        return self.expr_type(value, facts, env);
                    }
                    if let Some(local) = env.get(name) {
                        return facts
                            .rigid_callable(local)
                            .or_else(|| rust_callables::signature(local))
                            .map(|signature| signature.result);
                    }
                    if let Some(signature) = facts.callable_signatures.get(name)
                        && rust_callables::signature(&signature.result).is_some()
                    {
                        return Some(signature.result.clone());
                    }
                    facts
                        .functions
                        .get(name)
                        .map(|(_, ret)| ret.clone())
                        .or_else(|| is_builtin_fn(name).then(|| builtin_return_type(name)))
                } else {
                    self.expr_type(callee, facts, env)
                        .as_ref()
                        .and_then(rust_callables::signature)
                        .map(|signature| signature.result)
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

fn for_element_type(typ: Typ) -> Option<Typ> {
    match typ {
        Typ::Array(element) | Typ::Vector(element) => Some(*element),
        Typ::Named(name) => {
            if let Some(reference) = crate::core_ir::rust_references::decode(&name) {
                let op = if reference.mutable {
                    crate::core_ir::rust_references::ITER_MUTABLE
                } else {
                    crate::core_ir::rust_references::ITER_SHARED
                };
                return crate::core_ir::rust_references::iteration_type(op, Typ::Named(name))
                    .and_then(for_element_type);
            }
            crate::core_ir::rust_arrays::decode(&name).map(|array| array.element)
        }
        _ => None,
    }
}

fn is_concrete(typ: &Typ) -> bool {
    crate::core_ir::rust_generics::is_rigid_type(typ)
        || !matches!(typ, Typ::Named(_) | Typ::Generic(_))
}

fn is_numeric(typ: &Typ) -> bool {
    matches!(typ, Typ::Int | Typ::Float)
}

fn is_compatible(expected: &Typ, actual: &Typ) -> bool {
    if matches!(expected, Typ::Generic(_)) || matches!(actual, Typ::Generic(_)) {
        return true;
    }
    if crate::core_ir::rust_references::has_reference_proof(expected)
        && crate::core_ir::rust_references::has_reference_proof(actual)
        && crate::core_ir::rust_references::same_source_type(expected, actual)
    {
        return true;
    }
    expected.compatible_with(actual)
}

fn is_builtin_fn(name: &str) -> bool {
    if crate::core_ir_verifier::intrinsic_return_type(name).is_some() {
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
            | "vector-push"
            | "vector-extend"
            | "vector-clear"
            | "vector-capacity"
            | "vector-with-capacity-int" | "vector-with-capacity-float" | "vector-with-capacity-bool"
            | "array-pop"
            | "array-len" | "array_len"
            | "bool-to-int"
            | "int-to-bool"
            // x86 bare-metal intrinsics
            | "outb" | "inb" | "outw" | "inw" | "outl" | "inl"
            | "load8" | "load16" | "load32" | "load64"
            | "store8" | "store16" | "store32" | "store64"
            | "hlt" | "cli" | "sti" | "pause"
            | "lidt" | "lgdt" | "ltr" | "invlpg" | "read-cr2" | "read-cr3" | "cr3-read"
            | "write-cr3" | "cr3-write" | "rdmsr" | "wrmsr" | "wrmsr64" | "mem-fill64" | "mem-copy64"
            | "invoke" | "invoke1" | "invoke2" | "invoke-native" | "invoke-native1" | "invoke-native2" | "invoke_native" | "invoke_native1" | "invoke_native2"
    )
}

fn builtin_return_type(name: &str) -> Typ {
    if let Some(sig) = crate::core_atomic::signature(name) {
        return sig.ret.clone();
    }
    match name {
        "len" | "str-len" | "array-len" | "array_len" | "vector-capacity" | "bool-to-int"
        | "to-int" | "str-to-int" | "str-index-of" | "str-table-get-int" | "inb" | "inw"
        | "inl" | "load8" | "load16" | "load32" | "load64" | "read-cr2" | "read-cr3"
        | "cr3-read" | "rdmsr" | "invoke" | "invoke1" | "invoke2" | "invoke-native"
        | "invoke-native1" | "invoke-native2" | "invoke_native" | "invoke_native1"
        | "invoke_native2" => Typ::Int,
        "str-eq" | "str-contains" | "str-starts-with" | "str-is-int" | "str-table-has"
        | "int-to-bool" => Typ::Bool,
        "str-concat" | "str-trim" | "str-slice" | "to-string" => Typ::String,
        _ => crate::core_ir_verifier::intrinsic_return_type(name)
            .cloned()
            .unwrap_or(Typ::Void),
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

/// Numeric C-family normalization is independent of full frontend typechecking support.
pub fn uses_family_normalization(parser_id: ParserId) -> bool {
    matches!(
        parser_id,
        ParserId::C | ParserId::Cpp | ParserId::ObjC | ParserId::ObjCpp
    ) || uses_family_typecheck(parser_id)
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
    normalize_module_with_semantics(
        parser_id,
        module,
        crate::integer_semantics::IntegerSemantics::Word64,
    )
}

pub fn normalize_module_with_semantics(
    parser_id: ParserId,
    module: &UnifiedModule,
    semantics: crate::integer_semantics::IntegerSemantics,
) -> UnifiedModule {
    let decls = module
        .decls
        .iter()
        .map(|decl| normalize_decl(parser_id, decl))
        .collect();
    let mut normalized = UnifiedModule::with_identity(decls, module.identity.clone());
    normalized.regions.clone_from(&module.regions);
    if parser_id == ParserId::Lua {
        crate::core_opt::map_decl_exprs_mut(&mut normalized.decls, &mut |expr| {
            match expr {
                Expr::Binary { op, .. }
                    if matches!(op.as_str(), "//" | "%" | "^" | "<<" | ">>") =>
                {
                    *expr = Expr::Ident("#unsupported-lua-operator-semantics".into());
                }
                Expr::Binary { op, .. } if op == "~" => *op = "lua-bitxor".into(),
                Expr::Binary { op, .. } if op == "&" => *op = "lua-bitand".into(),
                Expr::Binary { op, .. } if op == "|" => *op = "lua-bitor".into(),
                Expr::Binary { op, .. } if op == "+" => *op = "lua-add".into(),
                Expr::Binary { op, .. } if op == "/" => *op = "lua-div".into(),
                Expr::Binary { op, .. } if op == "~=" => *op = "!=".into(),
                Expr::Binary { op, .. } if op == "and" => *op = "&&".into(),
                Expr::Binary { op, .. } if op == "or" => *op = "||".into(),
                Expr::Unary { op, expr: value } if op == "not" => {
                    // Existing typed comparisons retain Boolean local/call proofs.
                    *expr = Expr::Binary {
                        op: "==".into(),
                        lhs: std::mem::replace(value, Box::new(Expr::UnitLit)),
                        rhs: Box::new(Expr::BoolLit(false)),
                    };
                }
                _ => {}
            }
        });
    }
    if parser_id == ParserId::Lua {
        TypeChecker::new().resolve_complements(&mut normalized);
    }
    if parser_id == ParserId::Lua && semantics == crate::integer_semantics::IntegerSemantics::Word32
    {
        // Lua integers are signed64 values, unlike generic unsigned MMIO word
        // carriers. Reject before target-word folding can reinterpret the sign.
        crate::core_opt::map_decl_exprs_mut(&mut normalized.decls, &mut |expr| {
            if matches!(expr, Expr::IntLit(value) if i32::try_from(*value).is_err()) {
                *expr = Expr::Call {
                    callee: Box::new(Expr::Ident("#unsupported-lua-integer-word32".into())),
                    args: Vec::new(),
                };
            }
        });
    }
    if matches!(
        parser_id,
        ParserId::C | ParserId::Cpp | ParserId::ObjC | ParserId::ObjCpp
    ) {
        TypeChecker::new().resolve_c_value_promotions(&mut normalized);
        // Keep destination widths through conversion insertion, then give all
        // native consumers the same canonical carriers as other frontends.
        crate::core_bindings::rewrite_named_types(&mut normalized.decls, &|name| {
            crate::core_ir::integer_type_name(name).map(|_| Typ::Int)
        });
    }
    if parser_id == ParserId::Rust {
        TypeChecker::new().resolve_rust_floats(&mut normalized, ResolvePolicy::RustFloatValidate);
        rust_arrays::validate(&TypeChecker::new(), &mut normalized);
        rust_generics::normalize(&mut normalized);
        TypeChecker::new().resolve_rust_callables(&mut normalized, false, semantics);
        rust_arrays::normalize(&TypeChecker::new(), &mut normalized);
        // Stage stores after array value copies, before float source facts are erased.
        crate::lower_core::lower_value_spills(&mut normalized.decls);
        TypeChecker::new().resolve_complements(&mut normalized);
        crate::lower_core::lower_closures(&mut normalized);
        TypeChecker::new().resolve_rust_floats(&mut normalized, ResolvePolicy::RustFloatLower);
        erase_rust_float_metadata(&mut normalized);
        rust_integers::normalize(&TypeChecker::new(), &mut normalized, semantics);
        TypeChecker::new().resolve_rust_callables(&mut normalized, true, semantics);
        erase_rust_callable_metadata(&mut normalized);
        crate::lower_core::lower_inline_constants_with_semantics(&mut normalized.decls, semantics);
        refuse_rust_array_tuple_values(&mut normalized);
    }
    normalized
}

fn erase_rust_callable_metadata(module: &mut UnifiedModule) {
    use crate::core_ir::{rust_callables as source, type_proofs::ResolutionFailure};
    crate::core_ir::tuples::project_declarations(&mut module.decls, |decls| {
        crate::core_bindings::rewrite_named_types(decls, &|name| {
            if !source::is_encoded(name) {
                return None;
            }
            let value = Typ::Named(name.into());
            if let Some(failure) = crate::core_ir::type_proofs::failure_in_type(&value) {
                return Some(failure.typ());
            }
            if source::signature_of(&value).is_some() {
                Some(Typ::Int)
            } else {
                Some(
                    ResolutionFailure::Unsupported("malformed Rust callable signature".into())
                        .typ(),
                )
            }
        });
    });
    for decl in &mut module.decls {
        if let Decl::Function { body, .. } = decl {
            body.retain(|statement| {
                let Stmt::Expr(Expr::Unary { op, expr }) = statement else {
                    return true;
                };
                if !matches!(expr.as_ref(), Expr::UnitLit) {
                    return true;
                }
                if source::decode_alias_origin(op).is_some() {
                    return false;
                }
                if let Some(signature) = source::decode_declaration(op) {
                    let mut retained_failure = None;
                    signature.visit_types(&mut |child| {
                        retained_failure = crate::core_ir::type_proofs::prefer_failure(
                            retained_failure.take(),
                            crate::core_ir::type_proofs::failure_in_type(child),
                        );
                    });
                    return retained_failure.is_some();
                }
                true
            });
        }
    }
    let declared_functions: HashSet<String> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Function { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
        if let Expr::Call { callee, args } = expr
            && let Expr::Ident(name) = callee.as_ref()
            && matches!(
                name.as_str(),
                "std::hint::black_box" | "core::hint::black_box"
            )
            && !declared_functions.contains(name)
            && args.len() == 1
        {
            *expr = args.remove(0);
            return;
        }
        let Expr::Unary { op, expr: child } = expr else {
            return;
        };
        if source::is_declaration(op) || source::is_alias_origin(op) {
            let valid = (source::decode_declaration(op).is_some()
                || source::decode_alias_origin(op).is_some())
                && matches!(child.as_ref(), Expr::UnitLit);
            let mut retained_failure = None;
            if let Some(signature) = source::decode_declaration(op) {
                signature.visit_types(&mut |child| {
                    retained_failure = crate::core_ir::type_proofs::prefer_failure(
                        retained_failure.take(),
                        crate::core_ir::type_proofs::failure_in_type(child),
                    );
                });
            }
            *expr = if let Some(failure) = retained_failure {
                Expr::Ident(failure.symbol())
            } else if valid {
                Expr::UnitLit
            } else {
                Expr::Ident(
                    ResolutionFailure::Unsupported(
                        "malformed Rust callable declaration provenance".into(),
                    )
                    .symbol(),
                )
            };
        } else if source::is_cast(op) {
            // Every reached cast must have been checked and lowered by the scoped pass.
            *expr = Expr::Ident(
                ResolutionFailure::Unsupported("unresolved Rust callable cast".into()).symbol(),
            );
        }
    });
}

fn erase_rust_float_metadata(module: &mut UnifiedModule) {
    let parameters: HashMap<_, Vec<_>> = module
        .decls
        .iter()
        .filter_map(|decl| {
            let Decl::Function { name, params, .. } = decl else {
                return None;
            };
            Some((
                name.clone(),
                params
                    .iter()
                    .filter_map(|(name, typ)| {
                        (crate::core_ir::rust_floats::kind(typ)
                            == Some(crate::core_ir::rust_floats::Kind::F32))
                        .then_some(name.clone())
                    })
                    .collect(),
            ))
        })
        .collect();
    crate::core_ir::tuples::project_declarations(&mut module.decls, |decls| {
        crate::core_bindings::rewrite_named_types(decls, &|name| {
            crate::core_ir::rust_floats::is_proof(name).then_some(Typ::Float)
        });
        crate::core_opt::map_decl_exprs_mut(decls, &mut |expr| {
            // Contextual result edges have finished; remaining literals use Rust's default.
            if matches!(expr, Expr::Unary { op, .. } if crate::core_ir::rust_floats::is_literal(op))
            {
                match crate::core_ir::rust_floats::materialize_literal(expr, None) {
                    Ok(Some(value)) => *expr = Expr::FloatLit(crate::core_ir::FloatVal(value)),
                    Err(proof) => *expr = Expr::Ident(proof.symbol()),
                    Ok(None) => {}
                }
            }
            if matches!(expr, Expr::Unary { op, .. } if crate::core_ir::rust_floats::value_kind(op).is_some())
                && let Expr::Unary { expr: value, .. } = expr
            {
                *expr = (**value).clone();
            }
        });
    });
    for decl in &mut module.decls {
        if let Decl::Function { name, body, .. } = decl
            && let Some(params) = parameters.get(name)
        {
            let initializers = params.iter().map(|name| {
                Stmt::Assign(
                    name.clone(),
                    Expr::Unary {
                        op: "bits-to-float-f32".into(),
                        expr: Box::new(Expr::Unary {
                            op: "float-to-bits-f32".into(),
                            expr: Box::new(Expr::Ident(name.clone())),
                        }),
                    },
                )
            });
            body.splice(0..0, initializers);
        }
    }
}

/// Dynamic Array headers cannot implement Rust fixed-array tuple value copies.
/// Preserve refusal after tuple inference, while leaving unused schemas available.
fn refuse_rust_array_tuple_values(module: &mut UnifiedModule) {
    fn array_value(typ: &Typ) -> bool {
        match typ {
            Typ::Array(_) => true,
            Typ::Named(name) => crate::core_ir::tuples::fields(name)
                .is_some_and(|fields| fields.iter().any(|(_, typ)| array_value(typ))),
            _ => false,
        }
    }
    fn tuple_value(typ: &Typ) -> bool {
        matches!(typ, Typ::Named(name) if crate::core_ir::tuples::fields(name).is_some())
            && array_value(typ)
    }
    fn marker() -> Expr {
        Expr::Ident("#unsupported-rust-fixed-array-tuple-value".into())
    }
    for decl in &mut module.decls {
        if let Decl::Function {
            params, ret, body, ..
        } = decl
            && (tuple_value(ret) || params.iter().any(|(_, typ)| tuple_value(typ)))
        {
            body.insert(0, Stmt::Expr(marker()));
        }
    }
    crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
        if matches!(expr, Expr::StructInit { name, .. } if tuple_value(&Typ::Named(name.clone()))) {
            *expr = marker();
        }
        if let Expr::Closure {
            params, ret, body, ..
        } = expr
            && (tuple_value(ret) || params.iter().any(|(_, typ)| tuple_value(typ)))
        {
            body.insert(0, Stmt::Expr(marker()));
        }
    });
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
    if parser_id == ParserId::Lua && normalized == Typ::Void {
        let checker = TypeChecker::new();
        // Only owned literal/local value kinds prove Lua return inference. Generic
        // operator typing does not establish Lua truth, modulo, or power semantics.
        let resolve = |value: &Expr, env: &HashMap<String, Typ>| {
            let typ = match value {
                Expr::Ident(name) => env.get(name).cloned(),
                _ => expr_type_hint(value),
            }?;
            matches!(
                typ.canonical(),
                Typ::Int | Typ::Float | Typ::Bool | Typ::String
            )
            .then_some(typ)
        };
        let update = |stmt: &Stmt, env: &mut HashMap<String, Typ>| {
            match stmt {
                Stmt::Let(name, annotation, value) => {
                    let inferred = resolve(value, env);
                    if let Some(annotation) = annotation
                        && inferred.as_ref().map(Typ::canonical) != Some(annotation.canonical())
                    {
                        return None;
                    }
                    if inferred.is_none() {
                        env.remove(name);
                    }
                }
                Stmt::Assign(name, value) => {
                    let typ = resolve(value, env)?;
                    // Branch/loop environments are scoped copies: require a stable
                    // kind instead of incorrectly inferring a join from one path.
                    if env.get(name).map(Typ::canonical) != Some(typ.canonical()) {
                        return None;
                    }
                    env.insert(name.clone(), typ);
                }
                _ => {}
            }
            Some(())
        };
        return checker
            .infer_return_with(
                body,
                &checker.collect_decl_facts(&[]),
                &HashMap::new(),
                &resolve,
                &update,
            )
            .unwrap_or(normalized);
    }
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
        Expr::UnitLit => Some(Typ::Void),
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum CIntegerKind {
    I32,
    U32,
}

impl CIntegerKind {
    fn typ(self) -> Typ {
        Typ::Named(
            match self {
                Self::I32 => "#integer-i32",
                Self::U32 => "#integer-u32",
            }
            .into(),
        )
    }
    fn common(left: Option<Self>, right: Option<Self>) -> Option<Self> {
        match (left?, right?) {
            (Self::U32, _) | (_, Self::U32) => Some(Self::U32),
            _ => Some(Self::I32),
        }
    }
}

fn c_expression_leaf_kind(expr: &Expr, typ: Option<&Typ>) -> Option<CIntegerKind> {
    let name = match expr {
        Expr::Unary { op, .. } if op == "!" => return Some(CIntegerKind::I32),
        Expr::Unary { op, .. } if op == "c-integer-literal-i32" => return Some(CIntegerKind::I32),
        Expr::Unary { op, .. } if op == "c-integer-literal-u32" => return Some(CIntegerKind::U32),
        Expr::Call { callee, args } if matches!(callee.as_ref(), Expr::Ident(name) if name == "c-numeric-cast") =>
        {
            let [_, Expr::StringLit(target)] = args.as_slice() else {
                return None;
            };
            if target == "Bool" {
                return Some(CIntegerKind::I32);
            }
            target.as_str()
        }
        _ => match typ? {
            Typ::Bool => return Some(CIntegerKind::I32),
            Typ::Named(name) => crate::core_ir::integer_type_name(name)?,
            _ => return None,
        },
    };
    match name {
        "u32" => Some(CIntegerKind::U32),
        "i8" | "u8" | "i16" | "u16" | "i32" => Some(CIntegerKind::I32),
        _ => None,
    }
}

fn c_unsigned_value(value: Expr) -> Expr {
    Expr::Binary {
        op: "&".into(),
        lhs: Box::new(value),
        rhs: Box::new(Expr::IntLit(0xffff_ffff)),
    }
}

fn resolve_c_unsigned_expression(
    expr: &mut Expr,
    kind: Option<CIntegerKind>,
    operands: [Option<CIntegerKind>; 2],
) {
    let Expr::Binary { op, lhs, rhs } = expr else {
        if kind == Some(CIntegerKind::U32)
            && (matches!(expr, Expr::Unary { op, .. } if matches!(op.as_str(), "-" | "~"))
                || matches!(expr, Expr::Conditional { .. }))
        {
            *expr = c_unsigned_value(expr.clone());
        }
        return;
    };
    let compare = matches!(op.as_str(), "==" | "!=" | "<" | ">" | "<=" | ">=");
    let numeric = matches!(
        op.as_str(),
        "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "xor" | "<<" | ">>"
    ) || compare;
    let common = CIntegerKind::common(operands[0], operands[1]);
    // Source long widths and literal ranks beyond int/unsigned int are not inferred.
    if numeric
        && !matches!(op.as_str(), "<<" | ">>")
        && operands.contains(&Some(CIntegerKind::U32))
        && common.is_none()
    {
        *expr = Expr::Call {
            callee: Box::new(Expr::Ident("unsupported-c-integer-rank".into())),
            args: vec![expr.clone()],
        };
        return;
    }
    if compare && common == Some(CIntegerKind::U32) {
        if matches!(op.as_str(), "==" | "!=") {
            **lhs = c_unsigned_value((**lhs).clone());
            **rhs = c_unsigned_value((**rhs).clone());
        } else if let Some(value) = crate::core_ir::unsigned_compare_expr(
            op,
            (**lhs).clone(),
            (**rhs).clone(),
            32,
            crate::integer_semantics::IntegerSemantics::Word64,
        ) {
            *expr = value;
        }
    } else if kind == Some(CIntegerKind::U32) && numeric {
        match op.as_str() {
            "/" | "%" => {
                *op = if op == "/" { "udiv" } else { "urem" }.into();
                **lhs = c_unsigned_value((**lhs).clone());
                **rhs = c_unsigned_value((**rhs).clone());
            }
            ">>" => {
                *op = "lshr".into();
                **lhs = c_unsigned_value((**lhs).clone());
            }
            _ => {}
        }
        *expr = c_unsigned_value(expr.clone());
    }
}

fn c_fixed_integer_name(name: &str) -> bool {
    matches!(name, "i8" | "u8" | "i16" | "u16" | "i32" | "u32")
}

fn normalize_parser_type(parser_id: ParserId, typ: &Typ) -> Typ {
    if matches!(
        parser_id,
        ParserId::C | ParserId::Cpp | ParserId::ObjC | ParserId::ObjCpp
    ) && let Typ::Named(name) = typ
    {
        if crate::core_ir::integer_type_name(name).is_some() {
            return typ.clone();
        }
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
    #[test]
    fn option_schemas_coalesce_only_after_metadata_erasure() {
        for source in [
            "fn first(x:Option<fn(i64)->i64>)->Option<fn(i64)->i64>{x}fn second(x:Option<fn(bool)->bool>)->Option<fn(bool)->bool>{x}fn main()->i64{42}",
            "fn first(x:Option<[i64;2]>)->Option<[i64;2]>{x}fn second(x:Option<[i64;3]>)->Option<[i64;3]>{x}fn main()->i64{42}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let result = |wanted| {
                parsed
                    .decls
                    .iter()
                    .find_map(|decl| match decl {
                        Decl::Function { name, ret, .. } if name == wanted => Some(ret),
                        _ => None,
                    })
                    .unwrap()
            };
            assert_ne!(result("first"), result("second"), "source types differ");
            let normalized =
                super::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            let report = crate::core_ir_verifier::verify_for_entry(&normalized, "main");
            assert!(report.ok, "{report:?}");
        }
    }

    #[test]
    fn numeric_cast_type_projects_destination_before_value_spills() {
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        let env = HashMap::from([("value".into(), Typ::Named("#rust-integer-i32".into()))]);
        for (target, expected) in [
            ("i64", Some(Typ::Int)),
            ("u8", Some(Typ::Int)),
            ("usize", Some(Typ::Int)),
            ("f32", Some(Typ::Float)),
            ("f64", Some(Typ::Float)),
            ("bool", None),
            ("u7", None),
            ("not-a-type", None),
        ] {
            let expression = Expr::Unary {
                op: format!("numeric-cast-{target}"),
                expr: Box::new(Expr::Ident("value".into())),
            };
            assert_eq!(
                checker.expr_type(&expression, &facts, &env),
                expected,
                "{target}"
            );
        }
        let source_value = Expr::Unary {
            op: "rust-integer-value-i64".into(),
            expr: Box::new(Expr::Unary {
                op: "numeric-cast-i64".into(),
                expr: Box::new(Expr::Ident("value".into())),
            }),
        };
        assert_eq!(
            checker.expr_type(&source_value, &facts, &env),
            Some(Typ::Named("#rust-integer-i64".into()))
        );
    }

    #[test]
    fn float_array_copy_preserves_source_type_until_lowering() {
        let source = "fn main()->i64{let original=[1.0f32,16777217.0];let mut copied=original;copied[0]=2.0;if original[0]==1.0 && copied[0]==2.0 && original[1]==16777216.0 && copied[1]==16777216.0{42}else{7}}";
        let mut module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        let normalized = super::normalize_module(crate::parser_registry::ParserId::Rust, &module);
        assert!(
            !crate::core_ir::type_proofs::present(&normalized.decls),
            "full normalization: {normalized:#?}"
        );
        assert!(
            !crate::core_ir::rust_floats::present(&normalized.decls),
            "leftover source metadata: {normalized:#?}"
        );
        let repeated = super::normalize_module(crate::parser_registry::ParserId::Rust, &normalized);
        assert!(
            !crate::core_ir::type_proofs::present(&repeated.decls),
            "repeated normalization: {repeated:#?}"
        );
        let checker = super::TypeChecker::new();
        checker.resolve_rust_floats(&mut module, super::ResolvePolicy::RustFloatValidate);
        super::rust_arrays::validate(&checker, &mut module);
        super::rust_generics::normalize(&mut module);
        super::rust_arrays::normalize(&checker, &mut module);
        checker.resolve_complements(&mut module);
        let before = format!("{module:#?}");
        checker.resolve_rust_floats(&mut module, super::ResolvePolicy::RustFloatLower);
        assert!(
            !crate::core_ir::type_proofs::present(&module.decls),
            "before: {before}\nafter: {module:#?}"
        );
    }

    use super::uses_family_typecheck;
    use crate::parser_registry::ParserId;

    #[test]
    fn c_intrinsic_arguments_use_c_value_promotions_without_changing_defined_abis() {
        use crate::core_ir::{Decl, Expr};
        for name in [
            "load8", "load16", "load32", "load64", "store8", "store16", "store32", "store64",
        ] {
            let store = name.starts_with("store");
            for prototype in [false, true] {
                let declaration = if !prototype {
                    String::new()
                } else if store {
                    format!("extern void {name}(long long, long long);\n")
                } else {
                    format!("extern long long {name}(long long);\n")
                };
                let body = if store {
                    format!("{name}(0,flag);return 42;")
                } else {
                    format!("return {name}(flag);")
                };
                let source = format!("{declaration}long long main(void){{_Bool flag=1;{body}}}");
                let path = std::env::temp_dir().join(format!(
                    "in-c-mmio-{}-{}.c",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::write(&path, source).unwrap();
                let parsed = crate::compiler::tree_front::parse_polyglot_file(ParserId::C, &path);
                std::fs::remove_file(&path).unwrap();
                let mut normalized = super::normalize_module(ParserId::C, &parsed.unwrap());
                let report = crate::core_ir_verifier::verify_for_entry(&normalized, "main");
                assert!(report.ok, "{name}/{prototype}: {report:?}");
                let mut conversions = 0;
                crate::core_opt::map_decl_exprs_mut(&mut normalized.decls, &mut |expr| {
                    if matches!(expr, Expr::Unary { op, .. } if op == "bool-to-int") {
                        conversions += 1;
                    }
                });
                assert_eq!(conversions, 1, "{name}/{prototype}");
            }
        }
        let parsed = crate::in_lang_parse::parse_in_source("fn load32(flag:Bool)->Int{if flag{return 42;}return 0;}\nfn main()->Int{let flag:Bool=true;return load32(flag);}").unwrap();
        let normalized = super::normalize_module(ParserId::C, &parsed);
        let Decl::Function { body, .. } = normalized.decls.last().unwrap() else {
            panic!("main");
        };
        assert!(
            matches!(body.last(), Some(crate::core_ir::Stmt::Return(Some(Expr::Call {args,..}))) if matches!(args.as_slice(), [Expr::Ident(_)]))
        );
        assert!(crate::core_ir_verifier::verify_for_entry(&normalized, "main").ok);

        let strict = crate::in_lang_parse::parse_in_source(
            "fn main()->Int{let flag:Bool=true;return load32(flag);}",
        )
        .unwrap();
        assert!(!crate::core_ir_verifier::verify_for_entry(&strict, "main").ok);
    }

    #[test]
    fn c_conditional_converts_selected_signed_branch_to_common_unsigned_kind() {
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![Stmt::Return(Some(Expr::Conditional {
                cond: Box::new(Expr::BoolLit(true)),
                then_expr: Box::new(Expr::Unary {
                    op: "c-integer-literal-i32".into(),
                    expr: Box::new(Expr::IntLit(-1)),
                }),
                else_expr: Box::new(Expr::Unary {
                    op: "c-integer-literal-u32".into(),
                    expr: Box::new(Expr::IntLit(0)),
                }),
            }))],
        }]);
        let mut normalized = super::normalize_module(ParserId::C, &module);
        assert_eq!(
            normalized,
            super::normalize_module(ParserId::C, &normalized)
        );
        crate::core_opt::map_decl_exprs_mut(&mut normalized.decls, &mut |expr| {
            *expr = crate::core_opt::fold_expr(expr.clone());
        });
        let Decl::Function { body, .. } = &normalized.decls[0] else {
            panic!("main")
        };
        assert!(
            matches!(&body[0], Stmt::Return(Some(Expr::Binary { op, lhs, rhs }))
            if op == "&" && matches!(lhs.as_ref(), Expr::Conditional { then_expr, .. } if **then_expr == Expr::IntLit(-1)) && **rhs == Expr::IntLit(4294967295))
        );
    }

    #[test]
    fn c_destination_widths_convert_before_all_type_aliases_are_erased() {
        let parsed = UnifiedModule::new(vec![
            Decl::Struct {
                name: "Box".into(),
                fields: vec![("small".into(), Typ::Named("#integer-u8".into()))],
                type_params: vec![],
            },
            Decl::Global {
                name: "global".into(),
                typ: Typ::Named("#integer-u16".into()),
                init: Some(Box::new(Expr::IntLit(65578))),
                mutable: true,
                constant_value: false,
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![
                    Stmt::Let(
                        "box".into(),
                        Some(Typ::Named("Box".into())),
                        Expr::StructInit {
                            name: "Box".into(),
                            fields: vec![("small".into(), Expr::IntLit(298))],
                        },
                    ),
                    Stmt::FieldAssign {
                        base: Expr::Ident("box".into()),
                        name: "small".into(),
                        value: Expr::IntLit(554),
                    },
                    Stmt::Return(Some(Expr::Field {
                        base: Box::new(Expr::Ident("box".into())),
                        name: "small".into(),
                    })),
                ],
            },
        ]);
        let mut normalized = super::normalize_module(ParserId::C, &parsed);
        let report = crate::core_ir_verifier::verify_for_entry(&normalized, "main");
        assert!(report.ok, "{report:?}");
        let remaining = std::cell::Cell::new(0);
        crate::core_bindings::rewrite_named_types(&mut normalized.decls, &|name| {
            if crate::core_ir::integer_type_name(name).is_some() {
                remaining.set(remaining.get() + 1);
            }
            None
        });
        assert_eq!(remaining.get(), 0);
        assert_eq!(
            normalized,
            super::normalize_module(ParserId::C, &normalized)
        );
        crate::core_opt::map_decl_exprs_mut(&mut normalized.decls, &mut |expr| {
            *expr = crate::core_opt::fold_expr(expr.clone());
        });
        assert!(
            matches!(&normalized.decls[1], Decl::Global { typ: Typ::Int, init: Some(value), .. } if **value == Expr::IntLit(42))
        );
        let mut checked = 0;
        crate::core_opt::map_decl_exprs_mut(&mut normalized.decls, &mut |expr| {
            if let Expr::StructInit { fields, .. } = expr {
                assert_eq!(fields[0].1, Expr::IntLit(42));
                checked += 1;
            }
        });
        assert_eq!(checked, 1);
        let Decl::Function { body, .. } = &normalized.decls[2] else {
            panic!("main")
        };
        assert!(matches!(
            &body[1],
            Stmt::FieldAssign {
                value: Expr::IntLit(42),
                ..
            }
        ));
    }

    #[test]
    fn c_narrow_casts_normalize_width_and_preserve_one_operand_use() {
        use crate::core_ir::{Decl, Expr, Stmt};
        for (target, value, expected) in [
            ("u8", 256i64, 0),
            ("u8", -1, 255),
            ("u16", 65536, 0),
            ("u16", -1, 65535),
            ("i8", -128, -128),
            ("i8", 127, 127),
            ("i16", -32768, -32768),
            ("i16", 32767, 32767),
            ("i32", 4294967295, -1),
            ("u32", -1, 4294967295),
        ] {
            let source = format!("fn main()->Int{{return c-numeric-cast({value},\"{target}\");}}");
            let parsed = crate::in_lang_parse::parse_in_source(&source).unwrap();
            let module = super::normalize_module(ParserId::C, &parsed);
            assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
            let Decl::Function { body, .. } = &module.decls[0] else {
                panic!("main")
            };
            let Stmt::Return(Some(value)) = &body[0] else {
                panic!("return")
            };
            for semantics in [
                crate::integer_semantics::IntegerSemantics::Word32,
                crate::integer_semantics::IntegerSemantics::Word64,
            ] {
                assert_eq!(
                    crate::core_opt::map_expr(value.clone(), &mut |expr| {
                        crate::core_opt::fold_expr_with_semantics(expr, semantics)
                    }),
                    Expr::IntLit(semantics.normalize_word(expected).unwrap())
                );
            }
        }
        let parsed = crate::in_lang_parse::parse_in_source(
            "fn next()->Int{return 511;}\nfn main()->Int{return c-numeric-cast(next(),\"u8\");}",
        )
        .unwrap();
        let mut module = super::normalize_module(ParserId::C, &parsed);
        let mut calls = 0;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if matches!(expr, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::Ident(name) if name == "next"))
            {
                calls += 1;
            }
        });
        assert_eq!(calls, 1);
        for expression in [
            "c-numeric-cast(42,\"u7\")",
            "c-numeric-cast(\"text\",\"u8\")",
        ] {
            let parsed = crate::in_lang_parse::parse_in_source(&format!(
                "fn main()->Int{{return {expression};}}"
            ))
            .unwrap();
            let module = super::normalize_module(ParserId::C, &parsed);
            assert!(!crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        }
    }

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
    fn c_family_normalization_does_not_expand_typecheck_capabilities() {
        for parser in [ParserId::C, ParserId::Cpp, ParserId::ObjC, ParserId::ObjCpp] {
            assert!(uses_family_normalization(parser));
        }
        assert!(!uses_family_typecheck(ParserId::ObjC));
        assert!(
            !crate::language_support::language_support_for_parser("objc")
                .unwrap()
                .can_typecheck()
        );
        assert!(uses_family_typecheck(ParserId::ObjCpp));
        assert_eq!(
            uses_family_normalization(ParserId::Go),
            uses_family_typecheck(ParserId::Go)
        );
        assert_eq!(
            uses_family_normalization(ParserId::FSharp),
            uses_family_typecheck(ParserId::FSharp)
        );
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
    fn match_bindings_keep_field_types() {
        let m = module(vec![
            Decl::Struct {
                name: "Text".into(),
                fields: vec![("value".into(), Typ::String)],
                type_params: vec![],
            },
            function(
                "main",
                Typ::Int,
                vec![("text".into(), Typ::Named("Text".into()))],
                vec![Stmt::Match {
                    scrutinee: Expr::Ident("text".into()),
                    arms: vec![crate::core_ir::MatchArm {
                        pattern: "Text { value: item }".into(),
                        guard: None,
                        body: vec![Stmt::Return(Some(Expr::Ident("item".into())))],
                    }],
                }],
            ),
        ]);
        let errors = TypeChecker::new().check_module(&m).unwrap_err();
        assert!(errors.iter().any(|error| matches!(
            error,
            TypeError::ReturnTypeMismatch {
                got: Typ::String,
                ..
            }
        )));
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
    fn recursive_unowned_projection_inference_terminates_without_guessing_widths() {
        for (source, supported) in [
            (
                "fn inspect(choose:bool)->i64{let mut node=external();while choose{if choose{node=node.left;}else{node=node.right;}}node.value+1}",
                false,
            ),
            (
                "struct Node{value:i64}fn inspect()->i64{let root=Node{value:41};let next=root;next.value+1}",
                true,
            ),
        ] {
            let source = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let normalized = normalize_module(crate::parser_registry::ParserId::Rust, &source);
            let report = crate::core_ir_verifier::verify_for_entry(&normalized, "inspect");
            assert_eq!(report.ok, supported, "{report:?}");
            if !supported {
                assert!(
                    report
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("aggregate source dependency")),
                    "{report:?}"
                );
            }
        }
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
    #[test]
    fn inferred_fixed_array_types_persist_only_when_concrete() {
        let checker = TypeChecker::new();
        let facts = checker.collect_facts(&module(vec![]));
        let mut env = HashMap::from([
            ("unknown".into(), Typ::Generic("T".into())),
            ("integer".into(), Typ::Int),
        ]);
        let cases = [
            (
                Expr::ArrayLit(vec![Expr::IntLit(20), Expr::IntLit(22)]),
                Some(Typ::Array(Box::new(Typ::Int))),
            ),
            (
                Expr::ArrayLit(vec![Expr::BoolLit(true)]),
                Some(Typ::Array(Box::new(Typ::Bool))),
            ),
            (
                Expr::ArrayLit(vec![Expr::FloatLit(crate::core_ir::FloatVal(1.0))]),
                Some(Typ::Array(Box::new(Typ::Float))),
            ),
            (
                Expr::ArrayLit(vec![Expr::StringLit("λ".into())]),
                Some(Typ::Array(Box::new(Typ::String))),
            ),
            (
                Expr::ArrayLit(vec![Expr::ArrayLit(vec![Expr::IntLit(42)])]),
                Some(Typ::Array(Box::new(Typ::Array(Box::new(Typ::Int))))),
            ),
            (Expr::ArrayLit(vec![]), None),
            (Expr::ArrayLit(vec![Expr::ArrayLit(vec![])]), None),
            (Expr::ArrayLit(vec![Expr::Ident("unknown".into())]), None),
        ];
        for (index, (value, expected)) in cases.into_iter().enumerate() {
            let mut body = vec![Stmt::Let(format!("array{index}"), None, value)];
            checker.resolve_typed_body(&mut body, &facts, &mut env, None, ResolvePolicy::Ordinary);
            let Stmt::Let(_, actual, _) = &body[0] else {
                panic!("expected binding")
            };
            assert_eq!(actual, &expected, "case {index}");
        }
    }

    #[test]
    fn inferred_fixed_arrays_preserve_annotations_and_other_inference() {
        let checker = TypeChecker::new();
        let facts = checker.collect_facts(&module(vec![function_with_ret(
            "pending",
            Typ::Void,
            vec![],
        )]));
        let explicit = Typ::Array(Box::new(Typ::Int));
        let mut body = vec![
            Stmt::Let(
                "empty".into(),
                Some(explicit.clone()),
                Expr::ArrayLit(vec![]),
            ),
            Stmt::Let(
                "mismatch".into(),
                Some(explicit.clone()),
                Expr::ArrayLit(vec![Expr::BoolLit(true)]),
            ),
            Stmt::Let(
                "pending_value".into(),
                None,
                Expr::Call {
                    callee: Box::new(Expr::Ident("pending".into())),
                    args: vec![],
                },
            ),
            Stmt::Let("flag".into(), None, Expr::BoolLit(true)),
            Stmt::Let("integer".into(), Some(Typ::Int), Expr::BoolLit(true)),
        ];
        checker.resolve_typed_body(
            &mut body,
            &facts,
            &mut HashMap::new(),
            None,
            ResolvePolicy::CValues,
        );
        for stmt in &body[..2] {
            assert!(matches!(stmt, Stmt::Let(_, Some(typ), _) if typ == &explicit));
        }
        assert!(matches!(&body[2], Stmt::Let(_, None, _)));
        assert!(matches!(&body[3], Stmt::Let(_, None, Expr::BoolLit(true))));
        assert!(
            matches!(&body[4], Stmt::Let(_, Some(Typ::Int), Expr::Unary { op, .. }) if op == "bool-to-int")
        );
    }
}

#[cfg(test)]
mod rust_array_tuple_value_tests {
    use super::*;

    #[test]
    fn rust_array_tuple_copies_remain_refused_after_tuple_inference() {
        for source in [
            "fn main()->i64{let mut a=([42,0],1);let b=a;a.0[0]=0;b.0[0]}",
            "fn main()->i64{let values=[42,0];let mut a=(values,1);let b=a;a.0[0]=0;b.0[0]}",
            "fn main()->i64{let mut a=((([42,0],1),2),3);let b=a;a.0.0.0[0]=0;b.0.0.0[0]}",
            "fn take(mut value:([i64;2],i64))->i64{value.0[0]=0;value.0[0]} fn main()->i64{take(([42,0],1))}",
            "fn make()->([i64;2],i64){([42,0],1)} fn main()->i64{make().0[0]}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module = normalize_module(ParserId::Rust, &parsed);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(!report.ok, "{source}");
            assert!(
                report.reason.as_deref().is_some_and(
                    |reason| reason.contains("#unsupported-rust-fixed-array-tuple-value")
                ),
                "{report:?}: {source}"
            );
            crate::lower_core::desugar_module(&mut module);
            match crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable,
            ) {
                Err(_) => {}
                Ok(lowered) => assert!(
                    lowered.degradations.iter().any(|item| item.reachable
                        && item
                            .reason
                            .contains("#unsupported-rust-fixed-array-tuple-value")),
                    "{source}: {:?}",
                    lowered.degradations
                ),
            }
            assert!(
                crate::native_emit::x86_64_lower::lower_module(&module, "main").is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn unused_array_tuple_schemas_and_ordinary_tuple_values_remain_usable() {
        let parsed = crate::compiler::rust_front::parse_rust_source(
            "fn helper()->i64{42} fn main()->i64{let values=(20,22);values.0+values.1}",
        )
        .unwrap();
        let mut module = parsed;
        let Typ::Named(name) =
            crate::core_ir::tuples::typ(&[Typ::Array(Box::new(Typ::Int)), Typ::Int])
        else {
            unreachable!()
        };
        module.decls.push(Decl::Struct {
            fields: crate::core_ir::tuples::fields(&name).unwrap(),
            name,
            type_params: vec![],
        });
        let mut module = normalize_module(ParserId::Rust, &module);
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        crate::lower_core::desugar_module(&mut module);
        assert!(
            crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .is_ok()
        );
        assert!(crate::native_emit::x86_64_lower::lower_module(&module, "main").is_ok());
    }
}

#[cfg(test)]
mod lua_word32_integer_tests {
    use super::*;
    use crate::integer_semantics::IntegerSemantics;
    #[test]
    fn lua_returned_locals_use_shared_dataflow_inference() {
        for (value, expected) in [
            (Expr::IntLit(42), Typ::Int),
            (Expr::FloatLit(crate::core_ir::FloatVal(42.0)), Typ::Float),
            (Expr::BoolLit(true), Typ::Bool),
            (Expr::StringLit("value".into()), Typ::String),
        ] {
            let module = UnifiedModule::new(vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Void,
                type_params: vec![],
                body: vec![
                    Stmt::Let("value".into(), None, value),
                    Stmt::Return(Some(Expr::Ident("value".into()))),
                ],
            }]);
            let normalized = normalize_module(ParserId::Lua, &module);
            assert!(matches!(&normalized.decls[0], Decl::Function {ret, ..} if ret == &expected));
            assert!(typecheck_for_parser(ParserId::Lua, &module).is_ok());
        }
    }
    fn lua_module(body: Vec<Stmt>) -> UnifiedModule {
        UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Void,
            type_params: vec![],
            body,
        }])
    }

    #[test]
    fn lua_unowned_return_operations_do_not_gain_integer_abi() {
        let operations = [
            Expr::Unary {
                op: "!".into(),
                expr: Box::new(Expr::IntLit(0)),
            },
            Expr::Binary {
                op: "%".into(),
                lhs: Box::new(Expr::IntLit(-5)),
                rhs: Box::new(Expr::IntLit(2)),
            },
            Expr::Binary {
                op: "^".into(),
                lhs: Box::new(Expr::IntLit(2)),
                rhs: Box::new(Expr::IntLit(3)),
            },
        ];
        for value in operations {
            for body in [
                vec![Stmt::Return(Some(value.clone()))],
                vec![
                    Stmt::Let("value".into(), None, Expr::IntLit(42)),
                    Stmt::Assign("value".into(), value.clone()),
                    Stmt::Return(Some(Expr::Ident("value".into()))),
                ],
                vec![
                    Stmt::Let("value".into(), None, Expr::IntLit(42)),
                    Stmt::If {
                        cond: Expr::BoolLit(true),
                        then_body: vec![Stmt::Assign("value".into(), value.clone())],
                        else_body: vec![],
                    },
                    Stmt::Return(Some(Expr::Ident("value".into()))),
                ],
            ] {
                let module = lua_module(body);
                let normalized = normalize_module(ParserId::Lua, &module);
                assert!(
                    matches!(&normalized.decls[0], Decl::Function {ret, ..} if ret == &Typ::Void)
                );
                assert!(typecheck_for_parser(ParserId::Lua, &module).is_err());
            }
        }
    }

    #[test]
    fn lua_return_inference_invalidates_shadow_and_requires_consistent_returns() {
        let unknown = Expr::Call {
            callee: Box::new(Expr::Ident("unknown".into())),
            args: vec![],
        };
        for body in [
            vec![
                Stmt::Let("value".into(), None, Expr::IntLit(42)),
                Stmt::Let("value".into(), None, unknown.clone()),
                Stmt::Return(Some(Expr::Ident("value".into()))),
            ],
            vec![Stmt::If {
                cond: Expr::Ident("flag".into()),
                then_body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                else_body: vec![Stmt::Return(Some(Expr::BoolLit(true)))],
            }],
        ] {
            assert_eq!(
                normalize_function_ret(ParserId::Lua, &Typ::Void, &body),
                Typ::Void
            );
        }
        // An unused unknown initializer must not erase a separately proven return.
        assert_eq!(
            normalize_function_ret(
                ParserId::Lua,
                &Typ::Void,
                &[
                    Stmt::Let("unused".into(), None, unknown),
                    Stmt::Let("value".into(), None, Expr::IntLit(20)),
                    Stmt::Assign("value".into(), Expr::IntLit(42)),
                    Stmt::Return(Some(Expr::Ident("value".into()))),
                ]
            ),
            Typ::Int
        );
    }

    #[test]
    fn lua_signed_literal_boundary_preserves_generic_unsigned_carriers() {
        for (value, refused) in [
            (42, false),
            (i32::MIN as i64, false),
            (i32::MAX as i64, false),
            (2147483648, true),
            (4294967295, true),
            (i64::MIN, true),
        ] {
            let module = UnifiedModule::new(vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![Stmt::Return(Some(Expr::IntLit(value)))],
            }]);
            for parser in [ParserId::Lua, ParserId::In] {
                for semantics in [IntegerSemantics::Word32, IntegerSemantics::Word64] {
                    let mut prepared = module.clone();
                    crate::lower_core::desugar_module_with_semantics(&mut prepared, semantics);
                    let normalized = normalize_module_with_semantics(parser, &prepared, semantics);
                    let Decl::Function { body, .. } = &normalized.decls[0] else {
                        panic!()
                    };
                    let expected_refusal =
                        refused && parser == ParserId::Lua && semantics == IntegerSemantics::Word32;
                    assert_eq!(
                        matches!(&body[0], Stmt::Return(Some(Expr::Call {callee, args})) if matches!(callee.as_ref(), Expr::Ident(name) if name == "#unsupported-lua-integer-word32") && args.is_empty()),
                        expected_refusal
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod boolean_negation_inference_tests {
    use super::TypeChecker;
    use crate::core_ir::{Expr, Typ};
    use std::collections::HashMap;

    #[test]
    fn boolean_negation_has_integer_facts_in_nested_expressions() {
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        for typ in [Typ::Bool, Typ::Named("Bool".into())] {
            let env = HashMap::from([("flag".into(), typ)]);
            let neg = Expr::Unary {
                op: "-".into(),
                expr: Box::new(Expr::Ident("flag".into())),
            };
            assert_eq!(checker.expr_type(&neg, &facts, &env), Some(Typ::Int));
            assert_eq!(
                checker.checked_expr_type(&neg, &facts, &env),
                Some(Typ::Int)
            );
            let comparison = Expr::Binary {
                op: "==".into(),
                lhs: Box::new(neg.clone()),
                rhs: Box::new(Expr::IntLit(-1)),
            };
            assert_eq!(
                checker.checked_expr_type(&comparison, &facts, &env),
                Some(Typ::Bool)
            );
            assert_eq!(
                checker.expr_type(&Expr::ArrayLit(vec![neg.clone()]), &facts, &env),
                Some(Typ::Array(Box::new(Typ::Int)))
            );
            let conditional = Expr::Conditional {
                cond: Box::new(Expr::BoolLit(true)),
                then_expr: Box::new(neg.clone()),
                else_expr: Box::new(Expr::IntLit(0)),
            };
            assert_eq!(
                checker.checked_expr_type(&conditional, &facts, &env),
                Some(Typ::Int)
            );
            let addition = Expr::Binary {
                op: "+".into(),
                lhs: Box::new(neg),
                rhs: Box::new(Expr::IntLit(1)),
            };
            assert_eq!(
                checker.checked_expr_type(&addition, &facts, &env),
                Some(Typ::Int)
            );
        }
        for typ in [Typ::Int, Typ::Float, Typ::Named("#rust-integer-u32".into())] {
            let env = HashMap::from([("value".into(), typ.clone())]);
            let neg = Expr::Unary {
                op: "-".into(),
                expr: Box::new(Expr::Ident("value".into())),
            };
            assert_eq!(checker.expr_type(&neg, &facts, &env), Some(typ));
        }
    }
}

#[cfg(test)]
mod rust_unary_binding_tests {
    use super::*;
    use crate::boundary_ir::UnaryMethodBindings;
    use crate::core_ir::rust_references::{self, ReferenceType};

    fn reference(mutable: bool, referent: Typ, runtime: Typ) -> Typ {
        rust_references::typ(ReferenceType {
            mutable,
            referent,
            runtime,
        })
    }
    fn function(name: &str, params: Vec<(String, Typ)>, value: Expr) -> Decl {
        Decl::Function {
            name: name.into(),
            params,
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(value))],
            type_params: vec![],
        }
    }
    fn neg(value: Expr) -> Expr {
        Expr::Unary {
            op: rust_references::NEGATE_VALUE.into(),
            expr: Box::new(value),
        }
    }
    fn returned(module: &UnifiedModule, name: &str) -> Expr {
        let Decl::Function { body, .. } = module
            .decls
            .iter()
            .find(|decl| matches!(decl,Decl::Function{name:found,..}if found==name))
            .unwrap()
        else {
            unreachable!()
        };
        let Stmt::Return(Some(value)) = &body[0] else {
            panic!("expected return")
        };
        value.clone()
    }
    #[test]
    fn exact_owned_shared_and_mutable_proofs_select_only_their_ufcs_targets() {
        let owned = Typ::Named("Row".into());
        let shared = reference(false, owned.clone(), Typ::Named("&Row".into()));
        let mutable = reference(true, owned.clone(), Typ::Named("&mutRow".into()));
        let mut bindings = UnaryMethodBindings::new();
        let mut declarations = vec![Decl::Struct {
            name: "Row".into(),
            fields: vec![("x".into(), Typ::Int)],
            type_params: vec![],
        }];
        for (key, typ, target, result) in [
            ("Row", owned, "Row::std::ops::Neg::neg", 77),
            ("&Row", shared, "&Row::std::ops::Neg::neg", 88),
            ("&mut Row", mutable, "&mutRow::std::ops::Neg::neg", 99),
        ] {
            bindings.insert((key.into(), "-".into()), Ok(target.into()));
            declarations.push(function(
                target,
                vec![("self".into(), typ.clone())],
                Expr::IntLit(result),
            ));
            declarations.push(function(
                key,
                vec![("value".into(), typ)],
                neg(Expr::Ident("value".into())),
            ));
        }
        let mut module = UnifiedModule::new(declarations);
        TypeChecker::new()
            .resolve_unary_methods(&mut module, &bindings)
            .unwrap();
        for (key, target) in [
            ("Row", "Row::std::ops::Neg::neg"),
            ("&Row", "&Row::std::ops::Neg::neg"),
            ("&mut Row", "&mutRow::std::ops::Neg::neg"),
        ] {
            assert_eq!(
                returned(&module, key),
                Expr::Call {
                    callee: Box::new(Expr::Ident(target.into())),
                    args: vec![Expr::Ident("value".into())]
                }
            );
        }
    }
    #[test]
    fn borrowed_holder_field_retains_its_raw_reference_proof() {
        let row = Typ::Named("Row".into());
        let field = reference(false, row.clone(), row);
        let holder = reference(
            false,
            Typ::Named("Holder".into()),
            Typ::Named("Holder".into()),
        );
        let operand = Expr::Field {
            base: Box::new(Expr::Ident("holder".into())),
            name: "value".into(),
        };
        let mut module = UnifiedModule::new(vec![
            Decl::Struct {
                name: "Row".into(),
                fields: vec![("x".into(), Typ::Int)],
                type_params: vec![],
            },
            Decl::Struct {
                name: "Holder".into(),
                fields: vec![("value".into(), field.clone())],
                type_params: vec![],
            },
            function(
                "borrowed_neg",
                vec![("self".into(), field)],
                Expr::IntLit(88),
            ),
            function(
                "apply",
                vec![("holder".into(), holder)],
                neg(operand.clone()),
            ),
        ]);
        let bindings =
            UnaryMethodBindings::from([(("&Row".into(), "-".into()), Ok("borrowed_neg".into()))]);
        TypeChecker::new()
            .resolve_unary_methods(&mut module, &bindings)
            .unwrap();
        assert_eq!(
            returned(&module, "apply"),
            Expr::Call {
                callee: Box::new(Expr::Ident("borrowed_neg".into())),
                args: vec![operand]
            }
        );
    }
    #[test]
    fn ambiguous_binding_preserves_original_expression_and_generic_proof_defers() {
        let original = neg(Expr::Ident("value".into()));
        let mut module = UnifiedModule::new(vec![function(
            "bad",
            vec![("value".into(), Typ::Named("Row".into()))],
            original.clone(),
        )]);
        let bindings = UnaryMethodBindings::from([(
            ("Row".into(), "-".into()),
            Err("ambiguous exact receiver".into()),
        )]);
        assert!(
            TypeChecker::new()
                .resolve_unary_methods(&mut module, &bindings)
                .unwrap_err()
                .contains("ambiguous")
        );
        assert_eq!(returned(&module, "bad"), original);
        let mut module = UnifiedModule::new(vec![function(
            "generic",
            vec![("value".into(), Typ::Generic("T".into()))],
            original.clone(),
        )]);
        TypeChecker::new()
            .resolve_unary_methods(&mut module, &bindings)
            .unwrap();
        assert_eq!(returned(&module, "generic"), original);
    }
    #[test]
    fn canonical_minus_from_another_front_never_uses_rust_trait_bindings() {
        let original = Expr::Unary {
            op: "-".into(),
            expr: Box::new(Expr::Ident("value".into())),
        };
        let mut module = UnifiedModule::new(vec![
            function(
                "neg_impl",
                vec![("self".into(), Typ::Named("Row".into()))],
                Expr::IntLit(77),
            ),
            function(
                "in_front",
                vec![("value".into(), Typ::Named("Row".into()))],
                original.clone(),
            ),
        ]);
        let bindings =
            UnaryMethodBindings::from([(("Row".into(), "-".into()), Ok("neg_impl".into()))]);
        TypeChecker::new()
            .resolve_unary_methods(&mut module, &bindings)
            .unwrap();
        assert_eq!(returned(&module, "in_front"), original);
    }
}

#[cfg(test)]
mod numeric_negation_checked_domain_tests {
    use super::*;
    #[test]
    fn checked_negation_rejects_domains_for_both_source_origins() {
        let checker = TypeChecker::new();
        let facts = checker.collect_decl_facts(&[]);
        for op in ["-", crate::core_ir::rust_references::NEGATE_VALUE] {
            for typ in [
                Typ::String,
                Typ::Array(Box::new(Typ::Int)),
                Typ::Vector(Box::new(Typ::Int)),
            ] {
                let env = HashMap::from([("value".into(), typ.clone())]);
                let expression = Expr::Unary {
                    op: op.into(),
                    expr: Box::new(Expr::Ident("value".into())),
                };
                assert_eq!(
                    checker.checked_expr_type(&expression, &facts, &env),
                    None,
                    "{op} {typ:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod inline_function_proof_tests {
    use super::*;
    fn function(name: &str, params: Vec<(String, Typ)>, ret: Typ, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params,
            ret,
            body,
            type_params: vec![],
        }
    }
    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args,
        }
    }
    #[test]
    fn scoped_proof_is_per_function_and_retains_generic_deferral() {
        let array = Typ::Array(Box::new(Typ::Int));
        let declarations = vec![
            function(
                "ignore",
                vec![("value".into(), array.clone())],
                Typ::Int,
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "bad",
                vec![],
                Typ::Int,
                vec![Stmt::Return(Some(call(
                    "ignore",
                    vec![Expr::StringLit("bad".into())],
                )))],
            ),
            function(
                "scoped",
                vec![("value".into(), Typ::String)],
                Typ::Int,
                vec![Stmt::Return(Some(Expr::Block {
                    body: vec![Stmt::Let(
                        "value".into(),
                        Some(array),
                        Expr::ArrayLit(vec![Expr::IntLit(42)]),
                    )],
                    result: Some(Box::new(call("ignore", vec![Expr::Ident("value".into())]))),
                }))],
            ),
            function(
                "generic",
                vec![("value".into(), Typ::Generic("T".into()))],
                Typ::Generic("T".into()),
                vec![Stmt::Return(Some(Expr::Ident("value".into())))],
            ),
            function(
                "bad_callee",
                vec![("value".into(), Typ::Int)],
                Typ::Int,
                vec![
                    Stmt::Let(
                        "wrong".into(),
                        Some(Typ::String),
                        Expr::Ident("value".into()),
                    ),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ),
        ];
        let mut declarations = declarations;
        declarations.push(function(
            "bad_return",
            vec![("value".into(), Typ::Int)],
            Typ::Int,
            vec![Stmt::Return(Some(Expr::StringLit("wrong".into())))],
        ));
        let checked = TypeChecker::new().checked_inline_functions(&declarations);
        assert!(!checked.contains("bad_return"));
        assert!(checked.contains("ignore"));
        assert!(checked.contains("scoped"));
        assert!(checked.contains("generic"));
        assert!(!checked.contains("bad"));
        assert!(!checked.contains("bad_callee"));
    }
    #[test]
    fn inline_proof_rejects_unresolved_values_closures_and_skipped_exceptions_only_in_proof_mode() {
        let closure = Expr::Closure {
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            captures: vec![],
        };
        let unresolved = Expr::Conditional {
            cond: Box::new(Expr::BoolLit(true)),
            then_expr: Box::new(Expr::IntLit(42)),
            else_expr: Box::new(Expr::StringLit("bad".into())),
        };
        let declarations = vec![
            function(
                "unresolved",
                vec![],
                Typ::Int,
                vec![Stmt::Return(Some(unresolved))],
            ),
            function(
                "closure",
                vec![],
                Typ::Int,
                vec![Stmt::Expr(closure), Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "exception",
                vec![],
                Typ::Int,
                vec![
                    Stmt::Throw(Expr::IntLit(1)),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ),
            function(
                "propagate",
                vec![],
                Typ::Int,
                vec![Stmt::Propagate, Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "try",
                vec![],
                Typ::Int,
                vec![
                    Stmt::Try {
                        body: vec![Stmt::Expr(Expr::IntLit(1))],
                        catches: vec![],
                    },
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ),
        ];
        assert!(
            TypeChecker::new()
                .checked_inline_functions(&declarations)
                .is_empty()
        );
        assert!(
            TypeChecker::new()
                .check_module(&UnifiedModule::new(vec![
                    declarations[2].clone(),
                    declarations[3].clone()
                ]))
                .is_ok(),
            "ordinary deferred exception policy is unchanged"
        );
    }
    #[test]
    fn identical_reference_source_proof_accepts_contextual_runtime_only_for_two_refs() {
        use crate::core_ir::rust_references::{self, ReferenceType};
        let proof = Typ::Named("Row".into());
        let formal = rust_references::typ(ReferenceType {
            mutable: false,
            referent: proof.clone(),
            runtime: Typ::Named("&Row".into()),
        });
        let actual = rust_references::typ(ReferenceType {
            mutable: false,
            referent: proof.clone(),
            runtime: proof.clone(),
        });
        assert!(is_compatible(&formal, &actual));
        assert!(!is_compatible(&formal, &proof));
        let declarations = vec![
            Decl::Struct {
                name: "Row".into(),
                fields: vec![("x".into(), Typ::Int)],
                type_params: vec![],
            },
            function(
                "borrowed_neg",
                vec![("self".into(), formal)],
                Typ::Int,
                vec![Stmt::Return(Some(Expr::IntLit(88)))],
            ),
            function(
                "caller",
                vec![("value".into(), actual)],
                Typ::Int,
                vec![Stmt::Return(Some(call(
                    "borrowed_neg",
                    vec![Expr::Ident("value".into())],
                )))],
            ),
        ];
        assert!(
            TypeChecker::new()
                .checked_inline_functions(&declarations)
                .contains("caller")
        );
    }
    #[test]
    fn scoped_proof_does_not_certify_a_shadowed_direct_callee() {
        let declarations = vec![
            function(
                "target",
                vec![("value".into(), Typ::Int)],
                Typ::Int,
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "shadowed",
                vec![("target".into(), Typ::Int)],
                Typ::Int,
                vec![Stmt::Return(Some(call("target", vec![Expr::IntLit(1)])))],
            ),
        ];
        let checked = TypeChecker::new().checked_inline_functions(&declarations);
        assert!(checked.contains("target"));
        assert!(!checked.contains("shadowed"));
    }
}

#[cfg(test)]
mod function_facts_check_tests {
    use super::*;
    #[test]
    fn isolated_function_check_reuses_facts_globals_and_parameter_shadowing() {
        let checker = TypeChecker::new();
        let function = |name: &str, params: Vec<(String, Typ)>, value: Expr| Decl::Function {
            name: name.into(),
            params,
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(value))],
            type_params: vec![],
        };
        let helper = function("helper", vec![], Expr::IntLit(42));
        let valid = function(
            "valid",
            vec![("VALUE".into(), Typ::Int)],
            Expr::Call {
                callee: Box::new(Expr::Ident("helper".into())),
                args: vec![],
            },
        );
        let shadow = function(
            "shadow",
            vec![("VALUE".into(), Typ::Int)],
            Expr::Ident("VALUE".into()),
        );
        let invalid = function("invalid", vec![], Expr::Ident("VALUE".into()));
        let facts =
            checker.collect_decl_facts(&[helper, valid.clone(), shadow.clone(), invalid.clone()]);
        let globals = HashMap::from([("VALUE".into(), Typ::String)]);
        assert!(
            checker
                .check_function_with_facts(&valid, &facts, &globals)
                .is_empty()
        );
        assert!(
            checker
                .check_function_with_facts(&shadow, &facts, &globals)
                .is_empty()
        );
        assert!(
            !checker
                .check_function_with_facts(&invalid, &facts, &globals)
                .is_empty()
        );
        let nonfunction = Decl::Struct {
            name: "Row".into(),
            fields: vec![],
            type_params: vec![],
        };
        assert!(
            checker
                .check_function_with_facts(&nonfunction, &facts, &globals)
                .is_empty()
        );
    }
}

#[cfg(test)]
mod scoped_value_walk_tests {
    use super::*;
    fn nested(depth: usize) -> String {
        let mut value = "1.0000000596046447753906250000001".to_string();
        for index in 0..depth {
            value = format!("{{let inner{index}={value};inner{index}}}");
        }
        format!("fn sample()->f32{{let value:f32={value};value}}")
    }
    #[test]
    fn scoped_value_body_visits_are_linear_per_inference_iteration() {
        for depth in 0..=8 {
            let mut module =
                crate::compiler::rust_front::parse_rust_source(&nested(depth)).unwrap();
            let checker = TypeChecker::new();
            let facts = checker.collect_facts(&module);
            let Decl::Function { body, ret, .. } = module
                .decls
                .iter_mut()
                .find(|decl| matches!(decl,Decl::Function{name,..}if name=="sample"))
                .unwrap()
            else {
                unreachable!()
            };
            TYPED_BODY_VISITS.with(|visits| visits.set(0));
            checker.resolve_typed_body(
                body,
                &facts,
                &mut HashMap::new(),
                Some(ret),
                ResolvePolicy::RustFloatInference,
            );
            let visits = TYPED_BODY_VISITS.with(std::cell::Cell::get);
            assert_eq!(
                visits,
                depth + 1,
                "one function body and each nested block once at depth {depth}"
            );
        }
    }
    #[test]
    fn scoped_value_match_block_tuple_visits_each_body_once() {
        for depth in 0..=8 {
            let mut value = "1.0000000596046447753906250000001".to_string();
            for index in 0..depth {
                value = format!("{{let inner{index}={value};inner{index}}}");
            }
            let source = format!(
                "fn main()->i64{{let pair:(f32,f32)=match true{{true=>({value},0.0f32),false=>(0.0f32,0.0f32)}};42}}"
            );
            let mut module = crate::compiler::rust_front::parse_rust_source(&source).unwrap();
            let mut blocks = 0;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if matches!(expr, Expr::Block { .. }) {
                    blocks += 1;
                }
            });
            assert_eq!(
                blocks,
                depth + 2,
                "both synthetic match arms plus nested blocks"
            );
            let checker = TypeChecker::new();
            let facts = checker.collect_facts(&module);
            let Decl::Function { body, ret, .. } = module
                .decls
                .iter_mut()
                .find(|decl| matches!(decl,Decl::Function{name,..}if name=="main"))
                .unwrap()
            else {
                unreachable!()
            };
            TYPED_BODY_VISITS.with(|visits| visits.set(0));
            checker.resolve_typed_body(
                body,
                &facts,
                &mut HashMap::new(),
                Some(ret),
                ResolvePolicy::RustFloatInference,
            );
            let visits = TYPED_BODY_VISITS.with(std::cell::Cell::get);
            assert_eq!(
                visits,
                blocks + 1,
                "function body and every match-arm/nested block exactly once at depth {depth}"
            );
        }
    }
    #[test]
    fn scoped_value_generic_parent_reaches_original_f32_literal_before_defaulting() {
        let source = "fn id<T>(x:T)->T{x}fn main()->i64{let x:f32=id({let a=1.0000000596046447753906250000001;a});if x==1.00000011920928955078125f32{42}else{7}}";
        let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        let mut module = normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        assert!(
            TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty(),
            "{module:#?}"
        );
        let mut rounded = 0;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if matches!(expr,Expr::FloatLit(value)if value.0==f64::from(f32::from_bits(0x3f80_0001)))
            {
                rounded += 1;
            }
        });
        assert!(
            rounded >= 2,
            "both contextual initializer and comparison must contain next f32, not only the pretyped comparison"
        );
    }
    #[test]
    fn scoped_value_generic_peers_and_ieee_integer_block_contexts_are_preserved() {
        for source in [
            "fn first<T>(a:T,b:T)->T{a}fn main()->i64{let value=first({let a=1.0000000596046447753906250000001;a},1.00000011920928955078125f32);if value==1.00000011920928955078125f32{42}else{7}}",
            "fn second<T>(a:T,b:T)->T{b}fn main()->i64{let value=second(1.00000011920928955078125f32,{let a=1.0000000596046447753906250000001;a});if value==1.00000011920928955078125f32{42}else{7}}",
            "fn main()->i64{if f32::from_bits({let bits=1065353216;bits})==1.0f32{42}else{7}}",
            "fn main()->i64{if f64::from_bits({let bits=4607182418800017408;bits})==1.0f64{42}else{7}}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let module = normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{source}: {module:#?}"
            );
        }
    }
    #[test]
    fn scoped_value_fixed_array_block_transfer_materializes_one_copy() {
        let source = "fn main()->i64{let original:[i64;1]=[42i64];let mut copied:[i64;1]={let side=0;original};copied[0]=7;original[0]}";
        let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
        let mut module = normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        assert!(
            TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .is_empty(),
            "{module:#?}"
        );
        let mut copies = 0;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if matches!(expr,Expr::Unary{op,..}if crate::core_ir::array_copy::count(op).is_some()) {
                copies += 1;
            }
        });
        assert_eq!(
            copies, 1,
            "only the actual copied binding transfer owns a copy: {module:#?}"
        );
    }
    #[test]
    fn scoped_value_inner_return_and_independent_source_errors_keep_their_contexts() {
        for source in [
            "fn main()->i64{let value:f32={if false{return 42;}let inner=1.0000000596046447753906250000001;inner};if value==1.00000011920928955078125f32{42}else{7}}",
            "fn main()->i64{let value:f32={let nested={let value=1.0000000596046447753906250000001;value};nested};if value==1.00000011920928955078125f32{42}else{7}}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let module = normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{source}: {module:#?}"
            );
        }
        let parsed = crate::compiler::rust_front::parse_rust_source(
            "fn main()->i64{let value:f32={let invalid:i8=128;1.0};42}",
        )
        .unwrap();
        let module = normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        assert!(
            TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .values()
                .any(|proof| proof.is_invalid())
        );
    }
}

#[cfg(test)]
mod generic_policy_purity_tests {
    use super::*;
    fn parsed(source: &str) -> UnifiedModule {
        crate::compiler::rust_front::parse_rust_artifact_source(source, "policy-purity")
            .unwrap()
            .semantic
    }
    #[test]
    fn generic_argument_phase_preserves_closure_return_inference_ownership() {
        let mut module = parsed(
            "fn identity<T>(value:T)->T{value}fn main()->i64{let zero=identity(0i64);let answer=||[20,22];let items=answer();items[0]+items[1]+zero}",
        );
        let mut before = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Closure { ret, .. } = expr {
                before.push(ret.clone());
            }
        });
        TypeChecker::new().resolve_typed_operators(
            &mut module,
            ResolvePolicy::RustGenericArguments,
            None,
            None,
        );
        let mut after = Vec::new();
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if let Expr::Closure { ret, .. } = expr {
                after.push(ret.clone());
            }
        });
        assert_eq!(
            after, before,
            "generic inference must not pin dynamic array closure returns before fixed-array lowering"
        );
    }
    #[test]
    fn fixed_array_inline_const_has_one_outer_copy_and_no_payload_copy() {
        let mut module =
            parsed("fn main()->i64{let value=const{let mut a=[20,0];a[1]=22;a};value[0]+value[1]}");
        TypeChecker::new().resolve_typed_operators(
            &mut module,
            ResolvePolicy::RustFixedArrays,
            None,
            None,
        );
        let mut outer = 0;
        crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
            if matches!(expr,Expr::Unary{op,expr:inner}if crate::core_ir::array_copy::count(op)==Some(2)&&matches!(inner.as_ref(),Expr::Call{callee,..}if matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::INLINE_CONST)))
            {
                outer += 1;
            }
            if let Expr::Call { callee, args } = expr
                && matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::INLINE_CONST)
            {
                let mut payload = args[0].clone();
                let mut copies = 0;
                crate::core_opt::map_expr_mut(&mut payload, &mut |expr| {
                    if matches!(expr,Expr::Unary{op,..}if crate::core_ir::array_copy::count(op).is_some())
                    {
                        copies += 1;
                    }
                });
                assert_eq!(copies, 0, "constant payload must remain foldable");
            }
        });
        assert_eq!(outer, 1, "retain the actual runtime whole-result copy");
    }
    #[test]
    fn array_closure_and_inline_const_work_in_plain_and_generic_modules() {
        for source in [
            "fn main()->i64{let answer=||[20,22];let items=answer();items[0]+items[1]}",
            "fn identity<T>(value:T)->T{value}fn main()->i64{let zero=identity(0i64);let answer=||[20,22];let items=answer();items[0]+items[1]+zero}",
            "fn main()->i64{let original=const{let mut a=[20,0];a[1]=22;a};let mut changed=original;changed[0]=7;original[0]+original[1]}",
            "fn identity<T>(value:T)->T{value}fn main()->i64{let zero=identity(0i64);let original=const{let mut a=[20,0];a[1]=22;a};let mut changed=original;changed[0]=7;original[0]+original[1]+zero}",
        ] {
            let mut module =
                normalize_module(crate::parser_registry::ParserId::Rust, &parsed(source));
            assert!(
                TypeChecker::new()
                    .typed_proof_nodes(&module.decls)
                    .is_empty(),
                "{source}: {module:#?}"
            );
            crate::lower_core::desugar_module(&mut module);
            assert!(
                crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:#?}"
            );
            let mut constants = 0;
            crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
                if matches!(expr,Expr::Call{callee,..}if matches!(callee.as_ref(),Expr::Ident(name)if name==crate::core_ir::INLINE_CONST))
                {
                    constants += 1;
                }
            });
            assert_eq!(
                constants, 0,
                "compile-time value must be evaluated before execution"
            );
        }
    }
}
