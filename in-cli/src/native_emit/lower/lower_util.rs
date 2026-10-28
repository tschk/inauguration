use super::FunctionInfo;
use crate::core_ir::{Expr, Typ};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::HashMap;

/// Strip generic arguments while retaining the declaration's module path.
pub(crate) fn base_struct_name(name: &str) -> &str {
    name.split('<')
        .next()
        .unwrap_or(name)
        .trim()
        .trim_end_matches("::")
        .trim()
}

pub(crate) fn canonical_type(typ: &Typ) -> Typ {
    if crate::core_ir::rust_references::aggregate_referent(typ).is_some() {
        typ.clone()
    } else {
        typ.canonical()
    }
}

pub(crate) fn pick_scratch(exclude: &[u8]) -> u8 {
    (2..=15).find(|reg| !exclude.contains(reg)).unwrap_or(15)
}

pub(crate) fn emit_failure_return(
    emitter: &mut CodeEmitter,
    ctx: &mut super::LowerCtx<'_>,
    fn_name: &str,
) -> Result<(), String> {
    emitter.emit_insns(&aarch64::load_i64(0, 1));
    if matches!(
        ctx.ret_typ.canonical(),
        Typ::String | Typ::Array(_) | Typ::Vector(_) | Typ::Named(_)
    ) {
        super::lower_stmt::emit_error_slot_adr(emitter, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, 27, 8));
        emitter.emit_u32(aarch64::str64(0, 27, 16));
        emitter.emit_u32(aarch64::strb(0, 27, 0));
        super::lower_stmt::emit_error_exit(emitter, ctx);
    } else {
        emit_epilogue(emitter, ctx.prologue_stack_reserve);
    }
    Ok(())
}

pub(crate) fn field_path(expr: &Expr) -> Option<(&str, String)> {
    let mut names = Vec::new();
    let mut base = expr;
    loop {
        match base {
            Expr::Field { base: parent, name } => {
                names.push(name.as_str());
                base = parent;
            }
            Expr::Unary { op, expr }
                if crate::core_ir::rust_references::value_kind(op).is_some() =>
            {
                base = expr;
            }
            _ => break,
        }
    }
    let Expr::Ident(root) = base else {
        return None;
    };
    names.reverse();
    Some((root, names.join(".")))
}

/// Find a field offset in a flattened struct field map, supporting nested struct access.
/// When `field_map` has `"inner.val"` and we look up `"inner"`, returns the offset of `"inner.val"`.
pub(crate) fn find_field_offset<'a>(
    field_map: &'a HashMap<String, u32>,
    name: &str,
) -> Option<&'a u32> {
    if let Some(offset) = field_map.get(name) {
        return Some(offset);
    }
    // Try prefix match for nested structs: "inner" → "inner.val"
    field_map
        .iter()
        .filter_map(|(k, v)| {
            if k.len() > name.len() && k.starts_with(name) && k.as_bytes()[name.len()] == b'.' {
                Some(v)
            } else {
                None
            }
        })
        .min()
}

/// Branch condition that tests `op` against flags already set by a compare.
///
/// Floats need different codes from integers for `<` and `<=`: `fcmp` reports an
/// unordered pair (NaN) as `N=0, Z=0, C=1, V=1`, and the integer aliases `LT`
/// (`N != V`) and `LE` (`Z == 1 || N != V`) are both true for that, while `MI`
/// (`N == 1`) and `LS` (`C == 0 || Z == 1`) are both false. `>`, `>=`, `==`, and
/// `!=` agree between the two, so NaN compares false and `!=` true, as IEEE 754
/// requires.
fn comparison_cond(op: &str, is_float: bool) -> Result<u8, String> {
    Ok(match op {
        "==" | "===" => 0,
        "!=" | "!==" => 1,
        "<" if is_float => 4,
        "<" => 11,
        ">" => 12,
        "<=" if is_float => 9,
        "<=" => 13,
        ">=" => 10,
        _ => {
            return Err(format!(
                "native-lower: unsupported comparison operator `{op}`"
            ));
        }
    })
}

pub(crate) fn lower_comparison_result(
    emitter: &mut CodeEmitter,
    rd: u8,
    op: &str,
    is_float: bool,
) -> Result<(), String> {
    let cond = comparison_cond(op, is_float)?;
    let true_branch = emitter.emit_insn(aarch64::b_cond(cond, 0));
    emitter.emit_insns(&aarch64::load_i64(rd, 0));
    let end_branch = emitter.emit_insn(aarch64::b(0));
    let true_offset = emitter.len() as i32 - true_branch as i32;
    emitter.patch_u32(true_branch, aarch64::b_cond(cond, true_offset));
    emitter.emit_insns(&aarch64::load_i64(rd, 1));
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b(end_offset));
    Ok(())
}

pub(crate) fn emit_epilogue(emitter: &mut CodeEmitter, _stack_reserve: u32) {
    // Restore the frame base even when an error leaves operand spills outstanding.
    emitter.emit_u32(aarch64::mov_reg64(aarch64::REG_SP, aarch64::REG_FP));
    emitter.emit_u32(0xA8C1_7BFD);
    emitter.emit_u32(aarch64::ret());
}

pub(crate) fn ensure_return_type(
    ret: &Typ,
    fn_name: &str,
    structs: &HashMap<String, Vec<(String, Typ)>>,
) -> Result<(), String> {
    ensure_unit_sequence_layout(ret)?;
    match ret {
        Typ::Int | Typ::Float | Typ::Bool | Typ::String | Typ::Void => Ok(()),
        Typ::Named(struct_name) => {
            native_struct_fields(structs, struct_name, fn_name)?;
            Ok(())
        }
        Typ::Array(elem) => {
            ensure_native_array_element(elem, fn_name, "return")?;
            record_array_fields(elem, structs)?;
            Ok(())
        }
        Typ::Vector(_) => Ok(()),
        Typ::Generic(_) => Ok(()),
    }
}

pub(crate) fn call_return_type<'a>(
    callee: &Expr,
    functions: &'a HashMap<String, FunctionInfo>,
    _fn_name: &str,
) -> Result<Option<&'a Typ>, String> {
    let Expr::Ident(target) = callee else {
        // Non-Ident callee: return None (caller handles fallback)
        return Ok(None);
    };
    if let Some(func) = functions.get(target) {
        return Ok(Some(&func.ret));
    }
    if let Some(idx) = target.rfind("::") {
        let last = &target[idx + 2..];
        if let Some(func) = functions.get(last) {
            return Ok(Some(&func.ret));
        }
    }
    Ok(None)
}

pub(crate) fn reject_unsupported_function(
    func: &FunctionInfo,
    structs: &HashMap<String, Vec<(String, Typ)>>,
) -> Result<(), String> {
    if native_param_abi_slots(&func.params, structs, &func.name)? > 128 {
        return Err(format!(
            "native-lower: too many parameters in `{}`",
            func.name
        ));
    }
    Ok(())
}

pub(crate) fn native_param_abi_slots(
    params: &[(String, Typ)],
    structs: &HashMap<String, Vec<(String, Typ)>>,
    fn_name: &str,
) -> Result<usize, String> {
    let mut slots = 0usize;
    fn resolve_self<'a>(
        name: &str,
        fn_name: &'a str,
        structs: &HashMap<String, Vec<(String, Typ)>>,
    ) -> Option<&'a str> {
        if name == "Self" {
            if let Some(outer) = fn_name.split("::").next() {
                if structs.contains_key(outer) {
                    return Some(outer);
                }
            }
        }
        None
    }
    fn count_type_slots(
        typ: &Typ,
        structs: &HashMap<String, Vec<(String, Typ)>>,
        fn_name: &str,
        visited: &mut Vec<String>,
        depth: u32,
    ) -> Result<usize, String> {
        if depth > 40 {
            return Ok(1); // deep nesting, treat as opaque
        }
        ensure_unit_sequence_layout(typ)?;
        match typ {
            Typ::Int | Typ::Bool | Typ::String | Typ::Float => Ok(1),
            Typ::Void => Ok(0),
            Typ::Named(struct_name) => {
                if struct_name == "String[]" {
                    return Ok(2);
                }
                let lookup = resolve_self(struct_name, fn_name, structs).unwrap_or(struct_name);
                let base = base_struct_name(lookup);
                if visited.contains(&base.to_string()) {
                    return Ok(1);
                }
                let Some(fields) = structs.get(base) else {
                    return Ok(1);
                };
                visited.push(base.to_string());
                let mut total = 0usize;
                for (_, field_ty) in fields {
                    if let Typ::Array(elem) = field_ty.canonical() {
                        ensure_native_array_field_element(&elem, fn_name)?;
                    }
                    total += count_type_slots(field_ty, structs, fn_name, visited, depth + 1)?;
                }
                visited.pop();
                Ok(total)
            }
            Typ::Array(elem) => {
                ensure_native_array_element(elem, fn_name, "parameter")?;
                Ok(2)
            }
            Typ::Vector(_) => Ok(3),
            _ => Err(format!(
                "native-lower: unsupported parameter type `{typ:?}` in `{fn_name}`"
            )),
        }
    }
    for (name, typ) in params {
        if name == "self" && matches!(typ, Typ::Named(_)) {
            slots += 1;
        } else {
            let mut visited = Vec::new();
            slots += count_type_slots(typ, structs, fn_name, &mut visited, 0)?;
        }
    }
    Ok(slots)
}

pub(crate) fn native_struct_fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    typ: &str,
    fn_name: &str,
) -> Result<Vec<(String, Typ)>, String> {
    if typ == "ZST" {
        return Ok(vec![]);
    }
    let lookup = if typ == "Self" {
        fn_name
            .split("::")
            .next()
            .filter(|outer| structs.contains_key(*outer))
            .unwrap_or(typ)
    } else {
        typ
    };
    let base = base_struct_name(lookup);
    let Some(fields) = structs.get(base) else {
        return Ok(vec![]);
    };
    let cleaned = fields
        .iter()
        .map(|(name, field_ty)| match canonical_type(field_ty) {
            typ @ (Typ::Int | Typ::Bool | Typ::String | Typ::Float) => Ok((name.clone(), typ)),
            typ if crate::core_ir::rust_references::aggregate_referent(&typ).is_some() => {
                Ok((name.clone(), typ))
            }
            Typ::Named(inner) if structs.contains_key(inner.as_str()) => {
                Ok((name.clone(), field_ty.clone()))
            }
            Typ::Vector(_) => Ok((name.clone(), field_ty.clone())),
            Typ::Array(elem) => ensure_native_array_field_element(&elem, fn_name)
                .map(|()| (name.clone(), Typ::Array(elem)))
                .map_err(|_| ()),
            _ => Err(()),
        })
        .collect::<Result<Vec<_>, ()>>();
    let cleaned = match cleaned {
        Ok(fields) => fields,
        Err(_) => {
            return Err(format!(
                "native-lower: unsupported struct field layout `{typ}` in `{fn_name}`"
            ));
        }
    };
    if cleaned.len() > 32 {
        return Err(format!(
            "native-lower: struct `{typ}` has too many fields (>32) in `{fn_name}`"
        ));
    }
    Ok(cleaned)
}

pub(crate) fn is_native_scalar_type(typ: &Typ) -> bool {
    crate::core_ir::rust_references::aggregate_referent(typ).is_some()
        || matches!(
            canonical_type(typ),
            Typ::Int | Typ::Bool | Typ::String | Typ::Float
        )
}

pub(crate) fn ensure_unit_sequence_layout(typ: &Typ) -> Result<(), String> {
    match typ {
        Typ::Array(item) | Typ::Vector(item) => {
            if item.canonical() == Typ::Void {
                return Err(
                    "native-lower: unit sequence elements require a zero-width layout".into(),
                );
            }
            ensure_unit_sequence_layout(item)
        }
        _ => Ok(()),
    }
}

pub(crate) fn ensure_native_array_element(
    elem: &Typ,
    fn_name: &str,
    context: &str,
) -> Result<(), String> {
    if elem.canonical() == Typ::Void {
        return Err("native-lower: unit array elements require a zero-width layout".into());
    }
    match canonical_type(elem) {
        Typ::Int | Typ::Bool | Typ::String | Typ::Float => Ok(()),
        Typ::Array(_) => Err(format!(
            "native-lower[native-array-nested-unsupported]: unsupported {context} array element type in `{fn_name}` (nested arrays are not supported)"
        )),
        Typ::Named(_) => Ok(()), // aggregate types: treat as opaque pointers
        _ => Ok(()),             // accept any element type as opaque
    }
}

/// Array fields use a pointer/length pair; aggregate element layouts remain unsupported.
pub(crate) fn ensure_native_array_field_element(elem: &Typ, fn_name: &str) -> Result<(), String> {
    ensure_native_array_element(elem, fn_name, "struct field")?;
    if !is_native_scalar_type(elem) {
        return Err(format!(
            "native-lower: struct array field requires scalar elements in `{fn_name}`"
        ));
    }
    Ok(())
}

pub(crate) fn array_item_matches(expected: &Typ, actual: &Typ) -> bool {
    let expected = canonical_type(expected);
    let actual = canonical_type(actual);
    expected == actual || matches!((&expected, &actual), (Typ::Int, Typ::Bool))
}

pub(crate) fn expr_type(expr: &Expr) -> Option<Typ> {
    match expr {
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => crate::core_ir::conditional_type(
            then_expr,
            expr_type(then_expr),
            else_expr,
            expr_type(else_expr),
        ),
        Expr::Block { result, .. } => result.as_deref().and_then(expr_type),
        Expr::UnitLit => Some(Typ::Void),
        Expr::IntLit(_) => Some(Typ::Int),
        Expr::FloatLit(_) => Some(Typ::Float),
        Expr::BoolLit(_) => Some(Typ::Bool),
        Expr::StringLit(_) => Some(Typ::String),
        Expr::ArrayLit(items) => Some(Typ::Array(Box::new(
            items.iter().find_map(expr_type).unwrap_or(Typ::Void),
        ))),
        Expr::StructInit { name, .. } => Some(Typ::Named(name.clone())),
        _ => None,
    }
}

pub(crate) use crate::core_ir::expr_contains_call;

/// Record array slots contain pointers to independent, declaration-ordered payloads.
/// This is deliberately narrower than recursively flattened scalar struct ABI layouts.
pub(crate) fn record_array_fields(
    elem: &Typ,
    structs: &HashMap<String, Vec<(String, Typ)>>,
) -> Result<Option<Vec<(String, Typ)>>, String> {
    match elem.canonical() {
        Typ::Named(name) => {
            crate::native_emit::struct_layout::flat_record_fields(structs, &name).map(Some)
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::comparison_cond;

    #[test]
    fn scalar_array_struct_fields_count_two_argument_words() {
        use crate::core_ir::Typ;
        use std::collections::HashMap;
        let structs = HashMap::from([(
            "Holder".into(),
            vec![
                ("before".into(), Typ::Int),
                ("items".into(), Typ::Array(Box::new(Typ::String))),
                ("after".into(), Typ::Bool),
            ],
        )]);
        assert_eq!(
            super::native_param_abi_slots(
                &[("value".into(), Typ::Named("Holder".into()))],
                &structs,
                "take"
            )
            .unwrap(),
            4
        );
        assert_eq!(
            super::native_param_abi_slots(
                &[("self".into(), Typ::Named("Holder".into()))],
                &structs,
                "Holder::read"
            )
            .unwrap(),
            1
        );
        let unsupported = HashMap::from([(
            "Holder".into(),
            vec![(
                "items".into(),
                Typ::Array(Box::new(Typ::Named("Unknown".into()))),
            )],
        )]);
        assert!(
            super::native_param_abi_slots(
                &[("value".into(), Typ::Named("Holder".into()))],
                &unsupported,
                "take"
            )
            .is_err()
        );
    }

    /// `fcmp` reports an unordered pair as `N=0, Z=0, C=1, V=1`, so `<` and `<=`
    /// must use the `MI` and `LS` codes: the integer aliases `LT` (`N != V`) and
    /// `LE` (`Z == 1 || N != V`) are true for NaN and would make `nan < 1.0` true.
    #[test]
    fn float_less_than_uses_the_unordered_safe_codes() {
        assert_eq!(comparison_cond("<", true).unwrap(), 4, "MI");
        assert_eq!(comparison_cond("<=", true).unwrap(), 9, "LS");
        assert_eq!(comparison_cond("<", false).unwrap(), 11, "LT");
        assert_eq!(comparison_cond("<=", false).unwrap(), 13, "LE");
    }

    /// The remaining float codes are the integer ones, and they already treat
    /// NaN as unordered: `>`, `>=`, and `==` false, `!=` true.
    #[test]
    fn float_comparisons_agree_with_integers_where_they_should() {
        for op in ["==", "!=", ">", ">="] {
            assert_eq!(
                comparison_cond(op, true).unwrap(),
                comparison_cond(op, false).unwrap(),
                "{op}"
            );
        }
    }
}
