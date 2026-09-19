use crate::core_ir::{Expr, Typ};
use crate::native_emit::thumb::{
    self, COND_EQ, COND_GE, COND_GT, COND_LE, COND_LT, COND_NE, CodeEmitter, R0, R1, R2, R3, R4,
    REG_RET,
};
use std::collections::HashMap;

use super::ctx::{LocalSlot, LowerCtx, PendingCall, RelocKind};

pub(crate) fn lower_expr_into(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    dest: u8,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    match expr {
        Expr::IntLit(v) => {
            // Accept any value that fits a 32-bit word: Cortex-M MMIO/system
            // register addresses live above i32::MAX (0xE0000000+), so values
            // in the u32 range are reinterpreted as their bit pattern.
            if *v < i32::MIN as i64 || *v > u32::MAX as i64 {
                return Err(format!("thumb-lower: int lit {v} out of 32-bit range"));
            }
            thumb::load_i32(emitter, dest, *v as i32);
            Ok(())
        }
        Expr::BoolLit(b) => {
            thumb::load_i32(emitter, dest, if *b { 1 } else { 0 });
            Ok(())
        }
        Expr::Ident(name) => {
            match ctx.locals.get(name) {
                Some(LocalSlot::Scalar(off)) => {
                    // load to r0 then move if needed
                    emitter.emit_u16(thumb::ldr_sp(R0, *off)?);
                    if dest != R0 {
                        emitter.emit_u16(thumb::mov_low(dest, R0));
                    }
                    Ok(())
                }
                Some(LocalSlot::Struct { .. }) => Err(format!(
                    "thumb-lower: struct `{name}` used as scalar in `{}`",
                    ctx.fn_name
                )),
                Some(LocalSlot::Array { .. }) => Err(format!(
                    "thumb-lower: array `{name}` used as scalar in `{}`",
                    ctx.fn_name
                )),
                None => {
                    if ctx.functions.contains_key(name) {
                        // Bare function reference → load the function's address.
                        // movw/movt with zero immediates; the linker patches
                        // both halves (R_ARM_THM_MOVW_ABS_NC / MOVT_ABS).
                        let is_extern = ctx
                            .functions
                            .get(name)
                            .map(|f| f.body.is_empty())
                            .unwrap_or(false);
                        let movw_site = emitter.len();
                        emitter.emit_u32_thumb(thumb::movw(dest, 0));
                        let movt_site = emitter.len();
                        emitter.emit_u32_thumb(thumb::movt(dest, 0));
                        pending.push(PendingCall {
                            site: movw_site,
                            site2: movt_site,
                            target: name.to_string(),
                            is_extern,
                            kind: RelocKind::MovwAbs,
                        });
                        Ok(())
                    } else {
                        Err(format!(
                            "thumb-lower: unknown ident `{name}` in `{}`",
                            ctx.fn_name
                        ))
                    }
                }
            }
        }
        Expr::Unary { op, expr } => {
            lower_expr_into(emitter, ctx, expr, R0, pending)?;
            match op.as_str() {
                "-" | "neg" => {
                    emitter.emit_u16(thumb::rsbs0(R0, R0));
                    if dest != R0 {
                        emitter.emit_u16(thumb::mov_low(dest, R0));
                    }
                    Ok(())
                }
                "!" | "not" => {
                    // !x → x == 0
                    emitter.emit_u16(thumb::cmp_imm8(R0, 0));
                    emitter.emit_u16(thumb::movs_imm8(R0, 0));
                    let bne = emitter.len();
                    emitter.emit_u32_thumb(thumb::b_cond_wide(COND_NE, 0));
                    emitter.emit_u16(thumb::movs_imm8(R0, 1));
                    let end = emitter.len();
                    patch_b_cond(emitter, bne, end)?;
                    if dest != R0 {
                        emitter.emit_u16(thumb::mov_low(dest, R0));
                    }
                    Ok(())
                }
                other => Err(format!("thumb-lower: unsupported unary `{other}`")),
            }
        }
        Expr::Binary { op, lhs, rhs } => lower_binary(emitter, ctx, op, lhs, rhs, dest, pending),
        Expr::Call { callee, args } => {
            let Expr::Ident(name) = callee.as_ref() else {
                return Err(format!(
                    "thumb-lower: indirect call not supported in `{}`",
                    ctx.fn_name
                ));
            };
            if matches!(name.as_str(), "invoke" | "invoke1" | "invoke2") {
                return lower_invoke(emitter, ctx, args, dest, pending);
            }
            if try_lower_mmio(emitter, ctx, name, args, dest, pending)? {
                return Ok(());
            }
            lower_call_args(emitter, ctx, name, args, dest, pending)
        }
        Expr::FloatLit(_) => Err("thumb-lower: float not supported".into()),
        Expr::StringLit(_) => Err("thumb-lower: string not supported".into()),
        Expr::StructInit { name, fields } => {
            lower_struct_init(emitter, ctx, name, fields, dest, pending)
        }
        Expr::Field { base, name } => lower_field_load(emitter, ctx, base, name, dest, pending),
        Expr::ArrayLit(_) => Err("thumb-lower: array literal in expression position".into()),
        Expr::Index { base, index } => lower_index_load(emitter, ctx, base, index, dest, pending),
        Expr::Closure { .. } => Err("thumb-lower: closures not supported".into()),
    }
}

pub(crate) fn lower_call_args(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    name: &str,
    args: &[Expr],
    dest: u8,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let n = args.len();
    let arg_regs = [R0, R1, R2, R3];
    let base = ctx.acquire_call_arg_temps(n)?;

    // 1. Evaluate args left-to-right into temp frame slots (SP stays fixed).
    for i in 0..n {
        lower_expr_into(emitter, ctx, &args[i], R0, pending)?;
        emitter.emit_u16(thumb::str_sp(R0, ctx.call_arg_temps[base + i])?);
    }

    // 2. Load first four args into r0-r3.
    for i in 0..n.min(4) {
        emitter.emit_u16(thumb::ldr_sp(arg_regs[i], ctx.call_arg_temps[base + i])?);
    }

    // 3. Push extra args right-to-left using r4 so arg 4 ends up at [sp].
    if n > 4 {
        for i in (4..n).rev() {
            emitter.emit_u16(thumb::ldr_sp(R4, ctx.call_arg_temps[base + i])?);
            emitter.emit_u16(thumb::push(1 << 4, false));
        }
    }

    // 4. BL (internal or external).
    let is_extern = ctx
        .functions
        .get(name)
        .map(|f| f.body.is_empty())
        .unwrap_or(false);
    let site = emitter.len();
    let bl_rel = if is_extern { -2 } else { 0 };
    let enc = thumb::bl_rel(bl_rel)?;
    emitter.emit_u32_thumb(enc);
    pending.push(PendingCall {
        site,
        site2: 0,
        target: name.to_string(),
        is_extern,
        kind: RelocKind::Call,
    });

    // 5. Caller cleans up stack arguments.
    if n > 4 {
        let extra = ((n - 4) * 4) as u32;
        emitter.emit_u16(thumb::add_sp_imm(extra)?);
    }

    ctx.release_call_arg_temps();

    if dest != REG_RET {
        emitter.emit_u16(thumb::mov_low(dest, REG_RET));
    }
    Ok(())
}

/// Indirect call via `invoke`/`invoke1`/`invoke2`. arg0 is the target address;
/// remaining args are the callee's arguments (AAPCS r0-r3).
///
///   invoke(addr)          → r0 = addr; blx r0
///   invoke1(addr, a1)     → r0 = a1; r3 = addr; blx r3
///   invoke2(addr, a1, a2) → r0 = a1; r1 = a2; r2 = addr; blx r2
fn lower_invoke(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    args: &[Expr],
    dest: u8,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let n = args.len();
    if !(1..=3).contains(&n) {
        return Err(format!(
            "thumb-lower: `invoke` supports 1..=3 arguments in `{}`",
            ctx.fn_name
        ));
    }
    let base = ctx.acquire_call_arg_temps(n)?;
    for i in 0..n {
        lower_expr_into(emitter, ctx, &args[i], R0, pending)?;
        emitter.emit_u16(thumb::str_sp(R0, ctx.call_arg_temps[base + i])?);
    }
    match n {
        1 => {
            emitter.emit_u16(thumb::ldr_sp(R0, ctx.call_arg_temps[base])?);
            emitter.emit_u16(thumb::blx_reg(R0));
        }
        2 => {
            emitter.emit_u16(thumb::ldr_sp(R0, ctx.call_arg_temps[base + 1])?);
            emitter.emit_u16(thumb::ldr_sp(R3, ctx.call_arg_temps[base])?);
            emitter.emit_u16(thumb::blx_reg(R3));
        }
        _ => {
            emitter.emit_u16(thumb::ldr_sp(R0, ctx.call_arg_temps[base + 1])?);
            emitter.emit_u16(thumb::ldr_sp(R1, ctx.call_arg_temps[base + 2])?);
            emitter.emit_u16(thumb::ldr_sp(R2, ctx.call_arg_temps[base])?);
            emitter.emit_u16(thumb::blx_reg(R2));
        }
    }
    ctx.release_call_arg_temps();
    if dest != REG_RET {
        emitter.emit_u16(thumb::mov_low(dest, REG_RET));
    }
    Ok(())
}

pub(crate) fn lower_struct_init_into(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    fields: &HashMap<String, u32>,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let Expr::StructInit { name, fields: init } = expr else {
        return Err(format!("thumb-lower: expected struct initializer"));
    };
    let expected = ctx
        .structs
        .get(name)
        .ok_or_else(|| format!("thumb-lower: unknown struct `{name}`"))?;
    for (field, ty) in expected {
        let value = init
            .iter()
            .find(|(n, _)| n == field)
            .map(|(_, e)| e)
            .ok_or_else(|| format!("thumb-lower: missing field `{field}` for `{name}`"))?;
        let field_off = *fields
            .get(field)
            .ok_or_else(|| format!("thumb-lower: field `{field}` not in layout"))?;
        match (ty.canonical(), value) {
            (Typ::Named(inner), Expr::StructInit { .. }) if ctx.structs.contains_key(&inner) => {
                let prefix = format!("{field}.");
                let mut sub = HashMap::new();
                for (k, off) in fields.iter() {
                    if let Some(rest) = k.strip_prefix(&prefix) {
                        sub.insert(rest.to_string(), *off);
                    }
                }
                lower_struct_init_into(emitter, ctx, value, &sub, pending)?;
            }
            (Typ::Named(inner), _) if ctx.structs.contains_key(&inner) => {
                return Err(format!(
                    "thumb-lower: expected struct initializer for `{field}` (`{inner}`)"
                ));
            }
            _ => {
                lower_expr_into(emitter, ctx, value, R0, pending)?;
                emitter.emit_u16(thumb::str_sp(R0, field_off)?);
            }
        }
    }
    Ok(())
}

pub(crate) fn flatten_field_chain(base: &Expr, suffix: &str) -> Result<(String, String), String> {
    let mut parts = vec![suffix.to_string()];
    let mut cur = base;
    loop {
        match cur {
            Expr::Ident(local) => {
                parts.reverse();
                return Ok((local.clone(), parts.join(".")));
            }
            Expr::Field { base: inner, name } => {
                parts.push(name.clone());
                cur = inner;
            }
            _ => return Err("thumb-lower: unsupported field base".into()),
        }
    }
}

pub(crate) fn resolve_field_offset(
    ctx: &LowerCtx<'_>,
    base: &Expr,
    suffix: &str,
) -> Result<u32, String> {
    let (local, dotted) = flatten_field_chain(base, suffix)?;
    match ctx.locals.get(&local).cloned() {
        Some(LocalSlot::Struct { fields }) => fields
            .get(&dotted)
            .copied()
            .ok_or_else(|| format!("thumb-lower: unknown field `{dotted}` on `{local}`")),
        _ => Err(format!(
            "thumb-lower: `{local}` is not a struct in `{}`",
            ctx.fn_name
        )),
    }
}

pub(crate) fn lower_field_load(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    name: &str,
    dest: u8,
    _pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let off = resolve_field_offset(ctx, base, name)?;
    emitter.emit_u16(thumb::ldr_sp(R0, off)?);
    if dest != R0 {
        emitter.emit_u16(thumb::mov_low(dest, R0));
    }
    Ok(())
}

pub(crate) fn lower_field_assign(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    name: &str,
    value: &Expr,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let off = resolve_field_offset(ctx, base, name)?;
    lower_expr_into(emitter, ctx, value, R0, pending)?;
    emitter.emit_u16(thumb::str_sp(R0, off)?);
    Ok(())
}

pub(crate) fn emit_array_index_address(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: u32,
    elem_size: u32,
    len: usize,
    index: &Expr,
    pending: &mut Vec<PendingCall>,
) -> Result<(u32, u32), String> {
    lower_expr_into(emitter, ctx, index, R2, pending)?;

    // Bounds: index < 0 or index >= len
    let neg = emitter.len();
    emitter.emit_u32_thumb(thumb::b_cond_wide(COND_LT, 0));
    thumb::load_i32(emitter, R3, len as i32);
    emitter.emit_u16(thumb::cmp_reg(R2, R3));
    let oob = emitter.len();
    emitter.emit_u32_thumb(thumb::b_cond_wide(COND_GE, 0));

    // Address = sp + base + index * elem_size
    emitter.emit_u16(thumb::mov_sp(R1));
    thumb::load_i32(emitter, R3, base as i32);
    emitter.emit_u16(thumb::adds_reg(R1, R1, R3));
    thumb::load_i32(emitter, R3, elem_size as i32);
    emitter.emit_u16(thumb::muls(R2, R3));
    emitter.emit_u16(thumb::adds_reg(R1, R1, R2));

    Ok((neg, oob))
}

pub(crate) fn resolve_array_slot(
    ctx: &LowerCtx<'_>,
    base: &Expr,
) -> Result<(u32, u32, usize), String> {
    let Expr::Ident(name) = base else {
        return Err("thumb-lower: array index base must be a local".into());
    };
    let Some(LocalSlot::Array {
        base,
        elem_size,
        len,
    }) = ctx.locals.get(name).cloned()
    else {
        return Err(format!(
            "thumb-lower: `{name}` is not an array in `{}`",
            ctx.fn_name
        ));
    };
    Ok((base, elem_size, len))
}

pub(crate) fn lower_index_load(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    index: &Expr,
    dest: u8,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let (base, elem_size, len) = resolve_array_slot(ctx, base)?;
    let (neg, oob) = emit_array_index_address(emitter, ctx, base, elem_size, len, index, pending)?;

    emitter.emit_u16(thumb::ldr_imm(R0, R1, 0)?);

    let b_end = emitter.len();
    emitter.emit_u32_thumb(thumb::b_wide(0));
    let fail = emitter.len();
    thumb::load_i32(emitter, R0, 0);
    let end = emitter.len();
    patch_b(emitter, b_end, end)?;
    patch_b_cond(emitter, neg, fail)?;
    patch_b_cond(emitter, oob, fail)?;

    if dest != R0 {
        emitter.emit_u16(thumb::mov_low(dest, R0));
    }
    Ok(())
}

pub(crate) fn lower_index_assign(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    index: &Expr,
    value: &Expr,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let (base, elem_size, len) = resolve_array_slot(ctx, base)?;
    lower_expr_into(emitter, ctx, value, R0, pending)?;
    emitter.emit_u16(thumb::mov_low(R4, R0));
    let (neg, oob) = emit_array_index_address(emitter, ctx, base, elem_size, len, index, pending)?;

    emitter.emit_u16(thumb::str_imm(R4, R1, 0)?);

    let end = emitter.len();
    patch_b_cond(emitter, neg, end)?;
    patch_b_cond(emitter, oob, end)?;
    Ok(())
}

pub(crate) fn lower_struct_init(
    _emitter: &mut CodeEmitter,
    _ctx: &mut LowerCtx<'_>,
    _name: &str,
    _fields: &[(String, Expr)],
    _dest: u8,
    _pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    Err("thumb-lower: struct value not supported in expression position".into())
}

pub(crate) fn try_lower_mmio(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    name: &str,
    args: &[Expr],
    dest: u8,
    pending: &mut Vec<PendingCall>,
) -> Result<bool, String> {
    match name {
        "load8" => {
            if args.len() != 1 {
                return Err(format!(
                    "thumb-lower: `load8` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            lower_expr_into(emitter, ctx, &args[0], R1, pending)?;
            emitter.emit_u16(thumb::ldrb_imm(R0, R1, 0)?);
            if dest != R0 {
                emitter.emit_u16(thumb::mov_low(dest, R0));
            }
            Ok(true)
        }
        "load16" => {
            if args.len() != 1 {
                return Err(format!(
                    "thumb-lower: `load16` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            lower_expr_into(emitter, ctx, &args[0], R1, pending)?;
            emitter.emit_u16(thumb::ldrh_imm(R0, R1, 0)?);
            if dest != R0 {
                emitter.emit_u16(thumb::mov_low(dest, R0));
            }
            Ok(true)
        }
        "load32" => {
            if args.len() != 1 {
                return Err(format!(
                    "thumb-lower: `load32` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            lower_expr_into(emitter, ctx, &args[0], R1, pending)?;
            emitter.emit_u16(thumb::ldr_imm(R0, R1, 0)?);
            if dest != R0 {
                emitter.emit_u16(thumb::mov_low(dest, R0));
            }
            Ok(true)
        }
        "load64" => {
            if args.len() != 1 {
                return Err(format!(
                    "thumb-lower: `load64` requires 1 argument in `{}`",
                    ctx.fn_name
                ));
            }
            // Cortex-M is 32-bit; expose low word only (matches freestanding Int).
            lower_expr_into(emitter, ctx, &args[0], R1, pending)?;
            emitter.emit_u16(thumb::ldr_imm(R0, R1, 0)?);
            if dest != R0 {
                emitter.emit_u16(thumb::mov_low(dest, R0));
            }
            Ok(true)
        }
        "store8" => {
            if args.len() != 2 {
                return Err(format!(
                    "thumb-lower: `store8` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            // Hold address in callee-saved r4 so val evaluation keeps local SP offsets valid.
            lower_expr_into(emitter, ctx, &args[0], R0, pending)?;
            emitter.emit_u16(thumb::mov_low(R4, R0));
            lower_expr_into(emitter, ctx, &args[1], R0, pending)?;
            emitter.emit_u16(thumb::strb_imm(R0, R4, 0)?);
            Ok(true)
        }
        "store16" => {
            if args.len() != 2 {
                return Err(format!(
                    "thumb-lower: `store16` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            lower_expr_into(emitter, ctx, &args[0], R0, pending)?;
            emitter.emit_u16(thumb::mov_low(R4, R0));
            lower_expr_into(emitter, ctx, &args[1], R0, pending)?;
            emitter.emit_u16(thumb::strh_imm(R0, R4, 0)?);
            Ok(true)
        }
        "store32" => {
            if args.len() != 2 {
                return Err(format!(
                    "thumb-lower: `store32` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            lower_expr_into(emitter, ctx, &args[0], R0, pending)?;
            emitter.emit_u16(thumb::mov_low(R4, R0));
            lower_expr_into(emitter, ctx, &args[1], R0, pending)?;
            emitter.emit_u16(thumb::str_imm(R0, R4, 0)?);
            Ok(true)
        }
        "store64" => {
            if args.len() != 2 {
                return Err(format!(
                    "thumb-lower: `store64` requires 2 arguments in `{}`",
                    ctx.fn_name
                ));
            }
            lower_expr_into(emitter, ctx, &args[0], R0, pending)?;
            emitter.emit_u16(thumb::mov_low(R4, R0));
            lower_expr_into(emitter, ctx, &args[1], R0, pending)?;
            emitter.emit_u16(thumb::str_imm(R0, R4, 0)?);
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub(crate) fn lower_binary(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    dest: u8,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    if op == "&&" || op == "||" {
        return lower_short_circuit(emitter, ctx, op, lhs, rhs, dest, pending);
    }

    // Evaluate both operands into dedicated scratch slots (stable SP offsets)
    // instead of push/pop: pushing shifts SP, which misaligns the SP-relative
    // access to locals in the nested operand expressions. Each evaluation
    // depth gets its own pair so a nested binary operand (e.g. the `b * c`
    // in `a - b * c`) cannot clobber the outer operand's stashed value.
    let (scratch0, scratch1) = ctx.acquire_scratch_temps()?;
    lower_expr_into(emitter, ctx, lhs, R0, pending)?;
    emitter.emit_u16(thumb::str_sp(R0, scratch0)?);
    lower_expr_into(emitter, ctx, rhs, R0, pending)?;
    emitter.emit_u16(thumb::str_sp(R0, scratch1)?);
    emitter.emit_u16(thumb::ldr_sp(R1, scratch0)?);
    emitter.emit_u16(thumb::ldr_sp(R2, scratch1)?);
    ctx.release_scratch_temps();

    match op {
        "+" => {
            emitter.emit_u16(thumb::adds_reg(R0, R1, R2));
        }
        "-" => {
            emitter.emit_u16(thumb::subs_reg(R0, R1, R2));
        }
        "*" => {
            emitter.emit_u16(thumb::mov_low(R0, R1));
            emitter.emit_u16(thumb::muls(R0, R2));
        }
        "&" => {
            emitter.emit_u16(thumb::mov_low(R0, R1));
            emitter.emit_u16(thumb::ands(R0, R2));
        }
        "|" => {
            emitter.emit_u16(thumb::mov_low(R0, R1));
            emitter.emit_u16(thumb::orrs(R0, R2));
        }
        "^" => {
            emitter.emit_u16(thumb::mov_low(R0, R1));
            emitter.emit_u16(thumb::eors(R0, R2));
        }
        "<<" => {
            emitter.emit_u16(thumb::mov_low(R0, R1));
            emitter.emit_u32_thumb(thumb::lsls_reg(R0, R1, R2));
        }
        ">>" => {
            emitter.emit_u16(thumb::mov_low(R0, R1));
            emitter.emit_u32_thumb(thumb::asrs_reg(R0, R1, R2));
        }
        "/" => {
            emitter.emit_u32_thumb(thumb::sdiv(R0, R1, R2));
        }
        "%" => {
            // a % b = a - (a / b) * b
            emitter.emit_u32_thumb(thumb::sdiv(R0, R1, R2));
            emitter.emit_u16(thumb::mov_low(R0, R0));
            emitter.emit_u16(thumb::muls(R0, R2));
            emitter.emit_u16(thumb::subs_reg(R0, R1, R0));
        }
        "==" | "!=" | "<" | "<=" | ">" | ">=" => {
            emitter.emit_u16(thumb::cmp_reg(R1, R2));
            let cond = match op {
                "==" => COND_EQ,
                "!=" => COND_NE,
                "<" => COND_LT,
                "<=" => COND_LE,
                ">" => COND_GT,
                ">=" => COND_GE,
                _ => unreachable!(),
            };
            // cmp flags must not be clobbered before the conditional branch.
            // b<cond> true; movs r0,#0; b end; true: movs r0,#1; end:
            let b_true = emitter.len();
            emitter.emit_u32_thumb(thumb::b_cond_wide(cond, 0));
            emitter.emit_u16(thumb::movs_imm8(R0, 0));
            let b_end = emitter.len();
            emitter.emit_u32_thumb(thumb::b_wide(0));
            let true_site = emitter.len();
            patch_b_cond(emitter, b_true, true_site)?;
            emitter.emit_u16(thumb::movs_imm8(R0, 1));
            let end = emitter.len();
            patch_b(emitter, b_end, end)?;
        }
        other => {
            return Err(format!(
                "thumb-lower: unsupported binary `{other}` in `{}`",
                ctx.fn_name
            ));
        }
    }
    if dest != R0 {
        emitter.emit_u16(thumb::mov_low(dest, R0));
    }
    Ok(())
}

pub(crate) fn lower_short_circuit(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    op: &str,
    lhs: &Expr,
    rhs: &Expr,
    dest: u8,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    // lhs truth value in r1; if it decides the result, skip rhs evaluation.
    lower_expr_into(emitter, ctx, lhs, R1, pending)?;
    emitter.emit_u16(thumb::cmp_imm8(R1, 0));
    let deciding_cond = if op == "&&" { COND_EQ } else { COND_NE };
    let branch = emitter.len();
    emitter.emit_u32_thumb(thumb::b_cond_wide(deciding_cond, 0));

    lower_expr_into(emitter, ctx, rhs, R1, pending)?;
    emitter.emit_u16(thumb::cmp_imm8(R1, 0));
    let branch2 = emitter.len();
    emitter.emit_u32_thumb(thumb::b_cond_wide(deciding_cond, 0));

    // both operands evaluated: && is true, || is false
    let both_val = if op == "&&" { 1 } else { 0 };
    // short-circuit decided value: && is false, || is true
    let decided_val = if op == "&&" { 0 } else { 1 };

    thumb::load_i32(emitter, R0, both_val);
    let b_end = emitter.len();
    emitter.emit_u32_thumb(thumb::b_wide(0));

    let decided = emitter.len();
    patch_b_cond(emitter, branch, decided)?;
    patch_b_cond(emitter, branch2, decided)?;
    thumb::load_i32(emitter, R0, decided_val);

    let end = emitter.len();
    patch_b(emitter, b_end, end)?;

    if dest != R0 {
        emitter.emit_u16(thumb::mov_low(dest, R0));
    }
    Ok(())
}

pub(crate) fn patch_b(emitter: &mut CodeEmitter, site: u32, target: u32) -> Result<(), String> {
    let next = site as i32 + 4;
    let rel = (target as i32 - next) / 2;
    if !(-(1 << 20)..(1 << 20)).contains(&rel) {
        return Err(format!("thumb-lower: b range {rel}"));
    }
    emitter.patch_u32(site, thumb::b_wide(rel as i32));
    Ok(())
}

pub(crate) fn patch_b_cond(
    emitter: &mut CodeEmitter,
    site: u32,
    target: u32,
) -> Result<(), String> {
    // Wide branch (32-bit): next is the end of the instruction.
    let next = site as i32 + 4;
    let rel = (target as i32 - next) / 2;
    if !(-(1 << 20)..(1 << 20)).contains(&rel) {
        return Err(format!("thumb-lower: bcond range {rel}"));
    }
    // The cond field lives in bits 25-22 of the 32-bit instruction = bits 9-6
    // of the high halfword (which is stored first in memory).
    let old = u16::from_le_bytes([
        emitter.bytes[site as usize],
        emitter.bytes[site as usize + 1],
    ]);
    let cond = ((old >> 6) & 0xF) as u8;
    emitter.patch_u32(site, thumb::b_cond_wide(cond, rel as i32));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{Expr, Stmt, Typ};
    use crate::native_emit::thumb::CodeEmitter;
    use crate::native_emit::thumb_lower::ctx::FunctionInfo;
    use crate::native_emit::thumb_lower::stmt::lower_function;
    use std::collections::HashMap;

    #[test]
    fn nested_binary_operands_use_distinct_scratch_slots() {
        // `a - b * c`: the inner `b * c` is a binary operand of the outer
        // subtraction. Each depth must stash operands in its own scratch pair;
        // sharing one pair clobbers the outer lhs before the subtract runs.
        let func = FunctionInfo {
            name: "f".to_string(),
            params: vec![
                ("a".to_string(), Typ::Int),
                ("b".to_string(), Typ::Int),
                ("c".to_string(), Typ::Int),
            ],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: "-".to_string(),
                lhs: Box::new(Expr::Ident("a".to_string())),
                rhs: Box::new(Expr::Binary {
                    op: "*".to_string(),
                    lhs: Box::new(Expr::Ident("b".to_string())),
                    rhs: Box::new(Expr::Ident("c".to_string())),
                }),
            }))],
        };
        let mut functions = HashMap::new();
        functions.insert("f".to_string(), func.clone());
        let structs = HashMap::new();
        let mut emitter = CodeEmitter::new();
        let mut pending = Vec::new();
        lower_function(&mut emitter, &func, &functions, &structs, &mut pending)
            .expect("lower f");

        // str rt, [sp, #imm] (T1) halfword: 0x9000 | rt<<8 | imm8 where the
        // immediate encodes imm/4. R0 stores: 0x9000 | imm8.
        let str_imm_offsets: Vec<u32> = emitter
            .bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .filter(|hw| hw & 0xF000 == 0x9000 && (hw >> 8) & 0x7 == 0)
            .map(|hw| ((hw & 0xFF) as u32) * 4)
            .collect();

        // Params sit at frame offsets 0/4/8; the first scratch depth gets
        // offsets 12/16, the nested depth 20/24. The operand stashes must be
        // a: 12 (outer lhs), b: 20 (inner lhs), c: 24 (inner rhs), and the
        // product: 16 (outer rhs) — four distinct slots, in that order.
        let want = [12, 20, 24, 16];
        let mut hits = Vec::new();
        for off in &want {
            match str_imm_offsets.iter().position(|s| s == off) {
                Some(_) => hits.push(*off),
                None => panic!(
                    "expected scratch store to [sp, #{off}] not emitted; stores: {str_imm_offsets:?}"
                ),
            }
        }
        // Ordered subsequence check: strip to want-matching stores in emit order.
        let mut idx = 0;
        for s in &str_imm_offsets {
            if idx < want.len() && s == &want[idx] {
                idx += 1;
            }
        }
        assert_eq!(idx, want.len(), "scratch stores out of order: {str_imm_offsets:?}");
        assert_eq!(hits, want);
    }
}
