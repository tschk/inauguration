//! Typed tuples and owned Options use ordinary Core IR structs after type resolution.
use super::{Decl, Expr, Stmt, Typ, UnifiedModule};
use std::collections::BTreeMap;

pub(crate) const VALUE: &str = "#tuple-value";
pub(crate) const LET_CHECK: &str = "#tuple-let-check";
pub(crate) const LET_FIELD: &str = "#tuple-let-field";

pub(crate) fn is_marker(name: &str) -> bool {
    matches!(name, VALUE | LET_CHECK | LET_FIELD)
}

pub(crate) fn let_field(typ: &Typ, index: i64, from_end: bool) -> Option<(String, Typ)> {
    let Typ::Named(name) = typ else {
        return None;
    };
    let fields = fields(name)?;
    let index = usize::try_from(index).ok()?;
    let index = if from_end {
        fields.len().checked_sub(index.checked_add(1)?)?
    } else {
        index
    };
    fields.get(index).cloned()
}

const PREFIX: &str = "in-tuple-";

#[derive(serde::Serialize, serde::Deserialize)]
enum Shape {
    Core(Typ),
    Tuple(Vec<Shape>),
    Array(Box<Shape>),
    Vector(Box<Shape>),
    Option(Box<Shape>),
}

impl Shape {
    fn from_type(typ: &Typ) -> Self {
        // Preserve source proofs until reference, array and numeric operator resolution.
        if super::rust_references::has_reference_proof(typ)
            || matches!(typ, Typ::Named(name) if super::rust_arrays::decode(name).is_some())
            || matches!(typ, Typ::Named(name) if super::rust_integer_type_name(name).is_some() || super::rust_floats::is_proof(name))
        {
            return Self::Core(typ.clone());
        }
        match typ {
            Typ::Array(item) => return Self::Array(Box::new(Self::from_type(item))),
            Typ::Vector(item) => return Self::Vector(Box::new(Self::from_type(item))),
            _ => {}
        }
        match typ.canonical() {
            Typ::Named(name) => {
                if let Some(payload) = super::rust_options::payload(&name) {
                    Self::Option(Box::new(Self::from_type(&payload)))
                } else {
                    match decode(&name) {
                        Some(items) => Self::Tuple(items),
                        None => Self::Core(Typ::Named(name)),
                    }
                }
            }
            typ => Self::Core(typ),
        }
    }
    fn into_type(self) -> Typ {
        match self {
            Self::Core(typ) => typ,
            Self::Tuple(items) => Typ::Named(encode(&items)),
            Self::Array(item) => Typ::Array(Box::new(item.into_type())),
            Self::Vector(item) => Typ::Vector(Box::new(item.into_type())),
            Self::Option(item) => super::rust_options::typ(item.into_type()),
        }
    }
}

fn encode(items: &[Shape]) -> String {
    let bytes = serde_json::to_vec(items).expect("Core IR tuple shapes serialize");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{PREFIX}{hex}")
}

fn decode(name: &str) -> Option<Vec<Shape>> {
    let hex = name.strip_prefix(PREFIX)?;
    if hex.len() % 2 != 0 || !hex.is_ascii() {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    let items: Vec<Shape> = serde_json::from_slice(&bytes).ok()?;
    if items.is_empty() || encode(&items) != name {
        return None;
    }
    Some(items)
}

pub(crate) fn typ(items: &[Typ]) -> Typ {
    if items.is_empty() {
        return Typ::Void;
    }
    Typ::Named(encode(
        &items.iter().map(Shape::from_type).collect::<Vec<_>>(),
    ))
}

pub(crate) fn fields(name: &str) -> Option<Vec<(String, Typ)>> {
    Some(
        decode(name)?
            .into_iter()
            .enumerate()
            .map(|(i, typ)| (format!("_{i}"), typ.into_type()))
            .collect(),
    )
}

/// Project generated aggregate identities while retaining preexisting schema conflicts.
pub(crate) fn project_declarations(decls: &mut Vec<Decl>, mut project: impl FnMut(&mut [Decl])) {
    let originals: Vec<_> = decls
        .iter()
        .map(|decl| match decl {
            Decl::Struct {
                name,
                fields: actual,
                type_params,
            } => fields(name)
                .or_else(|| super::rust_options::fields(name))
                .map(|expected| {
                    let canonical = type_params.is_empty() && &expected == actual;
                    (
                        name.clone(),
                        canonical,
                        Decl::Struct {
                            name: name.clone(),
                            fields: expected,
                            type_params: vec![],
                        },
                    )
                }),
            _ => None,
        })
        .collect();
    let mut schemas: Vec<_> = originals
        .iter()
        .filter_map(|original| original.as_ref().map(|(_, _, schema)| schema.clone()))
        .collect();
    project(decls);
    project(&mut schemas);
    let mut schemas = schemas.into_iter();
    for (decl, original) in decls.iter_mut().zip(&originals) {
        if let Some((_, canonical, _)) = original
            && let Some(Decl::Struct {
                name: projected,
                fields: projected_fields,
                ..
            }) = schemas.next()
            && let Decl::Struct { name, fields, .. } = decl
        {
            // Derive identity from the original encoding, never malformed payload fields.
            *name = projected;
            if *canonical {
                *fields = projected_fields;
            }
        }
    }
    let mut seen: std::collections::HashMap<Decl, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    let mut index = 0;
    decls.retain(|decl| {
        let original = &originals[index];
        index += 1;
        let Some((original, true, _)) = original else {
            return true;
        };
        let origins = seen.entry(decl.clone()).or_default();
        let first = origins.is_empty();
        let repeated_original = !origins.insert(original.clone());
        // Repeated original declarations remain errors even if their first occurrence merged.
        first || repeated_original
    });
}

pub(crate) fn register(module: &mut UnifiedModule) {
    fn add(typ: &Typ, shapes: &mut BTreeMap<String, Vec<(String, Typ)>>) {
        match typ {
            Typ::Named(name) if !shapes.contains_key(name) => {
                if let Some(fields) = fields(name).or_else(|| super::rust_options::fields(name)) {
                    shapes.insert(name.clone(), fields.clone());
                    for (_, typ) in fields {
                        add(&typ, shapes);
                    }
                } else if let Some(signature) = super::rust_callables::signature_of(typ) {
                    signature.visit_types(&mut |typ| add(typ, shapes));
                } else if let Some(reference) = super::rust_references::decode(name) {
                    add(&reference.referent, shapes);
                    add(&reference.runtime, shapes);
                } else if let Some(array) = super::rust_arrays::decode(name) {
                    add(&array.element, shapes);
                }
            }
            Typ::Array(item) | Typ::Vector(item) => add(item, shapes),
            _ => {}
        }
    }
    fn expr(value: &Expr, shapes: &mut BTreeMap<String, Vec<(String, Typ)>>) {
        match value {
            Expr::StructInit { name, .. } => add(&Typ::Named(name.clone()), shapes),
            Expr::Block { body: stmts, .. } => body(stmts, shapes),
            Expr::Closure {
                params,
                ret,
                body: stmts,
                ..
            } => {
                for (_, typ) in params {
                    add(typ, shapes);
                }
                add(ret, shapes);
                body(stmts, shapes);
            }
            _ => {}
        }
        crate::core_ir::for_each_expr_child(value, &mut |value| expr(value, shapes));
    }
    fn body(stmts: &[Stmt], shapes: &mut BTreeMap<String, Vec<(String, Typ)>>) {
        for stmt in stmts {
            match stmt {
                Stmt::Declare(_, typ) => add(typ, shapes),
                Stmt::Let(_, typ, value) => {
                    if let Some(typ) = typ {
                        add(typ, shapes);
                    }
                    expr(value, shapes);
                }
                Stmt::Assign(_, value)
                | Stmt::Expr(value)
                | Stmt::Throw(value)
                | Stmt::Return(Some(value)) => expr(value, shapes),
                Stmt::IndexAssign { base, index, value } => {
                    for value in [base, index, value] {
                        expr(value, shapes);
                    }
                }
                Stmt::FieldAssign { base, value, .. } => {
                    expr(base, shapes);
                    expr(value, shapes);
                }
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    expr(cond, shapes);
                    body(then_body, shapes);
                    body(else_body, shapes);
                }
                Stmt::Loop {
                    cond, body: stmts, ..
                } => {
                    if let Some(cond) = cond {
                        expr(cond, shapes);
                    }
                    body(stmts, shapes);
                }
                Stmt::Match { scrutinee, arms } => {
                    expr(scrutinee, shapes);
                    for arm in arms {
                        if let Some(guard) = &arm.guard {
                            expr(guard, shapes);
                        }
                        body(&arm.body, shapes);
                    }
                }
                Stmt::Try {
                    body: stmts,
                    catches,
                } => {
                    body(stmts, shapes);
                    for catch in catches {
                        body(&catch.body, shapes);
                    }
                }
                _ => {}
            }
        }
    }
    fn walk_decls(decls: &[Decl], shapes: &mut BTreeMap<String, Vec<(String, Typ)>>) {
        for decl in decls {
            match decl {
                Decl::Struct { fields, .. } => {
                    for (_, typ) in fields {
                        add(typ, shapes);
                    }
                }
                Decl::Class {
                    fields, methods, ..
                } => {
                    for (_, typ) in fields {
                        add(typ, shapes);
                    }
                    walk_decls(methods, shapes);
                }
                Decl::Function {
                    params,
                    ret,
                    body: stmts,
                    ..
                } => {
                    for (_, typ) in params {
                        add(typ, shapes);
                    }
                    add(ret, shapes);
                    body(stmts, shapes);
                }
                Decl::Global { typ, init, .. } => {
                    add(typ, shapes);
                    if let Some(value) = init {
                        expr(value, shapes);
                    }
                }
                Decl::Interface { methods, .. } => {
                    for method in methods {
                        for (_, typ) in &method.params {
                            add(typ, shapes);
                        }
                        add(&method.ret, shapes);
                    }
                }
                _ => {}
            }
        }
    }
    let mut shapes = BTreeMap::new();
    walk_decls(&module.decls, &mut shapes);
    for (name, fields) in shapes {
        if !module.decls.iter().any(|decl| matches!(decl, Decl::Struct { name: existing, fields: existing_fields, .. } if existing == &name && existing_fields == &fields)) {
            // Conflicting user declarations stay duplicated so the verifier rejects them.
            module.decls.push(Decl::Struct { name, fields, type_params: vec![] });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expression_blocks_register_body_types_and_result_shapes() {
        let local = typ(&[Typ::Int, Typ::Bool]);
        let result = typ(&[Typ::Float, Typ::String]);
        let Typ::Named(result_name) = &result else {
            panic!()
        };
        let mut module = UnifiedModule::new(vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Void,
            type_params: vec![],
            body: vec![Stmt::Expr(Expr::Block {
                body: vec![Stmt::Let(
                    "local".into(),
                    Some(local.clone()),
                    Expr::IntLit(0),
                )],
                result: Some(Box::new(Expr::StructInit {
                    name: result_name.clone(),
                    fields: vec![],
                })),
            })],
        }]);
        register(&mut module);
        for expected in [local, result] {
            let Typ::Named(expected) = expected else {
                panic!()
            };
            assert!(
                module
                    .decls
                    .iter()
                    .any(|decl| matches!(decl,Decl::Struct {name,..} if name==&expected))
            );
        }
    }

    #[test]
    fn registration_is_idempotent_and_conflicting_names_fail_verification() {
        for typ in [
            typ(&[Typ::Int, Typ::Bool]),
            super::super::rust_options::typ(Typ::Int),
            super::super::rust_options::typ(Typ::Array(Box::new(Typ::Void))),
            super::super::rust_options::typ(super::super::rust_arrays::typ(Typ::Void, 2)),
        ] {
            let Typ::Named(name) = &typ else {
                panic!("tuple type");
            };
            let function = Decl::Function {
                name: "main".into(),
                params: vec![("pair".into(), typ.clone())],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            };
            let mut module = UnifiedModule::new(vec![function.clone()]);
            register(&mut module);
            let registered = module.clone();
            register(&mut module);
            assert_eq!(module, registered);
            assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
            let mut conflicting = UnifiedModule::new(vec![
                Decl::Struct {
                    name: name.clone(),
                    fields: vec![("wrong".into(), Typ::String)],
                    type_params: vec![],
                },
                function,
            ]);
            register(&mut conflicting);
            let report = crate::core_ir_verifier::verify_for_entry(&conflicting, "main");
            assert_eq!(
                report.reason_code.as_deref(),
                Some("duplicate-top-level-name")
            );
        }
    }

    #[test]
    fn private_intrinsic_declarations_are_rejected() {
        for name in [
            VALUE,
            LET_CHECK,
            LET_FIELD,
            "#receiver-method:answer",
            crate::core_ir::BORROW,
        ] {
            let module = UnifiedModule::new(vec![Decl::Function {
                name: name.into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            }]);
            let report = crate::core_ir_verifier::verify_for_entry(&module, name);
            assert_eq!(
                report.reason_code.as_deref(),
                Some("reserved-intrinsic-name")
            );
        }
    }

    #[test]
    fn nested_shape_names_grow_with_structure_and_round_trip() {
        let mut item = Typ::Int;
        let mut previous = 0;
        for _ in 0..24 {
            let expected = vec![("_0".into(), item.clone()), ("_1".into(), Typ::Bool)];
            item = typ(&[item, Typ::Bool]);
            let Typ::Named(name) = &item else {
                panic!("tuple shape");
            };
            assert!(name.len() < previous + 160);
            assert_eq!(fields(name).unwrap(), expected);
            previous = name.len();
        }
        assert_eq!(typ(&[]), Typ::Void);
        assert!(fields("in-tuple-not-hex").is_none());
    }
}

#[cfg(test)]
mod reference_shape_tests {
    use super::*;
    #[test]
    fn tuple_shapes_preserve_reference_and_integer_proofs() {
        let reference = crate::core_ir::rust_references::value_type(
            crate::core_ir::rust_references::SHARED_VALUE,
            Typ::Named("Row".into()),
        )
        .unwrap();
        let items = vec![
            reference.clone(),
            Typ::Array(Box::new(reference.clone())),
            Typ::Vector(Box::new(reference.clone())),
            Typ::Named("#rust-integer-i8".into()),
        ];
        let Typ::Named(name) = typ(&items) else {
            panic!()
        };
        let recovered: Vec<_> = fields(&name)
            .unwrap()
            .into_iter()
            .map(|(_, typ)| typ)
            .collect();
        assert_eq!(recovered, items);
        let erased = crate::core_ir::rust_references::erase_type(Typ::Named(name));
        assert_eq!(
            erased,
            typ(&[
                Typ::Named("Row".into()),
                Typ::Array(Box::new(Typ::Named("Row".into()))),
                Typ::Vector(Box::new(Typ::Named("Row".into()))),
                Typ::Named("#rust-integer-i8".into())
            ])
        );
    }
}

#[cfg(test)]
mod declaration_tuple_tests {
    use super::*;
    #[test]
    fn declaration_only_tuple_shape_is_registered() {
        let tuple = typ(&[Typ::Int, Typ::Bool]);
        let Typ::Named(name) = &tuple else { panic!() };
        let mut module = UnifiedModule::new(vec![Decl::Function {
            name: "work".into(),
            params: vec![],
            ret: Typ::Void,
            type_params: vec![],
            body: vec![Stmt::Declare("pair".into(), tuple.clone())],
        }]);
        register(&mut module);
        assert!(module.decls.iter().any(|decl| matches!(decl, Decl::Struct {name:actual, fields:actual_fields,..} if actual == name && actual_fields == &fields(name).unwrap())));
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    fn schema(typ: &Typ) -> Decl {
        let Typ::Named(name) = typ else {
            panic!("tuple")
        };
        Decl::Struct {
            name: name.clone(),
            fields: fields(name).unwrap(),
            type_params: vec![],
        }
    }
    fn erase_integers(decls: &mut Vec<Decl>) {
        project_declarations(decls, |decls| {
            crate::core_bindings::rewrite_named_types(decls, &|name| {
                super::super::rust_integer_type_name(name).map(|_| Typ::Int)
            });
        });
    }
    #[test]
    fn integer_proof_and_runtime_tuple_schemas_merge_after_projection() {
        let proof = typ(&[
            Typ::Named("#rust-integer-i64".into()),
            Typ::Named("#rust-integer-i64".into()),
        ]);
        let runtime = typ(&[Typ::Int, Typ::Int]);
        assert_ne!(proof, runtime);
        let mut declarations = vec![schema(&proof), schema(&runtime)];
        erase_integers(&mut declarations);
        assert_eq!(declarations, vec![schema(&runtime)]);
    }
    #[test]
    fn tuple_projection_retains_preexisting_duplicate_origins() {
        let proof = typ(&[Typ::Named("#rust-integer-u8".into())]);
        let runtime = typ(&[Typ::Int]);
        let mut declarations = vec![schema(&runtime), schema(&proof), schema(&proof)];
        erase_integers(&mut declarations);
        assert_eq!(declarations, vec![schema(&runtime), schema(&runtime)]);
    }
    #[test]
    fn tuple_projection_preserves_malformed_identity_and_generic_conflicts() {
        let proof = typ(&[Typ::Named("#rust-integer-u8".into())]);
        let runtime = typ(&[Typ::Int]);
        let mut malformed = schema(&proof);
        let Decl::Struct { fields, .. } = &mut malformed else {
            panic!()
        };
        fields[0].1 = Typ::String;
        let mut generic = schema(&proof);
        let Decl::Struct { type_params, .. } = &mut generic else {
            panic!()
        };
        type_params.push("T".into());
        let mut declarations = vec![schema(&proof), malformed, generic];
        erase_integers(&mut declarations);
        assert_eq!(declarations.len(), 3);
        let Decl::Struct { name, fields, .. } = &declarations[1] else {
            panic!()
        };
        assert_eq!(Typ::Named(name.clone()), runtime);
        assert_eq!(fields, &vec![("_0".into(), Typ::String)]);
    }
}
