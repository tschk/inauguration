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
        Expr::UnitLit => {
            thumb::load_i32(emitter, dest, 0);
            Ok(())
        }
        Expr::MatchValue { .. } => {
            Err("thumb-lower: match values require owned ARM64 or x86 lowering".into())
        }
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
            if op.starts_with("int-cast-") {
                let Some((width, _)) = crate::core_ir::integer_cast_spec(op) else {
                    return Err(format!("thumb-lower: unsupported integer cast `{op}`"));
                };
                if width > 32 {
                    return Err(format!(
                        "thumb-lower: integer cast `{op}` exceeds the 32-bit word"
                    ));
                }
                // The shared expansion contains the operand exactly once and
                // uses ordinary word operators already lowered below.
                let value = crate::core_ir::integer_cast_expr((**expr).clone(), op).unwrap();
                return lower_expr_into(emitter, ctx, &value, dest, pending);
            }
            if matches!(op.as_str(), "-" | "neg") {
                let operand = crate::typecheck::TypeChecker::new().checked_expr_type(
                    expr,
                    &ctx.facts,
                    &ctx.local_types,
                );
                if crate::core_ir::numeric_negation_domain(operand.as_ref(), false)
                    != crate::core_ir::NumericNegationDomain::Numeric
                {
                    return Err("thumb-lower: numeric negation requires a resolved Int, Bool, or Float operand".into());
                }
            }
            lower_expr_into(emitter, ctx, expr, R0, pending)?;
            match op.as_str() {
                "-" | "neg" => {
                    emitter.emit_u16(thumb::rsbs0(R0, R0));
                    if dest != R0 {
                        emitter.emit_u16(thumb::mov_low(dest, R0));
                    }
                    Ok(())
                }
                "~" | "bitnot" => {
                    emitter.emit_u16(thumb::mvns(dest, R0));
                    Ok(())
                }
                "!" | "not" => {
                    // !x → x == 0
                    emitter.emit_u16(thumb::cmp_imm8(R0, 0));
                    // MOVW preserves the comparison flags consumed by BNE.
                    emitter.emit_u32_thumb(thumb::movw(R0, 0));
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
            if name == crate::core_ir::COMPILER_UNREACHABLE {
                if !args.is_empty() || ctx.functions.contains_key(name) {
                    return Err("thumb-lower: private unreachable requires zero args and cannot be declared".into());
                }
                emitter.emit_u16(0xDE00); // UDF #0, independent of a hosted runtime.
                return Ok(());
            }
            if crate::core_ir::compiler_print_callee(name).is_some() {
                return Err("thumb-lower: private print requires a hosted runtime".into());
            }
            // A lexical callee binding cannot fall through to a global intrinsic.
            if ctx.local_types.contains_key(name) || ctx.locals.contains_key(name) {
                return Err(
                    "thumb-lower: local indirect callee requires a function signature".into(),
                );
            }
            if let Some(function) = ctx.functions.get(name) {
                crate::native_emit::validate_word32_scalar_signature(
                    &function.params,
                    &function.ret,
                )?;
            }
            // Defined functions override intrinsic spellings; extern MMIO wrappers do not.
            if ctx
                .functions
                .get(name)
                .is_some_and(|function| !function.body.is_empty())
            {
                return lower_call_args(emitter, ctx, name, args, dest, pending);
            }
            if crate::core_ir_verifier::is_native_invoke(name) && !ctx.functions.contains_key(name)
            {
                return lower_native_invoke(emitter, ctx, name, args, dest, pending);
            }
            if matches!(name.as_str(), "invoke" | "invoke1" | "invoke2")
                && !ctx.functions.contains_key(name)
            {
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
        Expr::Conditional { .. } | Expr::Block { .. } => Err(
            "thumb-lower: conditional and block values require structured value lowering".into(),
        ),
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

    // 3. Reserve one aligned outgoing block. Source temps were staged before
    // moving SP, so their offsets include this block; arg 4 is at caller SP.
    let stack_bytes = (((n.saturating_sub(4) * 4) + 7) & !7) as u32;
    if stack_bytes != 0 {
        emitter.emit_u16(thumb::sub_sp_imm(stack_bytes)?);
        for i in 4..n {
            emitter.emit_u16(thumb::ldr_sp(
                R4,
                ctx.call_arg_temps[base + i] + stack_bytes,
            )?);
            emitter.emit_u16(thumb::str_sp(R4, ((i - 4) * 4) as u32)?);
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
    if stack_bytes != 0 {
        emitter.emit_u16(thumb::add_sp_imm(stack_bytes)?);
    }

    ctx.release_call_arg_temps(n)?;

    if dest != REG_RET {
        emitter.emit_u16(thumb::mov_low(dest, REG_RET));
    }
    Ok(())
}

fn lower_native_invoke(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    name: &str,
    args: &[Expr],
    dest: u8,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    let expected = match name {
        "invoke-native" | "invoke_native" => 1,
        "invoke-native1" | "invoke_native1" => 2,
        _ => 3,
    };
    if args.len() != expected {
        return Err(format!(
            "thumb-lower: `{name}` requires {expected} Int arguments"
        ));
    }
    if let Expr::Ident(target) = &args[0]
        && !ctx.local_types.contains_key(target)
        && let Some(function) = ctx.functions.get(target)
    {
        if let Some(error) =
            crate::core_ir_verifier::invoke_signature_error(name, &function.params, &function.ret)
        {
            return Err(format!("thumb-lower: {error}"));
        }
        let throwing =
            crate::core_effects::fallible_functions(ctx.functions.values().map(|function| {
                (
                    function.name.as_str(),
                    function.body.as_slice(),
                    &function.ret,
                    function.params.as_slice(),
                )
            }));
        if throwing.contains(target) {
            return Err(format!(
                "thumb-lower: raw native target `{target}` may throw"
            ));
        }
    }
    let checker = crate::typecheck::TypeChecker::new();
    if args.iter().any(|arg| {
        checker
            .expr_type(arg, &ctx.facts, &ctx.local_types)
            .map(|typ| typ.canonical())
            != Some(Typ::Int)
    }) {
        return Err(format!("thumb-lower: `{name}` requires Int arguments"));
    }
    let base = ctx.acquire_call_arg_temps(args.len())?;
    for (index, arg) in args.iter().enumerate() {
        lower_expr_into(emitter, ctx, arg, R0, pending)?;
        emitter.emit_u16(thumb::str_sp(R0, ctx.call_arg_temps[base + index])?);
    }
    // r3 is outside the raw ABI's two argument registers. A null address
    // traps locally; this ABI never dereferences an error-page carrier.
    emitter.emit_u16(thumb::ldr_sp(R3, ctx.call_arg_temps[base])?);
    emitter.emit_u16(thumb::cmp_imm8(R3, 0));
    emitter.emit_u16(thumb::b_cond_rel8(COND_NE, 0));
    emitter.emit_u16(0xDE00); // UDF #0
    for index in 1..args.len() {
        emitter.emit_u16(thumb::ldr_sp(
            (index - 1) as u8,
            ctx.call_arg_temps[base + index],
        )?);
    }
    emitter.emit_u16(thumb::blx_reg(R3));
    ctx.release_call_arg_temps(args.len())?;
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
    ctx.release_call_arg_temps(n)?;
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

    // Materializing an index with MOVW/MOVT need not set flags.
    emitter.emit_u16(thumb::cmp_imm8(R2, 0));
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
    emit_scalar_failure_return(emitter, ctx)?;
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

    let branch = emitter.len();
    emitter.emit_u32_thumb(thumb::b_wide(0));
    let fail = emitter.len();
    emit_scalar_failure_return(emitter, ctx)?;
    let end = emitter.len();
    patch_b(emitter, branch, end)?;
    patch_b_cond(emitter, neg, fail)?;
    patch_b_cond(emitter, oob, fail)?;
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
    if matches!(
        name,
        "load8" | "load16" | "load32" | "load64" | "store8" | "store16" | "store32" | "store64"
    ) {
        let checker = crate::typecheck::TypeChecker::new();
        crate::native_emit::stdlib_wrappers::validate_intrinsic_call(
            name,
            ctx.functions
                .get(name)
                .map(|function| (function.params.as_slice(), &function.ret)),
            args.iter()
                .map(|argument| checker.expr_type(argument, &ctx.facts, &ctx.local_types)),
        )
        .map_err(|error| {
            format!(
                "thumb-lower: incompatible MMIO {} `{name}`",
                error.category()
            )
        })?;
    }
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

// Bounds and arithmetic failures leave the current scalar function without
// exposing a normal expression result or writing a language error carrier.
fn emit_scalar_failure_return(emitter: &mut CodeEmitter, ctx: &LowerCtx<'_>) -> Result<(), String> {
    emitter.emit_u16(thumb::movs_imm8(R0, 1));
    thumb::emit_epilogue(emitter, ctx.frame_reserve())
}

// Match the owned scalar contract: a zero divisor returns 1 from this
// function, rather than producing a normal expression value or raising an
// implicit language exception. Operands have already been evaluated once.
fn lower_checked_div_or_mod(
    emitter: &mut CodeEmitter,
    ctx: &LowerCtx<'_>,
    modulo: bool,
    unsigned: bool,
) -> Result<(), String> {
    emitter.emit_u16(thumb::cmp_imm8(R2, 0));
    let nonzero = emitter.len();
    emitter.emit_u32_thumb(thumb::b_cond_wide(COND_NE, 0));
    emit_scalar_failure_return(emitter, ctx)?;
    let divide = emitter.len();
    patch_b_cond(emitter, nonzero, divide)?;
    emitter.emit_u32_thumb(if unsigned {
        thumb::udiv(R0, R1, R2)
    } else {
        thumb::sdiv(R0, R1, R2)
    });
    if modulo {
        // Subtract the quotient product using native word arithmetic.
        emitter.emit_u16(thumb::muls(R0, R2));
        emitter.emit_u16(thumb::subs_reg(R0, R1, R0));
    }
    Ok(())
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
        "^" | "xor" => {
            emitter.emit_u16(thumb::mov_low(R0, R1));
            emitter.emit_u16(thumb::eors(R0, R2));
        }
        "<<" | ">>" | "lshr" => {
            // Match native target-word shifts; Cortex-M otherwise uses the
            // low byte and saturates counts at least 32 instead of wrapping.
            emitter.emit_u16(thumb::movs_imm8(R3, 31));
            emitter.emit_u16(thumb::ands(R2, R3));
            emitter.emit_u32_thumb(match op {
                "<<" => thumb::lsls_reg(R0, R1, R2),
                ">>" => thumb::asrs_reg(R0, R1, R2),
                _ => thumb::lsrs_reg(R0, R1, R2),
            });
        }
        "/" | "%" | "udiv" | "urem" => {
            lower_checked_div_or_mod(
                emitter,
                ctx,
                matches!(op, "%" | "urem"),
                matches!(op, "udiv" | "urem"),
            )?;
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
        lower_function(&mut emitter, &func, &functions, &structs, &mut pending).expect("lower f");

        // str rt, [sp, #imm] (T1) halfword: 0x9000 | rt<<8 | imm8 where the
        // immediate encodes imm/4. R0 stores: 0x9000 | imm8.
        let str_imm_offsets: Vec<u32> = emitter
            .bytes
            .as_chunks::<2>()
            .0
            .iter()
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
        assert_eq!(
            idx,
            want.len(),
            "scratch stores out of order: {str_imm_offsets:?}"
        );
        assert_eq!(hits, want);
    }
}

#[cfg(test)]
mod raw_native_tests {
    use super::super::ctx::FunctionInfo;
    use super::super::stmt::lower_function;
    use super::*;
    use crate::core_ir::Stmt;

    fn compile(params: Vec<(String, Typ)>, args: Vec<Expr>, name: &str) -> Result<Vec<u8>, String> {
        let function = FunctionInfo {
            name: "entry".into(),
            params,
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::Call {
                callee: Box::new(Expr::Ident(name.into())),
                args,
            }))],
        };
        let functions = HashMap::from([("entry".into(), function.clone())]);
        let structs = HashMap::new();
        let mut emitter = CodeEmitter::new();
        lower_function(
            &mut emitter,
            &function,
            &functions,
            &structs,
            &mut Vec::new(),
        )?;
        Ok(emitter.bytes)
    }

    #[test]
    fn raw_native_calls_stage_integer_arguments_and_emit_null_udf() {
        for (name, count) in [
            ("invoke-native", 1),
            ("invoke_native1", 2),
            ("invoke-native2", 3),
        ] {
            let args = (0..count).map(|index| Expr::IntLit(42 + index)).collect();
            let code = compile(vec![], args, name).unwrap();
            let words: Vec<_> = code
                .as_chunks::<2>()
                .0
                .iter()
                .map(|word| u16::from_le_bytes([word[0], word[1]]))
                .collect();
            assert!(words.contains(&0xDE00), "missing dynamic null trap: {name}");
            assert!(
                words.contains(&thumb::blx_reg(R3)),
                "missing raw call: {name}"
            );
        }
    }

    #[test]
    fn raw_native_calls_refuse_bool_locals_and_wrong_arity() {
        assert!(
            compile(
                vec![("value".into(), Typ::Bool)],
                vec![Expr::IntLit(1), Expr::Ident("value".into())],
                "invoke-native1"
            )
            .unwrap_err()
            .contains("Int arguments")
        );
        assert!(
            compile(vec![], vec![Expr::IntLit(1)], "invoke-native2")
                .unwrap_err()
                .contains("requires 3")
        );
    }
}

#[cfg(test)]
mod outgoing_stack_argument_tests {
    use super::*;

    #[test]
    fn five_six_and_seven_argument_calls_keep_aligned_ordered_stack_slots() {
        for count in [5usize, 6, 7] {
            let functions = HashMap::new();
            let structs = HashMap::new();
            let mut ctx = LowerCtx::new("caller", &[], &functions, &structs);
            ctx.call_arg_temps = (0..count).map(|_| ctx.alloc_slot()).collect();
            let args = (1..=count)
                .map(|n| Expr::IntLit(n as i64))
                .collect::<Vec<_>>();
            let mut emitter = CodeEmitter::new();
            let mut pending = Vec::new();
            lower_call_args(&mut emitter, &mut ctx, "take", &args, R0, &mut pending).unwrap();
            let words = emitter
                .bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|w| u16::from_le_bytes(*w))
                .collect::<Vec<_>>();
            let bytes = if count <= 6 { 8 } else { 16 };
            assert_eq!(bytes % 8, 0);
            let start = words
                .iter()
                .position(|word| *word == thumb::sub_sp_imm(bytes).unwrap())
                .unwrap();
            for index in 4..count {
                assert_eq!(
                    words[start + 1 + (index - 4) * 2],
                    thumb::ldr_sp(R4, ctx.call_arg_temps[index] + bytes).unwrap()
                );
                assert_eq!(
                    words[start + 2 + (index - 4) * 2],
                    thumb::str_sp(R4, ((index - 4) * 4) as u32).unwrap()
                );
            }
            assert_eq!(words.last(), Some(&thumb::add_sp_imm(bytes).unwrap()));
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].target, "take");
        }
    }
}

#[cfg(test)]
mod dynamic_bitnot_tests {
    use super::*;

    #[test]
    fn logical_not_preserves_comparison_flags_and_requested_register() {
        for op in ["!", "not"] {
            let functions = HashMap::new();
            let structs = HashMap::new();
            let mut ctx = LowerCtx::new("logical_not", &[], &functions, &structs);
            let offset = ctx.alloc_slot();
            ctx.locals.insert("value".into(), LocalSlot::Scalar(offset));
            let mut emitter = CodeEmitter::new();
            let mut pending = Vec::new();
            let expression = Expr::Unary {
                op: op.into(),
                expr: Box::new(Expr::Ident("value".into())),
            };
            lower_expr_into(&mut emitter, &mut ctx, &expression, R4, &mut pending).unwrap();
            let mut prefix = CodeEmitter::new();
            prefix.emit_u16(thumb::ldr_sp(R0, offset).unwrap());
            prefix.emit_u16(thumb::cmp_imm8(R0, 0));
            prefix.emit_u32_thumb(thumb::movw(R0, 0));
            assert!(emitter.bytes.starts_with(&prefix.bytes));
            assert_eq!(emitter.bytes.len(), 16);
            assert_eq!(&emitter.bytes[14..], &thumb::mov_low(R4, R0).to_le_bytes());
            assert!(pending.is_empty());
        }
    }

    #[test]
    fn bitnot_aliases_read_operand_once_and_write_requested_register() {
        for op in ["~", "bitnot"] {
            let functions = HashMap::new();
            let structs = HashMap::new();
            let mut ctx = LowerCtx::new("complement", &[], &functions, &structs);
            let offset = ctx.alloc_slot();
            ctx.locals.insert("value".into(), LocalSlot::Scalar(offset));
            let mut emitter = CodeEmitter::new();
            let mut pending = Vec::new();
            let expression = Expr::Unary {
                op: op.into(),
                expr: Box::new(Expr::Ident("value".into())),
            };
            lower_expr_into(&mut emitter, &mut ctx, &expression, R4, &mut pending).unwrap();
            let words = emitter
                .bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|word| u16::from_le_bytes(*word))
                .collect::<Vec<_>>();
            assert_eq!(
                words,
                vec![thumb::ldr_sp(R0, offset).unwrap(), thumb::mvns(R4, R0)]
            );
            assert!(pending.is_empty());
        }
    }
}

#[cfg(test)]
mod checked_division_tests {
    use super::*;

    #[test]
    fn signed_and_unsigned_division_guard_zero_and_restore_current_frame() {
        let functions = HashMap::new();
        let structs = HashMap::new();
        let mut ctx = LowerCtx::new("divide", &[], &functions, &structs);
        ctx.frame_size = 32;
        for (modulo, unsigned) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut emitter = CodeEmitter::new();
            lower_checked_div_or_mod(&mut emitter, &ctx, modulo, unsigned).unwrap();
            let words = emitter
                .bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|word| u16::from_le_bytes(*word))
                .collect::<Vec<_>>();
            assert_eq!(words[0], thumb::cmp_imm8(R2, 0));
            // The nonzero branch skips the complete failure return, landing
            // at the divide; the zero path restores this function's frame first.
            let mut encoded = CodeEmitter::new();
            encoded.emit_u32_thumb(thumb::b_cond_wide(COND_NE, 3));
            assert_eq!(&emitter.bytes[2..6], encoded.bytes.as_slice());
            assert_eq!(words[3], thumb::movs_imm8(R0, 1));
            assert_eq!(words[4], thumb::add_sp_imm(ctx.frame_reserve()).unwrap());
            assert_eq!(words[5], thumb::pop(0b1111_0000, true));
            let mut divide = CodeEmitter::new();
            divide.emit_u32_thumb(if unsigned {
                thumb::udiv(R0, R1, R2)
            } else {
                thumb::sdiv(R0, R1, R2)
            });
            assert_eq!(&emitter.bytes[12..16], divide.bytes.as_slice());
            if modulo {
                assert_eq!(words[8], thumb::muls(R0, R2));
                assert_eq!(words[9], thumb::subs_reg(R0, R1, R0));
            }
            assert_eq!(emitter.bytes.len(), if modulo { 20 } else { 16 });
        }
    }

    #[test]
    fn target_word_shifts_mask_counts_after_staging_operands() {
        let functions = HashMap::new();
        let structs = HashMap::new();
        for (op, instruction) in [
            ("<<", thumb::lsls_reg(R0, R1, R2)),
            (">>", thumb::asrs_reg(R0, R1, R2)),
            ("lshr", thumb::lsrs_reg(R0, R1, R2)),
        ] {
            for dest in [R0, R4] {
                let mut ctx = LowerCtx::new("shift", &[], &functions, &structs);
                ctx.scratch_temps = vec![0, 4];
                let mut emitter = CodeEmitter::new();
                let mut pending = Vec::new();
                lower_binary(
                    &mut emitter,
                    &mut ctx,
                    op,
                    &Expr::IntLit(-84),
                    &Expr::IntLit(33),
                    dest,
                    &mut pending,
                )
                .unwrap();
                let mut shift = CodeEmitter::new();
                shift.emit_u16(thumb::ldr_sp(R1, 0).unwrap());
                shift.emit_u16(thumb::ldr_sp(R2, 4).unwrap());
                shift.emit_u16(thumb::movs_imm8(R3, 31));
                shift.emit_u16(thumb::ands(R2, R3));
                shift.emit_u32_thumb(instruction);
                if dest != R0 {
                    shift.emit_u16(thumb::mov_low(dest, R0));
                }
                assert!(emitter.bytes.ends_with(&shift.bytes), "{op} r{dest}");
                assert_eq!(ctx.scratch_depth, 0);
                assert!(pending.is_empty());
            }
        }
    }
}

#[cfg(test)]
mod checked_index_branch_tests {
    use super::*;

    fn words(emitter: &CodeEmitter) -> Vec<u16> {
        emitter
            .bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|word| u16::from_le_bytes(*word))
            .collect()
    }

    // These small test functions use forward offsets below 4096 bytes. Decode
    // the actual instruction displacement independently of the patch helpers.
    fn forward_target(words: &[u16], site: usize, conditional: bool) -> usize {
        let high = words[site / 2];
        let low = words[site / 2 + 1];
        if conditional {
            assert_eq!(high & 0xfc3f, 0xf000);
            assert_eq!(low & 0xf800, 0x8000);
        } else {
            assert_eq!(high, 0xf000);
            assert_eq!(low & 0xf800, 0xb800);
        }
        site + 4 + usize::from(low & 0x07ff) * 2
    }

    fn assert_bounds_return_paths(emitter: &CodeEmitter, frame: u32) {
        let words = words(emitter);
        let negative_cmp = words
            .iter()
            .position(|word| *word == thumb::cmp_imm8(R2, 0))
            .unwrap();
        let upper_cmp = words
            .iter()
            .position(|word| *word == thumb::cmp_reg(R2, R3))
            .unwrap();
        let failure = words
            .windows(3)
            .position(|sequence| {
                sequence
                    == [
                        thumb::movs_imm8(R0, 1),
                        thumb::add_sp_imm(frame).unwrap(),
                        thumb::pop(0b1111_0000, true),
                    ]
            })
            .unwrap()
            * 2;
        for (comparison, condition) in [(negative_cmp, COND_LT), (upper_cmp, COND_GE)] {
            let branch = (comparison + 1) * 2;
            assert_eq!((words[branch / 2] >> 6) & 0xf, u16::from(condition));
            assert_eq!(forward_target(&words, branch, true), failure);
        }
        // A successful access skips the complete failure epilogue.
        assert_eq!(forward_target(&words, failure - 4, false), failure + 6);
    }

    #[test]
    fn checked_reads_compare_fresh_index_flags_and_only_move_success_to_destination() {
        for index in [
            Expr::IntLit(-1),
            Expr::IntLit(0xffffffff),
            Expr::IntLit(0),
            Expr::IntLit(1),
            Expr::Ident("index".into()),
        ] {
            let functions = HashMap::new();
            let structs = HashMap::new();
            let mut ctx = LowerCtx::new("read", &[], &functions, &structs);
            ctx.frame_size = 32;
            ctx.locals.insert(
                "values".into(),
                LocalSlot::Array {
                    base: 8,
                    elem_size: 4,
                    len: 1,
                },
            );
            ctx.locals.insert("index".into(), LocalSlot::Scalar(4));
            let mut emitter = CodeEmitter::new();
            let mut pending = Vec::new();
            lower_index_load(
                &mut emitter,
                &mut ctx,
                &Expr::Ident("values".into()),
                &index,
                R4,
                &mut pending,
            )
            .unwrap();
            assert_bounds_return_paths(&emitter, ctx.frame_reserve());
            let words = words(&emitter);
            assert_eq!(words.last(), Some(&thumb::mov_low(R4, R0)));
            assert!(words.contains(&thumb::ldr_imm(R0, R1, 0).unwrap()));
            assert!(pending.is_empty());
        }
    }

    #[test]
    fn checked_stores_evaluate_value_and_index_calls_once_in_order_before_bounds_return() {
        let functions = HashMap::new();
        let structs = HashMap::new();
        let mut ctx = LowerCtx::new("write", &[], &functions, &structs);
        ctx.frame_size = 32;
        ctx.locals.insert(
            "values".into(),
            LocalSlot::Array {
                base: 8,
                elem_size: 4,
                len: 1,
            },
        );
        let mut emitter = CodeEmitter::new();
        let mut pending = Vec::new();
        let call = |name: &str| Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args: vec![],
        };
        lower_index_assign(
            &mut emitter,
            &mut ctx,
            &Expr::Ident("values".into()),
            &call("index_once"),
            &call("value_once"),
            &mut pending,
        )
        .unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|call| call.target.as_str())
                .collect::<Vec<_>>(),
            ["value_once", "index_once"]
        );
        assert!(pending[0].site < pending[1].site);
        assert_bounds_return_paths(&emitter, ctx.frame_reserve());
        assert!(words(&emitter).contains(&thumb::str_imm(R4, R1, 0).unwrap()));
        assert_eq!(ctx.call_arg_used, 0);
    }
}

#[cfg(test)]
mod declared_call_priority_tests {
    use super::super::{ctx::FunctionInfo, stmt::lower_function};
    use super::*;
    use crate::core_ir::Stmt;

    #[test]
    fn declared_builtin_names_emit_calls_including_externs() {
        for name in [
            "load8",
            "load16",
            "load32",
            "load64",
            "store8",
            "store16",
            "store32",
            "store64",
            "invoke",
            "invoke1",
            "invoke2",
            "invoke-native",
            "invoke-native1",
            "invoke-native2",
            "invoke_native",
            "invoke_native1",
            "invoke_native2",
        ] {
            for external in [false, true] {
                if external && (name.starts_with("load") || name.starts_with("store")) {
                    continue;
                }
                let declared = FunctionInfo {
                    name: name.into(),
                    params: vec![("value".into(), Typ::Int)],
                    ret: Typ::Int,
                    body: if external {
                        vec![]
                    } else {
                        vec![Stmt::Return(Some(Expr::IntLit(42)))]
                    },
                };
                let entry = FunctionInfo {
                    name: "entry".into(),
                    params: vec![],
                    ret: Typ::Int,
                    body: vec![Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident(name.into())),
                        args: vec![Expr::IntLit(9)],
                    }))],
                };
                let functions =
                    HashMap::from([(name.into(), declared), ("entry".into(), entry.clone())]);
                let mut emitter = CodeEmitter::new();
                let mut pending = Vec::new();
                lower_function(
                    &mut emitter,
                    &entry,
                    &functions,
                    &HashMap::new(),
                    &mut pending,
                )
                .unwrap();
                assert_eq!(pending.len(), 1, "{name}");
                assert_eq!(pending[0].target, name);
                assert_eq!(pending[0].is_extern, external);
                assert_eq!(pending[0].kind, RelocKind::Call);
            }
        }
    }

    #[test]
    fn bodyless_mmio_declaration_keeps_intrinsic_lowering() {
        let function = FunctionInfo {
            name: "load32".into(),
            params: vec![("address".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![],
        };
        let functions = HashMap::from([("load32".into(), function)]);
        let structs = HashMap::new();
        let mut ctx = LowerCtx::new("entry", &[], &functions, &structs);
        let mut emitter = CodeEmitter::new();
        let mut pending = Vec::new();
        lower_expr_into(
            &mut emitter,
            &mut ctx,
            &Expr::Call {
                callee: Box::new(Expr::Ident("load32".into())),
                args: vec![Expr::IntLit(0x38001000)],
            },
            R0,
            &mut pending,
        )
        .unwrap();
        assert!(pending.is_empty());
        let ldr = thumb::ldr_imm(R0, R1, 0).unwrap().to_le_bytes();
        assert!(emitter.bytes.ends_with(&ldr));
    }

    #[test]
    fn local_builtin_names_cannot_fall_through_to_intrinsics() {
        for name in ["load32", "invoke", "invoke-native"] {
            let function = FunctionInfo {
                name: "entry".into(),
                params: vec![(name.into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident(name.into())),
                    args: vec![Expr::IntLit(9)],
                }))],
            };
            let functions = HashMap::from([("entry".into(), function.clone())]);
            let error = lower_function(
                &mut CodeEmitter::new(),
                &function,
                &functions,
                &HashMap::new(),
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(error.contains("local indirect callee"), "{error}");
        }
    }
}

#[cfg(test)]
mod selected_mmio_contract_tests {
    fn lower(source: &str) -> Result<(), String> {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        super::super::lower_module(&module, "main").map(|_| ())
    }

    #[test]
    fn raw_lowering_checks_selected_mmio_declarations_and_actual_arguments() {
        for name in [
            "load8", "load16", "load32", "load64", "store8", "store16", "store32", "store64",
        ] {
            let store = name.starts_with("store");
            let signature = if store {
                "address:Int,value:Int"
            } else {
                "address:Int"
            };
            let ret = if store { "void" } else { "Int" };
            let arguments = if store { "0,42" } else { "0" };
            let source = format!(
                "extern asm fn {name}({signature})->{ret}\nfn main()->Int{{{name}({arguments});return 42;}}"
            );
            assert!(lower(&source).is_ok(), "canonical {name}");

            let bad_signature = if store {
                "address:Int,value:Bool"
            } else {
                "address:Bool"
            };
            let bad_arguments = if store { "0,false" } else { "false" };
            for source in [
                format!(
                    "extern asm fn {name}({bad_signature})->{ret}\nfn main()->Int{{{name}({bad_arguments});return 42;}}"
                ),
                format!(
                    "extern asm fn {name}({signature})->Bool\nfn main()->Int{{{name}({arguments});return 42;}}"
                ),
            ] {
                let error = lower(&source).unwrap_err();
                assert!(
                    error.contains("incompatible MMIO declaration"),
                    "{name}: {error}"
                );
            }
            for arguments in [
                bad_arguments,
                if store { "0,missing" } else { "missing" },
                "",
                if store { "0,42,7" } else { "0,42" },
            ] {
                let source = format!("fn main()->Int{{{name}({arguments});return 42;}}");
                let error = lower(&source).unwrap_err();
                assert!(
                    error.contains("incompatible MMIO arguments"),
                    "{name}: {error}"
                );
            }
        }
    }

    #[test]
    fn defined_mmio_names_keep_their_ordinary_function_signature() {
        for name in [
            "load8", "load16", "load32", "load64", "store8", "store16", "store32", "store64",
        ] {
            // This is a real definition, not a selected memory intrinsic. Its
            // Bool parameter and Int return must retain their declared meaning.
            let source = format!(
                "fn {name}(value:Bool)->Int{{if value{{return 1;}}return 42;}}\nfn main()->Int{{return {name}(false);}}"
            );
            assert!(lower(&source).is_ok(), "defined override {name}");
        }
    }
}

#[cfg(test)]
mod integer_cast_tests {
    use super::super::ctx::FunctionInfo;
    use super::*;
    use crate::core_ir::Stmt;

    #[test]
    fn scalar_integer_casts_reuse_shared_expansion_and_evaluate_call_once() {
        let producer = FunctionInfo {
            name: "producer".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(298)))],
        };
        let functions = HashMap::from([("producer".into(), producer)]);
        let structs = HashMap::new();
        for op in [
            "int-cast-i8",
            "int-cast-u8",
            "int-cast-i16",
            "int-cast-u16",
            "int-cast-i32",
            "int-cast-u32",
        ] {
            let value = Expr::Call {
                callee: Box::new(Expr::Ident("producer".into())),
                args: vec![],
            };
            let mut emissions = Vec::new();
            for expression in [
                Expr::Unary {
                    op: op.into(),
                    expr: Box::new(value.clone()),
                },
                crate::core_ir::integer_cast_expr(value, op).unwrap(),
            ] {
                let mut ctx = LowerCtx::new("entry", &[], &functions, &structs);
                for _ in 0..16 {
                    let slot = ctx.alloc_slot();
                    ctx.scratch_temps.push(slot);
                }
                let mut emitter = CodeEmitter::new();
                let mut pending = Vec::new();
                lower_expr_into(&mut emitter, &mut ctx, &expression, R0, &mut pending).unwrap();
                assert_eq!(pending.len(), 1, "{op}");
                assert_eq!(pending[0].target, "producer");
                assert_eq!(ctx.scratch_depth, 0);
                assert_eq!(ctx.call_arg_used, 0);
                emissions.push(emitter.bytes);
            }
            assert_eq!(emissions[0], emissions[1], "{op}");
        }
    }

    #[test]
    fn unknown_and_wide_casts_refuse_before_evaluating_operand() {
        let functions = HashMap::new();
        let structs = HashMap::new();
        for op in [
            "int-cast-u7",
            "int-cast-i64",
            "int-cast-u64",
            "int-cast-isize",
            "int-cast-usize",
        ] {
            let mut ctx = LowerCtx::new("entry", &[], &functions, &structs);
            let mut emitter = CodeEmitter::new();
            let mut pending = Vec::new();
            let expression = Expr::Unary {
                op: op.into(),
                expr: Box::new(Expr::IntLit(42)),
            };
            assert!(
                lower_expr_into(&mut emitter, &mut ctx, &expression, R0, &mut pending).is_err()
            );
            assert!(emitter.bytes.is_empty());
            assert!(pending.is_empty());
        }
    }
    #[test]
    fn word32_float_external_calls_refuse_before_argument_staging() {
        for source in [
            "extern asm fn read_float()->Float\nfn main()->Int{read_float();return 42;}",
            "extern asm fn read_float()->Float\nextern asm fn consume_float(value:Float)->Int\nfn main()->Int{return consume_float(read_float());}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            let error = super::super::lower_module(&module, "main").err().unwrap();
            assert!(
                error.contains("Float requires 64-bit mode"),
                "{source}: {error}"
            );
        }
        for source in [
            "fn main(value:Int)->Int{return value;}",
            "fn main(value:Bool)->Bool{return value;}",
            "extern asm fn read_int()->Int\nfn main()->Int{return read_int();}",
            "extern asm fn read_float()->Float\nfn main()->Int{return 42;}",
            "extern asm fn read_float()->Float\nfn main()->Int{return read_float;}",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            assert!(
                !super::super::lower_module(&module, "main")
                    .unwrap()
                    .code
                    .is_empty(),
                "{source}"
            );
        }
    }
}

#[cfg(test)]
mod numeric_negation_domain_tests {
    use super::*;
    #[test]
    fn numeric_negation_checks_operand_domain_before_emission() {
        for op in ["-", "neg"] {
            for (typ, valid) in [
                (Typ::Int, true),
                (Typ::Bool, true),
                (Typ::String, false),
                (Typ::Array(Box::new(Typ::Int)), false),
                (Typ::Vector(Box::new(Typ::Int)), false),
                (Typ::Generic("T".into()), false),
                (Typ::Named("Unknown".into()), false),
            ] {
                let functions = HashMap::new();
                let structs = HashMap::new();
                let mut ctx = LowerCtx::new("negation", &[], &functions, &structs);
                let offset = ctx.alloc_slot();
                ctx.locals.insert("value".into(), LocalSlot::Scalar(offset));
                ctx.local_types.insert("value".into(), typ.clone());
                let mut emitter = CodeEmitter::new();
                let mut pending = Vec::new();
                let expression = Expr::Unary {
                    op: op.into(),
                    expr: Box::new(Expr::Ident("value".into())),
                };
                let result = lower_expr_into(&mut emitter, &mut ctx, &expression, R4, &mut pending);
                assert_eq!(result.is_ok(), valid, "{op} {typ:?}: {result:?}");
                if valid {
                    assert_eq!(&emitter.bytes[2..4], &thumb::rsbs0(R0, R0).to_le_bytes());
                    assert_eq!(&emitter.bytes[4..], &thumb::mov_low(R4, R0).to_le_bytes());
                } else {
                    assert!(emitter.bytes.is_empty());
                }
                assert!(pending.is_empty());
            }
        }
    }
}
