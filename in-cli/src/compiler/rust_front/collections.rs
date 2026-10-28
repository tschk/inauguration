//! Concrete owned storage for standard ordered maps.
use super::*;

const MAP: &str = "#rust-btree-map:";
const RESULT: &str = "#rust-stored-result:";

fn map_type(key: Typ, value: Typ) -> Typ {
    let Typ::Named(pair) = crate::core_ir::tuples::typ(&[key, value]) else {
        unreachable!("a pair has a tuple identity")
    };
    Typ::Named(format!("{MAP}{pair}"))
}

fn pair(name: &str) -> Option<Typ> {
    let pair = name.strip_prefix(MAP)?;
    (crate::core_ir::tuples::fields(pair)?.len() == 2).then(|| Typ::Named(pair.into()))
}

fn result_fields(name: &str) -> Option<Vec<(String, Typ)>> {
    let fields = crate::core_ir::tuples::fields(name.strip_prefix(RESULT)?)?;
    (fields.len() == 3 && fields[0].1 == Typ::Bool).then_some(fields)
}

/// Project exact standard source identities into owned storage, not rustc's
/// collection/enum layouts. In particular a stored Result retains both variants.
pub(super) fn storage(source: &Typ) -> Option<Typ> {
    match source {
        Typ::Named(name) if name.starts_with("#rust-source-signature:") => match name.as_str() {
            "#rust-source-signature:std:string:String" => Some(Typ::String),
            _ => crate::core_ir::source_types::nominal_identity(name)
                .map(|identity| Typ::Named(identity.into())),
        },
        Typ::Named(name) => {
            if let Some(fields) = crate::core_ir::tuples::fields(name) {
                let types = fields.iter().map(|(_, typ)| typ).collect::<Vec<_>>();
                if let Some(Typ::Named(constructor)) = types.first() {
                    match (constructor.as_str(), types.as_slice()) {
                        ("#rust-source-signature:std:option:Option", [_, value]) => {
                            return Some(crate::core_ir::rust_options::typ(storage(value)?));
                        }
                        ("#rust-source-signature:std:result:Result", [_, value, error]) => {
                            let Typ::Named(shape) = crate::core_ir::tuples::typ(&[
                                Typ::Bool,
                                storage(value)?,
                                storage(error)?,
                            ]) else {
                                unreachable!("a Result has a tagged storage shape")
                            };
                            return Some(Typ::Named(format!("{RESULT}{shape}")));
                        }
                        ("#rust-source-signature:std:collections:BTreeMap", [_, key, value]) => {
                            return Some(map_type(storage(key)?, storage(value)?));
                        }
                        ("#rust-source-signature:tuple", [_, items @ ..]) => {
                            return Some(crate::core_ir::tuples::typ(
                                &items
                                    .iter()
                                    .copied()
                                    .map(storage)
                                    .collect::<Option<Vec<_>>>()?,
                            ));
                        }
                        _ if constructor.starts_with("#rust-source-signature:") => return None,
                        _ => {}
                    }
                }
                return Some(crate::core_ir::tuples::typ(
                    &types.into_iter().map(storage).collect::<Option<Vec<_>>>()?,
                ));
            }
            crate::core_ir::rust_integer_type_name(name).map(|_| source.clone())
        }
        Typ::Int | Typ::Bool | Typ::Float | Typ::String | Typ::Void => Some(source.clone()),
        _ => None,
    }
}

pub(super) fn member(owner: &Typ, member: &str) -> Option<String> {
    let Typ::Named(name) = owner else {
        return None;
    };
    pair(name)?;
    matches!(member, "new" | "len" | "is_empty").then(|| format!("{name}::{member}"))
}

pub(super) fn retain_helpers(module: &mut UnifiedModule) {
    let maps = std::cell::RefCell::new(std::collections::BTreeSet::new());
    crate::core_bindings::rewrite_named_types(&mut module.decls, &|name| {
        if pair(name).is_some() || result_fields(name).is_some() {
            maps.borrow_mut().insert(name.to_owned());
        }
        None
    });
    crate::core_opt::map_decl_exprs_mut(&mut module.decls, &mut |expr| {
        if let Expr::Ident(target) = expr
            && let Some((owner, _)) = target.rsplit_once("::")
            && pair(owner).is_some()
        {
            maps.borrow_mut().insert(owner.to_owned());
        }
    });
    let mut completed = HashSet::new();
    loop {
        let Some(name) = maps.borrow_mut().pop_first() else {
            break;
        };
        if !completed.insert(name.clone()) {
            continue;
        }
        let first = module.decls.len();
        if let Some(fields) = result_fields(&name) {
            module.decls.push(Decl::Struct {
                name,
                fields,
                type_params: vec![],
            });
        } else {
            let entries = Typ::Vector(Box::new(pair(&name).expect("collected a concrete map")));
            let typ = Typ::Named(name.clone());
            let field = || Expr::Field {
                base: Box::new(Expr::Ident("self".into())),
                name: "entries".into(),
            };
            let length = || Expr::Call {
                callee: Box::new(Expr::Ident("array-len".into())),
                args: vec![field()],
            };
            module.decls.push(Decl::Struct {
                name: name.clone(),
                fields: vec![("entries".into(), entries.clone())],
                type_params: vec![],
            });
            module.decls.push(Decl::Function {
                name: format!("{name}::new"),
                params: vec![],
                ret: typ.clone(),
                body: vec![Stmt::Return(Some(Expr::StructInit {
                    name: name.clone(),
                    fields: vec![(
                        "entries".into(),
                        Expr::Block {
                            body: vec![Stmt::Let(
                                "#map-entries".into(),
                                Some(entries),
                                Expr::ArrayLit(vec![]),
                            )],
                            result: Some(Box::new(Expr::Ident("#map-entries".into()))),
                        },
                    )],
                }))],
                type_params: vec![],
            });
            for (method, ret, result) in [
                ("len", Typ::Named("#rust-integer-usize".into()), length()),
                (
                    "is_empty",
                    Typ::Bool,
                    Expr::Binary {
                        op: "==".into(),
                        lhs: Box::new(length()),
                        rhs: Box::new(Expr::IntLit(0)),
                    },
                ),
            ] {
                module.decls.push(Decl::Function {
                    name: format!("{name}::{method}"),
                    params: vec![("self".into(), typ.clone())],
                    ret,
                    body: vec![Stmt::Return(Some(result))],
                    type_params: vec![],
                });
            }
        }
        crate::core_bindings::rewrite_named_types(&mut module.decls[first..], &|name| {
            if pair(name).is_some() || result_fields(name).is_some() {
                maps.borrow_mut().insert(name.to_owned());
            }
            None
        });
    }
}
