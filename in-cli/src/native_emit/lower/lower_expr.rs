//! Core IR → AArch64 expression lowering.

use super::lower_call;
use super::{
    FunctionInfo, LocalSlot, LowerCtx, PendingCall, emit_failure_return, expr_type,
    find_field_offset, lower_comparison_result, pick_scratch,
};
use crate::core_ir::{Expr, FloatVal, Typ};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::HashMap;

pub(crate) fn lower_value_control<F>(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
    mut lower: F,
) -> Result<(), String>
where
    F: FnMut(
        &mut CodeEmitter,
        &mut LowerCtx<'_>,
        &Expr,
        &mut Vec<PendingCall>,
    ) -> Result<(), String>,
{
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            if native_expr_type(expr, ctx, functions).is_none() {
                return Err(
                    "native-lower: match value arm type mismatch or unknown result type".into(),
                );
            }
            let (name, typ, planned) = ctx
                .match_values
                .get(&(expr as *const Expr as usize))
                .cloned()
                .ok_or("native-lower: missing match value storage plan")?;
            if !crate::core_ir::match_value_exhaustive(&typ, arms, &|name| {
                ctx.structs.get(name).map(Vec::as_slice)
            }) {
                return Err("native-lower: nonexhaustive match value".into());
            }
            super::lower_stmt::lower_store_local(
                emitter,
                ctx,
                &name,
                scrutinee,
                functions,
                pending_calls,
                fn_name,
            )?;
            let mut ends = Vec::new();
            for (arm, (test, bindings)) in arms.iter().zip(planned) {
                lower_expr_into(emitter, ctx, &test, 0, functions, pending_calls, fn_name)?;
                emitter.emit_u32(aarch64::cmp_reg64(0, aarch64::REG_XZR));
                let next = emitter.emit_insn(aarch64::b_cond(0, 0));
                let locals = ctx.locals.clone();
                let types = ctx.scalar_types.clone();
                let pointers = ctx.receiver_pointers.clone();
                let ret = ctx.ret_typ.clone();
                for binding in &bindings {
                    super::lower_stmt::lower_stmt(
                        emitter,
                        ctx,
                        binding,
                        functions,
                        pending_calls,
                        fn_name,
                        &ret,
                    )?;
                }
                let guard_next = if let Some(guard) = &arm.guard {
                    if !crate::core_ir::expr_diverges(guard)
                        && native_expr_type(guard, ctx, functions) != Some(Typ::Bool)
                    {
                        return Err("native-lower: match guard must be Bool".into());
                    }
                    lower_expr_into(emitter, ctx, guard, 0, functions, pending_calls, fn_name)?;
                    emitter.emit_u32(aarch64::cmp_reg64(0, aarch64::REG_XZR));
                    Some(emitter.emit_insn(aarch64::b_cond(0, 0)))
                } else {
                    None
                };
                lower(emitter, ctx, &arm.value, pending_calls)?;
                ends.push(emitter.emit_insn(aarch64::b(0)));
                ctx.locals = locals;
                ctx.scalar_types = types;
                ctx.receiver_pointers = pointers;
                emitter.patch_u32(next, aarch64::b_cond(0, emitter.len() as i32 - next as i32));
                if let Some(next) = guard_next {
                    emitter.patch_u32(next, aarch64::b_cond(0, emitter.len() as i32 - next as i32));
                }
            }
            emit_failure_return(emitter, ctx, fn_name)?;
            for end in ends {
                emitter.patch_u32(end, aarch64::b(emitter.len() as i32 - end as i32));
            }
            Ok(())
        }
        Expr::Conditional {
            cond,
            then_expr,
            else_expr,
        } => {
            if !crate::core_ir::expr_diverges(cond)
                && native_expr_type(cond, ctx, functions).map(|typ| typ.canonical())
                    != Some(Typ::Bool)
            {
                return Err("native-lower: conditional requires a Bool condition".into());
            }
            let then_type = native_expr_type(then_expr, ctx, functions);
            let else_type = native_expr_type(else_expr, ctx, functions);
            if !crate::core_ir::expr_diverges(then_expr)
                && !crate::core_ir::expr_diverges(else_expr)
                && let (Some(then_type), Some(else_type)) = (then_type, else_type)
                && crate::core_ir::conditional_type(
                    then_expr,
                    Some(then_type),
                    else_expr,
                    Some(else_type),
                )
                .is_none()
            {
                return Err("native-lower: conditional branch type mismatch".into());
            }
            lower_expr_into(emitter, ctx, cond, 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::cmp_reg64(0, aarch64::REG_XZR));
            let otherwise = emitter.emit_insn(aarch64::b_cond(0, 0));
            lower(emitter, ctx, then_expr, pending_calls)?;
            let done = emitter.emit_insn(aarch64::b(0));
            emitter.patch_u32(
                otherwise,
                aarch64::b_cond(0, emitter.len() as i32 - otherwise as i32),
            );
            lower(emitter, ctx, else_expr, pending_calls)?;
            emitter.patch_u32(done, aarch64::b(emitter.len() as i32 - done as i32));
            Ok(())
        }
        Expr::Block { body, result } => {
            let locals = ctx.locals.clone();
            let scalar_types = ctx.scalar_types.clone();
            let pointers = ctx.receiver_pointers.clone();
            let params = ctx.params.clone();
            let param_types = ctx.param_types.clone();
            let lowered = (|| {
                let ret = ctx.ret_typ.clone();
                for stmt in body {
                    super::lower_stmt::lower_stmt(
                        emitter,
                        ctx,
                        stmt,
                        functions,
                        pending_calls,
                        fn_name,
                        &ret,
                    )?;
                }
                if crate::core_ir::stmts_diverge(body) {
                    return Ok(());
                }
                if let Some(result) = result {
                    lower(emitter, ctx, result, pending_calls)
                } else {
                    lower(emitter, ctx, &Expr::IntLit(0), pending_calls)
                }
            })();
            ctx.locals = locals;
            ctx.scalar_types = scalar_types;
            ctx.receiver_pointers = pointers;
            ctx.params = params;
            ctx.param_types = param_types;
            lowered
        }
        _ => Err("native-lower: expected conditional or block value".into()),
    }
}

pub(crate) fn lower_expr_into(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if matches!(
        expr,
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. }
    ) {
        return lower_value_control(
            emitter,
            ctx,
            expr,
            functions,
            pending_calls,
            fn_name,
            |emitter, ctx, value, pending_calls| {
                lower_expr_into(emitter, ctx, value, rd, functions, pending_calls, fn_name)
            },
        );
    }
    match expr {
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. } => unreachable!(),
        Expr::UnitLit => {
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            Ok(())
        }
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
            if ctx.scalar_type(name).map(|typ| typ.canonical()) == Some(Typ::Void) {
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
                if matches!(
                    ctx.global_types.get(name).map(Typ::canonical),
                    Some(Typ::Array(_))
                ) {
                    emitter.emit_u32(aarch64::ldr64(rd + 1, rd, 0));
                    emitter.emit_u32(aarch64::add_imm64(rd, rd, 16));
                } else {
                    emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
                }
            } else if functions.contains_key(name) {
                let site = emitter.emit_insn(aarch64::adr(rd, 0));
                pending_calls.push(PendingCall {
                    site,
                    target: name.clone(),
                    kind: super::FunctionReferenceKind::Address { rd },
                });
            } else {
                return Err(format!(
                    "native-lower: unresolved identifier `{name}` in `{fn_name}`"
                ));
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
        Expr::ArrayLit(items) => {
            // Scalar value contexts use the same data carrier as array locals.
            // Keep the existing empty carrier; this does not define sequence truthiness.
            if items.is_empty() {
                emitter.emit_insns(&aarch64::load_i64(rd, 0));
                return Ok(());
            }
            let Some(Typ::Array(elem)) = native_expr_type(expr, ctx, functions) else {
                return Err("native-lower: array literal needs a resolved element type".into());
            };
            super::lower_util::ensure_native_array_element(&elem, fn_name, "value")?;
            if !super::lower_util::is_native_scalar_type(&elem)
                && super::lower_util::record_array_fields(&elem, ctx.structs)?.is_none()
            {
                return Err(
                    "native-lower: array value requires scalar or flat record elements".into(),
                );
            }
            // This was a literal leaf: implicit allocation must not clobber
            // registers retained by scalar/aggregate/stdlib operand consumers.
            // Preserve the Darwin platform register x18 without using it as scratch.
            let base = ctx.acquire_scalar_array_spills(fn_name)?;
            for reg in 0..19u8 {
                if reg != rd {
                    emitter.emit_u32(aarch64::str64(
                        reg,
                        aarch64::REG_SP,
                        ctx.scalar_array_spills[base + reg as usize],
                    ));
                }
            }
            let result = super::lower_stmt::lower_array_expr_into_regs(
                emitter,
                ctx,
                expr,
                &elem,
                functions,
                pending_calls,
                fn_name,
            );
            if result.is_ok() {
                if rd != 0 {
                    emitter.emit_u32(aarch64::mov_reg64(rd, 0));
                }
                for reg in 0..19u8 {
                    if reg != rd {
                        emitter.emit_u32(aarch64::ldr64(
                            reg,
                            aarch64::REG_SP,
                            ctx.scalar_array_spills[base + reg as usize],
                        ));
                    }
                }
            }
            ctx.release_scalar_array_spills();
            result
        }
        Expr::Closure { .. } => {
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
    if matches!(native_expr_type(base, ctx, functions).map(|typ| typ.canonical()), Some(Typ::Array(elem)) if matches!(elem.canonical(), Typ::Named(_)))
    {
        return Err(
            "native-lower: record array index requires a typed value or field context".into(),
        );
    }
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
    if matches!(
        base,
        Expr::Call { .. }
            | Expr::ArrayLit(_)
            | Expr::Field { .. }
            | Expr::Index { .. }
            | Expr::Conditional { .. }
            | Expr::Block { .. }
    ) || matches!(base, Expr::Unary {op, ..} if op == "vector-clone" || crate::core_ir::array_copy::count(op).is_some())
        || matches!(base, Expr::Ident(name) if !ctx.locals.contains_key(name) && matches!(ctx.global_types.get(name).map(Typ::canonical), Some(Typ::Array(_))))
    {
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
        } else if let Some(Typ::Vector(elem)) = native_expr_type(base, ctx, functions) {
            super::lower_call::lower_vector_call_arg(
                emitter,
                ctx,
                base,
                &elem,
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
        if matches!(native_expr_type(base, ctx, functions).map(|typ| typ.canonical()), Some(Typ::Array(elem)) if matches!(elem.canonical(), Typ::Array(_)))
        {
            let root = crate::native_emit::global_data::indexed_root(base)
                .ok_or("native-lower: nested indexing needs static array storage")?;
            if ctx.locals.contains_key(root) || !ctx.global_types.contains_key(root) {
                return Err("native-lower: nested indexing needs static array storage".into());
            }
            let offset_reg = pick_scratch(&[0, 1, rd, rd + 1, index_reg]);
            emitter.emit_u32(aarch64::ldr64_reg_offset(offset_reg, 0, index_reg));
            emitter.emit_u32(aarch64::sub_imm64(0, 0, 16));
            emitter.emit_u32(aarch64::add_reg64(rd, 0, offset_reg));
            emitter.emit_u32(aarch64::ldr64(rd + 1, rd, 0));
            emitter.emit_u32(aarch64::add_imm64(rd, rd, 16));
        } else {
            emitter.emit_u32(aarch64::ldr64_reg_offset(rd, 0, index_reg));
        }
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
        emit_failure_return(emitter, ctx, fn_name)?;
        let end_offset = emitter.len() as i32 - end_branch as i32;
        emitter.patch_u32(end_branch, aarch64::b(end_offset));
        ctx.release_binop_temp();
        ctx.release_binop_temp();
        return Ok(());
    }
    let slot = match base {
        Expr::Ident(name) => {
            match ctx.locals.get(name.as_str()).cloned() {
                Some(slot) => slot,
                None => {
                    // Unknown identifier (enum variant, constant): emit 0
                    emitter.emit_insns(&aarch64::load_i64(rd, 0));
                    return Ok(());
                }
            }
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
    let base_reg = match slot {
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
    emit_failure_return(emitter, ctx, fn_name)?;
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

pub(crate) fn borrowed_field_place<'a>(
    ctx: &LowerCtx<'_>,
    base: &'a Expr,
    name: &str,
    functions: &HashMap<String, FunctionInfo>,
) -> Result<Option<(&'a Expr, u32)>, String> {
    let base = if let Expr::Unary { op, expr } = base
        && op == crate::core_ir::rust_boxes::DEREF
        && native_expr_type(base, ctx, functions)
            .as_ref()
            .and_then(crate::core_ir::rust_references::aggregate_referent)
            .is_none()
    {
        expr.as_ref()
    } else {
        base
    };
    if let Some(Typ::Named(record)) = native_expr_type(base, ctx, functions)
        .as_ref()
        .and_then(crate::core_ir::rust_references::aggregate_referent)
    {
        let fields = crate::native_emit::struct_layout::arm_fields(ctx.structs, &record)?;
        let prefix = format!("{name}.");
        let index = fields
            .iter()
            .position(|(field, _)| field == name || field.starts_with(&prefix))
            .ok_or_else(|| format!("native-lower: unknown borrowed field `{record}.{name}`"))?;
        let offset = index
            .checked_mul(8)
            .filter(|offset| *offset <= 4095)
            .ok_or("native-lower: borrowed field exceeds AArch64 address offset subset")?;
        return Ok(Some((base, offset as u32)));
    }
    if let Expr::Field { base, name: parent } = base {
        return borrowed_field_place(ctx, base, &format!("{parent}.{name}"), functions);
    }
    Ok(None)
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
    if let Some((pointer, offset)) = borrowed_field_place(ctx, base, name, functions)? {
        let typ = native_expr_type(
            &Expr::Field {
                base: Box::new(base.clone()),
                name: name.into(),
            },
            ctx,
            functions,
        )
        .ok_or("native-lower: unknown borrowed field type")?;
        lower_expr_into(emitter, ctx, pointer, rd, functions, pending_calls, fn_name)?;
        if super::lower_util::is_native_scalar_type(&typ) {
            emitter.emit_u32(aarch64::ldr64(rd, rd, offset));
        } else {
            emitter.emit_u32(aarch64::add_imm64(rd, rd, offset as u16));
        }
        return Ok(());
    }
    if let Expr::Unary { op, expr } = base
        && crate::core_ir::rust_references::value_kind(op).is_some()
    {
        return lower_field(
            emitter,
            ctx,
            expr,
            name,
            rd,
            functions,
            pending_calls,
            fn_name,
        );
    }
    if let Expr::Index { base: array, index } = base {
        let fields = lower_record_index_address(
            emitter,
            ctx,
            array,
            index,
            rd,
            functions,
            pending_calls,
            fn_name,
        )?;
        let field = fields
            .iter()
            .position(|(field, _)| field == name)
            .ok_or_else(|| format!("native-lower: unknown record field `{name}`"))?;
        let offset = u32::try_from(
            field
                .checked_mul(8)
                .ok_or("native-lower: record field offset overflow")?,
        )
        .map_err(|_| "native-lower: record field offset overflow")?;
        if offset > 4095 * 8 {
            return Err(
                "native-lower: indexed record field exceeds AArch64 load offset subset".into(),
            );
        }
        emitter.emit_u32(aarch64::ldr64(rd, rd, offset));
        return Ok(());
    }
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
                    return Err(format!(
                        "native-lower: field `{name}` not found on `{local}`"
                    ));
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
        _ => Err(format!(
            "native-lower: unsupported field base for `{name}` in `{fn_name}`"
        )),
    }
}

// Array returns escape their frame and use the owning materializer. A copy source
// instead borrows inline storage only until the immediate clone finishes.
fn lower_array_copy_source(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    elem: &Typ,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if matches!(
        expr,
        Expr::Block { .. } | Expr::Conditional { .. } | Expr::MatchValue { .. }
    ) {
        return lower_value_control(
            emitter,
            ctx,
            expr,
            functions,
            pending_calls,
            fn_name,
            |emitter, ctx, value, pending_calls| {
                lower_array_copy_source(
                    emitter,
                    ctx,
                    value,
                    elem,
                    functions,
                    pending_calls,
                    fn_name,
                )
            },
        );
    }
    if let Expr::Ident(name) = expr
        && let Some(LocalSlot::Array {
            elem: actual,
            offsets,
        }) = ctx.locals.get(name)
    {
        if actual.canonical() != elem.canonical()
            || offsets
                .windows(2)
                .any(|pair| pair[0].checked_add(8) != Some(pair[1]))
        {
            return Err(
                "native-lower: array copy needs contiguous matching inline elements".into(),
            );
        }
        if let Some(offset) = offsets.first() {
            emitter.emit_u32(aarch64::add_imm64(0, aarch64::REG_SP, 0));
            emitter.emit_insns(&aarch64::load_i64(15, i64::from(*offset)));
            emitter.emit_u32(aarch64::add_reg64(0, 0, 15));
        } else {
            emitter.emit_insns(&aarch64::load_i64(0, 0));
        }
        emitter.emit_insns(&aarch64::load_i64(1, offsets.len() as i64));
        return Ok(());
    }
    super::lower_stmt::lower_array_expr_into_regs(
        emitter,
        ctx,
        expr,
        elem,
        functions,
        pending_calls,
        fn_name,
    )
}

/// Materialize one checked scalar Array prefix, preserving surrounding operand registers.
pub(crate) fn lower_array_copy(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    count: usize,
    rd: u8,
    descriptor: bool,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let Some(Typ::Array(elem)) = native_expr_type(expr, ctx, functions) else {
        return Err("native-lower: array copy requires an Array".into());
    };
    if !(matches!(elem.canonical(), Typ::Int | Typ::Float | Typ::Bool)
        || (count == 0 && elem.canonical() == Typ::Void))
    {
        return Err("native-lower: array copy requires scalar elements".into());
    }
    count
        .checked_add(2)
        .and_then(|words| words.checked_mul(8))
        .and_then(|bytes| i64::try_from(bytes).ok())
        .ok_or("native-lower: array copy allocation overflow")?;
    if rd > 17 || (descriptor && rd > 16) {
        return Err("native-lower: array copy result register is unsupported".into());
    }
    let base = ctx.acquire_scalar_array_spills(fn_name)?;
    let result = (|| {
        for reg in 0..19u8 {
            if reg != rd && !(descriptor && reg == rd + 1) {
                emitter.emit_u32(aarch64::str64(
                    reg,
                    aarch64::REG_SP,
                    ctx.scalar_array_spills[base + reg as usize],
                ));
            }
        }
        lower_array_copy_source(emitter, ctx, expr, &elem, functions, pending_calls, fn_name)?;
        if count != 0 {
            emitter.emit_insns(&aarch64::load_i64(15, count as i64));
            emitter.emit_u32(aarch64::cmp_reg64(1, 15));
            let short = emitter.emit_insn(aarch64::b_cond(3, 0));
            emitter.emit_u32(aarch64::cmp_reg64(0, 31));
            let null = emitter.emit_insn(aarch64::b_cond(0, 0));
            emitter.emit_insns(&aarch64::load_i64(15, 7));
            emitter.emit_u32(aarch64::and_reg64(15, 0, 15));
            emitter.emit_u32(aarch64::cmp_reg64(15, 31));
            let unaligned = emitter.emit_insn(aarch64::b_cond(1, 0));
            let sufficient = emitter.emit_insn(aarch64::b(0));
            let failed = emitter.len();
            emitter.patch_u32(short, aarch64::b_cond(3, failed as i32 - short as i32));
            emitter.patch_u32(null, aarch64::b_cond(0, failed as i32 - null as i32));
            emitter.patch_u32(
                unaligned,
                aarch64::b_cond(1, failed as i32 - unaligned as i32),
            );
            super::lower_stmt::emit_error_slot_adr(emitter, fn_name)?;
            emitter.emit_insns(&aarch64::load_i64(0, 1));
            emitter.emit_u32(aarch64::str64(0, 27, 8));
            emitter.emit_u32(aarch64::str64(0, 27, 16));
            emitter.emit_u32(aarch64::strb(0, 27, 0));
            super::lower_stmt::emit_error_exit(emitter, ctx);
            emitter.patch_u32(
                sufficient,
                aarch64::b(emitter.len() as i32 - sufficient as i32),
            );
        }
        emitter.emit_insns(&aarch64::load_i64(1, count as i64));
        if super::TL_NATIVE_MODE.with(|mode| *mode.borrow()) {
            let target = if count == 0 {
                // Empty copies still own a distinct allocation; vector clone canonicalizes empties.
                emitter.emit_insns(&aarch64::load_i64(0, 1));
                crate::inrt::INRT_VECTOR_ALLOC
            } else {
                emitter.emit_u32(aarch64::mov_reg64(2, 1));
                crate::inrt::INRT_VECTOR_CLONE
            };
            let site = emitter.len();
            emitter.emit_u32(aarch64::bl(0));
            ctx.pending_inrt_calls.push(super::PendingInrtCall {
                site,
                target: target.into(),
            });
            if count == 0 {
                emitter.emit_insns(&aarch64::load_i64(1, 0));
            }
        } else {
            emitter.emit_u32(aarch64::mov_reg64(2, 1));
            emitter.emit_u32(aarch64::mov_reg64(1, 0));
            emitter.emit_u32(aarch64::mov_reg64(0, 27));
            emitter.emit_insns(&aarch64::load_i64(
                15,
                crate::jit_runtime::clone_array as *const () as i64,
            ));
            emitter.emit_u32(0xD63F_0000 | (15 << 5));
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_u32(aarch64::add_imm64(0, 0, 16));
        }
        if descriptor && rd != 0 {
            emitter.emit_u32(aarch64::mov_reg64(rd + 1, 1));
        }
        if rd != 0 {
            emitter.emit_u32(aarch64::mov_reg64(rd, 0));
        }
        for reg in 0..19u8 {
            if reg != rd && !(descriptor && reg == rd + 1) {
                emitter.emit_u32(aarch64::ldr64(
                    reg,
                    aarch64::REG_SP,
                    ctx.scalar_array_spills[base + reg as usize],
                ));
            }
        }
        Ok(())
    })();
    ctx.release_scalar_array_spills();
    result
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
    if matches!(
        op,
        crate::core_ir::rust_boxes::DEREF
            | crate::core_ir::rust_boxes::SHARED
            | crate::core_ir::rust_boxes::MUTABLE
    ) {
        let source = if op == crate::core_ir::rust_boxes::DEREF {
            expr.clone()
        } else {
            crate::core_ir::rust_boxes::data(expr.clone())
        };
        lower_expr_into(emitter, ctx, &source, rd, functions, pending_calls, fn_name)?;
        if let Some(result) = native_expr_type(&source, ctx, functions)
            .as_ref()
            .and_then(|typ| {
                crate::core_ir::rust_boxes::value_type(crate::core_ir::rust_boxes::DEREF, typ)
            })
            && (crate::core_ir::rust_references::aggregate_referent(&result).is_some()
                || (op == crate::core_ir::rust_boxes::DEREF
                    && !matches!(result.canonical(), Typ::Named(_))))
        {
            emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
        }
        return Ok(());
    }
    if crate::core_ir::rust_references::value_kind(op).is_some() {
        if let Some(Typ::Named(name)) = native_expr_type(expr, ctx, functions)
            .map(|typ| crate::core_ir::rust_references::dereferenced(&typ))
            && ctx.structs.contains_key(&name)
        {
            return super::lower_call::lower_struct_ptr_arg(
                emitter,
                ctx,
                expr,
                rd,
                functions,
                pending_calls,
                fn_name,
            )
            .map(|_| ());
        }
        return lower_expr_into(emitter, ctx, expr, rd, functions, pending_calls, fn_name);
    }
    if let Some(count) = crate::core_ir::array_copy::count(op) {
        return lower_array_copy(
            emitter,
            ctx,
            expr,
            count,
            rd,
            false,
            functions,
            pending_calls,
            fn_name,
        );
    }
    if op == "!"
        && native_expr_type(expr, ctx, functions).is_some_and(|typ| typ.canonical() == Typ::Float)
    {
        // Numeric zero includes both signed zeros; unordered (NaN) compares false.
        return lower_float_binary(
            emitter,
            ctx,
            "==",
            expr,
            &Expr::FloatLit(FloatVal(0.0)),
            rd,
            functions,
            pending_calls,
            fn_name,
        );
    }
    if op == "vector-clone" {
        let Some(Typ::Vector(elem)) = native_expr_type(expr, ctx, functions) else {
            return Err("native-lower: vector clone requires a vector".into());
        };
        if !matches!(elem.canonical(), Typ::Int | Typ::Float | Typ::Bool) {
            return Err("native-lower: vector clone requires scalar elements".into());
        }
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
        if super::TL_NATIVE_MODE.with(|mode| *mode.borrow()) {
            let site = emitter.len();
            emitter.emit_u32(aarch64::bl(0));
            ctx.pending_inrt_calls.push(super::PendingInrtCall {
                site,
                target: crate::inrt::INRT_VECTOR_CLONE.into(),
            });
            if rd != 0 {
                for index in (0u8..3).rev() {
                    emitter.emit_u32(aarch64::mov_reg64(rd + index, index));
                }
            }
            return Ok(());
        }
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
        op if crate::core_ir::float_to_bits_width(op).is_some()
            || crate::core_ir::bits_to_float_width(op).is_some() =>
        {
            let to_bits = crate::core_ir::float_to_bits_width(op);
            let expected = if to_bits.is_some() {
                Typ::Float
            } else {
                Typ::Int
            };
            if native_expr_type(expr, ctx, functions) != Some(expected) {
                return Err(
                    "native-lower: IEEE bit conversion needs its canonical source type".into(),
                );
            }
            let width = to_bits
                .or_else(|| crate::core_ir::bits_to_float_width(op))
                .unwrap();
            if width == 32 {
                emit_f32_bit_conversion(emitter, rd, to_bits.is_some());
            }
            // The f64 Float ABI already carries the identical bits in an X register.
            Ok(())
        }
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
        op if crate::core_ir::integer_float_cast_spec(op).is_some() => {
            if native_expr_type(expr, ctx, functions) != Some(Typ::Int) {
                return Err("native-lower: integer conversion needs Int".into());
            }
            let (bits, signed) = crate::core_ir::integer_float_cast_spec(op).unwrap();
            // The operand is already width-normalized in the 64-bit Int carrier.
            // Convert directly to the destination precision to avoid double rounding.
            emitter.emit_u32(match (bits, signed) {
                (32, true) => aarch64::scvtf_s(rd, rd),
                (32, false) => aarch64::ucvtf_s(rd, rd),
                (64, true) => aarch64::scvtf_d(rd, rd),
                (64, false) => aarch64::ucvtf_d(rd, rd),
                _ => unreachable!("validated integer-to-float precision"),
            });
            if bits == 32 {
                emitter.emit_u32(aarch64::fcvt_d_s(rd, rd));
            }
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
            let operand = native_expr_type(expr, ctx, functions);
            if crate::core_ir::numeric_negation_domain(operand.as_ref(), false)
                != crate::core_ir::NumericNegationDomain::Numeric
            {
                return Err("native-lower: numeric negation requires a resolved Int, Bool, or Float operand".into());
            }
            if matches!(operand, Some(Typ::Float)) {
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

    if matches!(op, "==" | "!=" | "===" | "!==")
        && native_expr_type(lhs, ctx, functions).map(|typ| typ.canonical()) == Some(Typ::String)
        && native_expr_type(rhs, ctx, functions).map(|typ| typ.canonical()) == Some(Typ::String)
    {
        let base = ctx.acquire_call_arg_slots(fn_name, 2)?;
        for (index, value) in [lhs, rhs].into_iter().enumerate() {
            lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::str64(
                0,
                aarch64::REG_SP,
                ctx.call_arg_temps[base + index],
            ));
        }
        for index in 0..2 {
            emitter.emit_u32(aarch64::ldr64(
                index as u8,
                aarch64::REG_SP,
                ctx.call_arg_temps[base + index],
            ));
        }
        let result =
            super::lower_stdlib::emit_stdlib_wrapper_register_call(emitter, ctx, "in_str_eq", rd);
        ctx.release_call_arg_temps();
        result?;
        if matches!(op, "!=" | "!==") {
            emitter.emit_u32(aarch64::cmp_reg64(rd, aarch64::REG_XZR));
            lower_comparison_result(emitter, rd, "==", false)?;
        }
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
            return lower_checked_div_or_mod(emitter, ctx, rd, lhs_reg, rhs_reg, false, fn_name);
        }
        "%" => {
            return lower_checked_div_or_mod(emitter, ctx, rd, lhs_reg, rhs_reg, true, fn_name);
        }
        "udiv" | "urem" => {
            return lower_checked_word_div_or_mod(
                emitter,
                ctx,
                rd,
                lhs_reg,
                rhs_reg,
                (op == "urem", true),
                fn_name,
            );
        }
        "lshr" => aarch64::lsr_reg64(rd, lhs_reg, rhs_reg),
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
        "^" | "xor" | "^=" => aarch64::eor_reg64(rd, lhs_reg, rhs_reg),
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

/// Preserve NaN signaling/payload bits without passing NaNs through FP conversion.
/// Operand evaluation has completed before this leaf sequence changes SP.
fn emit_f32_bit_conversion(emitter: &mut CodeEmitter, rd: u8, to_bits: bool) {
    let sign = pick_scratch(&[rd]);
    let mask = pick_scratch(&[rd, sign]);
    emitter.emit_u32(aarch64::stp_pre(sign, mask, -16));
    if !to_bits {
        emitter.emit_insns(&aarch64::load_i64(mask, 0xffff_ffff));
        emitter.emit_u32(aarch64::and_reg64(rd, rd, mask));
    }
    let (exponent, fraction, sign_bit) = if to_bits {
        (
            0x7ff0_0000_0000_0000u64,
            0x000f_ffff_ffff_ffffu64,
            1u64 << 63,
        )
    } else {
        (0x7f80_0000u64, 0x007f_ffffu64, 1u64 << 31)
    };
    emitter.emit_insns(&aarch64::load_i64(mask, exponent as i64));
    emitter.emit_u32(aarch64::and_reg64(sign, rd, mask));
    emitter.emit_u32(aarch64::cmp_reg64(sign, mask));
    let finite = emitter.emit_insn(aarch64::b_cond(1, 0));
    emitter.emit_insns(&aarch64::load_i64(mask, fraction as i64));
    emitter.emit_u32(aarch64::and_reg64(sign, rd, mask));
    emitter.emit_u32(aarch64::cmp_reg64(sign, aarch64::REG_XZR));
    let nan = emitter.emit_insn(aarch64::b_cond(1, 0));
    emitter.patch_u32(
        finite,
        aarch64::b_cond(1, emitter.len() as i32 - finite as i32),
    );
    if to_bits {
        emitter.emit_u32(aarch64::fmov_from_gp(rd, rd));
        emitter.emit_u32(aarch64::fcvt_s_d(rd, rd));
        emitter.emit_u32(aarch64::fmov_s_to_gp(rd, rd));
    } else {
        emitter.emit_u32(aarch64::fmov_s_from_gp(rd, rd));
        emitter.emit_u32(aarch64::fcvt_d_s(rd, rd));
        emitter.emit_u32(aarch64::fmov_to_gp(rd, rd));
    }
    let done = emitter.emit_insn(aarch64::b(0));
    emitter.patch_u32(nan, aarch64::b_cond(1, emitter.len() as i32 - nan as i32));
    emitter.emit_insns(&aarch64::load_i64(mask, sign_bit as i64));
    emitter.emit_u32(aarch64::and_reg64(sign, rd, mask));
    emitter.emit_insns(&aarch64::load_i64(mask, 32));
    emitter.emit_u32(if to_bits {
        aarch64::lsr_reg64(sign, sign, mask)
    } else {
        aarch64::lsl_reg64(sign, sign, mask)
    });
    emitter.emit_insns(&aarch64::load_i64(mask, fraction as i64));
    emitter.emit_u32(aarch64::and_reg64(rd, rd, mask));
    emitter.emit_insns(&aarch64::load_i64(mask, 29));
    emitter.emit_u32(if to_bits {
        aarch64::lsr_reg64(rd, rd, mask)
    } else {
        aarch64::lsl_reg64(rd, rd, mask)
    });
    if to_bits {
        // A non-embedded f64 NaN can lose every payload bit when narrowed.
        emitter.emit_u32(aarch64::cmp_reg64(rd, aarch64::REG_XZR));
        let nonzero = emitter.emit_insn(aarch64::b_cond(1, 0));
        emitter.emit_insns(&aarch64::load_i64(rd, 1));
        emitter.patch_u32(
            nonzero,
            aarch64::b_cond(1, emitter.len() as i32 - nonzero as i32),
        );
    }
    let output_exponent = if to_bits {
        0x7f80_0000
    } else {
        0x7ff0_0000_0000_0000u64
    };
    emitter.emit_insns(&aarch64::load_i64(mask, output_exponent as i64));
    emitter.emit_u32(aarch64::orr_reg64(rd, rd, mask));
    emitter.emit_u32(aarch64::orr_reg64(rd, rd, sign));
    emitter.patch_u32(done, aarch64::b(emitter.len() as i32 - done as i32));
    emitter.emit_u32(aarch64::ldp_post(sign, mask, 16));
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
    native_expr_type_with_bindings(expr, ctx, functions, &HashMap::new())
}

fn native_expr_type_with_bindings(
    expr: &Expr,
    ctx: &LowerCtx<'_>,
    functions: &HashMap<String, FunctionInfo>,
    bindings: &HashMap<String, Typ>,
) -> Option<Typ> {
    if let Expr::Ident(name) = expr {
        if let Some(typ) = bindings.get(name) {
            return Some(typ.canonical());
        }
    }
    if let Expr::ArrayLit(items) = expr {
        return Some(Typ::Array(Box::new(
            items
                .iter()
                .filter(|item| !crate::core_ir::empty_sequence_literal(item))
                .find_map(|item| native_expr_type_with_bindings(item, ctx, functions, bindings))
                .unwrap_or(Typ::Void),
        )));
    }
    match expr {
        Expr::MatchValue { scrutinee, arms } => {
            let typ = native_expr_type_with_bindings(scrutinee, ctx, functions, bindings)?;
            let mut types = Vec::new();
            for arm in arms {
                let mut scoped = bindings.clone();
                let pattern = crate::core_ir::MatchPattern::parse(&arm.pattern).ok()?;
                for (name, typ) in pattern
                    .typed_bindings(&typ, &|name| ctx.structs.get(name).map(Vec::as_slice))
                    .ok()?
                {
                    scoped.insert(name, typ);
                }
                types.push(native_expr_type_with_bindings(
                    &arm.value, ctx, functions, &scoped,
                ));
            }
            crate::core_ir::match_value_type(arms, types)
        }
        Expr::Conditional {
            then_expr,
            else_expr,
            ..
        } => crate::core_ir::conditional_type(
            then_expr,
            native_expr_type_with_bindings(then_expr, ctx, functions, bindings),
            else_expr,
            native_expr_type_with_bindings(else_expr, ctx, functions, bindings),
        ),
        Expr::Block { body, result } => {
            let mut scoped = bindings.clone();
            for stmt in body {
                if let crate::core_ir::Stmt::Declare(name, typ) = stmt {
                    scoped.insert(name.clone(), typ.clone());
                }
                if let crate::core_ir::Stmt::Let(name, typ, value) = stmt {
                    if let Some(typ) = typ
                        .clone()
                        .or_else(|| native_expr_type_with_bindings(value, ctx, functions, &scoped))
                    {
                        scoped.insert(name.clone(), typ);
                    }
                }
            }
            match result {
                Some(result) => native_expr_type_with_bindings(result, ctx, functions, &scoped),
                None => Some(Typ::Void),
            }
        }

        Expr::Ident(name) => ctx
            .scalar_type(name)
            .or_else(|| match ctx.locals.get(name) {
                Some(LocalSlot::Struct { typ, .. }) => Some(Typ::Named(typ.clone())),
                Some(LocalSlot::Array { elem, .. } | LocalSlot::ArrayParam { elem, .. }) => {
                    Some(Typ::Array(Box::new(elem.clone())))
                }
                _ => None,
            })
            .or_else(|| functions.contains_key(name).then_some(Typ::Int)),
        Expr::Index { base, .. } => {
            match native_expr_type_with_bindings(base, ctx, functions, bindings)?.canonical() {
                Typ::Array(item) | Typ::Vector(item) => Some(*item),
                _ => None,
            }
        }
        Expr::Field { base, name } => {
            let mut typ = native_expr_type_with_bindings(base, ctx, functions, bindings)?;
            for field in name.split('.') {
                let struct_name = match crate::core_ir::rust_references::dereferenced(&typ) {
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
                .or_else(|| {
                    crate::core_ir::rust_boxes::allocation_payload(name)
                        .map(crate::core_ir::rust_boxes::address)
                })
                .or_else(|| crate::core_ir_verifier::intrinsic_return_type(name).cloned()),
            _ => None,
        },
        Expr::Unary { op, expr }
            if matches!(
                op.as_str(),
                crate::core_ir::rust_boxes::DEREF
                    | crate::core_ir::rust_boxes::SHARED
                    | crate::core_ir::rust_boxes::MUTABLE
            ) =>
        {
            crate::core_ir::rust_boxes::value_type(
                op,
                &native_expr_type_with_bindings(expr, ctx, functions, bindings)?,
            )
        }
        Expr::Unary { op, expr } if crate::core_ir::rust_references::value_kind(op).is_some() => {
            crate::core_ir::rust_references::value_type(
                op,
                native_expr_type_with_bindings(expr, ctx, functions, bindings)?,
            )
        }
        Expr::Unary { op, expr }
            if op == "-"
                || op == "vector-clone"
                || crate::core_ir::array_copy::count(op).is_some() =>
        {
            let typ = native_expr_type_with_bindings(expr, ctx, functions, bindings);
            if op == "-" && typ.as_ref().is_some_and(|typ| typ.canonical() == Typ::Bool) {
                Some(Typ::Int)
            } else {
                typ
            }
        }
        Expr::Unary { op, .. } if op == "!" => Some(Typ::Bool),
        Expr::Unary { op, .. }
            if op == "~"
                || op == "bool-to-int"
                || op == "float-to-int"
                || crate::core_ir::integer_cast_spec(op).is_some()
                || crate::core_ir::saturating_float_cast_spec(op).is_some()
                || crate::core_ir::float_to_bits_width(op).is_some() =>
        {
            Some(Typ::Int)
        }
        Expr::Unary { op, .. }
            if crate::core_ir::integer_float_cast_spec(op).is_some()
                || crate::core_ir::bits_to_float_width(op).is_some()
                || matches!(op.as_str(), "float-cast-f32" | "float-cast-f64") =>
        {
            Some(Typ::Float)
        }
        Expr::Binary { op, lhs, rhs } if matches!(op.as_str(), "&" | "|" | "^" | "xor") => {
            crate::core_ir::bitwise_result_type(
                native_expr_type_with_bindings(lhs, ctx, functions, bindings),
                native_expr_type_with_bindings(rhs, ctx, functions, bindings),
            )
        }
        Expr::Binary { op, lhs, rhs }
            if matches!(op.as_str(), "<<" | ">>" | "%" | "lshr" | "udiv" | "urem") =>
        {
            if native_expr_type_with_bindings(lhs, ctx, functions, bindings) == Some(Typ::Int)
                && native_expr_type_with_bindings(rhs, ctx, functions, bindings) == Some(Typ::Int)
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
            let lhs = native_expr_type_with_bindings(lhs, ctx, functions, bindings);
            let rhs = native_expr_type_with_bindings(rhs, ctx, functions, bindings);
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
        _ => expr_type(expr),
    }
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
    ctx: &mut LowerCtx<'_>,
    rd: u8,
    lhs_reg: u8,
    rhs_reg: u8,
    modulo: bool,
    fn_name: &str,
) -> Result<(), String> {
    lower_checked_word_div_or_mod(emitter, ctx, rd, lhs_reg, rhs_reg, (modulo, false), fn_name)
}

fn lower_checked_word_div_or_mod(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    rd: u8,
    lhs_reg: u8,
    rhs_reg: u8,
    mode: (bool, bool),
    fn_name: &str,
) -> Result<(), String> {
    let (modulo, unsigned) = mode;
    emitter.emit_u32(aarch64::cmp_reg64(rhs_reg, aarch64::REG_XZR));
    let failure_branch = emitter.emit_insn(aarch64::b_cond(0, 0));
    if modulo {
        let quotient_reg = pick_scratch(&[rd, lhs_reg, rhs_reg]);
        emitter.emit_u32(if unsigned {
            aarch64::udiv64(quotient_reg, lhs_reg, rhs_reg)
        } else {
            aarch64::sdiv64(quotient_reg, lhs_reg, rhs_reg)
        });
        emitter.emit_u32(aarch64::msub64(rd, quotient_reg, rhs_reg, lhs_reg));
    } else {
        emitter.emit_u32(if unsigned {
            aarch64::udiv64(rd, lhs_reg, rhs_reg)
        } else {
            aarch64::sdiv64(rd, lhs_reg, rhs_reg)
        });
    }
    let end_branch = emitter.emit_insn(aarch64::b(0));
    let failure_offset = emitter.len() as i32;
    emitter.patch_u32(
        failure_branch,
        aarch64::b_cond(0, failure_offset - failure_branch as i32),
    );
    emit_failure_return(emitter, ctx, fn_name)?;
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

/// Snapshot an indexed record place without reading the payload or checking
/// bounds. Stores evaluate their entire RHS before the checked payload load.
pub(crate) struct RecordIndexPlace {
    ptr: u32,
    len: u32,
    index: u32,
    pub(crate) fields: Vec<(String, Typ)>,
}

pub(crate) fn stage_record_index_place(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    index: &Expr,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<RecordIndexPlace, String> {
    let Some(Typ::Array(elem)) = native_expr_type(base, ctx, functions).map(|typ| typ.canonical())
    else {
        return Err("native-lower: record indexing requires an array".into());
    };
    let fields = super::lower_util::record_array_fields(&elem, ctx.structs)?
        .ok_or("native-lower: record indexing requires a flat record element")?;
    let ptr = ctx.acquire_binop_temp(fn_name)?;
    let len = ctx.acquire_binop_temp(fn_name)?;
    let saved_index = ctx.acquire_binop_temp(fn_name)?;
    super::lower_stmt::lower_array_expr_into_regs(
        emitter,
        ctx,
        base,
        &elem,
        functions,
        pending_calls,
        fn_name,
    )?;
    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, ptr));
    emitter.emit_u32(aarch64::str64(1, aarch64::REG_SP, len));
    lower_expr_into(emitter, ctx, index, 2, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::str64(2, aarch64::REG_SP, saved_index));
    Ok(RecordIndexPlace {
        ptr,
        len,
        index: saved_index,
        fields,
    })
}

/// Complete one staged place and release its three snapshot slots. No source
/// expression is reevaluated. Null/sentinel storage and payloads take the same
/// existing failure exit as negative/out-of-bounds indices.
pub(crate) fn load_record_index_address(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    place: &RecordIndexPlace,
    rd: u8,
    fn_name: &str,
) -> Result<(), String> {
    emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, place.ptr));
    emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, place.len));
    emitter.emit_u32(aarch64::ldr64(2, aarch64::REG_SP, place.index));
    let sentinel = if rd == 3 { 4 } else { 3 };
    emitter.emit_insns(&aarch64::load_i64(sentinel, 1));
    emitter.emit_u32(aarch64::cmp_reg64(0, sentinel));
    let invalid_base = emitter.emit_insn(aarch64::b_cond(9, 0));
    emitter.emit_u32(aarch64::cmp_reg64(2, aarch64::REG_XZR));
    let negative = emitter.emit_insn(aarch64::b_cond(11, 0));
    emitter.emit_u32(aarch64::cmp_reg64(2, 1));
    let outside = emitter.emit_insn(aarch64::b_cond(10, 0));
    emitter.emit_u32(aarch64::ldr64_reg_offset(rd, 0, 2));
    emitter.emit_u32(aarch64::cmp_reg64(rd, sentinel));
    let invalid_payload = emitter.emit_insn(aarch64::b_cond(9, 0));
    let done = emitter.emit_insn(aarch64::b(0));
    let failure = emitter.len() as i32;
    for (site, condition) in [
        (invalid_base, 9),
        (negative, 11),
        (outside, 10),
        (invalid_payload, 9),
    ] {
        emitter.patch_u32(site, aarch64::b_cond(condition, failure - site as i32));
    }
    emit_failure_return(emitter, ctx, fn_name)?;
    emitter.patch_u32(done, aarch64::b(emitter.len() as i32 - done as i32));
    for _ in 0..3 {
        ctx.release_binop_temp();
    }
    Ok(())
}

/// Reads and receivers check immediately; stores deliberately split these steps.
pub(crate) fn lower_record_index_address(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    index: &Expr,
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<Vec<(String, Typ)>, String> {
    let place =
        stage_record_index_place(emitter, ctx, base, index, functions, pending_calls, fn_name)?;
    load_record_index_address(emitter, ctx, &place, rd, fn_name)?;
    Ok(place.fields)
}

#[cfg(test)]
mod ieee_bit_conversion_tests {
    use super::*;

    #[test]
    fn f32_sequences_restore_scratch_and_keep_nan_paths_integer_only() {
        for rd in [0, 2, 3, 15] {
            for to_bits in [false, true] {
                let mut emitter = CodeEmitter::new();
                emit_f32_bit_conversion(&mut emitter, rd, to_bits);
                let words: Vec<_> = emitter
                    .bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|bytes| u32::from_le_bytes(*bytes))
                    .collect();
                let first = pick_scratch(&[rd]);
                let second = pick_scratch(&[rd, first]);
                assert_eq!(words[0], aarch64::stp_pre(first, second, -16));
                assert_eq!(*words.last().unwrap(), aarch64::ldp_post(first, second, 16));
                let branches: Vec<_> = words
                    .iter()
                    .enumerate()
                    .filter(|(_, word)| **word & 0xff00_001f == 0x5400_0001)
                    .collect();
                let (site, word) = branches[1];
                let nan_start = site + ((word >> 5) & 0x7ffff) as usize;
                assert!(
                    words[nan_start..words.len() - 1]
                        .iter()
                        .all(|word| !matches!(word >> 24, 0x1e | 0x9e)),
                    "NaN path must not quiet through FP"
                );
                let finite = if to_bits {
                    [
                        aarch64::fmov_from_gp(rd, rd),
                        aarch64::fcvt_s_d(rd, rd),
                        aarch64::fmov_s_to_gp(rd, rd),
                    ]
                } else {
                    [
                        aarch64::fmov_s_from_gp(rd, rd),
                        aarch64::fcvt_d_s(rd, rd),
                        aarch64::fmov_to_gp(rd, rd),
                    ]
                };
                assert!(words.windows(3).any(|window| window == finite));
                let (done_site, done) = words
                    .iter()
                    .enumerate()
                    .find(|(_, word)| **word & 0xfc00_0000 == 0x1400_0000)
                    .unwrap();
                assert_eq!(done_site + (done & 0x03ff_ffff) as usize, words.len() - 1);
            }
        }
    }

    #[test]
    fn ieee_unary_ops_validate_types_and_f64_keeps_raw_bits() {
        let structs = HashMap::new();
        let strings = HashMap::new();
        let globals = HashMap::new();
        let types = HashMap::new();
        let mut runtime = Vec::new();
        let mut pending_strings = Vec::new();
        let mut ctx = LowerCtx::new(
            &[],
            &structs,
            &strings,
            &globals,
            &types,
            &mut runtime,
            &mut pending_strings,
            "test",
        )
        .unwrap();
        let functions = HashMap::new();
        let mut calls = Vec::new();
        for (op, to_bits, width) in [
            ("float-to-bits-f32", true, 32),
            ("float-to-bits-f64", true, 64),
            ("bits-to-float-f32", false, 32),
            ("bits-to-float-f64", false, 64),
        ] {
            let bits = 0xfff0_0000_0000_0001u64 as i64;
            let input = if to_bits {
                Expr::Unary {
                    op: "bits-to-float-f64".into(),
                    expr: Box::new(Expr::IntLit(bits)),
                }
            } else {
                Expr::IntLit(bits)
            };
            let mut emitter = CodeEmitter::new();
            lower_unary(
                &mut emitter,
                &mut ctx,
                op,
                &input,
                3,
                &functions,
                &mut calls,
                "test",
            )
            .unwrap();
            let prefix: Vec<_> = aarch64::load_i64(3, bits)
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect();
            assert!(emitter.bytes.starts_with(&prefix));
            if width == 64 {
                assert_eq!(emitter.bytes, prefix);
            }
            let expected = if to_bits { Typ::Int } else { Typ::Float };
            assert_eq!(
                native_expr_type(
                    &Expr::Unary {
                        op: op.into(),
                        expr: Box::new(input)
                    },
                    &ctx,
                    &functions
                ),
                Some(expected)
            );
            assert!(
                lower_unary(
                    &mut CodeEmitter::new(),
                    &mut ctx,
                    op,
                    &Expr::BoolLit(true),
                    3,
                    &functions,
                    &mut calls,
                    "test"
                )
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod integer_float_cast_tests {
    use super::*;

    #[test]
    fn integer_casts_convert_directly_then_widen_the_carrier() {
        let structs = HashMap::new();
        let strings = HashMap::new();
        let globals = HashMap::new();
        let types = HashMap::new();
        let mut runtime = Vec::new();
        let mut pending_strings = Vec::new();
        let mut ctx = LowerCtx::new(
            &[],
            &structs,
            &strings,
            &globals,
            &types,
            &mut runtime,
            &mut pending_strings,
            "test",
        )
        .unwrap();
        let functions = HashMap::new();
        let mut calls = Vec::new();
        for (op, conversion, single) in [
            ("int-to-float-f32", 0x9E220063, true),
            ("uint-to-float-f32", 0x9E230063, true),
            ("int-to-float", 0x9E620063, false),
            ("uint-to-float", 0x9E630063, false),
        ] {
            let mut emitter = CodeEmitter::new();
            // The high-bit operand is deliberately the same Int bit carrier for
            // signed and unsigned conversions; only the conversion differs.
            lower_unary(
                &mut emitter,
                &mut ctx,
                op,
                &Expr::IntLit(i64::MIN),
                3,
                &functions,
                &mut calls,
                "test",
            )
            .unwrap();
            let words: Vec<_> = emitter
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| u32::from_le_bytes(*bytes))
                .collect();
            let mut expected = vec![conversion];
            if single {
                expected.push(0x1E22C063);
            }
            expected.push(0x9E660063);
            assert!(words.ends_with(&expected), "{op}: {words:x?}");
            assert_eq!(words.iter().filter(|word| **word == conversion).count(), 1);
            assert_eq!(
                native_expr_type(
                    &Expr::Unary {
                        op: op.into(),
                        expr: Box::new(Expr::IntLit(1)),
                    },
                    &ctx,
                    &functions
                ),
                Some(Typ::Float)
            );
            assert!(
                lower_unary(
                    &mut CodeEmitter::new(),
                    &mut ctx,
                    op,
                    &Expr::BoolLit(true),
                    3,
                    &functions,
                    &mut calls,
                    "test"
                )
                .is_err()
            );
        }
    }
}

#[cfg(test)]
mod record_index_place_tests {
    use super::*;

    #[test]
    fn checked_record_place_has_base_and_payload_sentinel_guards() {
        let structs = HashMap::from([("Row".into(), vec![("x".into(), Typ::Int)])]);
        let strings = HashMap::new();
        let globals = HashMap::new();
        let types = HashMap::new();
        let params = vec![(
            "rows".into(),
            Typ::Array(Box::new(Typ::Named("Row".into()))),
        )];
        let mut runtime = Vec::new();
        let mut pending_strings = Vec::new();
        let mut ctx = LowerCtx::new(
            &params,
            &structs,
            &strings,
            &globals,
            &types,
            &mut runtime,
            &mut pending_strings,
            "test",
        )
        .unwrap();
        for index in 0..ctx.binop_temps.len() {
            ctx.binop_temps[index] = ctx.alloc_slot();
        }
        let mut emitter = CodeEmitter::new();
        let mut calls = Vec::new();
        let place = stage_record_index_place(
            &mut emitter,
            &mut ctx,
            &Expr::Ident("rows".into()),
            &Expr::IntLit(0),
            &HashMap::new(),
            &mut calls,
            "test",
        )
        .unwrap();
        let staged_end = emitter.len();
        assert_eq!(ctx.binop_depth, 3);
        load_record_index_address(&mut emitter, &mut ctx, &place, 0, "test").unwrap();
        assert_eq!(ctx.binop_depth, 0);
        let words = emitter.bytes[staged_end as usize..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes))
            .collect::<Vec<_>>();
        // LS rejects both null and the existing failure sentinel (1).
        assert_eq!(
            words
                .iter()
                .filter(|word| **word & 0xff00_001f == 0x5400_0009)
                .count(),
            2
        );
        assert!(words.iter().any(|word| *word & 0xff00_001f == 0x5400_000b));
        assert!(words.iter().any(|word| *word & 0xff00_001f == 0x5400_000a));
    }
}

#[cfg(test)]
mod scalar_array_preservation_tests {
    use super::*;

    #[test]
    fn array_copy_preserves_live_registers_and_checks_source_length() {
        for (rd, descriptor) in [(0u8, true), (7u8, false), (7u8, true)] {
            let structs = HashMap::new();
            let strings = HashMap::new();
            let globals = HashMap::new();
            let types = HashMap::new();
            let mut runtime = Vec::new();
            let mut pending_strings = Vec::new();
            let params = vec![("source".into(), Typ::Array(Box::new(Typ::Int)))];
            let mut ctx = LowerCtx::new(
                &params,
                &structs,
                &strings,
                &globals,
                &types,
                &mut runtime,
                &mut pending_strings,
                "test",
            )
            .unwrap();
            for _ in 0..19 {
                let slot = ctx.alloc_slot();
                ctx.scalar_array_spills.push(slot);
            }
            let mut emitter = CodeEmitter::new();
            let mut calls = vec![];
            lower_array_copy(
                &mut emitter,
                &mut ctx,
                &Expr::Ident("source".into()),
                2,
                rd,
                descriptor,
                &HashMap::new(),
                &mut calls,
                "test",
            )
            .unwrap();
            let words: Vec<u32> = emitter
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| u32::from_le_bytes(*bytes))
                .collect();
            for reg in 0..19u8 {
                if reg != rd && !(descriptor && reg == rd + 1) {
                    assert!(words.contains(&aarch64::str64(
                        reg,
                        aarch64::REG_SP,
                        ctx.scalar_array_spills[reg as usize]
                    )));
                    assert!(words.contains(&aarch64::ldr64(
                        reg,
                        aarch64::REG_SP,
                        ctx.scalar_array_spills[reg as usize]
                    )));
                }
            }
            let compare = words
                .iter()
                .position(|word| *word == aarch64::cmp_reg64(1, 15))
                .unwrap();
            let clone = words
                .iter()
                .position(|word| *word == (0xD63F_0000 | (15 << 5)))
                .unwrap();
            assert!(
                compare < clone,
                "length is checked before allocation/payload copying"
            );
            assert!(
                words[compare + 1] & 0xff00_001f == 0x5400_0003,
                "unsigned short-source refusal branch"
            );
            let pointer_check = words
                .iter()
                .position(|word| *word == aarch64::cmp_reg64(0, 31))
                .unwrap();
            assert!(pointer_check < clone, "null payloads refuse before copying");
            let alignment_check = words
                .iter()
                .position(|word| *word == aarch64::and_reg64(15, 0, 15))
                .unwrap();
            assert!(
                alignment_check < clone,
                "unaligned payloads refuse before copying"
            );
            assert_eq!(ctx.scalar_array_depth, 0);
            let mut empty = CodeEmitter::new();
            lower_array_copy(
                &mut empty,
                &mut ctx,
                &Expr::ArrayLit(vec![]),
                0,
                rd,
                descriptor,
                &HashMap::new(),
                &mut calls,
                "test",
            )
            .unwrap();
            let empty_words: Vec<u32> = empty
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| u32::from_le_bytes(*bytes))
                .collect();
            assert!(
                empty_words.contains(&(0xD63F_0000 | (15 << 5))),
                "zero-length copies still allocate fresh storage"
            );
            assert_eq!(ctx.scalar_array_depth, 0);
            let offsets = vec![ctx.alloc_slot(), ctx.alloc_slot()];
            ctx.locals.insert(
                "inline".into(),
                LocalSlot::Array {
                    elem: Typ::Int,
                    offsets: offsets.clone(),
                },
            );
            let mut inline = CodeEmitter::new();
            lower_array_copy(
                &mut inline,
                &mut ctx,
                &Expr::Ident("inline".into()),
                0,
                rd,
                descriptor,
                &HashMap::new(),
                &mut calls,
                "test",
            )
            .unwrap();
            let inline_words: Vec<u32> = inline
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| u32::from_le_bytes(*bytes))
                .collect();
            for offset in offsets {
                for reg in 0..19 {
                    assert!(
                        !inline_words.contains(&aarch64::ldr64(reg, aarch64::REG_SP, offset)),
                        "zero copy must not read inline payload"
                    );
                }
            }
            assert_eq!(ctx.scalar_array_depth, 0);
            let mut native_empty = CodeEmitter::new();
            let previous = super::super::TL_NATIVE_MODE.with(|mode| mode.replace(true));
            let previous_error = super::super::TL_ERROR_SLOT_MODE
                .with(|mode| mode.replace(super::super::ErrorSlotMode::Unsupported));
            let lowered = lower_array_copy(
                &mut native_empty,
                &mut ctx,
                &Expr::ArrayLit(vec![]),
                0,
                rd,
                descriptor,
                &HashMap::new(),
                &mut calls,
                "test",
            );
            super::super::TL_NATIVE_MODE.with(|mode| *mode.borrow_mut() = previous);
            super::super::TL_ERROR_SLOT_MODE.with(|mode| *mode.borrow_mut() = previous_error);
            lowered.expect("zero copies need no writable error slot");
            assert_eq!(
                ctx.pending_inrt_calls.last().unwrap().target,
                crate::inrt::INRT_VECTOR_ALLOC
            );
            assert!(
                native_empty
                    .bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|bytes| u32::from_le_bytes(*bytes) == aarch64::movz64(1, 0, 0)),
                "empty allocation has zero logical length"
            );
        }
    }

    #[test]
    fn scalar_array_literal_restores_other_gp_registers_and_releases_depth() {
        for rd in [0u8, 1, 7] {
            let structs = HashMap::new();
            let strings = HashMap::new();
            let globals = HashMap::new();
            let types = HashMap::new();
            let mut runtime = Vec::new();
            let mut pending_strings = Vec::new();
            let mut ctx = LowerCtx::new(
                &[],
                &structs,
                &strings,
                &globals,
                &types,
                &mut runtime,
                &mut pending_strings,
                "test",
            )
            .unwrap();
            for index in 0..ctx.binop_temps.len() {
                ctx.binop_temps[index] = ctx.alloc_slot();
            }
            for _ in 0..19 {
                let offset = ctx.alloc_slot();
                ctx.scalar_array_spills.push(offset);
            }
            let mut emitter = CodeEmitter::new();
            let mut calls = Vec::new();
            lower_expr_into(
                &mut emitter,
                &mut ctx,
                &Expr::ArrayLit(vec![Expr::IntLit(42)]),
                rd,
                &HashMap::new(),
                &mut calls,
                "test",
            )
            .unwrap();
            let words: Vec<u32> = emitter
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| u32::from_le_bytes(*bytes))
                .collect();
            let expected_stores: Vec<_> = (0..19u8)
                .filter(|reg| *reg != rd)
                .map(|reg| {
                    aarch64::str64(reg, aarch64::REG_SP, ctx.scalar_array_spills[reg as usize])
                })
                .collect();
            let expected_loads: Vec<_> = (0..19u8)
                .filter(|reg| *reg != rd)
                .map(|reg| {
                    aarch64::ldr64(reg, aarch64::REG_SP, ctx.scalar_array_spills[reg as usize])
                })
                .collect();
            assert!(words.starts_with(&expected_stores));
            assert!(words.ends_with(&expected_loads));
            // In particular the platform x18 is saved/restored, never poisoned.
            assert!(expected_stores.contains(&aarch64::str64(
                18,
                aarch64::REG_SP,
                ctx.scalar_array_spills[18]
            )));
            assert_eq!(ctx.scalar_array_depth, 0);
            let error = lower_expr_into(
                &mut emitter,
                &mut ctx,
                &Expr::ArrayLit(vec![
                    Expr::IntLit(42),
                    Expr::FloatLit(crate::core_ir::FloatVal(1.5)),
                ]),
                rd,
                &HashMap::new(),
                &mut calls,
                "test",
            )
            .unwrap_err();
            assert!(error.contains("array return type mismatch"), "{error}");
            assert_eq!(ctx.scalar_array_depth, 0);
        }
    }
}

#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
mod scalar_array_register_runtime_tests {
    use super::*;

    #[test]
    fn jit_scalar_array_literal_preserves_other_gp_registers() {
        struct NativeModeGuard(bool);
        impl Drop for NativeModeGuard {
            fn drop(&mut self) {
                super::super::TL_NATIVE_MODE.with(|mode| {
                    mode.replace(self.0);
                });
            }
        }
        let _mode = NativeModeGuard(super::super::TL_NATIVE_MODE.with(|mode| mode.replace(false)));
        for rd in [0u8, 1, 7] {
            let structs = HashMap::new();
            let strings = HashMap::new();
            let globals = HashMap::new();
            let types = HashMap::new();
            let mut runtime = Vec::new();
            let mut pending_strings = Vec::new();
            let mut ctx = LowerCtx::new(
                &[],
                &structs,
                &strings,
                &globals,
                &types,
                &mut runtime,
                &mut pending_strings,
                "probe",
            )
            .unwrap();
            for index in 0..ctx.binop_temps.len() {
                ctx.binop_temps[index] = ctx.alloc_slot();
            }
            for _ in 0..19 {
                let offset = ctx.alloc_slot();
                ctx.scalar_array_spills.push(offset);
            }
            let expected: Vec<_> = (0..19).map(|_| ctx.alloc_slot()).collect();
            let observed: Vec<_> = (0..19).map(|_| ctx.alloc_slot()).collect();
            let reserve = u16::try_from(ctx.stack_reserve()).unwrap();
            assert!(reserve <= 4095);
            let mut emitter = CodeEmitter::new();
            emitter.emit_u32(aarch64::stp_pre(aarch64::REG_FP, 30, -16));
            emitter.emit_u32(aarch64::mov_reg64(aarch64::REG_FP, aarch64::REG_SP));
            emitter.emit_u32(aarch64::sub_imm64(
                aarch64::REG_SP,
                aarch64::REG_SP,
                reserve,
            ));
            // Snapshot the platform register, never poison it.
            emitter.emit_u32(aarch64::str64(18, aarch64::REG_SP, expected[18]));
            for reg in 0..18u8 {
                emitter.emit_insns(&aarch64::load_i64(reg, 1000 + i64::from(reg)));
                emitter.emit_u32(aarch64::str64(reg, aarch64::REG_SP, expected[reg as usize]));
            }
            let mut calls = Vec::new();
            lower_expr_into(
                &mut emitter,
                &mut ctx,
                &Expr::ArrayLit(vec![Expr::IntLit(42)]),
                rd,
                &HashMap::new(),
                &mut calls,
                "probe",
            )
            .unwrap();
            assert!(calls.is_empty());
            assert_eq!(ctx.scalar_array_depth, 0);
            // Capture every result before checking with scratch x0/x1.
            for reg in 0..19u8 {
                emitter.emit_u32(aarch64::str64(reg, aarch64::REG_SP, observed[reg as usize]));
            }
            let mut failures = Vec::new();
            for reg in 0..19u8 {
                if reg == rd {
                    continue;
                }
                emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, observed[reg as usize]));
                emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, expected[reg as usize]));
                emitter.emit_u32(aarch64::cmp_reg64(0, 1));
                failures.push(emitter.emit_insn(aarch64::b_cond(1, 0))); // NE
            }
            emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, observed[rd as usize]));
            emitter.emit_insns(&aarch64::load_i64(1, 1));
            emitter.emit_u32(aarch64::cmp_reg64(0, 1));
            failures.push(emitter.emit_insn(aarch64::b_cond(9, 0))); // LS: null/failure sentinel
            emitter.emit_u32(aarch64::ldr64(0, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(1, 42));
            emitter.emit_u32(aarch64::cmp_reg64(0, 1));
            failures.push(emitter.emit_insn(aarch64::b_cond(1, 0)));
            emitter.emit_insns(&aarch64::load_i64(0, 42));
            let success = emitter.emit_insn(aarch64::b(0));
            let failed = emitter.len();
            emitter.emit_insns(&aarch64::load_i64(0, 7));
            let finish = emitter.len();
            for site in failures {
                // All failure branches have the condition encoded in their low bits.
                let word = u32::from_le_bytes(
                    emitter.bytes[site as usize..site as usize + 4]
                        .try_into()
                        .unwrap(),
                );
                emitter.patch_u32(
                    site,
                    aarch64::b_cond((word & 15) as u8, failed as i32 - site as i32),
                );
            }
            emitter.patch_u32(success, aarch64::b(finish as i32 - success as i32));
            super::super::lower_util::emit_epilogue(&mut emitter, u32::from(reserve));
            let length = emitter.len();
            let mut jit = crate::jit_runtime::JitRuntime::new();
            jit.load(&emitter.bytes, &[("probe".into(), 0, length)], &[])
                .unwrap();
            assert_eq!(unsafe { jit.invoke("probe", &[]) }, Some(42), "rd={rd}");
        }
    }
}
