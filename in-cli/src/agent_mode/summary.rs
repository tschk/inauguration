use crate::core_ir::{Decl, Stmt, Typ, UnifiedModule};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreIrSummary {
    pub identity: CoreIrIdentitySummary,
    pub decl_count: usize,
    pub struct_count: usize,
    pub function_count: usize,
    pub field_count: usize,
    pub param_count: usize,
    pub statement_count: usize,
    pub structs: Vec<StructSummary>,
    pub functions: Vec<FunctionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreIrIdentitySummary {
    pub package: Option<String>,
    pub module: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructSummary {
    pub name: String,
    pub field_count: usize,
    pub fields: Vec<FieldSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FieldSummary {
    pub name: String,
    pub typ: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FunctionSummary {
    pub name: String,
    pub param_count: usize,
    pub return_type: String,
    pub statement_count: usize,
    pub params: Vec<FieldSummary>,
}

pub(super) fn summarize_core_ir(module: &UnifiedModule) -> CoreIrSummary {
    let mut structs = Vec::new();
    let mut functions = Vec::new();
    for decl in &module.decls {
        match decl {
            Decl::Struct { name, fields, .. } => {
                structs.push(StructSummary {
                    name: name.clone(),
                    field_count: fields.len(),
                    fields: fields
                        .iter()
                        .map(|(field_name, typ)| FieldSummary {
                            name: field_name.clone(),
                            typ: typ_label(typ),
                        })
                        .collect(),
                });
            }
            Decl::Function {
                name,
                params,
                ret,
                body,
                ..
            } => {
                functions.push(FunctionSummary {
                    name: name.clone(),
                    param_count: params.len(),
                    return_type: typ_label(ret),
                    statement_count: stmt_count(body),
                    params: params
                        .iter()
                        .map(|(param_name, typ)| FieldSummary {
                            name: param_name.clone(),
                            typ: typ_label(typ),
                        })
                        .collect(),
                });
            }
            Decl::Class {
                name,
                fields,
                methods,
                ..
            } => {
                // Classes desugar into structs plus `Class_method` functions;
                // summarize that shape so the graph shows a class's methods
                // (and its fields as the struct they become).
                structs.push(StructSummary {
                    name: name.clone(),
                    field_count: fields.len(),
                    fields: fields
                        .iter()
                        .map(|(field_name, typ)| FieldSummary {
                            name: field_name.clone(),
                            typ: typ_label(typ),
                        })
                        .collect(),
                });
                for method in methods {
                    if let Decl::Function {
                        name: method_name,
                        params,
                        ret,
                        body,
                        ..
                    } = method
                    {
                        functions.push(FunctionSummary {
                            name: format!("{name}_{method_name}"),
                            param_count: params.len(),
                            return_type: typ_label(ret),
                            statement_count: stmt_count(body),
                            params: params
                                .iter()
                                .map(|(param_name, typ)| FieldSummary {
                                    name: param_name.clone(),
                                    typ: typ_label(typ),
                                })
                                .collect(),
                        });
                    }
                }
            }
            Decl::Interface { .. } | Decl::Component { .. } => {}
            Decl::Global { .. } => {}
        }
    }
    CoreIrSummary {
        identity: CoreIrIdentitySummary {
            package: module.identity.package.clone(),
            module: module.identity.module.clone(),
        },
        decl_count: module.decls.len(),
        struct_count: structs.len(),
        function_count: functions.len(),
        field_count: structs.iter().map(|s| s.field_count).sum(),
        param_count: functions.iter().map(|f| f.param_count).sum(),
        statement_count: functions.iter().map(|f| f.statement_count).sum(),
        structs,
        functions,
    }
}

fn typ_label(typ: &Typ) -> String {
    match typ {
        Typ::Int => "Int".to_string(),
        Typ::Float => "Float".to_string(),
        Typ::String => "String".to_string(),
        Typ::Bool => "Bool".to_string(),
        Typ::Void => "Void".to_string(),
        Typ::Array(item) => format!("[{}]", typ_label(item)),
        Typ::Vector(item) => format!("Vec<{}>", typ_label(item)),
        Typ::Named(name) => name.clone(),
        Typ::Generic(name) => name.clone(),
    }
}

fn stmt_count(stmts: &[Stmt]) -> usize {
    stmts
        .iter()
        .map(|stmt| match stmt {
            Stmt::If {
                then_body,
                else_body,
                ..
            } => 1 + stmt_count(then_body) + stmt_count(else_body),
            Stmt::Loop { body, .. } => 1 + stmt_count(body),
            Stmt::Match { arms, .. } => {
                1 + arms.iter().map(|arm| stmt_count(&arm.body)).sum::<usize>()
            }
            _ => 1,
        })
        .sum()
}
