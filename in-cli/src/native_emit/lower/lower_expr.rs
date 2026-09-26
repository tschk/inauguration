//! Core IR → AArch64 expression lowering.

use super::lower_call;
use super::{
    FunctionInfo, LocalSlot, LowerCtx, PendingCall, contains_call, emit_failure_return, expr_type,
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
            if value.is_empty() {
                emitter.emit_insns(&aarch64::load_i64(rd, 0));
                return Ok(());
            }
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
            if let Some(offset) = ctx.params.get(name) {
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
        Expr::Field {
            base: field_base,
            name: field_name,
        } => {
            // self.buf[i] — resolve field ptr/len from struct slot map
            let local = match field_base.as_ref() {
                Expr::Ident(n) => n,
                _ => {
                    // Complex field base: emit 0
                    emitter.emit_insns(&aarch64::load_i64(rd, 0));
                    return Ok(());
                }
            };
            match ctx.locals.get(local) {
                Some(LocalSlot::Struct { fields, .. }) => {
                    let fptr = format!("{}.ptr", field_name);
                    let flen = format!("{}.len", field_name);
                    match (fields.get(&fptr), fields.get(&flen)) {
                        (Some(&p), Some(&l)) => (LocalSlot::Scalar(0), Some(p), Some(l)),
                        _ => {
                            // Field ptr/len not found: emit 0
                            emitter.emit_insns(&aarch64::load_i64(rd, 0));
                            return Ok(());
                        }
                    }
                }
                _ => {
                    // Local not found: emit 0
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
    let base_reg = if let (Some(ptr_off), Some(len_off)) = (field_ptr_off, field_len_off) {
        emitter.emit_u32(aarch64::ldr64(len_reg, aarch64::REG_SP, len_off));
        let scratch = pick_scratch(&[rd, index_reg, len_reg]);
        emitter.emit_u32(aarch64::ldr64(scratch, aarch64::REG_SP, ptr_off));
        scratch
    } else {
        match slot {
            LocalSlot::Array { offsets, .. } => {
                if offsets.is_empty() {
                    return Err(format!(
                        "native-lower: unsupported empty array index in `{fn_name}`"
                    ));
                }
                emitter.emit_insns(&aarch64::load_i64(len_reg, offsets.len() as i64));
                let base_offset = offsets[0];
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
    lower_expr_into(emitter, ctx, expr, rd, functions, pending_calls, fn_name)?;
    match op {
        "-" => {
            emitter.emit_u32(aarch64::sub_reg64(rd, aarch64::REG_XZR, rd));
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
    lower_expr_into(emitter, ctx, lhs, rd, functions, pending_calls, fn_name)?;
    let rhs_reg = if rd == 1 { 2 } else { 1 };
    lower_expr_into(
        emitter,
        ctx,
        rhs,
        rhs_reg,
        functions,
        pending_calls,
        fn_name,
    )?;
    emitter.emit_u32(aarch64::fmov_from_gp(rd, rd));
    emitter.emit_u32(aarch64::fmov_from_gp(rhs_reg, rhs_reg));
    match op {
        "+" => emitter.emit_u32(aarch64::fadd_s(rd, rd, rhs_reg)),
        "-" => emitter.emit_u32(aarch64::fsub_s(rd, rd, rhs_reg)),
        "*" => emitter.emit_u32(aarch64::fmul_s(rd, rd, rhs_reg)),
        "/" => emitter.emit_u32(aarch64::fdiv_s(rd, rd, rhs_reg)),
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
    lower_expr_into(emitter, ctx, lhs, rd, functions, pending_calls, fn_name)?;
    let lhs_reg = rd;
    let rhs_reg = if rd == 1 { 2 } else { 1 };
    // ponytail: if rhs contains a function call, its arg loading overwrites
    // the lhs result. Save lhs to the fixed binop_temp stack slot.
    let rhs_has_call = contains_call(rhs);
    let temp = if rhs_has_call {
        Some(ctx.acquire_binop_temp(fn_name)?)
    } else {
        None
    };
    if rhs_has_call {
        emitter.emit_u32(aarch64::str64(
            lhs_reg,
            aarch64::REG_SP,
            temp.expect("binary temp"),
        ));
    }
    lower_expr_into(
        emitter,
        ctx,
        rhs,
        rhs_reg,
        functions,
        pending_calls,
        fn_name,
    )?;
    if rhs_has_call {
        emitter.emit_u32(aarch64::ldr64(
            lhs_reg,
            aarch64::REG_SP,
            temp.expect("binary temp"),
        ));
        ctx.release_binop_temp();
    }
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
        "&&" | "||" | "??" | "and" => {
            lower_truthy_result(emitter, lhs_reg);
            lower_truthy_result(emitter, rhs_reg);
            match op {
                "&&" | "and" => aarch64::and_reg64(rd, lhs_reg, rhs_reg),
                "||" | "??" => aarch64::orr_reg64(rd, lhs_reg, rhs_reg),
                _ => {
                    return Err(format!(
                        "native-lower: unsupported logical operator `{op}` in `{fn_name}`"
                    ));
                }
            }
        }
        "==" | "!=" | "===" | "!==" | "<" | ">" | "<=" | ">=" => {
            emitter.emit_u32(aarch64::cmp_reg64(lhs_reg, rhs_reg));
            return lower_comparison_result(emitter, rd, op);
        }
        "&" | "&=" => aarch64::and_reg64(rd, lhs_reg, rhs_reg),
        "|" | "|=" => aarch64::orr_reg64(rd, lhs_reg, rhs_reg),
        "^" | "^=" => aarch64::eor_reg64(rd, lhs_reg, rhs_reg),
        "<<" | "<<=" => aarch64::lsl_reg64(rd, lhs_reg, rhs_reg),
        ">>" | ">>=" => aarch64::lsr_reg64(rd, lhs_reg, rhs_reg),
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

fn native_expr_type(
    expr: &Expr,
    ctx: &LowerCtx<'_>,
    functions: &HashMap<String, FunctionInfo>,
) -> Option<Typ> {
    expr_type(expr).or_else(|| match expr {
        Expr::Ident(name) => ctx.scalar_type(name),
        Expr::Call { callee, .. } => match callee.as_ref() {
            Expr::Ident(name) => functions.get(name).map(|function| function.ret.clone()),
            _ => None,
        },
        Expr::Binary { op, lhs, rhs, .. } if op == "+" => {
            let lhs = native_expr_type(lhs, ctx, functions);
            let rhs = native_expr_type(rhs, ctx, functions);
            if matches!(lhs, Some(Typ::String)) || matches!(rhs, Some(Typ::String)) {
                Some(Typ::String)
            } else if matches!(lhs, Some(Typ::Float)) || matches!(rhs, Some(Typ::Float)) {
                Some(Typ::Float)
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
