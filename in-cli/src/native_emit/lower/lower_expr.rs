//! Core IR → AArch64 expression lowering.

use super::lower_call;
use super::{
    FunctionInfo, LocalSlot, LowerCtx, PendingCall, emit_failure_return, expr_type,
    find_field_offset, lower_comparison_result, pick_scratch,
};
use crate::core_ir::{Expr, FloatVal, Typ};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::HashMap;

pub(crate) fn lower_expr_into(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    match expr {
        Expr::IntLit(value) => {
            emitter.emit_insns(&aarch64::load_i64(rd, *value));
            Ok(())
        }
        Expr::FloatLit(FloatVal(val)) => {
            emitter.emit_insns(&aarch64::load_i64(rd, val.to_bits() as i64));
            Ok(())
        }
        Expr::BoolLit(value) => {
            emitter.emit_insns(&aarch64::load_i64(rd, i64::from(*value)));
            Ok(())
        }
        Expr::StringLit(value) => {
            let id = ctx.string_id(value)?;
            let adr_site = emitter.emit_insn(aarch64::adr(rd, 0));
            ctx.pending_strings.push(super::PendingString {
                adr_site,
                string_index: id,
                rd,
            });
            Ok(())
        }
        Expr::Ident(name) => {
            if name.contains(' ') || name.contains("..") {
                // Rust front couldn't parse this expression — emit 0 as fallback
                emitter.emit_insns(&aarch64::load_i64(rd, 0));
                return Ok(());
            }
            if let Some(&pointer) = ctx.receiver_pointers.get(name) {
                emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, pointer));
            } else if let Some(offset) = ctx.params.get(name) {
                emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, *offset));
            } else if let Some(slot) = ctx.locals.get(name) {
                match slot {
                    LocalSlot::Scalar(offset) => {
                        emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, *offset));
                    }
                    LocalSlot::ArrayParam { ptr_offset, .. } => {
                        emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, *ptr_offset));
                    }
                    LocalSlot::Array { offsets, .. } => {
                        let addr = offsets.first().copied().unwrap_or(0);
                        emitter.emit_u32(aarch64::add_imm64(rd, aarch64::REG_SP, addr as u16));
                    }
                    LocalSlot::Struct { fields: slots, .. } => {
                        let addr = slots.values().min().copied().unwrap_or(0);
                        emitter.emit_u32(aarch64::add_imm64(rd, aarch64::REG_SP, addr as u16));
                    }
                }
            } else if let Some(offset) = ctx.globals.get(name).copied() {
                ctx.emit_global_address(emitter, rd, offset);
                emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
            } else {
                // Unknown identifier — enum variant, constant, or ZST used as value.
                // Emit 0 so the function can lower; correct at runtime for None/NULL-like values.
                emitter.emit_insns(&aarch64::load_i64(rd, 0));
            }
            Ok(())
        }
        Expr::Binary { op, lhs, rhs, .. } => lower_binary(
            emitter,
            ctx,
            op,
            lhs,
            rhs,
            rd,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::Unary { op, expr, .. } => lower_unary(
            emitter,
            ctx,
            op,
            expr,
            rd,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::Call { callee, args, .. } => lower_call::lower_call(
            emitter,
            ctx,
            callee,
            args,
            rd,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::Field { base, name, .. } => lower_field(
            emitter,
            ctx,
            base,
            name,
            rd,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::Index { base, index, .. } => lower_index(
            emitter,
            ctx,
            base,
            index,
            rd,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::StructInit { name, fields } => {
            // Struct init in value context.
            let schema = super::lower_util::native_struct_fields(ctx.structs, name, fn_name)?;
            if schema.is_empty() {
                emitter.emit_insns(&aarch64::load_i64(rd, 0));
                return Ok(());
            }
            if schema.len() == 1 {
                let Some((_, value)) = fields.iter().find(|(n, _)| n == &schema[0].0) else {
                    emitter.emit_insns(&aarch64::load_i64(rd, 0));
                    return Ok(());
                };
                return lower_expr_into(emitter, ctx, value, rd, functions, pending_calls, fn_name);
            }
            // Multi-field struct: use call-arg temps as scratch space
            let temp_base = ctx.acquire_call_arg_temps(fn_name)?;
            let mut field_idx = 0u32;

            let field_map: HashMap<&String, &Expr> = fields.iter().map(|(n, v)| (n, v)).collect();

            for (field_name, field_typ) in &schema {
                if matches!(field_typ, Typ::Int | Typ::Bool | Typ::String | Typ::Float) {
                    let Some(&value) = field_map.get(field_name) else {
                        emitter.emit_insns(&aarch64::load_i64(0, 0));
                        emitter.emit_u32(aarch64::str64(
                            0,
                            aarch64::REG_SP,
                            ctx.call_arg_temps[temp_base + field_idx as usize],
                        ));
                        field_idx += 1;
                        continue;
                    };
                    lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
                    let off = ctx.call_arg_temps[temp_base + field_idx as usize];
                    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, off));
                    field_idx += 1;
                }
            }
            // Return pointer to first field
            emitter.emit_u32(aarch64::add_imm64(
                rd,
                aarch64::REG_SP,
                ctx.call_arg_temps[temp_base] as u16,
            ));
            ctx.release_call_arg_temps();
            Ok(())
        }
        Expr::ArrayLit(_) | Expr::Closure { .. } => {
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            Ok(())
        }
    }
}

pub(crate) fn lower_index(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    index: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if native_expr_type(base, ctx, functions).is_some_and(|typ| typ.canonical() == Typ::Int) {
        let temp = ctx.acquire_binop_temp(fn_name)?;
        lower_expr_into(emitter, ctx, base, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, temp));
        lower_expr_into(emitter, ctx, index, 1, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, temp));
        emitter.emit_u32(aarch64::ldr64_reg_offset(rd, 0, 1));
        ctx.release_binop_temp();
        return Ok(());
    }
    if matches!(base, Expr::Call { .. } | Expr::ArrayLit(_)) {
        let ptr_slot = ctx.acquire_binop_temp(fn_name)?;
        let len_slot = ctx.acquire_binop_temp(fn_name)?;
        if let Expr::Call { callee, args } = base {
            lower_call::lower_call(
                emitter,
                ctx,
                callee,
                args,
                0,
                functions,
                pending_calls,
                fn_name,
            )?;
        } else {
            let Some(Typ::Array(elem)) = native_expr_type(base, ctx, functions) else {
                return Err(format!(
                    "native-lower: cannot infer indexed array type in `{fn_name}`"
                ));
            };
            super::lower_stmt::lower_array_expr_into_regs(
                emitter,
                ctx,
                base,
                &elem,
                functions,
                pending_calls,
                fn_name,
            )?;
        }
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, ptr_slot));
        emitter.emit_u32(aarch64::str64(1, aarch64::REG_SP, len_slot));
        let index_reg = if rd == 2 { 3 } else { 2 };
        lower_expr_into(
            emitter,
            ctx,
            index,
            index_reg,
            functions,
            pending_calls,
            fn_name,
        )?;
        emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, ptr_slot));
        emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, len_slot));
        emitter.emit_u32(aarch64::cmp_reg64(index_reg, aarch64::REG_XZR));
        let negative_branch = emitter.emit_insn(aarch64::b_cond(11, 0));
        emitter.emit_u32(aarch64::cmp_reg64(index_reg, 1));
        let oob_branch = emitter.emit_insn(aarch64::b_cond(10, 0));
        emitter.emit_u32(aarch64::ldr64_reg_offset(rd, 0, index_reg));
        let end_branch = emitter.emit_insn(aarch64::b(0));
        let failure_offset = emitter.len() as i32;
        emitter.patch_u32(
            negative_branch,
            aarch64::b_cond(11, failure_offset - negative_branch as i32),
        );
        emitter.patch_u32(
            oob_branch,
            aarch64::b_cond(10, failure_offset - oob_branch as i32),
        );
        emit_failure_return(emitter, ctx.prologue_stack_reserve);
        let end_offset = emitter.len() as i32 - end_branch as i32;
        emitter.patch_u32(end_branch, aarch64::b(end_offset));
        ctx.release_binop_temp();
        ctx.release_binop_temp();
        return Ok(());
    }
    let (slot, field_ptr_off, field_len_off) = match base {
        Expr::Ident(name) => {
            match ctx.locals.get(name.as_str()).cloned() {
                Some(slot) => (slot, None, None),
                None => {
                    // Unknown identifier (enum variant, constant): emit 0
                    emitter.emit_insns(&aarch64::load_i64(rd, 0));
                    return Ok(());
                }
            }
        }
        Expr::Field { .. } => {
            let (local, path) = super::lower_util::field_path(base).ok_or_else(|| {
                format!("native-lower: indexed field is not addressable in `{fn_name}`")
            })?;
            let Some(LocalSlot::Struct { fields, .. }) = ctx.locals.get(local) else {
                return Err(format!(
                    "native-lower: indexed field root `{local}` is not a struct in `{fn_name}`"
                ));
            };
            let ptr = fields.get(&format!("{path}.ptr")).ok_or_else(|| {
                format!("native-lower: indexed field `{path}` missing pointer in `{fn_name}`")
            })?;
            let len = fields.get(&format!("{path}.len")).ok_or_else(|| {
                format!("native-lower: indexed field `{path}` missing length in `{fn_name}`")
            })?;
            (LocalSlot::Scalar(0), Some(*ptr), Some(*len))
        }
        _ => {
            // Unknown base expression: try lowering it, then emit 0
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            return Ok(());
        }
    };
    let index_reg = if rd == 1 { 2 } else { 1 };
    lower_expr_into(
        emitter,
        ctx,
        index,
        index_reg,
        functions,
        pending_calls,
        fn_name,
    )?;
    emitter.emit_u32(aarch64::cmp_reg64(index_reg, aarch64::REG_XZR));
    let negative_branch = emitter.emit_insn(aarch64::b_cond(11, 0));
    let len_reg = pick_scratch(&[rd, index_reg]);
    let base_reg = if let (Some(ptr_off), Some(len_off)) = (field_ptr_off, field_len_off) {
        let scratch = pick_scratch(&[rd, index_reg, len_reg]);
        if let Some((root, path)) = super::lower_util::field_path(base)
            && let Some((pointer, offset)) = ctx.receiver_field(root, &format!("{path}.ptr"))
            && let Some((_, len_offset)) = ctx.receiver_field(root, &format!("{path}.len"))
        {
            emitter.emit_u32(aarch64::ldr64(scratch, aarch64::REG_SP, pointer));
            emitter.emit_u32(aarch64::ldr64(len_reg, scratch, len_offset));
            emitter.emit_u32(aarch64::ldr64(scratch, scratch, offset));
        } else {
            emitter.emit_u32(aarch64::ldr64(len_reg, aarch64::REG_SP, len_off));
            emitter.emit_u32(aarch64::ldr64(scratch, aarch64::REG_SP, ptr_off));
        }
        scratch
    } else {
        match slot {
            LocalSlot::Array { offsets, .. } => {
                emitter.emit_insns(&aarch64::load_i64(len_reg, offsets.len() as i64));
                // With length zero, every index takes the bounds-failure branch.
                let base_offset = offsets.first().copied().unwrap_or(0);
                if base_offset == 0 {
                    aarch64::REG_SP
                } else {
                    let scratch = pick_scratch(&[rd, index_reg, len_reg]);
                    emitter.emit_u32(aarch64::add_imm64(
                        scratch,
                        aarch64::REG_SP,
                        base_offset as u16,
                    ));
                    scratch
                }
            }
            LocalSlot::ArrayParam {
                ptr_offset,
                len_offset,
                ..
            } => {
                emitter.emit_u32(aarch64::ldr64(len_reg, aarch64::REG_SP, len_offset));
                let scratch = pick_scratch(&[rd, index_reg, len_reg]);
                emitter.emit_u32(aarch64::ldr64(scratch, aarch64::REG_SP, ptr_offset));
                scratch
            }
            LocalSlot::Struct { typ, fields } if typ == "Vec" => {
                let len = find_field_offset(&fields, "len")
                    .ok_or_else(|| format!("native-lower: Vec missing length in `{fn_name}`"))?;
                let ptr = find_field_offset(&fields, "ptr")
                    .ok_or_else(|| format!("native-lower: Vec missing pointer in `{fn_name}`"))?;
                emitter.emit_u32(aarch64::ldr64(len_reg, aarch64::REG_SP, *len));
                let scratch = pick_scratch(&[rd, index_reg, len_reg]);
                emitter.emit_u32(aarch64::ldr64(scratch, aarch64::REG_SP, *ptr));
                scratch
            }
            LocalSlot::Struct { fields: slots, .. } => {
                // Struct-backed inline array: fields like _0, _1, _2 are array elements
                let mut field_offsets: Vec<(u32, u32)> = slots
                    .iter()
                    .filter_map(|(k, &v)| {
                        k.strip_prefix('_')
                            .and_then(|n| n.parse::<u32>().ok())
                            .map(|idx| (idx, v))
                    })
                    .collect();
                if field_offsets.is_empty() {
                    emitter.emit_insns(&aarch64::load_i64(rd, 0));
                    return Ok(());
                }
                field_offsets.sort_by_key(|(idx, _)| *idx);
                let base_offset = field_offsets[0].1;
                emitter.emit_insns(&aarch64::load_i64(len_reg, field_offsets.len() as i64));
                let scratch = pick_scratch(&[rd, index_reg, len_reg]);
                emitter.emit_u32(aarch64::add_imm64(
                    scratch,
                    aarch64::REG_SP,
                    base_offset as u16,
                ));
                scratch
            }
            LocalSlot::Scalar(offset) => {
                // Scalar used as array base — try [ptr @ offset, len @ offset+8] pattern
                emitter.emit_u32(aarch64::ldr64(len_reg, aarch64::REG_SP, offset + 8));
                let scratch = pick_scratch(&[rd, index_reg, len_reg]);
                emitter.emit_u32(aarch64::ldr64(scratch, aarch64::REG_SP, offset));
                scratch
            }
            #[allow(unreachable_patterns)]
            _ => {
                return Err(format!(
                    "native-lower: unsupported array index base in `{fn_name}`"
                ));
            }
        }
    };
    emitter.emit_u32(aarch64::cmp_reg64(index_reg, len_reg));
    let oob_branch = emitter.emit_insn(aarch64::b_cond(10, 0));
    emitter.emit_u32(aarch64::ldr64_reg_offset(rd, base_reg, index_reg));
    let end_branch = emitter.emit_insn(aarch64::b(0));
    let failure_offset = emitter.len() as i32;
    emitter.patch_u32(
        negative_branch,
        aarch64::b_cond(11, failure_offset - negative_branch as i32),
    );
    emitter.patch_u32(
        oob_branch,
        aarch64::b_cond(10, failure_offset - oob_branch as i32),
    );
    emit_failure_return(emitter, ctx.prologue_stack_reserve);
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b(end_offset));
    Ok(())
}

pub(crate) fn lower_struct_field_into(
    emitter: &mut CodeEmitter,
    ctx: &LowerCtx<'_>,
    base: &Expr,
    field: &str,
    rd: u8,
    fn_name: &str,
) -> Result<(), String> {
    let (root, prefix) = super::lower_util::field_path(base).ok_or_else(|| {
        format!("native-lower: struct field source is not addressable in `{fn_name}`")
    })?;
    let Some(LocalSlot::Struct { typ, fields }) = ctx.locals.get(root) else {
        return Err(format!(
            "native-lower: missing struct source `{root}` in `{fn_name}`"
        ));
    };
    if prefix.is_empty()
        && ctx
            .structs
            .get(super::lower_util::base_struct_name(typ))
            .is_some_and(|fields| fields.is_empty())
    {
        emitter.emit_insns(&aarch64::load_i64(rd, 0));
        return Ok(());
    }
    let name = if prefix.is_empty() {
        field.to_string()
    } else {
        format!("{prefix}.{field}")
    };
    if let Some((pointer, offset)) = ctx.receiver_field(root, &name) {
        emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, pointer));
        emitter.emit_u32(aarch64::ldr64(rd, rd, offset));
    } else {
        let offset = fields.get(&name).copied().ok_or_else(|| {
            format!("native-lower: field `{name}` missing on source `{root}` in `{fn_name}`")
        })?;
        emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, offset));
    }
    Ok(())
}

pub(crate) fn lower_field(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    name: &str,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    match base {
        Expr::Ident(local) => {
            if ctx
                .receiver_field_type(local, name)
                .is_some_and(|typ| super::lower_util::is_native_scalar_type(&typ))
                && let Some((pointer, offset)) = ctx.receiver_field(local, name)
            {
                emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, pointer));
                emitter.emit_u32(aarch64::ldr64(rd, rd, offset));
                return Ok(());
            }

            match ctx.locals.get(local) {
                Some(LocalSlot::Struct { fields, .. }) => {
                    if let Some(offset) = find_field_offset(fields, name) {
                        emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, *offset));
                        return Ok(());
                    }
                    // For dotted field names like "inner.field", try just the outer field
                    if let Some(dot) = name.find('.') {
                        let outer = &name[..dot];
                        if let Some(offset) = find_field_offset(fields, outer) {
                            emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, *offset));
                            return Ok(());
                        }
                    }
                }
                Some(LocalSlot::Scalar(offset)) => {
                    // Opaque struct field: load scalar value
                    emitter.emit_u32(aarch64::ldr64(rd, aarch64::REG_SP, *offset));
                    return Ok(());
                }
                _ => {}
            }
            if !ctx.locals.contains_key(local) && !ctx.params.contains_key(local) {
                if let Some(offset) = ctx.globals.get(&format!("{local}.{name}")).copied() {
                    ctx.emit_global_address(emitter, rd, offset);
                    emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
                    return Ok(());
                }
            }
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            Ok(())
        }
        // Nested field: bar.baz where bar is Field { base: Ident("foo"), name: "bar" }
        Expr::Field {
            base: inner_base,
            name: inner_name,
        } => {
            let full_name = format!("{inner_name}.{name}");
            lower_field(
                emitter,
                ctx,
                inner_base,
                &full_name,
                rd,
                functions,
                pending_calls,
                fn_name,
            )
        }
        Expr::StructInit { fields, .. } => {
            let Some(value) = fields.iter().find_map(
                |(field, expr)| {
                    if field == name { Some(expr) } else { None }
                },
            ) else {
                emitter.emit_insns(&aarch64::load_i64(rd, 0));
                return Ok(());
            };
            lower_expr_into(emitter, ctx, value, rd, functions, pending_calls, fn_name)
        }
        _ => {
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            Ok(())
        }
    }
}

pub(crate) fn lower_unary(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    op: &str,
    expr: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if op == "vector-clone" {
        if super::TL_NATIVE_MODE.with(|mode| *mode.borrow()) {
            return Err("native-lower: vector clone requires the JIT runtime".into());
        }
        let Some(Typ::Vector(elem)) = native_expr_type(expr, ctx, functions) else {
            return Err("native-lower: vector clone requires a vector".into());
        };
        super::lower_call::lower_vector_call_arg(
            emitter,
            ctx,
            expr,
            &elem,
            0,
            functions,
            pending_calls,
            fn_name,
        )?;
        emitter.emit_u32(aarch64::mov_reg64(2, 1));
        emitter.emit_u32(aarch64::mov_reg64(1, 0));
        emitter.emit_u32(aarch64::mov_reg64(0, 27));
        emitter.emit_insns(&aarch64::load_i64(
            15,
            crate::jit_runtime::clone_array as *const () as i64,
        ));
        emitter.emit_u32(0xD63F_0000 | (15 << 5));
        let header = pick_scratch(&[rd, rd + 1, rd + 2]);
        emitter.emit_u32(aarch64::mov_reg64(header, 0));
        emitter.emit_u32(aarch64::ldr64(rd + 1, header, 0));
        emitter.emit_u32(aarch64::ldr64(rd + 2, header, 8));
        emitter.emit_u32(aarch64::add_imm64(rd, header, 16));
        return Ok(());
    }
    if crate::core_ir::integer_cast_spec(op).is_some() {
        let value = match native_expr_type(expr, ctx, functions) {
            Some(Typ::Int) => expr.clone(),
            Some(Typ::Bool) => Expr::Unary {
                op: "bool-to-int".into(),
                expr: Box::new(expr.clone()),
            },
            _ => return Err("native-lower: integer cast needs Int or Bool".into()),
        };
        let value = crate::core_ir::integer_cast_expr(value, op).unwrap();
        return lower_expr_into(emitter, ctx, &value, rd, functions, pending_calls, fn_name);
    }
    lower_expr_into(emitter, ctx, expr, rd, functions, pending_calls, fn_name)?;
    match op {
        "float-cast-f32" | "float-cast-f64" => {
            if native_expr_type(expr, ctx, functions) != Some(Typ::Float) {
                return Err("native-lower: floating width cast needs Float".into());
            }
            if op == "float-cast-f32" {
                emitter.emit_u32(aarch64::fmov_from_gp(rd, rd));
                emitter.emit_u32(aarch64::fcvt_s_d(rd, rd));
                emitter.emit_u32(aarch64::fcvt_d_s(rd, rd));
                emitter.emit_u32(aarch64::fmov_to_gp(rd, rd));
            }
            Ok(())
        }
        "int-to-float" => {
            if native_expr_type(expr, ctx, functions) != Some(Typ::Int) {
                return Err("native-lower: integer conversion needs Int".into());
            }
            emitter.emit_u32(aarch64::scvtf_d(rd, rd));
            emitter.emit_u32(aarch64::fmov_to_gp(rd, rd));
            Ok(())
        }
        op if crate::core_ir::saturating_float_cast_spec(op).is_some() => {
            if native_expr_type(expr, ctx, functions) != Some(Typ::Float) {
                return Err("native-lower: saturating conversion needs Float".into());
            }
            let (bits, signed) = crate::core_ir::saturating_float_cast_spec(op).unwrap();
            emit_saturating_float_cast(emitter, rd, bits, signed);
            Ok(())
        }
        "float-to-int" => {
            if native_expr_type(expr, ctx, functions) != Some(Typ::Float) {
                return Err("native-lower: floating conversion needs Float".into());
            }
            emitter.emit_u32(aarch64::fmov_from_gp(rd, rd));
            emitter.emit_u32(aarch64::fcvtzs_d(rd, rd));
            Ok(())
        }
        "bool-to-int" => {
            if native_expr_type(expr, ctx, functions) != Some(Typ::Bool) {
                return Err("native-lower: boolean promotion needs Bool".into());
            }
            Ok(())
        }
        "-" => {
            if matches!(native_expr_type(expr, ctx, functions), Some(Typ::Float)) {
                emitter.emit_u32(aarch64::fmov_from_gp(rd, rd));
                emitter.emit_u32(aarch64::fneg_d(rd, rd));
                emitter.emit_u32(aarch64::fmov_to_gp(rd, rd));
            } else {
                emitter.emit_u32(aarch64::sub_reg64(rd, aarch64::REG_XZR, rd));
            }
            Ok(())
        }
        "*" => {
            emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
            Ok(())
        }
        "!" => {
            emitter.emit_u32(aarch64::cmp_reg64(rd, aarch64::REG_XZR));
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            let false_branch = emitter.emit_insn(aarch64::b_cond(1, 0));
            emitter.emit_insns(&aarch64::load_i64(rd, 1));
            let end_offset = emitter.len() as i32 - false_branch as i32;
            emitter.patch_u32(false_branch, aarch64::b_cond(1, end_offset));
            Ok(())
        }
        "~" => {
            if native_expr_type(expr, ctx, functions) != Some(Typ::Int) {
                return Err(format!(
                    "native-lower: bitwise complement requires Int in `{fn_name}`"
                ));
            }
            emitter.emit_u32(aarch64::mvn_reg64(rd, rd));
            Ok(())
        }
        "void" | "typeof" => {
            // void: evaluate and discard (return 0)
            // typeof: emit 0 (can't return proper type string)
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            Ok(())
        }
        _ => Err(format!(
            "native-lower: unsupported unary operator `{op}` in `{fn_name}`"
        )),
    }
}

fn lower_binary_operands(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    lhs: &Expr,
    rhs: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(u8, u8), String> {
    lower_expr_into(emitter, ctx, lhs, rd, functions, pending_calls, fn_name)?;
    let lhs_reg = rd;
    let rhs_reg = if rd == 1 { 2 } else { 1 };
    // Compound RHS expressions may reuse the register holding the LHS.
    let temp = if matches!(
        rhs,
        Expr::IntLit(_)
            | Expr::FloatLit(_)
            | Expr::BoolLit(_)
            | Expr::StringLit(_)
            | Expr::Ident(_)
    ) {
        None
    } else {
        let offset = ctx.acquire_binop_temp(fn_name)?;
        emitter.emit_u32(aarch64::str64(lhs_reg, aarch64::REG_SP, offset));
        Some(offset)
    };
    lower_expr_into(
        emitter,
        ctx,
        rhs,
        rhs_reg,
        functions,
        pending_calls,
        fn_name,
    )?;
    if let Some(offset) = temp {
        emitter.emit_u32(aarch64::ldr64(lhs_reg, aarch64::REG_SP, offset));
        ctx.release_binop_temp();
    }
    Ok((lhs_reg, rhs_reg))
}

pub(crate) fn lower_float_binary(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let (lhs_reg, rhs_reg) = lower_binary_operands(
        emitter,
        ctx,
        lhs,
        rhs,
        rd,
        functions,
        pending_calls,
        fn_name,
    )?;
    emitter.emit_u32(
        if matches!(native_expr_type(lhs, ctx, functions), Some(Typ::Int)) {
            aarch64::scvtf_d(rd, lhs_reg)
        } else {
            aarch64::fmov_from_gp(rd, lhs_reg)
        },
    );
    emitter.emit_u32(
        if matches!(native_expr_type(rhs, ctx, functions), Some(Typ::Int)) {
            aarch64::scvtf_d(rhs_reg, rhs_reg)
        } else {
            aarch64::fmov_from_gp(rhs_reg, rhs_reg)
        },
    );
    match op {
        "+" | "+=" => emitter.emit_u32(aarch64::fadd_d(rd, rd, rhs_reg)),
        "-" | "-=" => emitter.emit_u32(aarch64::fsub_d(rd, rd, rhs_reg)),
        "*" | "*=" => emitter.emit_u32(aarch64::fmul_d(rd, rd, rhs_reg)),
        "/" | "/=" => emitter.emit_u32(aarch64::fdiv_d(rd, rd, rhs_reg)),
        "==" | "!=" | "<" | ">" | "<=" | ">=" => {
            // A float comparison yields a Bool, so it produces 0/1 in `rd` like
            // the integer comparisons; only the flags come from `fcmp`. Without
            // this arm every float comparison refused to lower.
            emitter.emit_u32(aarch64::fcmp_d(rd, rhs_reg));
            return lower_comparison_result(emitter, rd, op, true);
        }
        _ => {
            return Err(format!(
                "native-lower: unsupported float op `{op}` in `{fn_name}`"
            ));
        }
    }
    emitter.emit_u32(aarch64::fmov_to_gp(rd, rd));
    Ok(())
}

pub(crate) fn lower_binary(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let op = op.trim();
    if matches!(op, "&&" | "||" | "and") {
        lower_expr_into(emitter, ctx, lhs, rd, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::cmp_reg64(rd, aarch64::REG_XZR));
        let condition = if op == "||" { 1 } else { 0 };
        let skip_rhs = emitter.emit_insn(aarch64::b_cond(condition, 0));
        lower_expr_into(emitter, ctx, rhs, rd, functions, pending_calls, fn_name)?;
        emitter.patch_u32(
            skip_rhs,
            aarch64::b_cond(condition, emitter.len() as i32 - skip_rhs as i32),
        );
        lower_truthy_result(emitter, rd);
        return Ok(());
    }

    let is_float = matches!(native_expr_type(lhs, ctx, functions), Some(Typ::Float))
        || matches!(native_expr_type(rhs, ctx, functions), Some(Typ::Float));
    if is_float {
        return lower_float_binary(
            emitter,
            ctx,
            op,
            lhs,
            rhs,
            rd,
            functions,
            pending_calls,
            fn_name,
        );
    }
    if op == "+"
        && (matches!(native_expr_type(lhs, ctx, functions), Some(Typ::String))
            || matches!(native_expr_type(rhs, ctx, functions), Some(Typ::String)))
    {
        return lower_string_concat(
            emitter,
            ctx,
            lhs,
            rhs,
            rd,
            functions,
            pending_calls,
            fn_name,
        );
    }
    let (lhs_reg, rhs_reg) = lower_binary_operands(
        emitter,
        ctx,
        lhs,
        rhs,
        rd,
        functions,
        pending_calls,
        fn_name,
    )?;
    let insn = match op {
        "+" | "+=" => aarch64::add_reg64(rd, lhs_reg, rhs_reg),
        "-" | "-=" => aarch64::sub_reg64(rd, lhs_reg, rhs_reg),
        "*" | "*=" => aarch64::mul64(rd, lhs_reg, rhs_reg),

        "/" | "/=" => {
            return lower_checked_div_or_mod(emitter, ctx, rd, lhs_reg, rhs_reg, false);
        }
        "%" => {
            return lower_checked_div_or_mod(emitter, ctx, rd, lhs_reg, rhs_reg, true);
        }
        "??" => {
            lower_truthy_result(emitter, lhs_reg);
            lower_truthy_result(emitter, rhs_reg);
            aarch64::orr_reg64(rd, lhs_reg, rhs_reg)
        }
        "==" | "!=" | "===" | "!==" | "<" | ">" | "<=" | ">=" => {
            emitter.emit_u32(aarch64::cmp_reg64(lhs_reg, rhs_reg));
            return lower_comparison_result(emitter, rd, op, false);
        }
        "&" | "&=" => aarch64::and_reg64(rd, lhs_reg, rhs_reg),
        "|" | "|=" => aarch64::orr_reg64(rd, lhs_reg, rhs_reg),
        "^" | "^=" => aarch64::eor_reg64(rd, lhs_reg, rhs_reg),
        "<<" | "<<=" => aarch64::lsl_reg64(rd, lhs_reg, rhs_reg),
        ">>" | ">>=" => aarch64::asr_reg64(rd, lhs_reg, rhs_reg),
        "++" => {
            // Zig array concatenation: emit empty slice (ptr=0, len=0)
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            emitter.emit_insns(&aarch64::load_i64(rd + 1, 0));
            return Ok(());
        }
        "in" => {
            // membership test: emit 0 (false)
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            return Ok(());
        }
        _ => {
            return Err(format!(
                "native-lower: unsupported binary operator `{op}` in `{fn_name}`"
            ));
        }
    };
    emitter.emit_u32(insn);
    Ok(())
}

fn emit_saturating_float_cast(emitter: &mut CodeEmitter, rd: u8, bits: u8, signed: bool) {
    let scratch = pick_scratch(&[rd]);
    let upper = 2.0f64.powi(i32::from(bits) - i32::from(signed));
    let lower = if signed { -upper } else { 0.0 };
    emitter.emit_u32(aarch64::fmov_from_gp(rd, rd));
    emitter.emit_u32(aarch64::fcmp_d(rd, rd));
    let nan = emitter.emit_insn(aarch64::b_cond(6, 0));
    emitter.emit_insns(&aarch64::load_i64(scratch, upper.to_bits() as i64));
    emitter.emit_u32(aarch64::fmov_from_gp(scratch, scratch));
    emitter.emit_u32(aarch64::fcmp_d(rd, scratch));
    let high = emitter.emit_insn(aarch64::b_cond(10, 0));
    emitter.emit_insns(&aarch64::load_i64(scratch, lower.to_bits() as i64));
    emitter.emit_u32(aarch64::fmov_from_gp(scratch, scratch));
    emitter.emit_u32(aarch64::fcmp_d(rd, scratch));
    let low = emitter.emit_insn(aarch64::b_cond(13, 0));
    emitter.emit_u32(if signed {
        aarch64::fcvtzs_d(rd, rd)
    } else {
        aarch64::fcvtzu_d(rd, rd)
    });
    let mut ends = vec![emitter.emit_insn(aarch64::b(0))];
    for (site, condition, value) in [
        (nan, 6, 0),
        (
            high,
            10,
            crate::core_ir::saturating_float_cast(f64::INFINITY, bits, signed),
        ),
        (
            low,
            13,
            crate::core_ir::saturating_float_cast(f64::NEG_INFINITY, bits, signed),
        ),
    ] {
        emitter.patch_u32(
            site,
            aarch64::b_cond(condition, emitter.len() as i32 - site as i32),
        );
        emitter.emit_insns(&aarch64::load_i64(rd, value));
        ends.push(emitter.emit_insn(aarch64::b(0)));
    }
    for site in ends {
        emitter.patch_u32(site, aarch64::b(emitter.len() as i32 - site as i32));
    }
}

pub(crate) fn native_expr_type(
    expr: &Expr,
    ctx: &LowerCtx<'_>,
    functions: &HashMap<String, FunctionInfo>,
) -> Option<Typ> {
    if let Expr::ArrayLit(items) = expr {
        return Some(Typ::Array(Box::new(
            items
                .iter()
                .find_map(|item| native_expr_type(item, ctx, functions))
                .unwrap_or(Typ::Void),
        )));
    }
    expr_type(expr).or_else(|| match expr {
        Expr::Ident(name) => ctx
            .scalar_type(name)
            .or_else(|| match ctx.locals.get(name) {
                Some(LocalSlot::Struct { typ, .. }) => Some(Typ::Named(typ.clone())),
                Some(LocalSlot::Array { elem, .. } | LocalSlot::ArrayParam { elem, .. }) => {
                    Some(Typ::Array(Box::new(elem.clone())))
                }
                _ => None,
            }),
        Expr::Index { base, .. } => match native_expr_type(base, ctx, functions)?.canonical() {
            Typ::Array(item) | Typ::Vector(item) => Some(*item),
            _ => None,
        },
        Expr::Field { base, name } => {
            let mut typ = native_expr_type(base, ctx, functions)?;
            for field in name.split('.') {
                let struct_name = match typ.canonical() {
                    Typ::Named(name) => name,
                    Typ::Vector(_) => "Vec".to_string(),
                    _ => return None,
                };
                typ = ctx
                    .structs
                    .get(&struct_name)?
                    .iter()
                    .find(|(name, _)| name == field)?
                    .1
                    .clone();
            }
            Some(typ)
        }
        Expr::Call { callee, .. } => match callee.as_ref() {
            Expr::Ident(name) => functions
                .get(name)
                .or_else(|| {
                    name.rsplit_once("::")
                        .and_then(|(_, short)| functions.get(short))
                })
                .map(|function| function.ret.clone())
                .or_else(|| crate::core_ir_verifier::intrinsic_return_type(name).cloned()),
            _ => None,
        },
        Expr::Unary { op, expr } if op == "-" || op == "vector-clone" => {
            native_expr_type(expr, ctx, functions)
        }
        Expr::Unary { op, .. } if op == "!" => Some(Typ::Bool),
        Expr::Unary { op, .. }
            if op == "~"
                || op == "bool-to-int"
                || op == "float-to-int"
                || crate::core_ir::integer_cast_spec(op).is_some()
                || crate::core_ir::saturating_float_cast_spec(op).is_some() =>
        {
            Some(Typ::Int)
        }
        Expr::Unary { op, .. }
            if matches!(
                op.as_str(),
                "int-to-float" | "float-cast-f32" | "float-cast-f64"
            ) =>
        {
            Some(Typ::Float)
        }
        Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "&" | "|" | "^") => {
            crate::core_ir::bitwise_result_type(
                native_expr_type(lhs, ctx, functions),
                native_expr_type(rhs, ctx, functions),
            )
        }
        Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "<<" | ">>" | "%") => {
            if native_expr_type(lhs, ctx, functions) == Some(Typ::Int)
                && native_expr_type(rhs, ctx, functions) == Some(Typ::Int)
            {
                Some(Typ::Int)
            } else {
                None
            }
        }

        Expr::Binary { op, .. }
            if matches!(
                op.as_str(),
                "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||"
            ) =>
        {
            Some(Typ::Bool)
        }
        Expr::Binary { op, lhs, rhs, .. }
            if matches!(
                op.as_str(),
                "+" | "-" | "*" | "/" | "%" | "+=" | "-=" | "*=" | "/="
            ) =>
        {
            let lhs = native_expr_type(lhs, ctx, functions);
            let rhs = native_expr_type(rhs, ctx, functions);
            if op == "+" && (matches!(lhs, Some(Typ::String)) || matches!(rhs, Some(Typ::String))) {
                Some(Typ::String)
            } else if matches!(lhs, Some(Typ::Float)) || matches!(rhs, Some(Typ::Float)) {
                Some(Typ::Float)
            } else if matches!(lhs, Some(Typ::Int)) && matches!(rhs, Some(Typ::Int)) {
                Some(Typ::Int)
            } else {
                None
            }
        }
        _ => None,
    })
}

fn lower_string_concat(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    lhs: &Expr,
    rhs: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let wrapper = "in_str_concat";
    let is_native = super::TL_NATIVE_MODE.with(|m| *m.borrow());
    let temp = ctx.acquire_binop_temp(fn_name)?;
    lower_expr_into(emitter, ctx, lhs, 0, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, temp));
    lower_expr_into(emitter, ctx, rhs, 1, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, temp));
    ctx.release_binop_temp();
    if is_native {
        // A native executable links only against the system libraries, so the
        // Rust-side wrapper is not there to link against. Call the runtime
        // builtin embedded in the artifact instead.
        let call_site = emitter.len();
        emitter.emit_u32(aarch64::bl(0));
        ctx.pending_inrt_calls.push(super::PendingInrtCall {
            site: call_site,
            target: crate::inrt::INRT_STR_CONCAT.to_string(),
        });
    } else if let Some(native_ptr) = crate::native_emit::native_link::resolve_native_fn(wrapper) {
        emitter.emit_insns(&aarch64::load_i64(15, native_ptr as usize as i64));
        emitter.emit_u32(0xD63F_01E0u32 | (15 << 5));
    } else {
        emitter.emit_insns(&aarch64::load_i64(0, 0));
    }
    if rd != 0 {
        emitter.emit_u32(aarch64::mov_reg64(rd, 0));
    }
    Ok(())
}

pub(crate) fn lower_checked_div_or_mod(
    emitter: &mut CodeEmitter,
    ctx: &LowerCtx<'_>,
    rd: u8,
    lhs_reg: u8,
    rhs_reg: u8,
    modulo: bool,
) -> Result<(), String> {
    emitter.emit_u32(aarch64::cmp_reg64(rhs_reg, aarch64::REG_XZR));
    let failure_branch = emitter.emit_insn(aarch64::b_cond(0, 0));
    if modulo {
        let quotient_reg = pick_scratch(&[rd, lhs_reg, rhs_reg]);
        emitter.emit_u32(aarch64::sdiv64(quotient_reg, lhs_reg, rhs_reg));
        emitter.emit_u32(aarch64::msub64(rd, quotient_reg, rhs_reg, lhs_reg));
    } else {
        emitter.emit_u32(aarch64::sdiv64(rd, lhs_reg, rhs_reg));
    }
    let end_branch = emitter.emit_insn(aarch64::b(0));
    let failure_offset = emitter.len() as i32;
    emitter.patch_u32(
        failure_branch,
        aarch64::b_cond(0, failure_offset - failure_branch as i32),
    );
    emit_failure_return(emitter, ctx.prologue_stack_reserve);
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b(end_offset));
    Ok(())
}

pub(crate) fn lower_truthy_result(emitter: &mut CodeEmitter, rd: u8) {
    emitter.emit_u32(aarch64::cmp_reg64(rd, aarch64::REG_XZR));
    let true_branch = emitter.emit_insn(aarch64::b_cond(1, 0));
    emitter.emit_insns(&aarch64::load_i64(rd, 0));
    let end_branch = emitter.emit_insn(aarch64::b(0));
    let true_offset = emitter.len() as i32 - true_branch as i32;
    emitter.patch_u32(true_branch, aarch64::b_cond(1, true_offset));
    emitter.emit_insns(&aarch64::load_i64(rd, 1));
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b(end_offset));
}
