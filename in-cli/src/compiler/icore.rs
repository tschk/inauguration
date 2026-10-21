//! **icore** — JSON interchange for [`crate::core_ir::UnifiedModule`].
//!
//! File extension **`.icore`** (or `#!in parser=icore`) loads a versioned JSON document so any
//! front (codegen, tree-sitter bridge, another compiler) can feed the same Core IR native pipeline
//! without re-implementing the `.in` lexer.

use crate::boundary_ir::{BoundaryModule, CompileArtifact};
use crate::boundary_verify::boundary_ir_verify;
use crate::core_ir::{Decl, Typ, UnifiedModule};
use crate::core_ir::{Expr, Stmt};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct IcoreFile {
    #[serde(default, rename = "icoreVersion")]
    icore_version: u32,
    decls: Vec<IcoreDecl>,
    #[serde(default)]
    boundary: Option<BoundaryModule>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum IcoreDecl {
    Struct {
        name: String,
        fields: Vec<IcoreField>,
    },
    Function {
        name: String,
        params: Vec<IcoreParam>,
        #[serde(rename = "return")]
        ret: String,
        #[serde(default)]
        body: Vec<serde_json::Value>,
    },
}

#[derive(Debug, Deserialize)]
struct IcoreField {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Debug, Deserialize)]
struct IcoreParam {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

fn parse_typ(s: &str) -> Typ {
    let s = s.trim();
    match s {
        "Int" => Typ::Int,
        "Float" => Typ::Float,
        "String" => Typ::String,
        "Bool" => Typ::Bool,
        "Void" | "void" | "VOID" => Typ::Void,
        other => Typ::Named(other.to_string()),
    }
}

/// Parse `.icore` JSON into [`UnifiedModule`]. Version **1** supports only **empty** function bodies
/// (`body` must be `[]`); statements are accepted from `.in` or future versions.
pub fn parse_icore_file(path: &Path) -> Result<UnifiedModule, String> {
    parse_icore_artifact(path).map(|artifact| artifact.semantic)
}

pub fn parse_icore_artifact(path: &Path) -> Result<CompileArtifact, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_icore_artifact_source(&raw)
}

pub fn parse_icore_artifact_source(raw: &str) -> Result<CompileArtifact, String> {
    let file: IcoreFile = serde_json::from_str(raw).map_err(|e| format!("icore JSON: {e}"))?;
    let boundary = file.boundary.map(|mut b| {
        if b.layout_hash.is_empty() {
            b = b.with_layout_hash();
        }
        b
    });
    if let Some(ref module) = boundary {
        let report = boundary_ir_verify(module);
        if !report.ok {
            return Err(format!("icore boundary: {}", report.diagnostics.join("; ")));
        }
    }
    let boundary_layout_names = boundary
        .as_ref()
        .map(|module| {
            module
                .layouts
                .iter()
                .map(|layout| layout.name.as_str())
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    let semantic = parse_icore_decls(
        file.icore_version,
        file.decls,
        boundary.is_some(),
        &boundary_layout_names,
    )?;
    Ok(if let Some(boundary) = boundary {
        CompileArtifact::with_boundary(semantic, boundary)
    } else {
        CompileArtifact::from_semantic(semantic)
    })
}

/// Parse JSON (for tests and tooling that already hold the string).
pub fn parse_icore_source(raw: &str) -> Result<UnifiedModule, String> {
    parse_icore_artifact_source(raw).map(|artifact| artifact.semantic)
}

fn parse_icore_decls(
    icore_version: u32,
    icore_decls: Vec<IcoreDecl>,
    has_boundary: bool,
    boundary_layout_names: &HashSet<&str>,
) -> Result<UnifiedModule, String> {
    if !matches!(icore_version, 1..=3) {
        return Err(format!(
            "icore: unsupported icoreVersion {icore_version} (only 1, 2, and 3 supported)"
        ));
    }

    let mut decls = Vec::new();
    for d in icore_decls {
        match d {
            IcoreDecl::Struct { name, fields } => {
                let flds: Vec<(String, Typ)> = fields
                    .into_iter()
                    .map(|f| (f.name, parse_typ(&f.ty)))
                    .collect();
                decls.push(Decl::Struct {
                    name,
                    fields: flds,
                    type_params: vec![],
                });
            }
            IcoreDecl::Function {
                name,
                params,
                ret,
                body,
            } => {
                let body = if icore_version == 1 {
                    if !body.is_empty() {
                        return Err(format!(
                            "icore: function `{name}` has non-empty body (v1 only supports body: [])"
                        ));
                    }
                    Vec::new()
                } else {
                    parse_v2_body(&name, body)?
                };
                let params: Vec<(String, Typ)> = params
                    .into_iter()
                    .map(|p| (p.name, parse_typ(&p.ty)))
                    .collect();
                decls.push(Decl::Function {
                    name,
                    params,
                    ret: parse_typ(&ret),
                    body,
                    type_params: vec![],
                });
            }
        }
    }

    let module = UnifiedModule::new(decls);
    validate_module(&module, icore_version, has_boundary, boundary_layout_names)?;
    Ok(module)
}

fn parse_v2_body(function_name: &str, body: Vec<Value>) -> Result<Vec<Stmt>, String> {
    body.into_iter()
        .enumerate()
        .map(|(index, stmt)| {
            let context = format!("icore v2 function `{function_name}` statement {index}");
            parse_v2_stmt(&stmt, &context)
        })
        .collect()
}

fn parse_v2_stmt(value: &Value, context: &str) -> Result<Stmt, String> {
    let object = value_object(value, context)?;
    let kind = string_field(object, "kind", context)?;
    match kind {
        "return" => parse_optional_expr_field(object, context).map(Stmt::Return),
        "assign" => {
            let target = one_of_string_fields(object, "target", "name", context)?;
            let value = required_field(object, "value", context)?;
            Ok(Stmt::Assign(
                target.to_string(),
                parse_v2_expr(value, context)?,
            ))
        }
        "let" => {
            let name = string_field(object, "name", context)?;
            let typ = object.get("type").and_then(Value::as_str).map(parse_typ);
            let value = required_field(object, "value", context)?;
            Ok(Stmt::Let(
                name.to_string(),
                typ,
                parse_v2_expr(value, context)?,
            ))
        }
        "expr" | "expression" => {
            let value = expr_or_value_field(object, context)?;
            Ok(Stmt::Expr(parse_v2_expr(value, context)?))
        }
        "call" => Ok(Stmt::Expr(parse_v2_expr(value, context)?)),
        "if" => parse_if_stmt(object, context),
        "match" => {
            let scrutinee = parse_v2_expr(required_field(object, "scrutinee", context)?, context)?;
            let arms = required_field(object, "arms", context)?
                .as_array()
                .ok_or_else(|| format!("{context}: `match` needs an `arms` array"))?;
            let arms = arms
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let ctx = format!("{context}.arms[{index}]");
                    let arm = value_object(value, &ctx)?;
                    let pattern = string_field(arm, "pattern", &ctx)?.to_string();
                    crate::core_ir::MatchPattern::parse(&pattern)
                        .map_err(|error| format!("{ctx}: {error}"))?;
                    let guard = arm
                        .get("guard")
                        .map(|value| parse_v2_expr(value, &format!("{ctx}.guard")))
                        .transpose()?;
                    let body = required_field(arm, "body", &ctx)?
                        .as_array()
                        .ok_or_else(|| format!("{ctx}: `body` must be an array"))?;
                    let body = body
                        .iter()
                        .enumerate()
                        .map(|(index, stmt)| parse_v2_stmt(stmt, &format!("{ctx}.body[{index}]")))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(crate::core_ir::MatchArm {
                        pattern,
                        guard,
                        body,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(Stmt::Match { scrutinee, arms })
        }
        other => Err(format!("{context}: unsupported statement kind `{other}`")),
    }
}

fn parse_if_stmt(object: &Map<String, Value>, context: &str) -> Result<Stmt, String> {
    let cond = required_field(object, "cond", context)?;
    let then_body = object
        .get("then_body")
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("{context}: `if` needs `then_body` array"))?;
    let else_body = object
        .get("else_body")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let then_stmts: Vec<Stmt> = then_body
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let ctx = format!("{context}.then_body[{idx}]");
            parse_v2_stmt(s, &ctx)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let else_stmts: Vec<Stmt> = else_body
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let ctx = format!("{context}.else_body[{idx}]");
            parse_v2_stmt(s, &ctx)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Stmt::If {
        cond: parse_v2_expr(cond, context)?,
        then_body: then_stmts,
        else_body: else_stmts,
    })
}

fn parse_v2_expr(value: &Value, context: &str) -> Result<Expr, String> {
    if let Some(value) = value.as_i64() {
        return Ok(Expr::IntLit(value));
    }
    if let Some(value) = value.as_bool() {
        return Ok(Expr::BoolLit(value));
    }
    if let Some(value) = value.as_str() {
        return Ok(Expr::StringLit(value.to_string()));
    }
    let object = value_object(value, context)?;
    let kind = string_field(object, "kind", context)?;
    match kind {
        "unit" => Ok(Expr::UnitLit),
        "int" => required_field(object, "value", context)?
            .as_i64()
            .map(Expr::IntLit)
            .ok_or_else(|| format!("{context}: int literal `value` must be an integer")),
        "float" => required_field(object, "value", context)?
            .as_f64()
            .filter(|value| value.is_finite())
            .map(|value| Expr::FloatLit(crate::core_ir::FloatVal(value)))
            .ok_or_else(|| format!("{context}: float literal `value` must be a finite number")),
        "string" => required_field(object, "value", context)?
            .as_str()
            .map(|value| Expr::StringLit(value.to_string()))
            .ok_or_else(|| format!("{context}: string literal `value` must be a string")),
        "bool" => required_field(object, "value", context)?
            .as_bool()
            .map(Expr::BoolLit)
            .ok_or_else(|| format!("{context}: bool literal `value` must be a boolean")),
        "ident" | "identifier" => {
            let name = string_field(object, "name", context)?;
            Ok(Expr::Ident(name.to_string()))
        }
        "binary" | "binop" => {
            let op = string_field(object, "op", context)?;
            if !is_supported_binary_op(op) {
                return Err(format!("{context}: unsupported binary operator `{op}`"));
            }
            let lhs = parse_v2_expr(required_field(object, "lhs", context)?, context)?;
            let rhs = parse_v2_expr(required_field(object, "rhs", context)?, context)?;
            Ok(Expr::Binary {
                op: op.to_string(),
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            })
        }
        "match-value" => {
            let scrutinee = Box::new(parse_v2_expr(
                required_field(object, "scrutinee", context)?,
                context,
            )?);
            let arms = required_field(object, "arms", context)?
                .as_array()
                .ok_or_else(|| format!("{context}: match `arms` must be an array"))?
                .iter()
                .map(|arm| {
                    let arm = value_object(arm, context)?;
                    let pattern = string_field(arm, "pattern", context)?.to_string();
                    crate::core_ir::MatchPattern::parse(&pattern)?;
                    let guard = arm
                        .get("guard")
                        .filter(|value| !value.is_null())
                        .map(|value| parse_v2_expr(value, context))
                        .transpose()?;
                    let value = parse_v2_expr(required_field(arm, "value", context)?, context)?;
                    Ok(crate::core_ir::MatchValueArm {
                        pattern,
                        guard,
                        value,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(Expr::MatchValue { scrutinee, arms })
        }
        "conditional" => Ok(Expr::Conditional {
            cond: Box::new(parse_v2_expr(
                required_field(object, "cond", context)?,
                context,
            )?),
            then_expr: Box::new(parse_v2_expr(
                required_field(object, "then", context)?,
                context,
            )?),
            else_expr: Box::new(parse_v2_expr(
                required_field(object, "else", context)?,
                context,
            )?),
        }),
        "block" => {
            let body = required_field(object, "body", context)?
                .as_array()
                .ok_or_else(|| format!("{context}: block `body` must be an array"))?
                .iter()
                .map(|stmt| parse_v2_stmt(stmt, context))
                .collect::<Result<Vec<_>, _>>()?;
            let result = object
                .get("result")
                .filter(|value| !value.is_null())
                .map(|value| parse_v2_expr(value, context).map(Box::new))
                .transpose()?;
            Ok(Expr::Block { body, result })
        }
        "call" => {
            let callee = parse_callee(required_field(object, "callee", context)?, context)?;
            let args = match object.get("args") {
                Some(value) => value
                    .as_array()
                    .ok_or_else(|| format!("{context}: call `args` must be an array"))?
                    .iter()
                    .map(|arg| parse_v2_expr(arg, context))
                    .collect::<Result<Vec<_>, _>>()?,
                None => Vec::new(),
            };
            Ok(Expr::Call {
                callee: Box::new(callee),
                args,
            })
        }
        other => Err(format!("{context}: unsupported expression kind `{other}`")),
    }
}

fn parse_callee(value: &Value, context: &str) -> Result<Expr, String> {
    if let Some(name) = value.as_str() {
        return Ok(Expr::Ident(name.to_string()));
    }
    parse_v2_expr(value, context)
}

fn is_supported_binary_op(op: &str) -> bool {
    matches!(
        op,
        "+" | "-"
            | "*"
            | "/"
            | "%"
            | "udiv"
            | "urem"
            | "lshr"
            | "<<"
            | ">>"
            | "^"
            | "xor"
            | "&"
            | "|"
            | "=="
            | "!="
            | "<"
            | ">"
            | "<="
            | ">="
            | "&&"
            | "||"
    )
}

fn value_object<'a>(value: &'a Value, context: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{context}: expected object"))
}

fn required_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a Value, String> {
    object
        .get(field)
        .ok_or_else(|| format!("{context}: missing `{field}`"))
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a str, String> {
    required_field(object, field, context)?
        .as_str()
        .ok_or_else(|| format!("{context}: `{field}` must be a string"))
}

fn one_of_string_fields<'a>(
    object: &'a Map<String, Value>,
    left: &str,
    right: &str,
    context: &str,
) -> Result<&'a str, String> {
    match (object.get(left), object.get(right)) {
        (Some(_), Some(_)) => Err(format!("{context}: use `{left}` or `{right}`, not both")),
        (Some(value), None) | (None, Some(value)) => value
            .as_str()
            .ok_or_else(|| format!("{context}: `{left}`/`{right}` must be a string")),
        (None, None) => Err(format!("{context}: missing `{left}` or `{right}`")),
    }
}

fn expr_or_value_field<'a>(
    object: &'a Map<String, Value>,
    context: &str,
) -> Result<&'a Value, String> {
    match (object.get("expr"), object.get("value")) {
        (Some(_), Some(_)) => Err(format!("{context}: use `expr` or `value`, not both")),
        (Some(value), None) | (None, Some(value)) => Ok(value),
        (None, None) => Err(format!("{context}: missing `expr` or `value`")),
    }
}

fn parse_optional_expr_field(
    object: &Map<String, Value>,
    context: &str,
) -> Result<Option<Expr>, String> {
    match (object.get("expr"), object.get("value")) {
        (Some(_), Some(_)) => Err(format!("{context}: use `expr` or `value`, not both")),
        (Some(value), None) | (None, Some(value)) => Ok(Some(parse_v2_expr(value, context)?)),
        (None, None) => Ok(None),
    }
}

fn type_known_with_boundary(
    struct_names: &HashSet<&str>,
    boundary_layout_names: &HashSet<&str>,
    t: &Typ,
) -> bool {
    match t {
        Typ::Named(name) => {
            struct_names.contains(name.as_str()) || boundary_layout_names.contains(name.as_str())
        }
        Typ::Array(item) => type_known_with_boundary(struct_names, boundary_layout_names, item),
        Typ::Vector(item) => type_known_with_boundary(struct_names, boundary_layout_names, item),
        Typ::Int | Typ::Float | Typ::String | Typ::Bool | Typ::Void => true,
        Typ::Generic(_) => false,
    }
}

fn validate_module(
    module: &UnifiedModule,
    icore_version: u32,
    has_boundary: bool,
    boundary_layout_names: &HashSet<&str>,
) -> Result<(), String> {
    if module.decls.is_empty() {
        return Err("icore: decls is empty".into());
    }
    let mut names = Vec::new();
    for d in &module.decls {
        match d {
            Decl::Struct { name, .. } | Decl::Function { name, .. } => names.push(name.clone()),
            Decl::Class { .. } | Decl::Interface { .. } | Decl::Component { .. } => {}
            Decl::Global { .. } => {}
        }
    }
    let mut seen = HashSet::new();
    for n in &names {
        if !seen.insert(n.clone()) {
            return Err(format!("icore: duplicate top-level name `{n}`"));
        }
    }
    let requires_main = !(icore_version == 3 && has_boundary);
    let has_main = module
        .decls
        .iter()
        .any(|d| matches!(d, Decl::Function { name, .. } if name == "main"));
    if requires_main && !has_main {
        return Err("icore: missing required function `main`".into());
    }

    let struct_names: HashSet<&str> = module
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Struct { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();

    for d in &module.decls {
        match d {
            Decl::Struct { name, fields, .. } => {
                for (field, ty) in fields {
                    if !type_known_with_boundary(&struct_names, boundary_layout_names, ty) {
                        return Err(format!(
                            "icore: unknown type in struct `{name}` field `{field}`"
                        ));
                    }
                }
            }
            Decl::Function {
                name, params, ret, ..
            } => {
                for (param, ty) in params {
                    if !type_known_with_boundary(&struct_names, boundary_layout_names, ty) {
                        return Err(format!(
                            "icore: unknown type in function `{name}` parameter `{param}`"
                        ));
                    }
                }
                if !type_known_with_boundary(&struct_names, boundary_layout_names, ret) {
                    return Err(format!("icore: unknown return type in function `{name}`"));
                }
            }
            Decl::Class { .. } | Decl::Interface { .. } | Decl::Component { .. } => {}
            Decl::Global { .. } => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn explicit_float_literals_preserve_binary64_values_and_signed_zero() {
        for (value, expected) in [
            (serde_json::json!(1.5), 1.5f64),
            (serde_json::json!(42), 42.0),
            (serde_json::json!(9007199254740993u64), 9007199254740992.0),
            (serde_json::json!(-0.0), -0.0),
        ] {
            let expr =
                super::parse_v2_expr(&serde_json::json!({"kind":"float","value":value}), "test")
                    .unwrap();
            let crate::core_ir::Expr::FloatLit(value) = expr else {
                panic!("expected Float")
            };
            assert_eq!(value.0.to_bits(), expected.to_bits());
        }
        for value in [
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!("1.5"),
        ] {
            assert!(
                super::parse_v2_expr(&serde_json::json!({"kind":"float","value":value}), "test")
                    .is_err()
            );
        }
        assert!(super::parse_v2_expr(&serde_json::json!({"kind":"float"}), "test").is_err());
    }

    #[test]
    fn explicit_float_type_is_primitive_while_shorthand_numbers_stay_integer_only() {
        let source = r#"{"icoreVersion":2,"decls":[{"kind":"function","name":"identity","params":[{"name":"value","type":"Float"}],"return":"Float","body":[{"kind":"return","value":{"kind":"ident","name":"value"}}]},{"kind":"function","name":"main","params":[],"return":"Float","body":[{"kind":"return","value":{"kind":"float","value":1.5}}]}]}"#;
        let module = super::parse_icore_source(source).unwrap();
        assert!(
            matches!(&module.decls[0], crate::core_ir::Decl::Function { params, ret, .. }
            if params[0].1 == crate::core_ir::Typ::Float && ret == &crate::core_ir::Typ::Float)
        );
        for raw in [
            "1.5",
            "1e2",
            "9223372036854775808",
            "18446744073709551616",
            "-9223372036854775809",
        ] {
            let value: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert!(super::parse_v2_expr(&value, "test").is_err(), "{raw}");
        }
        assert!(
            super::parse_v2_expr(
                &serde_json::json!({"kind":"int","value":9223372036854775808u64}),
                "test"
            )
            .is_err()
        );
        assert!(super::parse_icore_source(&source.replace("1.5", "1e400")).is_err());
    }

    #[test]
    fn parses_explicit_unit_without_accepting_legacy_null() {
        assert_eq!(
            super::parse_v2_expr(&serde_json::json!({"kind":"unit"}), "test").unwrap(),
            crate::core_ir::Expr::UnitLit
        );
        assert!(super::parse_v2_expr(&serde_json::Value::Null, "test").is_err());
    }

    #[test]
    fn parses_typed_match_values_and_rejects_malformed_arms() {
        let value = serde_json::json!({"kind":"match-value", "scrutinee":true, "arms":[
            {"pattern":"true", "value":42}, {"pattern":"false", "guard":true, "value":0}
        ]});
        let parsed = super::parse_v2_expr(&value, "test").unwrap();
        assert!(
            matches!(parsed, crate::core_ir::Expr::MatchValue { arms, .. } if arms.len()==2 && arms[1].guard==Some(crate::core_ir::Expr::BoolLit(true)))
        );
        assert!(
            super::parse_v2_expr(
                &serde_json::json!({"kind":"match-value","scrutinee":0,"arms":[{"pattern":"_"}]}),
                "test"
            )
            .is_err()
        );
    }

    #[test]
    fn parses_conditional_block_values_without_closures() {
        let value = serde_json::json!({
            "kind": "conditional", "cond": true,
            "then": {"kind": "block", "body": [{"kind":"return", "expr": 42}], "result": null},
            "else": {"kind": "block", "body": [], "result": 0}
        });
        let parsed = super::parse_v2_expr(&value, "test").unwrap();
        let crate::core_ir::Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } = parsed
        else {
            panic!("expected conditional");
        };
        let crate::core_ir::Expr::Block { body, result } = *then_expr else {
            panic!("expected block");
        };
        assert!(result.is_none());
        assert!(matches!(
            body.as_slice(),
            [crate::core_ir::Stmt::Return(Some(
                crate::core_ir::Expr::IntLit(42)
            ))]
        ));
        let crate::core_ir::Expr::Block {
            result: Some(result),
            ..
        } = *else_expr
        else {
            panic!("expected value block");
        };
        assert!(matches!(*result, crate::core_ir::Expr::IntLit(0)));
        assert!(
            super::parse_v2_expr(&serde_json::json!({"kind":"block", "body": 1}), "test").is_err()
        );
    }
    use super::*;

    #[test]
    fn parses_minimal_icore() {
        let j = r#"{
            "icoreVersion": 1,
            "decls": [
                { "kind": "struct", "name": "S", "fields": [{ "name": "x", "type": "Int" }] },
                { "kind": "function", "name": "main", "params": [], "return": "Void", "body": [] }
            ]
        }"#;
        let m = parse_icore_source(j).expect("ok");
        assert_eq!(m.decls.len(), 2);
    }

    #[test]
    fn rejects_nonempty_body_v1() {
        let j = r#"{
            "icoreVersion": 1,
            "decls": [
                { "kind": "function", "name": "main", "params": [], "return": "Void", "body": [1] }
            ]
        }"#;
        let err = parse_icore_source(j).expect_err("v1 body must be rejected");
        assert!(err.contains("v1 only supports body: []"), "{err}");
    }

    #[test]
    fn parses_v2_function_bodies() {
        let j = r#"{
            "icoreVersion": 2,
            "decls": [
                {
                    "kind": "function",
                    "name": "helper",
                    "params": [],
                    "return": "Int",
                    "body": [
                        { "kind": "return", "expr": { "kind": "int", "value": 7 } }
                    ]
                },
                {
                    "kind": "function",
                    "name": "main",
                    "params": [],
                    "return": "Void",
                    "body": [
                        { "kind": "let", "name": "seed", "type": "Int", "value": 0 },
                        {
                            "kind": "assign",
                            "target": "value",
                            "value": {
                                "kind": "call",
                                "callee": "helper",
                                "args": []
                            }
                        },
                        {
                            "kind": "call",
                            "callee": { "kind": "ident", "name": "helper" },
                            "args": [
                                1,
                                "ok",
                                true
                            ]
                        },
                        { "kind": "return" }
                    ]
                }
            ]
        }"#;
        let m = parse_icore_source(j).expect("ok");
        let helper_body = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "helper" => Some(body),
                _ => None,
            })
            .expect("helper body");
        assert_eq!(helper_body, &vec![Stmt::Return(Some(Expr::IntLit(7)))]);

        let main_body = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main body");
        assert_eq!(
            main_body,
            &vec![
                Stmt::Let("seed".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Assign(
                    "value".into(),
                    Expr::Call {
                        callee: Box::new(Expr::Ident("helper".into())),
                        args: vec![],
                    },
                ),
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("helper".into())),
                    args: vec![
                        Expr::IntLit(1),
                        Expr::StringLit("ok".into()),
                        Expr::BoolLit(true),
                    ],
                }),
                Stmt::Return(None),
            ]
        );
    }

    #[test]
    fn parses_v2_modulo_binary_expression() {
        let j = r#"{
            "icoreVersion": 2,
            "decls": [
                {
                    "kind": "function",
                    "name": "main",
                    "params": [],
                    "return": "Int",
                    "body": [
                        {
                            "kind": "return",
                            "expr": {
                                "kind": "binary",
                                "op": "%",
                                "lhs": { "kind": "int", "value": 7 },
                                "rhs": { "kind": "int", "value": 4 }
                            }
                        }
                    ]
                }
            ]
        }"#;
        let m = parse_icore_source(j).expect("ok");
        let body = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "main" => Some(body),
                _ => None,
            })
            .expect("main body");
        assert_eq!(
            body,
            &vec![Stmt::Return(Some(Expr::Binary {
                op: "%".into(),
                lhs: Box::new(Expr::IntLit(7)),
                rhs: Box::new(Expr::IntLit(4)),
            }))]
        );

        let sil = crate::lower_core::lower_to_textual_sil(m.clone(), "App");

        assert!(sil.contains("integer_literal $Builtin.Int64, 3"));
        assert!(!sil.contains("builtin_binop"));
    }

    #[test]
    fn parses_generic_unsigned_integer_binary_operations() {
        for op in ["udiv", "urem", "lshr"] {
            let value = serde_json::json!({"icoreVersion":2,"decls":[{
                "kind":"function","name":"main","params":[],"return":"Int",
                "body":[{"kind":"return","value":{"kind":"binary","op":op,"lhs":-1,"rhs":1}}]
            }]});
            let module = parse_icore_source(&value.to_string()).expect("unsigned operator");
            assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        }
    }

    #[test]
    fn rejects_v2_unknown_binary_operator() {
        let j = r#"{
            "icoreVersion": 2,
            "decls": [
                {
                    "kind": "function",
                    "name": "main",
                    "params": [],
                    "return": "Int",
                    "body": [
                        {
                            "kind": "return",
                            "expr": {
                                "kind": "binary",
                                "op": "%%",
                                "lhs": 7,
                                "rhs": 4
                            }
                        }
                    ]
                }
            ]
        }"#;
        let err = parse_icore_source(j).expect_err("unknown binary operator must be rejected");
        assert!(err.contains("unsupported binary operator `%%`"), "{err}");
    }

    #[test]
    fn parses_if_else_statement() {
        let j = r#"{
            "icoreVersion": 2,
            "decls": [
                {
                    "kind": "function",
                    "name": "check",
                    "params": [],
                    "return": "Int",
                    "body": [
                        {
                            "kind": "if",
                            "cond": { "kind": "bool", "value": true },
                            "then_body": [
                                { "kind": "return", "expr": { "kind": "int", "value": 1 } }
                            ],
                            "else_body": [
                                { "kind": "return", "expr": { "kind": "int", "value": 0 } }
                            ]
                        }
                    ]
                },
                {
                    "kind": "function",
                    "name": "main",
                    "params": [],
                    "return": "Void",
                    "body": []
                }
            ]
        }"#;
        let m = parse_icore_source(j).expect("ok");
        let body = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "check" => Some(body),
                _ => None,
            })
            .expect("check body");
        assert_eq!(
            body,
            &vec![Stmt::If {
                cond: Expr::BoolLit(true),
                then_body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
                else_body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
            }]
        );
    }

    #[test]
    fn parses_if_without_else() {
        let j = r#"{
            "icoreVersion": 2,
            "decls": [
                {
                    "kind": "function",
                    "name": "maybe",
                    "params": [],
                    "return": "Int",
                    "body": [
                        {
                            "kind": "if",
                            "cond": { "kind": "bool", "value": false },
                            "then_body": [
                                { "kind": "return", "expr": { "kind": "int", "value": 7 } }
                            ]
                        }
                    ]
                },
                {
                    "kind": "function",
                    "name": "main",
                    "params": [],
                    "return": "Void",
                    "body": []
                }
            ]
        }"#;
        let m = parse_icore_source(j).expect("ok");
        let body = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "maybe" => Some(body),
                _ => None,
            })
            .expect("maybe body");
        assert_eq!(
            body,
            &vec![Stmt::If {
                cond: Expr::BoolLit(false),
                then_body: vec![Stmt::Return(Some(Expr::IntLit(7)))],
                else_body: vec![],
            }]
        );
    }

    #[test]
    fn parses_v3_boundary_without_main() {
        let j = r#"{
            "icoreVersion": 3,
            "decls": [
                {
                    "kind": "function",
                    "name": "person_age",
                    "params": [{"name": "p", "type": "Person"}],
                    "return": "Int",
                    "body": [
                        {"kind": "return", "value": {"kind": "int", "value": 0}}
                    ]
                }
            ],
            "boundary": {
                "abi_version": 1,
                "module": "sample.person",
                "layouts": [
                    {
                        "name": "Person",
                        "kind": "struct",
                        "repr": "c",
                        "size": 24,
                        "align": 8,
                        "stride": 24,
                        "fields": [
                            {"name": "name", "offset": 0, "type": "InSliceU8", "transfer": "borrow"},
                            {"name": "age", "offset": 16, "type": "u32", "transfer": "copy"}
                        ]
                    }
                ],
                "symbols": [
                    {
                        "name": "person_new",
                        "signature_hash": "person_new_v1",
                        "ownership": "returns-owned-handle",
                        "calling_convention": "c"
                    }
                ],
                "allocators": [
                    {"id": 1, "kind": "host_arena", "free_with": "host"}
                ]
            }
        }"#;
        let artifact = parse_icore_artifact_source(j).expect("v3 boundary ingest");
        assert!(artifact.boundary.is_some());
        assert!(
            artifact
                .semantic
                .decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "person_age"))
        );
    }
}
