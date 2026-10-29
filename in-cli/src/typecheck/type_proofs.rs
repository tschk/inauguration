use super::*;
use crate::core_ir::type_proofs::{self, ResolutionFailure};
use std::cell::RefCell;

use crate::core_ir::type_proofs::prefer_failure as prefer;

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
                let result =
                    if let Some(signature) = crate::core_ir::rust_callables::signature_of(typ) {
                        let mut failure = None;
                        signature.visit_types(&mut |child| {
                            failure = prefer(failure.take(), self.type_proof(child, active));
                        });
                        failure
                    } else if crate::core_ir::rust_callables::is_encoded(name) {
                        Some(ResolutionFailure::Unsupported(
                            "malformed Rust callable signature".into(),
                        ))
                    } else if let Some(array) = crate::core_ir::rust_arrays::decode(name) {
                        self.type_proof(&array.element, active)
                    } else if let Some(reference) = crate::core_ir::rust_references::decode(name) {
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
    /// Observe typed sites and private value failures with the scope-aware walker. This
    /// query deliberately does not treat ordinary deferred checking as proof.
    /// A normalized private view makes local shadows distinct from globals and
    /// call targets, without changing the input or adding expression markers.
    pub(crate) fn typed_proof_nodes(&self, decls: &[Decl]) -> HashMap<String, ResolutionFailure> {
        if !type_proofs::present(decls) {
            return HashMap::new();
        }
        let mut view = UnifiedModule::new(decls.to_vec());
        crate::core_bindings::normalize_decls(&mut view.decls);
        if view
            .decls
            .iter()
            .any(|decl| matches!(decl, Decl::Class { .. }))
        {
            crate::lower_core::desugar_class_declarations(&mut view);
            crate::core_bindings::normalize_decls(&mut view.decls);
        }
        let facts = self.collect_decl_facts(&view.decls);
        let mut view = view.decls;
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
            crate::core_bindings::rewrite_nominal_names_with_candidates(
                std::slice::from_mut(decl),
                &|name| {
                    let proof = facts.type_proof(&Typ::Named(name.into()), &mut HashSet::new());
                    let previous = result.take();
                    *result.borrow_mut() = prefer(previous, proof);
                    None
                },
                &|name| {
                    let previous = result.take();
                    *result.borrow_mut() = prefer(previous, type_proofs::decode(name));
                    if nodes.contains(name) {
                        references.borrow_mut().insert(name.to_string());
                    }
                    None
                },
                &|_| None,
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
        proofs
    }

    /// Function proofs include structurally desugared class methods under their
    /// production names. Unreferenced global proofs do not poison clean callers.
    pub(crate) fn typed_proof_functions(
        &self,
        decls: &[Decl],
    ) -> HashMap<String, ResolutionFailure> {
        let mut proofs = self.typed_proof_nodes(decls);
        proofs.retain(|name, _| {
            !decls
                .iter()
                .any(|decl| matches!(decl, Decl::Global { name: global, .. } if global == name))
        });
        proofs
    }

    /// Initializer/type proofs keyed by actual global declarations, including
    /// dependencies on other globals and callable bodies.
    pub(crate) fn typed_proof_globals(&self, decls: &[Decl]) -> HashMap<String, ResolutionFailure> {
        let mut proofs = self.typed_proof_nodes(decls);
        proofs.retain(|name, _| {
            decls
                .iter()
                .any(|decl| matches!(decl, Decl::Global { name: global, .. } if global == name))
        });
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

#[cfg(test)]
mod declaration_proof_tests {
    use super::*;
    #[test]
    fn uninitialized_annotations_in_guards_and_closures_are_observed() {
        let bad = ResolutionFailure::Invalid("declaration cycle".into()).typ();
        let guard = Expr::Block {
            body: vec![Stmt::Declare("unused".into(), bad.clone())],
            result: Some(Box::new(Expr::BoolLit(true))),
        };
        let decls = vec![Decl::Function {
            name: "work".into(),
            params: vec![],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![
                Stmt::Expr(Expr::Closure {
                    params: vec![],
                    ret: Typ::Void,
                    captures: vec![],
                    body: vec![Stmt::Declare("nested".into(), bad)],
                }),
                Stmt::Match {
                    scrutinee: Expr::BoolLit(true),
                    arms: vec![crate::core_ir::MatchArm {
                        pattern: "true".into(),
                        guard: Some(guard),
                        body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                    }],
                },
            ],
        }];
        assert!(type_proofs::present(&decls));
        assert!(TypeChecker::new().typed_proof_functions(&decls)["work"].is_invalid());
    }
}

#[cfg(test)]
mod node_proof_tests {
    use super::*;

    fn function(name: &str, body: Vec<Stmt>) -> Decl {
        Decl::Function {
            name: name.into(),
            params: vec![],
            ret: Typ::Int,
            body,
            type_params: vec![],
        }
    }

    #[test]
    fn value_only_failures_follow_real_dependencies_without_mutating_source() {
        let call = |name: &str| Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args: vec![],
        };
        for failure in [
            ResolutionFailure::Invalid("value cycle".into()),
            ResolutionFailure::Unsupported("value projection".into()),
        ] {
            let decls = vec![
                function(
                    "helper",
                    vec![
                        Stmt::If {
                            cond: Expr::BoolLit(false),
                            then_body: vec![Stmt::Expr(call(&failure.symbol()))],
                            else_body: vec![],
                        },
                        Stmt::Return(Some(Expr::IntLit(42))),
                    ],
                ),
                Decl::Global {
                    name: "VALUE".into(),
                    typ: Typ::Int,
                    init: Some(Box::new(call("helper"))),
                    mutable: false,
                    constant_value: false,
                },
                function("main", vec![Stmt::Return(Some(call("helper")))]),
                function(
                    "reader",
                    vec![Stmt::Return(Some(Expr::Ident("VALUE".into())))],
                ),
                function("clean", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
                function(
                    "shadow",
                    vec![
                        Stmt::Let("VALUE".into(), Some(Typ::Int), Expr::IntLit(42)),
                        Stmt::Return(Some(Expr::Ident("VALUE".into()))),
                    ],
                ),
            ];
            let original = decls.clone();
            let checker = TypeChecker::new();
            let nodes = checker.typed_proof_nodes(&decls);
            assert_eq!(nodes.len(), 4);
            for name in ["helper", "VALUE", "main", "reader"] {
                assert_eq!(nodes[name], failure, "{name}");
            }
            assert!(!nodes.contains_key("clean"));
            assert!(!nodes.contains_key("shadow"));
            assert_eq!(
                checker.typed_proof_globals(&decls),
                HashMap::from([("VALUE".into(), failure)])
            );
            assert_eq!(decls, original);
        }
    }

    #[test]
    fn invalid_value_failure_dominates_unsupported_type_failure() {
        let invalid = ResolutionFailure::Invalid("value cycle".into());
        let declarations = vec![function(
            "main",
            vec![
                Stmt::Let(
                    "unused".into(),
                    Some(ResolutionFailure::Unsupported("opaque type".into()).typ()),
                    Expr::IntLit(0),
                ),
                Stmt::Expr(Expr::Binary {
                    op: "&&".into(),
                    lhs: Box::new(Expr::BoolLit(false)),
                    rhs: Box::new(Expr::Ident(invalid.symbol())),
                }),
                Stmt::Return(Some(Expr::IntLit(42))),
            ],
        )];
        assert_eq!(
            TypeChecker::new().typed_proof_nodes(&declarations)["main"],
            invalid
        );
    }

    #[test]
    fn global_annotation_proofs_are_retained_without_poisoning_unrelated_functions() {
        let bad = ResolutionFailure::Unsupported("opaque associated output".into());
        let decls = vec![
            Decl::Global {
                name: "bad".into(),
                typ: bad.clone().typ(),
                init: Some(Box::new(Expr::IntLit(77))),
                mutable: false,
                constant_value: false,
            },
            function(
                "reader",
                vec![Stmt::Return(Some(Expr::Ident("bad".into())))],
            ),
            function(
                "transitive",
                vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("reader".into())),
                    args: vec![],
                }))],
            ),
            function("clean", vec![Stmt::Return(Some(Expr::IntLit(42)))]),
        ];
        let original = decls.clone();
        let checker = TypeChecker::new();
        let nodes = checker.typed_proof_nodes(&decls);
        assert_eq!(nodes.len(), 3);
        for name in ["bad", "reader", "transitive"] {
            assert_eq!(nodes[name], bad);
        }
        let globals = checker.typed_proof_globals(&decls);
        assert_eq!(globals, HashMap::from([("bad".into(), bad.clone())]));
        let functions = checker.typed_proof_functions(&decls);
        assert_eq!(functions.len(), 2);
        assert!(!functions.contains_key("bad"));
        assert!(!functions.contains_key("clean"));
        assert_eq!(decls, original);
        // A global-only input must still expose its proof to the folding gate.
        assert_eq!(checker.typed_proof_nodes(&decls[..1]), globals);
        assert!(checker.typed_proof_functions(&decls[..1]).is_empty());
    }

    #[test]
    fn raw_class_methods_use_production_names_and_desugared_field_facts() {
        let bad = ResolutionFailure::Invalid("class field cycle".into());
        let decls = vec![Decl::Class {
            name: "Row".into(),
            fields: vec![("field".into(), bad.clone().typ())],
            methods: vec![function("get", vec![Stmt::Return(Some(Expr::IntLit(77)))])],
            visibility: crate::core_ir::Visibility::Pub,
            extends: None,
            implements: vec![],
            type_params: vec![],
        }];
        let original = decls.clone();
        let checker = TypeChecker::new();
        let nodes = checker.typed_proof_nodes(&decls);
        assert_eq!(nodes, HashMap::from([("Row_get".into(), bad)]));
        assert_eq!(checker.typed_proof_functions(&decls), nodes);
        assert!(checker.typed_proof_globals(&decls).is_empty());
        assert_eq!(decls, original);
    }
}
