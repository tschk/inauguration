//! Typed tuples use ordinary Core IR structs after expression type resolution.
use super::{Decl, Expr, Stmt, Typ, UnifiedModule};
use std::collections::BTreeMap;

pub(crate) const VALUE: &str = "#tuple-value";
const PREFIX: &str = "in-tuple-";

#[derive(serde::Serialize, serde::Deserialize)]
enum Shape {
    Core(Typ),
    Tuple(Vec<Shape>),
    Array(Box<Shape>),
    Vector(Box<Shape>),
}

impl Shape {
    fn from_type(typ: &Typ) -> Self {
        match typ.canonical() {
            Typ::Named(name) => match decode(&name) {
                Some(items) => Self::Tuple(items),
                None => Self::Core(Typ::Named(name)),
            },
            Typ::Array(item) => Self::Array(Box::new(Self::from_type(&item))),
            Typ::Vector(item) => Self::Vector(Box::new(Self::from_type(&item))),
            typ => Self::Core(typ),
        }
    }
    fn into_type(self) -> Typ {
        match self {
            Self::Core(typ) => typ,
            Self::Tuple(items) => Typ::Named(encode(&items)),
            Self::Array(item) => Typ::Array(Box::new(item.into_type())),
            Self::Vector(item) => Typ::Vector(Box::new(item.into_type())),
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

pub(crate) fn register(module: &mut UnifiedModule) {
    fn add(typ: &Typ, shapes: &mut BTreeMap<String, Vec<(String, Typ)>>) {
        match typ {
            Typ::Named(name) if !shapes.contains_key(name) => {
                if let Some(fields) = fields(name) {
                    shapes.insert(name.clone(), fields.clone());
                    for (_, typ) in fields {
                        add(&typ, shapes);
                    }
                }
            }
            Typ::Array(item) | Typ::Vector(item) => add(item, shapes),
            _ => {}
        }
    }
    fn expr(value: &Expr, shapes: &mut BTreeMap<String, Vec<(String, Typ)>>) {
        match value {
            Expr::StructInit { name, .. } => add(&Typ::Named(name.clone()), shapes),
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
    fn registration_is_idempotent_and_conflicting_names_fail_verification() {
        let typ = typ(&[Typ::Int, Typ::Bool]);
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
