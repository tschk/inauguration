//! Conservative ownership obligations for universally checked Rust type parameters.
use super::{Facts, TypeChecker};
use crate::core_ir::{
    Decl, Expr, MatchPattern, Stmt, Typ, rust_generics, rust_references,
    type_proofs::ResolutionFailure,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy)]
enum Use {
    Move,
    Borrow(bool),
}
#[derive(Default)]
struct Uses {
    moves: usize,
    borrows: usize,
    mutable: bool,
}
#[derive(Clone, Default)]
struct Environment {
    types: HashMap<String, Typ>,
    bindings: HashMap<String, usize>,
}
struct Check<'a> {
    checker: TypeChecker,
    facts: &'a Facts,
    next: usize,
    parameter_bindings: usize,
    uses: HashMap<usize, Uses>,
    failure: Option<ResolutionFailure>,
    in_loop: bool,
}

fn contains(typ: &Typ, generic: bool, facts: &Facts, active: &mut HashSet<String>) -> bool {
    match typ {
        Typ::Generic(_) => generic,
        Typ::Array(item) | Typ::Vector(item) => contains(item, generic, facts, active),
        Typ::Named(name) => {
            if rust_generics::is_rigid_type(typ) {
                return !generic;
            }
            if !active.insert(name.clone()) {
                return false;
            }
            let found = if let Some(array) = crate::core_ir::rust_arrays::decode(name) {
                contains(&array.element, generic, facts, active)
            } else if let Some(reference) = rust_references::decode(name) {
                contains(&reference.referent, generic, facts, active)
            } else if let Some(payload) = crate::core_ir::rust_boxes::payload(name)
                .or_else(|| crate::core_ir::rust_boxes::address_payload(name))
            {
                contains(&payload, generic, facts, active)
            } else if let Some(fields) = crate::core_ir::tuples::fields(name) {
                fields
                    .iter()
                    .any(|(_, typ)| contains(typ, generic, facts, active))
            } else {
                facts.structs.get(name).is_some_and(|fields| {
                    fields
                        .iter()
                        .any(|(_, typ)| contains(typ, generic, facts, active))
                })
            };
            active.remove(name);
            found
        }
        _ => false,
    }
}
impl Check<'_> {
    fn rigid(&self, typ: &Typ) -> bool {
        contains(typ, false, self.facts, &mut HashSet::new())
    }
    fn affine(&self, typ: &Typ) -> bool {
        self.rigid(typ)
            && !matches!(typ,Typ::Named(name) if rust_references::decode(name).is_some_and(|reference|!reference.mutable))
    }
    fn indexed_type(typ: Typ) -> Option<Typ> {
        match typ {
            Typ::Array(item) | Typ::Vector(item) => Some(*item),
            Typ::Named(name) => crate::core_ir::rust_arrays::decode(&name)
                .map(|array| array.element)
                .or_else(|| {
                    rust_references::decode(&name)
                        .and_then(|reference| Self::indexed_type(reference.referent))
                }),
            _ => None,
        }
    }
    fn uncertain(&mut self, reason: &str) {
        if self.failure.is_none() {
            self.failure = Some(ResolutionFailure::Unsupported(format!(
                "generic ownership requires proof: {reason}"
            )));
        }
    }
    fn bind(&mut self, env: &mut Environment, name: &str, typ: Typ) {
        let id = self.next;
        self.next += 1;
        env.bindings.insert(name.into(), id);
        env.types.insert(name.into(), typ);
    }
    fn identifier(&mut self, name: &str, env: &Environment, usage: Use) {
        let Some(typ) = env.types.get(name) else {
            return;
        };
        if !self.rigid(typ) {
            return;
        }
        // Shared references are Copy independently of their referent's generic type.
        if matches!(usage, Use::Move)
            && matches!(typ,Typ::Named(name) if rust_references::decode(name).is_some_and(|reference|!reference.mutable))
        {
            return;
        }
        let Some(id) = env.bindings.get(name) else {
            self.uncertain("unowned rigid value");
            return;
        };
        let stats = self.uses.entry(*id).or_default();
        match usage {
            Use::Move => stats.moves += 1,
            Use::Borrow(mutable) => {
                stats.borrows += 1;
                stats.mutable |= mutable;
            }
        }
        let failure = stats.moves > 1
            || (stats.moves > 0 && stats.borrows > 0)
            || (stats.mutable && stats.borrows > 1)
            || (self.in_loop && (stats.moves > 0 || stats.mutable));
        // shortcut: join paths conservatively. Mutually exclusive uses are Unsupported,
        // never an Invalid source claim, until shared CFG-aware move analysis exists.
        if failure {
            self.uncertain("repeated move, borrow/move combination, or loop use of a rigid value");
        }
    }
    fn pattern(&mut self, pattern: &str, typ: Option<Typ>, env: &mut Environment) {
        let Some(typ) = typ else {
            self.uncertain("unresolved pattern binding type");
            return;
        };
        let bindings = MatchPattern::parse(pattern).ok().and_then(|pattern| {
            pattern
                .typed_bindings(&typ, &|name| {
                    self.facts.structs.get(name).map(Vec::as_slice)
                })
                .ok()
        });
        if let Some(bindings) = bindings {
            for (name, typ) in bindings {
                self.bind(env, &name, typ);
            }
        } else if self.rigid(&typ) {
            self.uncertain("rigid pattern ownership");
        }
    }
    fn expression(&mut self, expr: &Expr, env: &mut Environment, usage: Use) {
        match expr {
            Expr::Ident(name) => self.identifier(name, env, usage),
            Expr::Unary { op, expr: operand } => {
                if matches!(op.as_str(), "*" | rust_references::VIEW_READ)
                    && matches!(usage, Use::Move)
                {
                    let operand_type = self.checker.expr_type(operand, self.facts, &env.types);
                    let referent = operand_type.as_ref().and_then(|typ| match typ {
                        Typ::Named(name) => {
                            rust_references::decode(name).map(|reference| reference.referent)
                        }
                        _ => None,
                    });
                    if referent.as_ref().is_some_and(|typ| self.affine(typ))
                        || (referent.is_none()
                            && operand_type.as_ref().is_some_and(|typ| self.rigid(typ)))
                    {
                        self.uncertain(
                            "moving a rigid referent through dereference requires Copy proof",
                        );
                    }
                }
                self.expression(
                    operand,
                    env,
                    match op.as_str() {
                        crate::core_ir::rust_boxes::SHARED => Use::Borrow(false),
                        crate::core_ir::rust_boxes::MUTABLE => Use::Borrow(true),
                        _ => rust_references::value_kind(op)
                            .or_else(|| {
                                rust_references::iteration_reference(op)
                                    .and_then(rust_references::value_kind)
                            })
                            .map_or(usage, Use::Borrow),
                    },
                );
            }
            Expr::Index { base, index } => {
                let result = self
                    .checker
                    .expr_type(expr, self.facts, &env.types)
                    .or_else(|| {
                        self.checker
                            .expr_type(base, self.facts, &env.types)
                            .and_then(Self::indexed_type)
                    });
                if matches!(usage, Use::Move) && result.as_ref().is_some_and(|typ| self.affine(typ))
                {
                    self.uncertain("moving a rigid indexed element requires Copy proof");
                }
                self.expression(
                    base,
                    env,
                    match usage {
                        Use::Move => Use::Borrow(false),
                        borrow => borrow,
                    },
                );
                self.expression(index, env, Use::Move);
            }
            Expr::Block { body, result } => {
                let mut inner = env.clone();
                self.body(body, &mut inner);
                if let Some(result) = result {
                    self.expression(result, &mut inner, usage);
                }
            }
            Expr::Conditional {
                cond,
                then_expr,
                else_expr,
            } => {
                self.expression(cond, env, Use::Move);
                self.expression(then_expr, &mut env.clone(), usage);
                self.expression(else_expr, &mut env.clone(), usage);
            }
            Expr::MatchValue { scrutinee, arms } => {
                let typ = self.checker.expr_type(scrutinee, self.facts, &env.types);
                self.expression(scrutinee, env, Use::Move);
                for arm in arms {
                    let mut inner = env.clone();
                    self.pattern(&arm.pattern, typ.clone(), &mut inner);
                    if let Some(guard) = &arm.guard {
                        self.expression(guard, &mut inner, Use::Move);
                    }
                    self.expression(&arm.value, &mut inner, usage);
                }
            }
            Expr::Call { callee, args } => {
                if matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::ARRAY_REPEAT)
                    && args
                        .first()
                        .and_then(|value| self.checker.expr_type(value, self.facts, &env.types))
                        .as_ref()
                        .is_some_and(|typ| self.affine(typ))
                {
                    // shortcut: even 0/1 or constant-expression lengths remain conservative
                    // until repetition length and Copy obligations share a source proof.
                    self.uncertain("repeating a rigid element requires Copy proof");
                }
                let borrow =
                    matches!(callee.as_ref(),Expr::Ident(name) if name==crate::core_ir::BORROW);
                if let Expr::Ident(name) = callee.as_ref()
                    && let Some((params, ret)) = self.facts.function_signature(name)
                    && (params
                        .iter()
                        .any(|(_, typ)| contains(typ, true, self.facts, &mut HashSet::new()))
                        || contains(ret, true, self.facts, &mut HashSet::new()))
                {
                    self.uncertain("inferred generic call has no universally validated instance");
                }
                if let Expr::Ident(name) = callee.as_ref()
                    && env
                        .types
                        .get(name)
                        .is_some_and(|typ| self.facts.rigid_callable(typ).is_some())
                {
                    // FnMut borrows end with the invocation; they neither move
                    // the receiver nor establish Copy for subsequent uses.
                    if env
                        .types
                        .get(name)
                        .is_some_and(rust_generics::is_rigid_type)
                        && env
                            .bindings
                            .get(name)
                            .is_none_or(|id| *id >= self.parameter_bindings)
                    {
                        self.uncertain("local callable place mutability is unresolved");
                    }
                    if env
                        .bindings
                        .get(name)
                        .and_then(|id| self.uses.get(id))
                        .is_some_and(|uses| uses.moves > 0 || uses.borrows > 0)
                    {
                        self.uncertain("calling a moved or escaping borrowed callable");
                    }
                } else {
                    self.expression(callee, env, Use::Move);
                }
                for arg in args {
                    self.expression(
                        arg,
                        env,
                        if borrow {
                            match usage {
                                Use::Borrow(_) => usage,
                                Use::Move => Use::Borrow(false),
                            }
                        } else {
                            Use::Move
                        },
                    );
                }
            }
            Expr::Closure { params, body, .. } => {
                // shortcut: inferred capture modes and FnOnce obligations are not available
                // here. A rigid capture must not become Copy merely by specialization.
                let outer_ids: HashSet<_> = env.bindings.values().copied().collect();
                let before: HashMap<_, _> = self
                    .uses
                    .iter()
                    .map(|(id, uses)| (*id, (uses.moves, uses.borrows)))
                    .collect();
                let mut inner = env.clone();
                for (name, typ) in params {
                    self.bind(&mut inner, name, typ.clone());
                }
                self.body(body, &mut inner);
                if self.uses.iter().any(|(id, uses)| {
                    outer_ids.contains(id)
                        && before.get(id).copied().unwrap_or_default() != (uses.moves, uses.borrows)
                }) {
                    self.uncertain("closure capture of a rigid value");
                }
            }
            _ => crate::core_ir::for_each_expr_child(expr, &mut |child| {
                self.expression(child, env, usage)
            }),
        }
    }
    fn body(&mut self, body: &[Stmt], env: &mut Environment) {
        for stmt in body {
            match stmt {
                Stmt::Declare(name, typ) => self.bind(env, name, typ.clone()),
                Stmt::Let(name, annotation, value) => {
                    let typ = annotation
                        .clone()
                        .or_else(|| self.checker.expr_type(value, self.facts, &env.types));
                    self.expression(value, env, Use::Move);
                    if let Some(typ) = typ {
                        self.bind(env, name, typ);
                    } else {
                        env.types.remove(name);
                        env.bindings.remove(name);
                        self.uncertain("unresolved local type");
                    }
                }
                Stmt::Assign(name, value) => {
                    self.expression(value, env, Use::Move);
                    // Assignment does not manufacture Copy evidence or a new binding identity.
                    if !env.types.contains_key(name) {
                        self.uncertain("unresolved assignment destination");
                    }
                }
                Stmt::Expr(value) | Stmt::Throw(value) | Stmt::Return(Some(value)) => {
                    self.expression(value, env, Use::Move)
                }
                Stmt::IndexAssign { base, index, value } => {
                    self.expression(base, env, Use::Borrow(true));
                    self.expression(index, env, Use::Move);
                    self.expression(value, env, Use::Move);
                }
                Stmt::FieldAssign { base, value, .. } => {
                    self.expression(base, env, Use::Borrow(true));
                    self.expression(value, env, Use::Move);
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    self.expression(cond, env, Use::Move);
                    self.body(then_body, &mut env.clone());
                    self.body(else_body, &mut env.clone());
                }
                Stmt::Loop { kind, cond, body } => {
                    let previous = self.in_loop;
                    let mut inner = env.clone();
                    if let crate::core_ir::LoopKind::For { binding } = kind {
                        // A for iterator is evaluated once; its element binding
                        // must retain affine facts inside the repeated body.
                        if let Some(element) = cond
                            .as_ref()
                            .and_then(|value| self.checker.expr_type(value, self.facts, &env.types))
                            .and_then(super::for_element_type)
                        {
                            self.bind(&mut inner, binding, element);
                        } else {
                            self.uncertain("iterator element ownership is unresolved");
                        }
                    } else {
                        self.in_loop = true;
                    }
                    if let Some(cond) = cond {
                        self.expression(cond, env, Use::Move);
                    }
                    self.in_loop = true;
                    self.body(body, &mut inner);
                    self.in_loop = previous;
                }
                Stmt::Match { scrutinee, arms } => {
                    let typ = self.checker.expr_type(scrutinee, self.facts, &env.types);
                    self.expression(scrutinee, env, Use::Move);
                    for arm in arms {
                        let mut inner = env.clone();
                        self.pattern(&arm.pattern, typ.clone(), &mut inner);
                        if let Some(guard) = &arm.guard {
                            self.expression(guard, &mut inner, Use::Move);
                        }
                        self.body(&arm.body, &mut inner);
                    }
                }
                Stmt::Try { body, catches } => {
                    self.body(body, &mut env.clone());
                    for catch in catches {
                        let mut inner = env.clone();
                        self.pattern(&catch.pattern, None, &mut inner);
                        self.body(&catch.body, &mut inner);
                    }
                }
                _ => {}
            }
        }
    }
}

pub(super) fn check(
    declaration: &Decl,
    facts: &Facts,
    globals: &HashMap<String, Typ>,
) -> Option<ResolutionFailure> {
    let Decl::Function { params, body, .. } = declaration else {
        return None;
    };
    let mut checker = Check {
        checker: TypeChecker::new(),
        facts,
        next: 0,
        parameter_bindings: 0,
        uses: HashMap::new(),
        failure: None,
        in_loop: false,
    };
    let mut env = Environment::default();
    for (name, typ) in globals
        .iter()
        .chain(params.iter().map(|(name, typ)| (name, typ)))
    {
        checker.bind(&mut env, name, typ.clone());
    }
    checker.parameter_bindings = checker.next;
    checker.body(body, &mut env);
    checker.failure
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rigid() -> Typ {
        rust_generics::rigid_type("T")
    }
    fn function(body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: "audit".into(),
            params: vec![("v".into(), rigid())],
            ret: rigid(),
            body,
            type_params: vec![],
        }
    }
    fn run(declaration: &Decl, others: Vec<Decl>) -> Option<ResolutionFailure> {
        let mut decls = others;
        decls.push(declaration.clone());
        let facts = TypeChecker::new().collect_decl_facts(&decls);
        check(declaration, &facts, &HashMap::new())
    }
    fn ident(name: &str) -> Expr {
        Expr::Ident(name.into())
    }
    fn shared(typ: Typ) -> Typ {
        rust_references::typ(rust_references::ReferenceType {
            mutable: false,
            referent: typ,
            runtime: Typ::Int,
        })
    }
    fn projected(parameter: Typ, value: Expr) -> Decl {
        Decl::Function {
            name: "audit".into(),
            params: vec![("v".into(), parameter)],
            ret: rigid(),
            body: vec![Stmt::Return(Some(value))],
            type_params: vec![],
        }
    }
    #[test]
    fn rigid_dereference_and_index_moves_require_copy_but_reborrows_pass() {
        let dereference = Expr::Unary {
            op: "*".into(),
            expr: Box::new(ident("v")),
        };
        let index = Expr::Index {
            base: Box::new(ident("v")),
            index: Box::new(Expr::IntLit(0)),
        };
        for (parameter, value) in [
            (shared(rigid()), dereference.clone()),
            (shared(Typ::Array(Box::new(rigid()))), index.clone()),
            (Typ::Vector(Box::new(rigid())), index.clone()),
        ] {
            assert!(matches!(
                run(&projected(parameter.clone(), value.clone()), vec![]),
                Some(ResolutionFailure::Unsupported(_))
            ));
            let reborrow = Expr::Unary {
                op: rust_references::SHARED_VALUE.into(),
                expr: Box::new(value),
            };
            assert!(run(&projected(parameter, reborrow), vec![]).is_none());
        }
        let copy_reference = projected(shared(shared(rigid())), dereference);
        assert!(
            run(&copy_reference, vec![]).is_none(),
            "dereferencing shared references may copy the inner shared reference"
        );
    }
    #[test]
    fn rigid_array_repetition_retains_copy_obligation() {
        let repeat = Expr::Call {
            callee: Box::new(ident(crate::core_ir::ARRAY_REPEAT)),
            args: vec![ident("v"), Expr::IntLit(2)],
        };
        assert!(matches!(
            run(&function(vec![Stmt::Expr(repeat)]), vec![]),
            Some(ResolutionFailure::Unsupported(_))
        ));
        let repeat_reference = Expr::Call {
            callee: Box::new(ident(crate::core_ir::ARRAY_REPEAT)),
            args: vec![ident("v"), Expr::IntLit(2)],
        };
        assert!(run(&projected(shared(rigid()), repeat_reference), vec![]).is_none());
    }
    #[test]
    fn identities_and_single_moves_to_inferred_locals_pass() {
        assert!(run(&function(vec![Stmt::Return(Some(ident("v")))]), vec![]).is_none());
        assert!(
            run(
                &function(vec![
                    Stmt::Let("next".into(), None, ident("v")),
                    Stmt::Return(Some(ident("next")))
                ]),
                vec![]
            )
            .is_none()
        );
        let declaration = function(vec![
            Stmt::Let("v".into(), None, Expr::IntLit(1)),
            Stmt::Expr(ident("v")),
            Stmt::Return(Some(ident("v"))),
        ]);
        assert!(
            run(&declaration, vec![]).is_none(),
            "scalar shadow must not inherit rigid identity"
        );
    }
    #[test]
    fn moves_across_branches_blocks_and_borrows_remain_unsupported() {
        let borrow = Expr::Unary {
            op: rust_references::SHARED_VALUE.into(),
            expr: Box::new(ident("v")),
        };
        for body in [
            vec![
                Stmt::Let("ignored".into(), None, ident("v")),
                Stmt::Return(Some(ident("v"))),
            ],
            vec![Stmt::If {
                cond: Expr::BoolLit(true),
                then_body: vec![Stmt::Return(Some(ident("v")))],
                else_body: vec![Stmt::Return(Some(ident("v")))],
            }],
            vec![Stmt::Expr(borrow.clone()), Stmt::Return(Some(ident("v")))],
            vec![
                Stmt::Expr(Expr::Block {
                    body: vec![Stmt::Expr(ident("v"))],
                    result: None,
                }),
                Stmt::Return(Some(ident("v"))),
            ],
        ] {
            assert!(matches!(
                run(&function(body), vec![]),
                Some(ResolutionFailure::Unsupported(_))
            ));
        }
        assert!(
            run(
                &function(vec![Stmt::Expr(borrow.clone()), Stmt::Expr(borrow)]),
                vec![]
            )
            .is_none(),
            "repeated shared borrows are not by-value copies"
        );
    }
    #[test]
    fn unresolved_inferred_generic_calls_refuse_but_explicit_instances_pass() {
        for (name, typ, unsupported) in [
            ("inferred", Typ::Generic("T".into()), true),
            ("#rust-specialization:explicit", rigid(), false),
        ] {
            let callee = Decl::Function {
                name: name.into(),
                params: vec![("value".into(), typ.clone())],
                ret: typ,
                body: vec![],
                type_params: vec![],
            };
            let declaration = function(vec![Stmt::Return(Some(Expr::Call {
                callee: Box::new(ident(name)),
                args: vec![ident("v")],
            }))]);
            assert_eq!(run(&declaration, vec![callee]).is_some(), unsupported);
        }
    }
    #[test]
    fn inferred_rigid_tuple_and_pattern_bindings_cannot_silently_copy() {
        let tuple = crate::core_ir::tuples::typ(&[rigid(), rigid()]);
        let declaration = Decl::Function {
            name: "audit".into(),
            params: vec![("values".into(), tuple)],
            ret: rigid(),
            type_params: vec![],
            body: vec![Stmt::Match {
                scrutinee: ident("values"),
                arms: vec![crate::core_ir::MatchArm {
                    pattern: "(left,right)".into(),
                    guard: None,
                    body: vec![Stmt::Expr(ident("left")), Stmt::Return(Some(ident("left")))],
                }],
            }],
        };
        assert!(matches!(
            run(&declaration, vec![]),
            Some(ResolutionFailure::Unsupported(_))
        ));
    }
}
