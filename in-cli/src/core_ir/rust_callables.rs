//! Transient exact Rust function-pointer source facts; not a callable ABI adapter.
//! Decode/compare before scalar erasure. Unsupported forms remain represented so
//! an unowned ABI, unsafe or higher-ranked pointer cannot become an ordinary word.
use super::{Expr, Typ};
use serde::{Deserialize, Serialize};

const PREFIX: &str = "#rust-callable-type:";
const DECLARATION: &str = "#rust-callable-declaration:";
const ITEM_PREFIX: &str = "#rust-callable-item:";
const ALIAS_ORIGIN: &str = "#rust-callable-alias-origin:";
const CAST: &str = "#rust-callable-cast:";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Abi {
    Rust,
    // Preserve literal ABI spelling. No ABI name implies compatibility here.
    Explicit(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Signature {
    pub abi: Abi,
    pub unsafe_call: bool,
    pub variadic: bool,
    // HRTB/lifetime obligations are deliberately not modeled by this codec.
    pub bound_lifetimes: bool,
    pub parameters: Vec<Typ>,
    pub result: Typ,
}

/// Function items are zero-sized source identities, distinct from fn pointers.
/// `identity` is the exact owned semantic symbol, never a basename/link target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Item {
    pub identity: String,
    pub signature: Signature,
}

pub(crate) fn item_type(item: Item) -> Typ {
    Typ::Named(format!(
        "{ITEM_PREFIX}{}",
        serde_json::to_string(&item).expect("callable item serializes")
    ))
}

pub(crate) fn is_item(name: &str) -> bool {
    name.starts_with(ITEM_PREFIX)
}

pub(crate) fn decode_item(name: &str) -> Option<Item> {
    let item: Item = serde_json::from_str(name.strip_prefix(ITEM_PREFIX)?).ok()?;
    (!item.identity.is_empty()).then_some(item)
}

/// Signature inspection deliberately does NOT establish source type equality or
/// item-to-pointer coercion. Callers retain `decode_item` identity separately.
pub(crate) fn signature_of(typ: &Typ) -> Option<Signature> {
    let Typ::Named(name) = typ else { return None };
    decode(name).or_else(|| decode_item(name).map(|item| item.signature))
}

pub(crate) fn typ(signature: Signature) -> Typ {
    Typ::Named(format!(
        "{PREFIX}{}",
        serde_json::to_string(&signature).expect("callable type serializes")
    ))
}

/// Pending explicit Rust fn-pointer cast. Target metadata is not evidence that
/// the child is callable: validate the resolved child summary before rewriting.
pub(crate) fn cast_operation(target: &Signature) -> String {
    format!(
        "{CAST}{}",
        serde_json::to_string(target).expect("callable cast serializes")
    )
}

pub(crate) fn cast(expression: Expr, target: Signature) -> Expr {
    Expr::Unary {
        op: cast_operation(&target),
        expr: Box::new(expression),
    }
}

pub(crate) fn decode_cast(operation: &str) -> Option<Signature> {
    serde_json::from_str(operation.strip_prefix(CAST)?).ok()
}

pub(crate) fn is_cast(operation: &str) -> bool {
    operation.starts_with(CAST)
}

/// Exact source target of a frontend-generated use-alias wrapper, represented
/// only as Unary(op, UnitLit). Never infer this identity from wrapper body shape.
pub(crate) fn alias_origin(target: &str) -> String {
    format!(
        "{ALIAS_ORIGIN}{}",
        serde_json::to_string(target).expect("alias target serializes")
    )
}

pub(crate) fn decode_alias_origin(operation: &str) -> Option<String> {
    let target: String = serde_json::from_str(operation.strip_prefix(ALIAS_ORIGIN)?).ok()?;
    (!target.is_empty()).then_some(target)
}

pub(crate) fn is_alias_origin(operation: &str) -> bool {
    operation.starts_with(ALIAS_ORIGIN)
}

/// Authoritative source declaration facts, stored as Unary(op, UnitLit).
/// This is metadata only; ordinary Facts function shape does not prove Rust ABI.
pub(crate) fn declaration(signature: &Signature) -> String {
    format!(
        "{DECLARATION}{}",
        serde_json::to_string(signature).expect("callable signature serializes")
    )
}

pub(crate) fn decode_declaration(operation: &str) -> Option<Signature> {
    serde_json::from_str(operation.strip_prefix(DECLARATION)?).ok()
}

pub(crate) fn is_declaration(operation: &str) -> bool {
    operation.starts_with(DECLARATION)
}

pub(crate) fn decode(name: &str) -> Option<Signature> {
    serde_json::from_str(name.strip_prefix(PREFIX)?).ok()
}

/// Recognize malformed private payloads too: callers must refuse them instead of
/// falling back to nominal/runtime-word inference.
pub(crate) fn is_encoded(name: &str) -> bool {
    name.starts_with(PREFIX) || is_item(name)
}

impl Signature {
    /// Syntax admission only. Source argument identity, backend word size and
    /// every parameter/result representation must still be checked separately.
    pub(crate) fn ordinary_rust_form(&self) -> bool {
        self.abi == Abi::Rust && !self.unsafe_call && !self.variadic && !self.bound_lifetimes
    }

    /// Expose children to the existing shared recursive type visitor. This does
    /// not recurse itself or erase reference/array/generic/source identities.
    pub(crate) fn visit_types(&self, visit: &mut impl FnMut(&Typ)) {
        for parameter in &self.parameters {
            visit(parameter);
        }
        visit(&self.result);
    }

    pub(crate) fn visit_types_mut(&mut self, visit: &mut impl FnMut(&mut Typ)) {
        for parameter in &mut self.parameters {
            visit(parameter);
        }
        visit(&mut self.result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signature(parameter: Typ) -> Signature {
        Signature {
            abi: Abi::Rust,
            unsafe_call: false,
            variadic: false,
            bound_lifetimes: false,
            parameters: vec![parameter],
            result: Typ::Named("#rust-integer-i64".into()),
        }
    }
    #[test]
    fn callable_codec_preserves_all_source_facts() {
        let original = signature(Typ::Named("#rust-integer-usize".into()));
        let Typ::Named(name) = typ(original.clone()) else {
            panic!()
        };
        assert_eq!(decode(&name), Some(original.clone()));
        assert_ne!(original, signature(Typ::Named("#rust-integer-u64".into())));
        assert_ne!(original, signature(Typ::Int));
        assert!(original.ordinary_rust_form());
        for changed in [
            Signature {
                abi: Abi::Explicit("C".into()),
                ..original.clone()
            },
            Signature {
                unsafe_call: true,
                ..original.clone()
            },
            Signature {
                variadic: true,
                ..original.clone()
            },
            Signature {
                bound_lifetimes: true,
                ..original.clone()
            },
        ] {
            assert!(!changed.ordinary_rust_form());
            let Typ::Named(encoded) = typ(changed.clone()) else {
                panic!()
            };
            assert_eq!(decode(&encoded), Some(changed));
        }
        assert!(decode("fn(i64)->i64").is_none());
        assert!(decode("#rust-callable-type:not-json").is_none());
        assert!(is_encoded("#rust-callable-type:not-json"));
    }
    #[test]
    fn pending_cast_retains_target_and_one_untrusted_child() {
        let target = signature(Typ::Generic("T".into()));
        let child = Expr::Call {
            callee: Box::new(Expr::Ident("effect".into())),
            args: vec![],
        };
        let Expr::Unary { op, expr } = cast(child.clone(), target.clone()) else {
            panic!()
        };
        assert!(is_cast(&op));
        assert_eq!(decode_cast(&op), Some(target));
        assert_eq!(*expr, child);
        assert!(decode(&op).is_none());
        assert!(decode_declaration(&op).is_none());
        assert!(is_cast("#rust-callable-cast:malformed"));
        assert!(decode_cast("#rust-callable-cast:malformed").is_none());
        // A numeric child stays numeric; the target never creates source proof.
        let Expr::Unary { expr, .. } = cast(Expr::IntLit(42), signature(Typ::Int)) else {
            panic!()
        };
        assert_eq!(*expr, Expr::IntLit(42));
    }

    #[test]
    fn alias_origin_is_explicit_source_symbol_metadata() {
        let marker = alias_origin("source::helper");
        assert_eq!(
            decode_alias_origin(&marker).as_deref(),
            Some("source::helper")
        );
        assert!(is_alias_origin(&marker));
        assert!(decode_declaration(&marker).is_none());
        assert!(decode_alias_origin("#rust-callable-alias-origin:malformed").is_none());
        assert!(is_alias_origin("#rust-callable-alias-origin:malformed"));
        assert!(decode_alias_origin(&alias_origin("")).is_none());
        // Namespace projection remaps the decoded symbol with its value callback.
        let remapped = alias_origin("dependency::source::helper");
        assert_eq!(
            decode_alias_origin(&remapped).as_deref(),
            Some("dependency::source::helper")
        );
    }

    #[test]
    fn function_items_keep_identity_separate_from_pointer_signatures() {
        let source = signature(Typ::Named("#rust-integer-i64".into()));
        let a = item_type(Item {
            identity: "module::a".into(),
            signature: source.clone(),
        });
        let b = item_type(Item {
            identity: "module::b".into(),
            signature: source.clone(),
        });
        let pointer = typ(source.clone());
        assert_ne!(a, b);
        assert_ne!(a, pointer);
        assert_eq!(signature_of(&a), Some(source.clone()));
        assert_eq!(signature_of(&pointer), Some(source.clone()));
        let Typ::Named(name) = a else { panic!() };
        let mut item = decode_item(&name).unwrap();
        assert!(decode(&name).is_none());
        // Namespace integration must use its value callback for identity, then
        // its recursive type callback for all signature children.
        item.identity = "dependency::module::a".into();
        item.signature.visit_types_mut(&mut |typ| {
            if matches!(typ, Typ::Named(name) if name == "#rust-integer-i64") {
                *typ = Typ::Named("#rust-integer-isize".into());
            }
        });
        let Typ::Named(remapped) = item_type(item.clone()) else {
            panic!()
        };
        assert_eq!(decode_item(&remapped), Some(item));
        assert!(is_encoded("#rust-callable-item:malformed"));
        assert!(decode_item("#rust-callable-item:malformed").is_none());
        assert!(signature_of(&Typ::Int).is_none());
    }

    #[test]
    fn declaration_marker_preserves_authoritative_source_signature() {
        let original = Signature {
            abi: Abi::Explicit("C".into()),
            unsafe_call: true,
            variadic: true,
            bound_lifetimes: true,
            ..signature(Typ::Generic("T".into()))
        };
        let encoded = declaration(&original);
        assert!(is_declaration(&encoded));
        assert_eq!(decode_declaration(&encoded), Some(original.clone()));
        assert!(decode(&encoded).is_none());
        assert!(decode_declaration("#rust-callable-declaration:malformed").is_none());
        assert!(is_declaration("#rust-callable-declaration:malformed"));
        let mut decoded = decode_declaration(&encoded).unwrap();
        decoded.visit_types_mut(&mut |typ| {
            if matches!(typ, Typ::Generic(name) if name == "T") {
                *typ = Typ::Named("scope::Row".into());
            }
        });
        let reparsed = decode_declaration(&declaration(&decoded)).unwrap();
        assert_eq!(reparsed.parameters, vec![Typ::Named("scope::Row".into())]);
        assert_eq!(reparsed.abi, original.abi);
        assert!(reparsed.unsafe_call && reparsed.variadic && reparsed.bound_lifetimes);
    }

    #[test]
    fn callable_children_share_substitution_and_failure_visitors() {
        let mut source = signature(Typ::Generic("T".into()));
        source.result = Typ::Generic("T".into());
        let invalid =
            super::super::type_proofs::ResolutionFailure::Invalid("cyclic source type".into())
                .typ();
        source.visit_types_mut(&mut |typ| {
            if matches!(typ, Typ::Generic(name) if name == "T") {
                *typ = invalid.clone();
            }
        });
        let mut children = vec![];
        source.visit_types(&mut |typ| children.push(typ.clone()));
        assert_eq!(children, vec![invalid.clone(), invalid]);
        assert_eq!(source.parameters.len(), 1);
    }
    #[test]
    fn callable_nested_proofs_retain_invalid_priority_and_metadata_presence() {
        use super::super::{
            Decl, UnifiedModule,
            type_proofs::{self, MetadataKind, ResolutionFailure},
        };
        let mut value = signature(ResolutionFailure::Unsupported("unknown argument".into()).typ());
        value.result = ResolutionFailure::Invalid("invalid result".into()).typ();
        let callback = typ(value);
        assert!(
            type_proofs::failure_in_type(&callback)
                .unwrap()
                .is_invalid()
        );
        let module = UnifiedModule::new(vec![Decl::Function {
            name: "apply".into(),
            params: vec![("callback".into(), callback)],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![],
        }]);
        assert!(type_proofs::present(&module.decls));
        assert!(type_proofs::metadata_present(
            &module.decls,
            MetadataKind::RustCallables
        ));
        assert!(
            crate::typecheck::TypeChecker::new()
                .typed_proof_nodes(&module.decls)
                .values()
                .any(ResolutionFailure::is_invalid)
        );
        let malformed = Typ::Named("#rust-callable-type:not-json".into());
        assert!(matches!(
            type_proofs::failure_in_type(&malformed),
            Some(ResolutionFailure::Unsupported(_))
        ));
    }

    #[test]
    fn shared_namespace_projection_rewrites_callable_children() {
        use super::super::{Decl, UnifiedModule};
        let callback = signature(Typ::Named("Local".into()));
        let mut module = UnifiedModule::new(vec![Decl::Function {
            name: "apply".into(),
            params: vec![("callback".into(), typ(callback))],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![],
        }]);
        crate::core_bindings::rewrite_named_types(&mut module.decls, &|name| {
            (name == "Local").then(|| Typ::Named("outer::Local".into()))
        });
        let Decl::Function { params, .. } = &module.decls[0] else {
            panic!()
        };
        let Typ::Named(encoded) = &params[0].1 else {
            panic!()
        };
        assert_eq!(
            decode(encoded).unwrap().parameters,
            vec![Typ::Named("outer::Local".into())]
        );
    }
}

/// Alias provenance names declaration targets, independent of runtime binders.
/// Reuse the expression traversal without rewriting any executable identifier.
pub(crate) fn rewrite_alias_origins(
    declarations: &mut [super::Decl],
    resolve: &impl Fn(&str) -> Option<String>,
) {
    crate::core_opt::map_decl_exprs_mut(declarations, &mut |expression| {
        if let super::Expr::Unary { op, .. } = expression
            && let Some(target) = decode_alias_origin(op)
            && let Some(qualified) = resolve(&target)
        {
            *op = alias_origin(&qualified);
        }
    });
}
#[cfg(test)]
mod alias_origin_namespace_tests {
    use super::*;
    use crate::core_ir::{Decl, Expr, Stmt, Typ};
    #[test]
    fn declaration_origin_is_qualified_despite_same_named_runtime_parameter() {
        let mut declarations = vec![Decl::Function {
            name: "pick".into(),
            params: vec![("first".into(), Typ::Int)],
            ret: Typ::Int,
            type_params: vec![],
            body: vec![
                Stmt::Expr(Expr::Unary {
                    op: alias_origin("first"),
                    expr: Box::new(Expr::UnitLit),
                }),
                Stmt::Return(Some(Expr::Ident("first".into()))),
            ],
        }];
        rewrite_alias_origins(&mut declarations, &|name| {
            (name == "first").then(|| "outer::first".into())
        });
        let Decl::Function { params, body, .. } = &declarations[0] else {
            panic!()
        };
        assert_eq!(params[0].0, "first");
        let Stmt::Expr(Expr::Unary { op, .. }) = &body[0] else {
            panic!()
        };
        assert_eq!(decode_alias_origin(op).as_deref(), Some("outer::first"));
        assert!(matches!(&body[1],Stmt::Return(Some(Expr::Ident(name))) if name=="first"));
    }
}
