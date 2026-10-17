use super::*;
use crate::core_ir::type_proofs::{self, ResolutionFailure};
use std::cell::RefCell;

fn prefer(
    left: Option<ResolutionFailure>,
    right: Option<ResolutionFailure>,
) -> Option<ResolutionFailure> {
    match (left, right) {
        (Some(left), Some(right))
            if (!left.is_invalid() && right.is_invalid())
                || (left.is_invalid() == right.is_invalid()
                    && right.message() < left.message()) =>
        {
            Some(right)
        }
        (Some(left), _) => Some(left),
        (None, right) => right,
    }
}
impl Facts {
    fn type_proof(&self, typ: &Typ, active: &mut HashSet<String>) -> Option<ResolutionFailure> {
        match typ {
            Typ::Generic(_) => None,
            Typ::Array(item) | Typ::Vector(item) => self.type_proof(item, active),
            Typ::Named(name) => {
                if let Some(proof) = type_proofs::decode(name) {
                    return Some(proof);
                }
                if !active.insert(name.clone()) {
                    return None;
                }
                let result = if let Some(reference) = crate::core_ir::rust_references::decode(name)
                {
                    prefer(
                        self.type_proof(&reference.referent, active),
                        self.type_proof(&reference.runtime, active),
                    )
                } else if let Some(fields) = crate::core_ir::tuples::fields(name) {
                    fields.iter().fold(None, |proof, (_, typ)| {
                        prefer(proof, self.type_proof(typ, active))
                    })
                } else if let Some(fields) = self.structs.get(name) {
                    fields.iter().fold(None, |proof, (_, typ)| {
                        prefer(proof, self.type_proof(typ, active))
                    })
                } else {
                    None
                };
                active.remove(name);
                result
            }
            _ => None,
        }
    }
}
impl TypeChecker {
    /// Observe all typed sites with the existing scope-aware name walker. This
    /// query deliberately does not treat ordinary deferred checking as proof.
    /// A normalized private view makes local shadows distinct from globals and
    /// call targets, without changing the input or adding expression markers.
    pub(crate) fn typed_proof_functions(
        &self,
        decls: &[Decl],
    ) -> HashMap<String, ResolutionFailure> {
        if !type_proofs::present(decls) {
            return HashMap::new();
        }
        let facts = self.collect_decl_facts(decls);
        let mut view = decls.to_vec();
        crate::core_bindings::normalize_decls(&mut view);
        let functions: HashSet<_> = view
            .iter()
            .filter_map(|decl| match decl {
                Decl::Function { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let nodes: HashSet<_> = view
            .iter()
            .filter_map(|decl| match decl {
                Decl::Function { name, .. } | Decl::Global { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let mut proofs = HashMap::new();
        let mut edges = HashMap::new();
        for decl in &mut view {
            let name = match decl {
                Decl::Function { name, .. } | Decl::Global { name, .. } => name.clone(),
                _ => continue,
            };
            let result = RefCell::new(None);
            let references = RefCell::new(HashSet::new());
            // Consume the normalized view in-place: no second declaration or
            // global initializer clone, no source AST mutation.
            crate::core_bindings::rewrite_nominal_names(
                std::slice::from_mut(decl),
                &|name| {
                    let proof = facts.type_proof(&Typ::Named(name.into()), &mut HashSet::new());
                    let previous = result.take();
                    *result.borrow_mut() = prefer(previous, proof);
                    None
                },
                &|name| {
                    if nodes.contains(name) {
                        references.borrow_mut().insert(name.to_string());
                    }
                    None
                },
            );
            if let Some(proof) = result.into_inner() {
                proofs.insert(name.clone(), proof);
            }
            edges.insert(name, references.into_inner());
        }
        // Propagate through callable and actual global-reference edges. This
        // includes invalid locals in an initializer's helper without treating
        // unrelated global declarations as roots. Recursive SCCs terminate.
        loop {
            let mut changes = Vec::new();
            for (name, references) in &edges {
                let mut proof = proofs.get(name).cloned();
                for target in references {
                    proof = prefer(proof, proofs.get(target).cloned());
                }
                if proof.as_ref() != proofs.get(name) {
                    if let Some(proof) = proof {
                        changes.push((name.clone(), proof));
                    }
                }
            }
            if changes.is_empty() {
                break;
            }
            proofs.extend(changes);
        }
        proofs.retain(|name, _| functions.contains(name));
        proofs
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn function(name: &str, ret: Typ, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params: vec![],
            ret,
            body,
            type_params: vec![],
        }
    }
    fn invalid() -> Typ {
        ResolutionFailure::Invalid("cycle".into()).typ()
    }
    #[test]
    fn recursive_proofs_follow_schemas_closures_and_real_globals_but_not_unrelated_roots() {
        let boxed = Typ::Named("Boxed".into());
        let tuple = crate::core_ir::tuples::typ(&[Typ::Array(Box::new(boxed.clone())), Typ::Int]);
        let mut decls = vec![
            Decl::Struct {
                name: "Boxed".into(),
                fields: vec![("bad".into(), invalid())],
                type_params: vec![],
            },
            Decl::Global {
                name: "bad".into(),
                typ: boxed.clone(),
                init: None,
                mutable: false,
                constant_value: false,
            },
            function(
                "clean",
                Typ::Int,
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "global",
                Typ::Int,
                vec![
                    Stmt::Expr(Expr::Ident("bad".into())),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ),
            function(
                "shadow",
                Typ::Int,
                vec![
                    Stmt::Let("bad".into(), Some(Typ::Int), Expr::IntLit(42)),
                    Stmt::Return(Some(Expr::Ident("bad".into()))),
                ],
            ),
            function(
                "container",
                Typ::Int,
                vec![
                    Stmt::Let("local".into(), Some(tuple), Expr::UnitLit),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ),
            function(
                "closure",
                Typ::Int,
                vec![
                    Stmt::Expr(Expr::Closure {
                        params: vec![("arg".into(), boxed)],
                        ret: Typ::Int,
                        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                        captures: vec![],
                    }),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ),
            function(
                "generic",
                Typ::Generic("T".into()),
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
        ];
        let proofs = TypeChecker::new().typed_proof_functions(&decls);
        for name in ["global", "container", "closure"] {
            assert!(proofs[name].is_invalid(), "{name}");
        }
        for name in ["clean", "shadow", "generic"] {
            assert!(!proofs.contains_key(name), "{name}");
        }
        decls.push(function(
            "ref",
            crate::core_ir::rust_references::value_type(
                crate::core_ir::rust_references::SHARED_VALUE,
                invalid(),
            )
            .unwrap(),
            vec![],
        ));
        assert!(TypeChecker::new().typed_proof_functions(&decls)["ref"].is_invalid());
    }
}

#[cfg(test)]
mod graph_tests {
    use super::*;
    fn function(name: &str, ret: Typ, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params: vec![],
            ret,
            body,
            type_params: vec![],
        }
    }
    #[test]
    fn globals_follow_initializer_helpers_and_call_cycles_without_poisoning_unrelated_functions() {
        let call = |name: &str| Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args: vec![],
        };
        let proof = ResolutionFailure::Invalid("cyclic alias".into()).typ();
        let declarations = vec![
            function(
                "helper",
                Typ::Int,
                vec![
                    Stmt::Let("unused".into(), Some(proof), Expr::IntLit(0)),
                    Stmt::Return(Some(Expr::IntLit(77))),
                ],
            ),
            Decl::Global {
                name: "global".into(),
                typ: Typ::Int,
                init: Some(Box::new(call("helper"))),
                mutable: false,
                constant_value: false,
            },
            function(
                "reader",
                Typ::Int,
                vec![Stmt::Return(Some(Expr::Ident("global".into())))],
            ),
            function(
                "clean",
                Typ::Int,
                vec![Stmt::Return(Some(Expr::IntLit(42)))],
            ),
            function(
                "a",
                Typ::Int,
                vec![
                    Stmt::Expr(call("b")),
                    Stmt::Return(Some(Expr::Ident("global".into()))),
                ],
            ),
            function("b", Typ::Int, vec![Stmt::Return(Some(call("a")))]),
        ];
        let proofs = TypeChecker::new().typed_proof_functions(&declarations);
        for name in ["reader", "helper", "a", "b"] {
            assert!(proofs[name].is_invalid(), "{name}");
        }
        assert!(!proofs.contains_key("clean"));
    }
}

#[cfg(test)]
mod ordering_and_guard_tests {
    use super::*;
    fn function(name: &str, ret: Typ, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params: vec![],
            ret,
            body,
            type_params: vec![],
        }
    }
    #[test]
    fn proof_diagnostics_are_independent_of_declaration_and_reference_order() {
        let call = |name: &str| {
            Stmt::Expr(Expr::Call {
                callee: Box::new(Expr::Ident(name.into())),
                args: vec![],
            })
        };
        let declarations = vec![
            function(
                "z",
                ResolutionFailure::Invalid("z failure".into()).typ(),
                vec![],
            ),
            function(
                "a",
                ResolutionFailure::Invalid("a failure".into()).typ(),
                vec![],
            ),
            function(
                "unsupported",
                ResolutionFailure::Unsupported("0 unsupported".into()).typ(),
                vec![],
            ),
            function(
                "main",
                Typ::Int,
                vec![
                    call("z"),
                    call("a"),
                    call("unsupported"),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
            ),
        ];
        let first = TypeChecker::new().typed_proof_functions(&declarations);
        let mut reversed = declarations.clone();
        reversed.reverse();
        for decl in &mut reversed {
            if let Decl::Function { name, body, .. } = decl
                && name == "main"
            {
                body.reverse();
            }
        }
        let second = TypeChecker::new().typed_proof_functions(&reversed);
        assert_eq!(first, second);
        assert_eq!(
            first["main"],
            ResolutionFailure::Invalid("a failure".into())
        );
    }
    #[test]
    fn typed_block_only_in_statement_match_guard_is_not_skipped_by_presence_scan() {
        let guard = Expr::Block {
            body: vec![Stmt::Let(
                "unused".into(),
                Some(ResolutionFailure::Invalid("guard cycle".into()).typ()),
                Expr::IntLit(0),
            )],
            result: Some(Box::new(Expr::BoolLit(true))),
        };
        let declarations = vec![function(
            "main",
            Typ::Int,
            vec![Stmt::Match {
                scrutinee: Expr::BoolLit(true),
                arms: vec![crate::core_ir::MatchArm {
                    pattern: "true".into(),
                    guard: Some(guard),
                    body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                }],
            }],
        )];
        assert!(type_proofs::present(&declarations));
        assert!(TypeChecker::new().typed_proof_functions(&declarations)["main"].is_invalid());
    }
}
