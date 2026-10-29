use super::lower_expr;
use super::lower_util::{
    array_item_matches, emit_epilogue, emit_failure_return, expr_contains_call, expr_type,
    find_field_offset, is_native_scalar_type, native_struct_fields,
};
use super::{FunctionInfo, LocalSlot, LowerCtx, PendingCall};
use crate::core_ir::{Expr, LoopKind, MatchPattern, Stmt, Typ};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::HashMap;

fn emit_error_type(
    emitter: &mut CodeEmitter,
    ctx: &LowerCtx<'_>,
    expr: &Expr,
    functions: &HashMap<String, FunctionInfo>,
) -> Result<(), String> {
    if let Expr::Ident(name) = expr
        && let Some(offset) = ctx.error_types.get(name)
    {
        emitter.emit_u32(aarch64::ldr64(9, aarch64::REG_SP, *offset));
        return Ok(());
    }
    let tag = lower_expr::native_expr_type(expr, ctx, functions)
        .as_ref()
        .and_then(crate::core_effects::error_type_tag)
        .ok_or_else(|| format!("native-lower: unsupported throw payload type for {expr:?}"))?;
    emitter.emit_insns(&aarch64::load_i64(9, tag));
    Ok(())
}

/// Point x27 at the throw/try error slot before it is used.
///
/// The JIT runtime sets x27 once for the whole invocation. A native executable
/// has no such runtime, so the generated assembly names a writable slot; here we
/// leave a placeholder instruction and record the site for `build_assembly`.
/// Refusing is the only other option, because writing through an unset x27 is
/// what used to crash these programs.
pub(crate) fn emit_error_slot_adr(emitter: &mut CodeEmitter, fn_name: &str) -> Result<(), String> {
    match super::error_slot_mode() {
        super::ErrorSlotMode::RuntimeProvided => Ok(()),
        super::ErrorSlotMode::AssemblyProvided => {
            // Reserve room for the adrp/add pair the assembler needs to name a
            // writable slot in another section; a bare `adr` cannot reach it.
            let site = emitter.len();
            emitter.emit_u32(aarch64::nop());
            emitter.emit_u32(aarch64::nop());
            super::note_error_slot_ref(fn_name, site);
            Ok(())
        }
        super::ErrorSlotMode::Unsupported => Err(format!(
            "native-lower: `{fn_name}` uses throw/try, which needs a writable error slot that this link path does not provide"
        )),
    }
}

pub(crate) fn emit_error_exit(emitter: &mut CodeEmitter, ctx: &mut LowerCtx<'_>) {
    if let Some(branches) = ctx.error_branches.last_mut() {
        branches.push(emitter.emit_insn(aarch64::b(0)));
    } else {
        if matches!(
            ctx.ret_typ.canonical(),
            Typ::String | Typ::Void | Typ::Named(_) | Typ::Array(_) | Typ::Vector(_)
        ) {
            emitter.emit_insns(&aarch64::load_i64(0, 0));
        } else {
            emitter.emit_u32(aarch64::ldr64(0, 27, 8));
        }
        emit_epilogue(emitter, ctx.prologue_stack_reserve);
    }
}

pub(crate) fn emit_error_check(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    fn_name: &str,
) -> Result<(), String> {
    emit_error_slot_adr(emitter, fn_name)?;
    emitter.emit_u32(aarch64::ldrb(9, 27, 0));
    let continue_branch = emitter.emit_insn(aarch64::cbz_w(9, 0));
    emit_error_exit(emitter, ctx);
    let delta = emitter.len() as i32 - continue_branch as i32;
    emitter.patch_u32(continue_branch, aarch64::cbz_w(9, delta));
    Ok(())
}

pub(crate) fn lower_stmt(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    stmt: &Stmt,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
    ret_typ: &Typ,
) -> Result<(), String> {
    match stmt {
        Stmt::Return(expr) => {
            if let Some(expr) = expr {
                if ret_typ.canonical() == Typ::Void
                    && !crate::core_ir::expr_diverges(expr)
                    && lower_expr::native_expr_type(expr, ctx, functions).map(|typ| typ.canonical())
                        != Some(Typ::Void)
                {
                    return Err("native-lower: void return requires a unit value".into());
                }
                match ret_typ {
                    typ if crate::core_ir::rust_references::aggregate_referent(typ).is_some() => {
                        lower_expr::lower_expr_into(
                            emitter,
                            ctx,
                            expr,
                            0,
                            functions,
                            pending_calls,
                            fn_name,
                        )?;
                    }
                    Typ::Named(struct_name) => {
                        lower_struct_expr_into_regs(
                            emitter,
                            ctx,
                            expr,
                            struct_name,
                            functions,
                            pending_calls,
                            fn_name,
                        )?;
                    }
                    Typ::Array(elem) => {
                        lower_array_expr_into_regs(
                            emitter,
                            ctx,
                            expr,
                            elem,
                            functions,
                            pending_calls,
                            fn_name,
                        )?;
                    }
                    Typ::Vector(elem) => {
                        if matches!(expr, Expr::ArrayLit(_)) {
                            super::lower_call::lower_vector_call_arg(
                                emitter,
                                ctx,
                                expr,
                                elem,
                                0,
                                functions,
                                pending_calls,
                                fn_name,
                            )?;
                        } else {
                            lower_struct_expr_into_regs(
                                emitter,
                                ctx,
                                expr,
                                "Vec",
                                functions,
                                pending_calls,
                                fn_name,
                            )?;
                        }
                    }
                    _ => {
                        lower_expr::lower_expr_into(
                            emitter,
                            ctx,
                            expr,
                            0,
                            functions,
                            pending_calls,
                            fn_name,
                        )?;
                    }
                }
            } else {
                emitter.emit_insns(&aarch64::load_i64(0, 0));
            }
            emit_epilogue(emitter, ctx.prologue_stack_reserve);
            ctx.emitted_return = true;
            Ok(())
        }
        Stmt::Declare(name, typ) => ctx.alloc_declared_local(name, typ, fn_name),
        Stmt::Let(name, typ, expr) => {
            ctx.alloc_let_local(name, typ.as_ref(), expr, fn_name, functions)?;
            lower_store_local(emitter, ctx, name, expr, functions, pending_calls, fn_name)
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => lower_if(
            emitter,
            ctx,
            cond,
            then_body,
            else_body,
            functions,
            pending_calls,
            fn_name,
            ret_typ,
        ),
        Stmt::Assign(name, expr) => {
            if !ctx.locals.contains_key(name) && !ctx.params.contains_key(name) {
                if let Some(offset) = ctx.globals.get(name).copied() {
                    if matches!(
                        ctx.global_types.get(name).map(Typ::canonical),
                        Some(Typ::Array(_))
                    ) {
                        return Err(
                            "native-lower: whole global array assignment is unsupported".into()
                        );
                    }
                    if let Some(Typ::Named(typ)) = ctx.global_types.get(name).map(Typ::canonical) {
                        ctx.validate_global_struct(name)?;
                        let schema = native_struct_fields(ctx.structs, &typ, fn_name)?;
                        let base = ctx.acquire_call_arg_slots(fn_name, schema.len().max(8))?;
                        let fields = super::lower_call::aggregate_scratch_fields(
                            ctx,
                            &typ,
                            ctx.call_arg_temps[base],
                            fn_name,
                        )?;
                        // Finish the RHS before writing any field of the destination.
                        if matches!(expr, Expr::IntLit(0)) {
                            emitter.emit_insns(&aarch64::load_i64(0, 0));
                            for &slot in fields.values() {
                                emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, slot));
                            }
                        } else {
                            lower_struct_expr_into_slots(
                                emitter,
                                ctx,
                                expr,
                                &typ,
                                &fields,
                                functions,
                                pending_calls,
                                fn_name,
                            )?;
                        }
                        for (field, slot) in fields {
                            let key = format!("{name}.{field}");
                            let offset = ctx.globals.get(&key).copied().ok_or_else(|| {
                                format!("native-lower: missing global struct field `{key}`")
                            })?;
                            emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, slot));
                            ctx.emit_global_address(emitter, 1, offset);
                            emitter.emit_u32(aarch64::str64(0, 1, 0));
                        }
                        ctx.release_call_arg_temps();
                        return Ok(());
                    }
                    lower_expr::lower_expr_into(
                        emitter,
                        ctx,
                        expr,
                        0,
                        functions,
                        pending_calls,
                        fn_name,
                    )?;
                    ctx.emit_global_address(emitter, 1, offset);
                    emitter.emit_u32(aarch64::str64(0, 1, 0));
                    return Ok(());
                }
            }
            lower_store_local(emitter, ctx, name, expr, functions, pending_calls, fn_name)
        }
        Stmt::IndexAssign {
            base, index, value, ..
        } => lower_index_assign(
            emitter,
            ctx,
            base,
            index,
            value,
            functions,
            pending_calls,
            fn_name,
        ),
        Stmt::FieldAssign {
            base, name, value, ..
        } => lower_field_assign(
            emitter,
            ctx,
            base,
            name,
            value,
            functions,
            pending_calls,
            fn_name,
        ),
        Stmt::Expr(expr) => {
            lower_expr::lower_expr_into(emitter, ctx, expr, 0, functions, pending_calls, fn_name)?;
            Ok(())
        }
        Stmt::Loop {
            kind: LoopKind::For { binding },
            cond: Some(iterator),
            body,
        } => super::lower_vec_for(
            emitter,
            ctx,
            binding,
            iterator,
            body,
            functions,
            pending_calls,
            fn_name,
            ret_typ,
        ),
        Stmt::Loop { cond, body, .. } => lower_loop(
            emitter,
            ctx,
            cond.as_ref(),
            body,
            functions,
            pending_calls,
            fn_name,
            ret_typ,
        ),
        Stmt::Match {
            scrutinee, arms, ..
        } => lower_match(
            emitter,
            ctx,
            scrutinee,
            arms,
            functions,
            pending_calls,
            fn_name,
            ret_typ,
        ),
        Stmt::Throw(expr) => {
            emit_error_slot_adr(emitter, fn_name)?;
            lower_expr::lower_expr_into(emitter, ctx, expr, 0, functions, pending_calls, fn_name)?;
            emit_error_type(emitter, ctx, expr, functions)?;
            emitter.emit_u32(aarch64::str64(9, 27, 16));
            // Store error value to global location via X27
            emitter.emit_u32(aarch64::str64(0, 27, 8));
            // Set error flag to 1
            emitter.emit_insns(&aarch64::load_i64(1, 1));
            emitter.emit_u32(aarch64::strb(1, 27, 0));
            emit_error_exit(emitter, ctx);
            Ok(())
        }
        Stmt::Try { body, catches, .. } => {
            emit_error_slot_adr(emitter, fn_name)?;
            let saved_flag_offset = *ctx
                .try_flag_slots
                .get(ctx.active_try_flags.len())
                .ok_or("native-lower: missing saved flag slot for try scope")?;
            ctx.active_try_flags.push(saved_flag_offset);
            // Save previous error flag from global location to stack
            emitter.emit_u32(aarch64::ldrb(1, 27, 0));
            emitter.emit_u32(aarch64::strb(1, aarch64::REG_SP, saved_flag_offset));
            // Clear global error flag
            emitter.emit_u32(aarch64::strb(aarch64::REG_XZR, 27, 0));

            ctx.error_branches.push(Vec::new());
            for stmt in body {
                lower_stmt(
                    emitter,
                    ctx,
                    stmt,
                    functions,
                    pending_calls,
                    fn_name,
                    ret_typ,
                )?;
            }

            let error_branches = ctx.error_branches.pop().unwrap();

            // Check global error flag
            emitter.emit_u32(aarch64::ldrb(0, 27, 0));
            let handler_branch = emitter.emit_insn(aarch64::cbnz_w(0, 0));
            let end_branch = emitter.emit_insn(aarch64::b(0));

            let handler_offset = emitter.len();
            for site in error_branches {
                emitter.patch_u32(site, aarch64::b(handler_offset as i32 - site as i32));
            }
            emitter.emit_u32(aarch64::mov_reg64(aarch64::REG_SP, aarch64::REG_FP));
            if ctx.prologue_stack_reserve > 0 {
                emitter.emit_u32(aarch64::sub_imm64(
                    aarch64::REG_SP,
                    aarch64::REG_SP,
                    ctx.prologue_stack_reserve as u16,
                ));
            }
            let mut caught_branches = Vec::new();
            for catch_arm in catches {
                let pattern = MatchPattern::parse(&catch_arm.pattern)?;
                emitter.emit_u32(aarch64::ldr64(0, 27, 8));
                let mut type_misses = Vec::new();
                let tag = match &pattern {
                    MatchPattern::IntPat(_) => Some(1),
                    MatchPattern::BoolPat(_) => Some(2),
                    MatchPattern::StringPat(_) => Some(4),
                    _ => None,
                };
                if let Some(tag) = tag {
                    emitter.emit_u32(aarch64::ldr64(9, 27, 16));
                    emitter.emit_insns(&aarch64::load_i64(1, tag));
                    emitter.emit_u32(aarch64::cmp_reg64(9, 1));
                    type_misses.push((emitter.emit_insn(aarch64::b_cond(1, 0)), 1));
                }
                let next_arm = match pattern {
                    MatchPattern::IntPat(value) => {
                        emitter.emit_insns(&aarch64::load_i64(1, value));
                        emitter.emit_u32(aarch64::cmp_reg64(0, 1));
                        Some(emitter.emit_insn(aarch64::b_cond(1, 0)))
                    }
                    MatchPattern::BoolPat(value) => {
                        emitter.emit_insns(&aarch64::load_i64(1, i64::from(value)));
                        emitter.emit_u32(aarch64::cmp_reg64(0, 1));
                        Some(emitter.emit_insn(aarch64::b_cond(1, 0)))
                    }
                    MatchPattern::StringPat(value) => {
                        let id = ctx.string_id(&value)?;
                        emitter.emit_u32(aarch64::mov_reg64(2, 0));
                        type_misses.extend(emit_string_match_test(emitter, ctx, id));
                        None
                    }
                    MatchPattern::WildPat => None,
                    MatchPattern::IdentPat(name) => {
                        if let Some(LocalSlot::Scalar(offset)) = ctx.locals.get(&name) {
                            emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, *offset));
                            if let Some(tag_offset) = ctx.error_types.get(&name) {
                                emitter.emit_u32(aarch64::ldr64(9, 27, 16));
                                emitter.emit_u32(aarch64::str64(9, aarch64::REG_SP, *tag_offset));
                            }
                        }
                        None
                    }
                    // ponytail: aggregate catch matching is not lowered yet.
                    _ => {
                        return Err(format!(
                            "native-lower: unsupported catch pattern `{}` in `{fn_name}`",
                            catch_arm.pattern
                        ));
                    }
                };
                emitter.emit_u32(aarch64::strb(aarch64::REG_XZR, 27, 0));
                for catch_stmt in &catch_arm.body {
                    lower_stmt(
                        emitter,
                        ctx,
                        catch_stmt,
                        functions,
                        pending_calls,
                        fn_name,
                        ret_typ,
                    )?;
                }
                caught_branches.push(emitter.emit_insn(aarch64::b(0)));
                type_misses.extend(next_arm.map(|site| (site, 1)));
                for (site, condition) in type_misses {
                    emitter.patch_u32(
                        site,
                        aarch64::b_cond(condition, emitter.len() as i32 - site as i32),
                    );
                }
            }
            emit_error_exit(emitter, ctx);

            let end_offset = emitter.len();
            for site in caught_branches {
                emitter.patch_u32(site, aarch64::b(end_offset as i32 - site as i32));
            }

            let handler_delta = handler_offset as i32 - handler_branch as i32;
            emitter.patch_u32(handler_branch, aarch64::cbnz_w(0, handler_delta));
            let end_delta = end_offset as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_delta));

            // Restore previous error flag from stack to global location
            emitter.emit_u32(aarch64::ldrb(1, aarch64::REG_SP, saved_flag_offset));
            emitter.emit_u32(aarch64::strb(1, 27, 0));

            ctx.active_try_flags.pop();
            Ok(())
        }
        Stmt::Propagate => emit_error_check(emitter, ctx, fn_name),
        Stmt::Break | Stmt::Continue => {
            let Some(branches) = ctx.loop_branches.last_mut() else {
                return Err(format!(
                    "native-lower: loop jump outside a loop in `{fn_name}`"
                ));
            };
            for &offset in ctx.active_try_flags[branches.try_depth..].iter().rev() {
                emitter.emit_u32(aarch64::ldrb(1, aarch64::REG_SP, offset));
                emitter.emit_u32(aarch64::strb(1, 27, 0));
            }
            let site = emitter.emit_insn(aarch64::b(0));
            if matches!(stmt, Stmt::Break) {
                branches.breaks.push(site);
            } else {
                branches.continues.push(site);
            }
            Ok(())
        }
    }
}

pub(crate) fn lower_store_local(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    name: &str,
    expr: &Expr,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if let Some(&pointer) = ctx.receiver_pointers.get(name)
        && lower_expr::native_expr_type(expr, ctx, functions)
            .as_ref()
            .and_then(crate::core_ir::rust_references::aggregate_referent)
            .is_some()
    {
        lower_expr::lower_expr_into(emitter, ctx, expr, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, pointer));
        return Ok(());
    }
    let slot = if ctx.locals.contains_key(name) {
        ctx.locals
            .get(name)
            .cloned()
            .ok_or_else(|| format!("native-lower: local `{name}` not found in `{fn_name}`"))?
    } else if let Some(offset) = ctx.params.get(name) {
        LocalSlot::Scalar(*offset)
    } else {
        let offset = ctx.alloc_slot();
        ctx.locals
            .insert(name.to_string(), LocalSlot::Scalar(offset));
        LocalSlot::Scalar(offset)
    };
    match slot {
        LocalSlot::Scalar(offset) => {
            lower_expr::lower_expr_into(emitter, ctx, expr, 0, functions, pending_calls, fn_name)?;
            if ctx.scalar_types.get(name).map(Typ::canonical) == Some(Typ::Void) {
                emitter.emit_insns(&aarch64::load_i64(0, 0));
            }
            emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, offset));
            if let Some(tag_offset) = ctx.error_types.get(name).copied() {
                emit_error_type(emitter, ctx, expr, functions)?;
                emitter.emit_u32(aarch64::str64(9, aarch64::REG_SP, tag_offset));
            }
            Ok(())
        }
        LocalSlot::Array { elem, offsets } => {
            if let Expr::Ident(source) = expr
                && let Some(LocalSlot::Array {
                    elem: actual,
                    offsets: source_offsets,
                }) = ctx.locals.get(source)
                && actual == &elem
                && source_offsets == &offsets
            {
                return Ok(());
            }
            let Expr::ArrayLit(items) = expr else {
                return Err(format!(
                    "native-lower: unsupported array assignment in `{fn_name}`"
                ));
            };
            if items.len() != offsets.len() {
                return Err(format!(
                    "native-lower: array assignment length mismatch in `{fn_name}`"
                ));
            }
            for (item, offset) in items.iter().zip(offsets) {
                if let Some(item_ty) = expr_type(item)
                    && !array_item_matches(&elem, &item_ty)
                {
                    return Err(format!(
                        "native-lower: array item type mismatch in `{fn_name}`"
                    ));
                }
                lower_expr::lower_expr_into(
                    emitter,
                    ctx,
                    item,
                    0,
                    functions,
                    pending_calls,
                    fn_name,
                )?;
                emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, offset));
            }
            Ok(())
        }
        LocalSlot::ArrayParam {
            elem,
            ptr_offset,
            len_offset,
        } => {
            lower_array_expr_into_regs(
                emitter,
                ctx,
                expr,
                &elem,
                functions,
                pending_calls,
                fn_name,
            )?;
            emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, ptr_offset));
            emitter.emit_u32(aarch64::str64(1, aarch64::REG_SP, len_offset));
            Ok(())
        }
        LocalSlot::Struct { typ, fields } => {
            lower_struct_expr_into_slots(
                emitter,
                ctx,
                expr,
                &typ,
                &fields,
                functions,
                pending_calls,
                fn_name,
            )?;
            // Receiver replacement commits only after every source field is staged.
            for (field, &slot) in &fields {
                if let Some((pointer, offset)) = ctx.receiver_field(name, field) {
                    emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, slot));
                    emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, pointer));
                    emitter.emit_u32(aarch64::str64(0, 1, offset));
                }
            }
            Ok(())
        }
    }
}

pub(crate) fn lower_field_assign(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    name: &str,
    value: &Expr,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if let Expr::Index { base: array, index } = base {
        let saved_value = ctx.acquire_binop_temp(fn_name)?;
        let place = lower_expr::stage_record_index_place(
            emitter,
            ctx,
            array,
            index,
            functions,
            pending_calls,
            fn_name,
        )?;
        let field = place
            .fields
            .iter()
            .position(|(field, _)| field == name)
            .ok_or("native-lower: unknown indexed record field")?;
        let actual = lower_expr::native_expr_type(value, ctx, functions)
            .ok_or("native-lower: unknown indexed record field value type")?;
        if !array_item_matches(&place.fields[field].1, &actual) {
            return Err("native-lower: indexed record field assignment type mismatch".into());
        }
        lower_expr::lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, saved_value));
        lower_expr::load_record_index_address(emitter, ctx, &place, 0, fn_name)?;
        emitter.emit_u32(aarch64::ldr64(3, aarch64::REG_SP, saved_value));
        let offset = field
            .checked_mul(8)
            .and_then(|offset| u32::try_from(offset).ok())
            .filter(|offset| *offset <= 4095 * 8)
            .ok_or("native-lower: indexed record field exceeds AArch64 store offset subset")?;
        emitter.emit_u32(aarch64::str64(3, 0, offset));
        ctx.release_binop_temp();
        return Ok(());
    }
    let field_type = lower_expr::native_expr_type(
        &Expr::Field {
            base: Box::new(base.clone()),
            name: name.to_string(),
        },
        ctx,
        functions,
    );
    if field_type.as_ref().is_some_and(is_native_scalar_type)
        && let Some((pointer, offset)) =
            lower_expr::borrowed_field_place(ctx, base, name, functions)?
    {
        let saved_value = ctx.acquire_binop_temp(fn_name)?;
        lower_expr::lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, saved_value));
        lower_expr::lower_expr_into(emitter, ctx, pointer, 1, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, saved_value));
        emitter.emit_u32(aarch64::str64(0, 1, offset));
        ctx.release_binop_temp();
        return Ok(());
    }
    if let Some(Typ::Named(record)) = field_type.as_ref().map(Typ::canonical)
        && let Some((pointer, offset)) =
            lower_expr::borrowed_field_place(ctx, base, name, functions)?
    {
        let schema = crate::native_emit::struct_layout::arm_fields(ctx.structs, &record)?;
        let temp = ctx.acquire_call_arg_slots(fn_name, schema.len().max(8))?;
        let staged = super::lower_call::aggregate_scratch_fields(
            ctx,
            &record,
            ctx.call_arg_temps[temp],
            fn_name,
        )?;
        lower_struct_expr_into_slots(
            emitter,
            ctx,
            value,
            &record,
            &staged,
            functions,
            pending_calls,
            fn_name,
        )?;
        lower_expr::lower_expr_into(emitter, ctx, pointer, 1, functions, pending_calls, fn_name)?;
        for (index, (field, _)) in schema.iter().enumerate() {
            let destination = usize::try_from(offset)
                .ok()
                .and_then(|offset| offset.checked_add(index.checked_mul(8)?))
                .filter(|offset| *offset <= 4095 * 8)
                .ok_or("native-lower: borrowed aggregate exceeds AArch64 store offset subset")?;
            emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, staged[field]));
            emitter.emit_u32(aarch64::str64(0, 1, destination as u32));
        }
        ctx.release_call_arg_temps();
        return Ok(());
    }
    let Some((base_name, prefix)) = super::lower_util::field_path(base) else {
        return Err(format!(
            "native-lower: field assignment base is not addressable in `{fn_name}`"
        ));
    };
    let qualified = (!prefix.is_empty()).then(|| format!("{prefix}.{name}"));
    let name = qualified.as_deref().unwrap_or(name);
    if let Some(Typ::Array(elem)) = &field_type {
        lower_array_expr_into_regs(emitter, ctx, value, elem, functions, pending_calls, fn_name)?;
        for (reg, suffix) in ["ptr", "len"].into_iter().enumerate() {
            let field = format!("{name}.{suffix}");
            if let Some((pointer, offset)) = ctx.receiver_field(base_name, &field) {
                emitter.emit_u32(aarch64::ldr64(2, aarch64::REG_SP, pointer));
                emitter.emit_u32(aarch64::str64(reg as u8, 2, offset));
            } else if let Some(LocalSlot::Struct { fields, .. }) = ctx.locals.get(base_name)
                && let Some(&offset) = fields.get(&field)
            {
                emitter.emit_u32(aarch64::str64(reg as u8, aarch64::REG_SP, offset));
            } else {
                return Err(format!(
                    "native-lower: missing array destination `{base_name}.{field}` in `{fn_name}`"
                ));
            }
        }
        return Ok(());
    }
    let aggregate = match &field_type {
        Some(Typ::Named(name)) => Some(name.clone()),
        Some(Typ::Vector(_)) => Some("Vec".to_string()),
        _ => None,
    };
    if let Some(typ) = aggregate.as_ref()
        && ctx
            .structs
            .contains_key(super::lower_util::base_struct_name(typ))
    {
        let words = super::lower_util::native_param_abi_slots(
            &[("value".to_string(), Typ::Named(typ.clone()))],
            ctx.structs,
            fn_name,
        )?;
        let temp = ctx.acquire_call_arg_slots(fn_name, words.max(8))?;
        let staged = super::lower_call::aggregate_scratch_fields(
            ctx,
            typ,
            ctx.call_arg_temps[temp],
            fn_name,
        )?;
        lower_struct_expr_into_slots(
            emitter,
            ctx,
            value,
            typ,
            &staged,
            functions,
            pending_calls,
            fn_name,
        )?;
        for (field, slot) in staged {
            let full = format!("{name}.{field}");
            emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, slot));
            if let Some((pointer, offset)) = ctx.receiver_field(base_name, &full) {
                emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, pointer));
                emitter.emit_u32(aarch64::str64(0, 1, offset));
            } else if let Some(LocalSlot::Struct { fields, .. }) = ctx.locals.get(base_name)
                && let Some(&offset) = fields.get(&full)
            {
                emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, offset));
            } else {
                return Err(format!(
                    "native-lower: missing aggregate destination `{base_name}.{full}` in `{fn_name}`"
                ));
            }
        }
        ctx.release_call_arg_temps();
        return Ok(());
    }
    if !ctx.locals.contains_key(base_name) && !ctx.params.contains_key(base_name) {
        if let Some(offset) = ctx.globals.get(&format!("{base_name}.{name}")).copied() {
            lower_expr::lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
            ctx.emit_global_address(emitter, 1, offset);
            emitter.emit_u32(aarch64::str64(0, 1, 0));
            return Ok(());
        }
    }
    if field_type.as_ref().is_some_and(is_native_scalar_type)
        && let Some((pointer, offset)) = ctx.receiver_field(base_name, name)
    {
        lower_expr::lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, pointer));
        emitter.emit_u32(aarch64::str64(0, 1, offset));
        return Ok(());
    }
    let scalar_offset = ctx.locals.get(base_name).and_then(|s| match s {
        LocalSlot::Scalar(off) => Some(*off),
        _ => None,
    });
    if let Some(off) = scalar_offset {
        lower_expr::lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, off));
        return Ok(());
    }
    let Some(LocalSlot::Struct { fields, .. }) = ctx.locals.get(base_name) else {
        return Err(format!(
            "native-lower: field assignment base `{base_name}` is not a struct local in `{fn_name}`"
        ));
    };
    let Some(&field_offset) = find_field_offset(fields, name) else {
        return Err(format!(
            "native-lower: field `{name}` not found in `{base_name}` in `{fn_name}`"
        ));
    };
    lower_expr::lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, field_offset));
    Ok(())
}

pub(crate) fn lower_index_assign(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    base: &Expr,
    index: &Expr,
    value: &Expr,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if let Some(Typ::Array(elem)) =
        lower_expr::native_expr_type(base, ctx, functions).map(|typ| typ.canonical())
        && let Typ::Named(typ) = elem.canonical()
    {
        let place = lower_expr::stage_record_index_place(
            emitter,
            ctx,
            base,
            index,
            functions,
            pending_calls,
            fn_name,
        )?;
        let stage = stage_record_array_value(
            emitter,
            ctx,
            value,
            &typ,
            &place.fields,
            functions,
            pending_calls,
            fn_name,
        )?;
        lower_expr::load_record_index_address(emitter, ctx, &place, 0, fn_name)?;
        copy_record_stage(emitter, stage, place.fields.len(), 0);
        ctx.release_record_array_scratch(place.fields.len())?;
        return Ok(());
    }
    if lower_expr::native_expr_type(base, ctx, functions)
        .is_some_and(|typ| typ.canonical() == Typ::Int)
    {
        let base_temp = ctx.acquire_binop_temp(fn_name)?;
        lower_expr::lower_expr_into(emitter, ctx, base, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, base_temp));
        let index_temp = ctx.acquire_binop_temp(fn_name)?;
        lower_expr::lower_expr_into(emitter, ctx, index, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, index_temp));
        lower_expr::lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::ldr64(2, aarch64::REG_SP, base_temp));
        emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, index_temp));
        emitter.emit_u32(aarch64::str64_reg_offset(0, 2, 1));
        ctx.release_binop_temp();
        ctx.release_binop_temp();
        return Ok(());
    }
    if matches!(lower_expr::native_expr_type(base, ctx, functions).map(|typ| typ.canonical()), Some(Typ::Array(elem)) if matches!(elem.canonical(), Typ::Array(_)))
    {
        return Err("native-lower: whole nested array assignment is unsupported".into());
    }
    let name = match base {
        Expr::Ident(name) => Some(name.as_str()),
        _ => None,
    };
    if expr_contains_call(index) || expr_contains_call(value) {
        return Err(format!(
            "native-lower: unsupported array assignment call operand in `{fn_name}`"
        ));
    }
    let mut global_temps = false;
    let slot = if let Some(slot) = name.and_then(|name| ctx.locals.get(name)).cloned() {
        slot
    } else if let Some(Typ::Array(elem)) = name
        .and_then(|name| ctx.global_types.get(name))
        .map(Typ::canonical)
    {
        let offset = ctx
            .globals
            .get(name.unwrap())
            .copied()
            .ok_or("native-lower: missing global array storage")?;
        let ptr_offset = ctx.acquire_binop_temp(fn_name)?;
        let len_offset = ctx.acquire_binop_temp(fn_name)?;
        ctx.emit_global_address(emitter, 0, offset);
        emitter.emit_u32(aarch64::ldr64(1, 0, 0));
        emitter.emit_u32(aarch64::add_imm64(0, 0, 16));
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, ptr_offset));
        emitter.emit_u32(aarch64::str64(1, aarch64::REG_SP, len_offset));
        global_temps = true;
        LocalSlot::ArrayParam {
            elem: *elem,
            ptr_offset,
            len_offset,
        }
    } else if matches!(base, Expr::Index { .. } | Expr::Field { .. }) {
        let Some(Typ::Array(elem)) = lower_expr::native_expr_type(base, ctx, functions) else {
            return Err("native-lower: indexed store base must be an array".into());
        };
        let ptr_offset = ctx.acquire_binop_temp(fn_name)?;
        let len_offset = ctx.acquire_binop_temp(fn_name)?;
        lower_array_expr_into_regs(emitter, ctx, base, &elem, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, ptr_offset));
        emitter.emit_u32(aarch64::str64(1, aarch64::REG_SP, len_offset));
        global_temps = true;
        LocalSlot::ArrayParam {
            elem: *elem,
            ptr_offset,
            len_offset,
        }
    } else {
        return Err(format!(
            "native-lower: unsupported array assignment base in `{fn_name}`"
        ));
    };
    let slot = match slot {
        LocalSlot::Struct { typ, fields } if typ == "Vec" => {
            let Some(Typ::Vector(elem)) = lower_expr::native_expr_type(base, ctx, functions) else {
                return Err(format!(
                    "native-lower: Vec assignment needs element type in `{fn_name}`"
                ));
            };
            let ptr_offset = *find_field_offset(&fields, "ptr")
                .ok_or_else(|| format!("native-lower: Vec missing pointer in `{fn_name}`"))?;
            let len_offset = *find_field_offset(&fields, "len")
                .ok_or_else(|| format!("native-lower: Vec missing length in `{fn_name}`"))?;
            LocalSlot::ArrayParam {
                elem: *elem,
                ptr_offset,
                len_offset,
            }
        }
        slot => slot,
    };
    let elem = match &slot {
        LocalSlot::Array { elem, .. } | LocalSlot::ArrayParam { elem, .. } => elem,
        _ => {
            return Err(format!(
                "native-lower: unsupported array assignment base in `{fn_name}`"
            ));
        }
    };
    if let Some(value_ty) = expr_type(value)
        && !array_item_matches(elem, &value_ty)
    {
        return Err(format!(
            "native-lower: array assignment item type mismatch in `{fn_name}`"
        ));
    }
    // Preserve the index while lowering the value; nested expressions may use
    // every scratch register. Call operands are handled by shared spilling.
    let index_temp = ctx.acquire_binop_temp(fn_name)?;
    lower_expr::lower_expr_into(emitter, ctx, index, 0, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, index_temp));
    lower_expr::lower_expr_into(emitter, ctx, value, 0, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::ldr64(4, aarch64::REG_SP, index_temp));
    ctx.release_binop_temp();
    emitter.emit_u32(aarch64::cmp_reg64(4, aarch64::REG_XZR));
    let negative_branch = emitter.emit_insn(aarch64::b_cond(11, 0));
    match &slot {
        LocalSlot::Array { offsets, .. } => {
            emitter.emit_insns(&aarch64::load_i64(5, offsets.len() as i64))
        }
        LocalSlot::ArrayParam { len_offset, .. } => {
            emitter.emit_u32(aarch64::ldr64(5, aarch64::REG_SP, *len_offset));
        }
        _ => unreachable!(),
    }
    emitter.emit_u32(aarch64::cmp_reg64(4, 5));
    let oob_branch = emitter.emit_insn(aarch64::b_cond(10, 0));
    let base_reg = match slot {
        LocalSlot::Array { offsets, .. } => {
            let base_offset = offsets.first().copied().unwrap_or(0);
            if base_offset == 0 {
                aarch64::REG_SP
            } else {
                emitter.emit_u32(aarch64::add_imm64(6, aarch64::REG_SP, base_offset as u16));
                6
            }
        }
        LocalSlot::ArrayParam { ptr_offset, .. } => {
            emitter.emit_u32(aarch64::ldr64(6, aarch64::REG_SP, ptr_offset));
            6
        }
        _ => unreachable!(),
    };
    emitter.emit_u32(aarch64::str64_reg_offset(0, base_reg, 4));
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
    if global_temps {
        ctx.release_binop_temp();
        ctx.release_binop_temp();
    }
    Ok(())
}

pub(crate) fn lower_struct_expr_into_slots(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    typ: &str,
    fields: &HashMap<String, u32>,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if matches!(
        expr,
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. }
    ) {
        return lower_expr::lower_value_control(
            emitter,
            ctx,
            expr,
            functions,
            pending_calls,
            fn_name,
            |emitter, ctx, value, pending_calls| {
                lower_struct_expr_into_slots(
                    emitter,
                    ctx,
                    value,
                    typ,
                    fields,
                    functions,
                    pending_calls,
                    fn_name,
                )
            },
        );
    }

    if let Expr::Index { base, index } = expr {
        if !matches!(lower_expr::native_expr_type(expr, ctx, functions).map(|typ| typ.canonical()), Some(Typ::Named(actual)) if actual == typ)
        {
            return Err("native-lower: indexed record value nominal type mismatch".into());
        }
        let schema = lower_expr::lower_record_index_address(
            emitter,
            ctx,
            base,
            index,
            0,
            functions,
            pending_calls,
            fn_name,
        )?;
        let expected = crate::native_emit::struct_layout::flat_record_fields(ctx.structs, typ)?;
        if schema != expected {
            return Err("native-lower: indexed record value type mismatch".into());
        }
        for (word, (field, _)) in schema.iter().enumerate() {
            let offset = find_field_offset(fields, field)
                .ok_or("native-lower: missing record destination field")?;
            emitter.emit_u32(aarch64::ldr64(3, 0, (word as u32) * 8));
            emitter.emit_u32(aarch64::str64(3, aarch64::REG_SP, *offset));
        }
        return Ok(());
    }
    match expr {
        Expr::Unary { op, .. } if typ == "Vec" && op == "vector-clone" => {
            lower_expr::lower_expr_into(emitter, ctx, expr, 0, functions, pending_calls, fn_name)?;
            for (reg, field) in ["ptr", "len", "cap"].into_iter().enumerate() {
                let offset = find_field_offset(fields, field)
                    .ok_or("native-lower: missing vector header field")?;
                emitter.emit_u32(aarch64::str64(reg as u8, aarch64::REG_SP, *offset));
            }
            Ok(())
        }
        Expr::ArrayLit(items) if typ == "Vec" => {
            let Some(&ptr_offset) = find_field_offset(fields, "ptr") else {
                return Err(format!(
                    "native-lower: Vec local missing `ptr` in `{fn_name}`"
                ));
            };
            let Some(&len_offset) = find_field_offset(fields, "len") else {
                return Err(format!(
                    "native-lower: Vec local missing `len` in `{fn_name}`"
                ));
            };
            let Some(&cap_offset) = find_field_offset(fields, "cap") else {
                return Err(format!(
                    "native-lower: Vec local missing `cap` in `{fn_name}`"
                ));
            };
            super::lower_stdlib::lower_vec_literal_into_slots(
                emitter,
                ctx,
                items,
                ptr_offset,
                len_offset,
                cap_offset,
                functions,
                pending_calls,
                fn_name,
            )
        }
        Expr::StructInit {
            name: _init,
            fields: values,
        } => {
            // Generic aliases may differ in name, but every slot must be initialized.
            let schema = native_struct_fields(ctx.structs, typ, fn_name)?;
            if values.len() != schema.len()
                || schema
                    .iter()
                    .any(|(field, _)| values.iter().filter(|(name, _)| name == field).count() != 1)
            {
                return Err(format!(
                    "native-lower: incomplete struct initializer for `{typ}`"
                ));
            }
            for (field, value) in values {
                let Some((_, field_typ)) = schema.iter().find(|(name, _)| name == field) else {
                    continue; // skip unknown fields
                };
                if crate::core_ir::rust_references::aggregate_referent(field_typ).is_some() {
                    let offset = find_field_offset(fields, field)
                        .ok_or("native-lower: missing reference field storage")?;
                    lower_expr::lower_expr_into(
                        emitter,
                        ctx,
                        value,
                        0,
                        functions,
                        pending_calls,
                        fn_name,
                    )?;
                    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, *offset));
                    continue;
                }
                if let Typ::Array(elem) = field_typ {
                    lower_array_expr_into_regs(
                        emitter,
                        ctx,
                        value,
                        elem,
                        functions,
                        pending_calls,
                        fn_name,
                    )?;
                    for (reg, suffix) in ["ptr", "len"].into_iter().enumerate() {
                        let key = format!("{field}.{suffix}");
                        let offset = find_field_offset(fields, &key)
                            .ok_or("native-lower: missing array field storage")?;
                        emitter.emit_u32(aarch64::str64(reg as u8, aarch64::REG_SP, *offset));
                    }
                    continue;
                }
                if let (Typ::Vector(elem), Expr::ArrayLit(items)) = (field_typ, value) {
                    let ptr_key = format!("{field}.ptr");
                    let len_key = format!("{field}.len");
                    let cap_key = format!("{field}.cap");
                    let Some(&ptr_offset) = find_field_offset(fields, &ptr_key) else {
                        return Err(format!(
                            "native-lower: Vec field `{field}` missing `ptr` in `{typ}` in `{fn_name}`"
                        ));
                    };
                    let Some(&len_offset) = find_field_offset(fields, &len_key) else {
                        return Err(format!(
                            "native-lower: Vec field `{field}` missing `len` in `{typ}` in `{fn_name}`"
                        ));
                    };
                    let Some(&cap_offset) = find_field_offset(fields, &cap_key) else {
                        return Err(format!(
                            "native-lower: Vec field `{field}` missing `cap` in `{typ}` in `{fn_name}`"
                        ));
                    };
                    if let Typ::Named(struct_name) = elem.as_ref() {
                        super::lower_call::lower_aggregate_vector_literal_into_slots(
                            emitter,
                            ctx,
                            items,
                            struct_name,
                            ptr_offset,
                            len_offset,
                            cap_offset,
                            functions,
                            pending_calls,
                            fn_name,
                        )?;
                    } else {
                        super::lower_stdlib::lower_vec_literal_into_slots(
                            emitter,
                            ctx,
                            items,
                            ptr_offset,
                            len_offset,
                            cap_offset,
                            functions,
                            pending_calls,
                            fn_name,
                        )?;
                    }
                    continue;
                }
                let inner = match field_typ.canonical() {
                    Typ::Named(name) => Some(name),
                    Typ::Vector(_) => Some("Vec".to_string()),
                    _ => None,
                };
                if let Some(inner) = inner
                    && ctx
                        .structs
                        .contains_key(super::lower_util::base_struct_name(&inner))
                {
                    let prefix = format!("{field}.");
                    let nested_fields = fields
                        .iter()
                        .filter_map(|(name, &offset)| {
                            name.strip_prefix(&prefix)
                                .map(|name| (name.to_string(), offset))
                        })
                        .collect();
                    lower_struct_expr_into_slots(
                        emitter,
                        ctx,
                        value,
                        &inner,
                        &nested_fields,
                        functions,
                        pending_calls,
                        fn_name,
                    )?;
                } else {
                    let Some(&offset) = find_field_offset(fields, field) else {
                        return Err(format!(
                            "native-lower: field `{field}` missing on struct `{typ}` in `{fn_name}`"
                        ));
                    };
                    lower_expr::lower_expr_into(
                        emitter,
                        ctx,
                        value,
                        0,
                        functions,
                        pending_calls,
                        fn_name,
                    )?;
                    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, offset));
                }
            }
            Ok(())
        }
        Expr::Ident(local) => {
            if !ctx.locals.contains_key(local)
                && !ctx.params.contains_key(local)
                && ctx.globals.contains_key(local)
            {
                ctx.validate_global_struct(local)?;
                for (field, &offset) in fields {
                    ctx.load_global_struct_field(emitter, local, field, 0, fn_name)?;
                    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, offset));
                }
                return Ok(());
            }

            let Some(LocalSlot::Struct {
                typ: local_typ,
                fields: local_fields,
            }) = ctx.locals.get(local).cloned()
            else {
                return Err(format!(
                    "native-lower: struct value `{local}` needs a known local or global layout in `{fn_name}`"
                ));
            };
            if local_typ != typ {
                return Err(format!(
                    "native-lower: struct assignment type mismatch: expected `{typ}`, found `{local_typ}` in `{fn_name}`"
                ));
            }
            for (field, &dst) in fields {
                let Some(&src) = local_fields.get(field) else {
                    return Err(format!(
                        "native-lower: field `{field}` missing on source struct `{typ}` in `{fn_name}`"
                    ));
                };
                if ctx.receiver_pointers.contains_key(local) {
                    lower_expr::lower_struct_field_into(emitter, ctx, expr, field, 0, fn_name)?;
                } else {
                    emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, src));
                }
                emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, dst));
            }
            Ok(())
        }
        Expr::Field { .. } => {
            for (field, &slot) in fields {
                lower_expr::lower_struct_field_into(emitter, ctx, expr, field, 0, fn_name)?;
                emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, slot));
            }
            Ok(())
        }
        Expr::Call { callee, args, .. } => {
            if let Some(return_typ) =
                super::lower_util::call_return_type(callee, functions, fn_name)?
            {
                if return_typ != &Typ::Named(typ.to_string())
                    && !(typ == "Vec" && matches!(return_typ, Typ::Vector(_)))
                    && !matches!(return_typ, Typ::Named(n) if n == "Self")
                    && typ != "Self"
                {
                    return Err(format!(
                        "native-lower: struct assignment return type mismatch: expected `{typ}`, got `{return_typ:?}` in `{fn_name}`"
                    ));
                }
            }
            super::lower_call::lower_call(
                emitter,
                ctx,
                callee,
                args,
                0,
                functions,
                pending_calls,
                fn_name,
            )?;
            let schema = crate::native_emit::struct_layout::arm_fields(ctx.structs, typ)
                .or_else(|_| native_struct_fields(ctx.structs, typ, fn_name))?;
            for (reg, (field, _)) in schema.iter().enumerate() {
                let Some(&offset) = find_field_offset(fields, field) else {
                    return Err(format!(
                        "native-lower: field `{field}` missing on struct `{typ}` in `{fn_name}`"
                    ));
                };
                emitter.emit_u32(aarch64::str64(reg as u8, aarch64::REG_SP, offset));
            }
            Ok(())
        }
        _ => Err(format!(
            "native-lower: unsupported struct assignment expression `{expr:?}` for `{typ}` in `{fn_name}`"
        )),
    }
}

pub(crate) fn lower_struct_expr_into_regs(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    typ: &str,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if matches!(
        expr,
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. }
    ) {
        return lower_expr::lower_value_control(
            emitter,
            ctx,
            expr,
            functions,
            pending_calls,
            fn_name,
            |emitter, ctx, value, pending_calls| {
                lower_struct_expr_into_regs(
                    emitter,
                    ctx,
                    value,
                    typ,
                    functions,
                    pending_calls,
                    fn_name,
                )
            },
        );
    }

    if let Ok(layout) = crate::native_emit::struct_layout::arm_fields(ctx.structs, typ) {
        // shortcut: scalar aggregate returns use eight GPRs; widen with an indirect ABI.
        if layout.len() > 8 {
            return Err(format!(
                "native-lower: aggregate return exceeds eight scalar fields in `{fn_name}`"
            ));
        }
        let base = ctx.acquire_call_arg_slots(fn_name, layout.len().max(1))?;
        let fields: HashMap<_, _> = layout
            .iter()
            .enumerate()
            .map(|(index, (field, _))| (field.clone(), ctx.call_arg_temps[base + index]))
            .collect();
        lower_struct_expr_into_slots(
            emitter,
            ctx,
            expr,
            typ,
            &fields,
            functions,
            pending_calls,
            fn_name,
        )?;
        for (reg, (field, _)) in layout.iter().enumerate() {
            emitter.emit_u32(aarch64::ldr64(reg as u8, aarch64::REG_SP, fields[field]));
        }
        ctx.release_call_arg_temps();
        return Ok(());
    }
    if crate::core_ir::tuples::fields(typ).is_some() {
        return Err(format!(
            "native-lower: tuple return needs a supported scalar-field layout in `{fn_name}`"
        ));
    }
    match expr {
        Expr::Unary { op, .. } if typ == "Vec" && op == "vector-clone" => {
            lower_expr::lower_expr_into(emitter, ctx, expr, 0, functions, pending_calls, fn_name)
        }
        Expr::StructInit {
            name: _init,
            fields: values,
        } => {
            // Relaxed type check: skip mismatch errors for generic/alias types
            let schema = native_struct_fields(ctx.structs, typ, fn_name)?;
            let mut reg = 0u8;

            // Build a lookup map for O(1) field access
            let values_map: HashMap<&String, &Expr> =
                values.iter().map(|(name, expr)| (name, expr)).collect();

            for (field, field_typ) in &schema {
                let value = if let Some(val) = values_map.get(field) {
                    *val
                } else {
                    // Missing field: emit 0 and advance reg (one field per register)
                    emitter.emit_insns(&crate::native_emit::aarch64::load_i64(reg, 0));
                    reg += 1;
                    continue;
                };
                if let (Typ::Vector(_), Expr::ArrayLit(items)) = (field_typ, value) {
                    let Some(ptr_offset) = ctx.vec_literal_header_offset else {
                        return Err(format!(
                            "native-lower: missing Vec return scratch space in `{fn_name}`"
                        ));
                    };
                    super::lower_stdlib::lower_vec_literal_into_slots(
                        emitter,
                        ctx,
                        items,
                        ptr_offset,
                        ptr_offset + 8,
                        ptr_offset + 16,
                        functions,
                        pending_calls,
                        fn_name,
                    )?;
                    for offset in [ptr_offset, ptr_offset + 8, ptr_offset + 16] {
                        emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, offset));
                        reg += 1;
                    }
                    continue;
                }
                lower_expr::lower_expr_into(
                    emitter,
                    ctx,
                    value,
                    reg,
                    functions,
                    pending_calls,
                    fn_name,
                )?;
                reg += match field_typ {
                    Typ::Named(name) if name == "PathBuf" => 3,
                    _ => 1,
                };
            }
            Ok(())
        }
        Expr::Ident(local) => {
            if !ctx.locals.contains_key(local)
                && !ctx.params.contains_key(local)
                && ctx.globals.contains_key(local)
            {
                let schema = native_struct_fields(ctx.structs, typ, fn_name)?;
                ctx.validate_global_struct(local)?;
                for (reg, (field, _)) in schema.iter().enumerate() {
                    ctx.load_global_struct_field(emitter, local, field, reg as u8, fn_name)?;
                }
                return Ok(());
            }

            let entry = ctx.locals.get(local).cloned();
            let (local_typ, fields) = match entry {
                Some(LocalSlot::Struct { typ, fields, .. }) => (typ, fields),
                Some(LocalSlot::Scalar(offset)) => {
                    // Opaque struct: load scalar value as return value
                    emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, offset));
                    return Ok(());
                }
                _ => {
                    // Unknown identifier: emit 0 (enum variant, constant, or ZST)
                    emitter.emit_insns(&crate::native_emit::aarch64::load_i64(0, 0));
                    return Ok(());
                }
            };
            let schema = native_struct_fields(ctx.structs, typ, fn_name).or_else(|_| {
                // If the expected type isn't in the structs table (generic/alias), try the local type
                native_struct_fields(ctx.structs, &local_typ, fn_name)
            })?;
            for (reg, (field, _)) in schema.iter().enumerate() {
                let Some(&offset) = find_field_offset(&fields, field) else {
                    emitter.emit_insns(&crate::native_emit::aarch64::load_i64(reg as u8, 0));
                    continue;
                };
                if let Some((pointer, relative)) = ctx.receiver_field(local, field) {
                    emitter.emit_u32(aarch64::ldr64(reg as u8, aarch64::REG_SP, pointer));
                    emitter.emit_u32(aarch64::ldr64(reg as u8, reg as u8, relative));
                } else {
                    emitter.emit_u32(aarch64::ldr64(reg as u8, aarch64::REG_SP, offset));
                }
            }
            Ok(())
        }
        Expr::Call { callee, args, .. } => super::lower_call::lower_call(
            emitter,
            ctx,
            callee,
            args,
            0,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::Field { base, name, .. } => {
            if let Some((root, _)) = super::lower_util::field_path(expr)
                && matches!(ctx.locals.get(root), Some(LocalSlot::Struct { .. }))
            {
                let schema = native_struct_fields(ctx.structs, typ, fn_name)?;
                for (reg, (field, _)) in schema.iter().enumerate() {
                    lower_expr::lower_struct_field_into(
                        emitter, ctx, expr, field, reg as u8, fn_name,
                    )?;
                }
                return Ok(());
            }

            if let Expr::Ident(local) = base.as_ref() {
                let offset = ctx.params.get(local).copied().or_else(|| {
                    ctx.locals.get(local).and_then(|s| match s {
                        LocalSlot::Struct { fields, .. } => fields.get(name).copied(),
                        LocalSlot::Scalar(off) => Some(*off),
                        _ => None,
                    })
                });
                if let Some(off) = offset {
                    emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, off));
                    return Ok(());
                }
            }
            // Fall through to general expr lowering
            super::lower_expr::lower_expr_into(
                emitter,
                ctx,
                expr,
                0,
                functions,
                pending_calls,
                fn_name,
            )?;
            Ok(())
        }
        Expr::Index { base, index, .. } => super::lower_expr::lower_index(
            emitter,
            ctx,
            base,
            index,
            0,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::Unary { op, expr, .. } if op == "*" => {
            super::lower_expr::lower_expr_into(
                emitter,
                ctx,
                expr,
                0,
                functions,
                pending_calls,
                fn_name,
            )?;
            // x0 holds the pointer; dereference it
            emitter.emit_u32(aarch64::ldr64(0, 0, 0));
            Ok(())
        }
        _ => {
            emitter.emit_insns(&crate::native_emit::aarch64::load_i64(0, 0));
            Ok(())
        }
    }
}

/// Allocate word-addressable owned storage using the existing array lifetime policy.
/// R0 receives data after the two-word header; R1 is the requested word count.
/// Record payloads can reuse this storage with a 16-byte physical header overhead.
pub(crate) fn lower_owned_word_storage(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    words: usize,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let (word_count, allocation_bytes) = owned_word_storage_size(words)?;
    if words == 0 {
        emitter.emit_insns(&aarch64::load_i64(0, 0));
        emitter.emit_insns(&aarch64::load_i64(1, 0));
        return Ok(());
    }
    if super::TL_NATIVE_MODE.with(|mode| *mode.borrow()) {
        // Runtime allocation must not bind to same-named user functions.
        let runtime_functions = HashMap::new();
        super::lower_call::lower_call(
            emitter,
            ctx,
            &Expr::Ident("malloc".into()),
            &[Expr::IntLit(allocation_bytes)],
            0,
            &runtime_functions,
            pending_calls,
            fn_name,
        )?;
        emitter.emit_u32(aarch64::cmp_reg64(0, aarch64::REG_XZR));
        let allocated = emitter.emit_insn(aarch64::b_cond(1, 0));
        super::lower_call::lower_call(
            emitter,
            ctx,
            &Expr::Ident("abort".into()),
            &[],
            0,
            &runtime_functions,
            pending_calls,
            fn_name,
        )?;
        emitter.patch_u32(
            allocated,
            aarch64::b_cond(1, emitter.len() as i32 - allocated as i32),
        );
    } else {
        emitter.emit_u32(aarch64::mov_reg64(0, 27));
        emitter.emit_insns(&aarch64::load_i64(1, word_count));
        emitter.emit_insns(&aarch64::load_i64(
            15,
            crate::jit_runtime::allocate_array as *const () as i64,
        ));
        emitter.emit_u32(0xD63F_0000 | (15 << 5));
    }
    emitter.emit_insns(&aarch64::load_i64(1, word_count));
    emitter.emit_u32(aarch64::str64(1, 0, 0));
    emitter.emit_u32(aarch64::str64(1, 0, 8));
    emitter.emit_u32(aarch64::add_imm64(0, 0, 16));
    Ok(())
}

fn owned_word_storage_size(words: usize) -> Result<(i64, i64), String> {
    let word_count = i64::try_from(words).map_err(|_| "native-lower: array allocation overflow")?;
    let allocation_bytes = words
        .checked_add(2)
        .and_then(|count| count.checked_mul(8))
        .and_then(|bytes| i64::try_from(bytes).ok())
        .ok_or("native-lower: array allocation overflow")?;
    Ok((word_count, allocation_bytes))
}

pub(crate) fn lower_array_expr_into_regs(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    elem: &Typ,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if let Expr::Unary { op, expr: source } = expr
        && crate::core_ir::rust_references::value_kind(op).is_some()
    {
        return lower_array_expr_into_regs(
            emitter,
            ctx,
            source,
            elem,
            functions,
            pending_calls,
            fn_name,
        );
    }
    if let Expr::Unary { op, expr: source } = expr
        && let Some(count) = crate::core_ir::array_copy::count(op)
    {
        if !matches!(lower_expr::native_expr_type(source,ctx,functions),Some(Typ::Array(actual)) if actual.canonical()==elem.canonical() || (count==0 && actual.canonical()==Typ::Void))
        {
            return Err("native-lower: array copy destination element type mismatch".into());
        }
        return lower_expr::lower_array_copy(
            emitter,
            ctx,
            source,
            count,
            0,
            true,
            functions,
            pending_calls,
            fn_name,
        );
    }
    if matches!(elem.canonical(), Typ::Named(_)) {
        super::lower_util::record_array_fields(elem, ctx.structs)?;
    }
    if matches!(
        expr,
        Expr::Conditional { .. } | Expr::Block { .. } | Expr::MatchValue { .. }
    ) {
        return lower_expr::lower_value_control(
            emitter,
            ctx,
            expr,
            functions,
            pending_calls,
            fn_name,
            |emitter, ctx, value, pending_calls| {
                lower_array_expr_into_regs(
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

    if crate::native_emit::global_data::indexed_root(expr).is_some_and(|name| {
        !ctx.locals.contains_key(name)
            && matches!(
                ctx.global_types.get(name).map(Typ::canonical),
                Some(Typ::Array(_))
            )
    }) {
        if !matches!(lower_expr::native_expr_type(expr, ctx, functions), Some(Typ::Array(actual)) if actual.canonical() == elem.canonical())
        {
            return Err(format!(
                "native-lower: static array element type mismatch in `{fn_name}`"
            ));
        }
        return lower_expr::lower_expr_into(
            emitter,
            ctx,
            expr,
            0,
            functions,
            pending_calls,
            fn_name,
        );
    }
    let record = super::lower_util::record_array_fields(elem, ctx.structs)?;
    if !is_native_scalar_type(elem)
        && record.is_none()
        && !(elem.canonical() == Typ::Void
            && matches!(expr,Expr::ArrayLit(items) if items.is_empty()))
    {
        return Err(format!(
            "native-lower: unsupported array element type `{elem:?}` in `{fn_name}`"
        ));
    }
    match expr {
        Expr::Field { .. } => {
            if !matches!(lower_expr::native_expr_type(expr, ctx, functions), Some(Typ::Array(actual)) if actual.canonical() == elem.canonical())
            {
                return Err(format!(
                    "native-lower: array field type mismatch in `{fn_name}`"
                ));
            }
            lower_expr::lower_struct_field_into(emitter, ctx, expr, "ptr", 0, fn_name)?;
            lower_expr::lower_struct_field_into(emitter, ctx, expr, "len", 1, fn_name)
        }
        Expr::Ident(local) => {
            let slot = ctx.locals.get(local).cloned().ok_or_else(|| {
                format!("native-lower: array references unknown local `{local}` in `{fn_name}`")
            })?;
            match slot {
                LocalSlot::ArrayParam {
                    elem: actual,
                    ptr_offset,
                    len_offset,
                } => {
                    if &actual != elem {
                        return Err(format!(
                            "native-lower: array return type mismatch in `{fn_name}`"
                        ));
                    }
                    emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, ptr_offset));
                    emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, len_offset));
                    Ok(())
                }
                LocalSlot::Array {
                    elem: actual,
                    offsets,
                } => {
                    if &actual != elem {
                        return Err(format!(
                            "native-lower: array return type mismatch in `{fn_name}`"
                        ));
                    }
                    // Copy inline storage before the callee's frame is released.
                    let items = (0..offsets.len())
                        .map(|index| Expr::Index {
                            base: Box::new(expr.clone()),
                            index: Box::new(Expr::IntLit(index as i64)),
                        })
                        .collect();
                    lower_array_expr_into_regs(
                        emitter,
                        ctx,
                        &Expr::ArrayLit(items),
                        elem,
                        functions,
                        pending_calls,
                        fn_name,
                    )
                }
                _ => Err(format!(
                    "native-lower: `{local}` is not an array in `{fn_name}`"
                )),
            }
        }
        Expr::Call { callee, args, .. } => {
            if let Some(return_typ) =
                super::lower_util::call_return_type(callee, functions, fn_name)?
            {
                if return_typ != &Typ::Array(Box::new(elem.clone())) {
                    return Err(format!(
                        "native-lower: array return type mismatch in `{fn_name}`"
                    ));
                }
            }
            super::lower_call::lower_call(
                emitter,
                ctx,
                callee,
                args,
                0,
                functions,
                pending_calls,
                fn_name,
            )?;
            if matches!(
                callee.as_ref(),
                Expr::Ident(name)
                    if matches!(name.as_str(), "str-split-lines" | "str-split-spaces" | "str-tokenize-expr" | "str_split_lines" | "str_split_spaces" | "str_tokenize_expr")
            ) {
                emitter.emit_u32(aarch64::ldr64(1, 0, 0));
                emitter.emit_u32(aarch64::add_imm64(0, 0, 16));
            }
            Ok(())
        }
        Expr::ArrayLit(items) => {
            for item in items {
                if let Some(actual) = lower_expr::native_expr_type(item, ctx, functions)
                    && !array_item_matches(elem, &actual)
                {
                    return Err(format!(
                        "native-lower: array return type mismatch in `{fn_name}`"
                    ));
                }
            }
            if items.is_empty() {
                emitter.emit_insns(&aarch64::load_i64(0, 0));
                emitter.emit_insns(&aarch64::load_i64(1, 0));
                return Ok(());
            }
            let data_bytes = items
                .len()
                .checked_mul(8)
                .and_then(|bytes| i64::try_from(bytes).ok())
                .ok_or("native-lower: array allocation overflow")?;
            lower_owned_word_storage(emitter, ctx, items.len(), pending_calls, fn_name)?;
            let cursor = ctx.acquire_binop_temp(fn_name)?;
            emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, cursor));
            for item in items {
                if let Some(fields) = &record {
                    let Typ::Named(typ) = elem.canonical() else {
                        unreachable!()
                    };
                    let stage = stage_record_array_value(
                        emitter,
                        ctx,
                        item,
                        &typ,
                        fields,
                        functions,
                        pending_calls,
                        fn_name,
                    )?;
                    // shortcut: each persistent payload uses the existing owned array allocator,
                    // retaining its 16-byte header overhead until a dedicated payload allocator exists.
                    lower_owned_word_storage(emitter, ctx, fields.len(), pending_calls, fn_name)?;
                    copy_record_stage(emitter, stage, fields.len(), 0);
                    ctx.release_record_array_scratch(fields.len())?;
                } else {
                    lower_expr::lower_expr_into(
                        emitter,
                        ctx,
                        item,
                        0,
                        functions,
                        pending_calls,
                        fn_name,
                    )?;
                }
                emitter.emit_u32(aarch64::ldr64(3, aarch64::REG_SP, cursor));
                emitter.emit_u32(aarch64::str64(0, 3, 0));
                emitter.emit_u32(aarch64::add_imm64(3, 3, 8));
                emitter.emit_u32(aarch64::str64(3, aarch64::REG_SP, cursor));
            }
            emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, cursor));
            emitter.emit_insns(&aarch64::load_i64(2, data_bytes));
            emitter.emit_u32(aarch64::sub_reg64(0, 0, 2));
            emitter.emit_insns(&aarch64::load_i64(1, items.len() as i64));
            ctx.release_binop_temp();
            Ok(())
        }
        _ => Err(format!(
            "native-lower: unsupported array return in `{fn_name}`"
        )),
    }
}

pub(crate) fn lower_if(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    cond: &Expr,
    then_body: &[Stmt],
    else_body: &[Stmt],
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
    ret_typ: &Typ,
) -> Result<(), String> {
    lower_expr::lower_expr_into(emitter, ctx, cond, 0, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::cmp_reg64(0, aarch64::REG_XZR));
    let else_branch = emitter.emit_insn(aarch64::b_cond(0, 0));
    for stmt in then_body {
        lower_stmt(
            emitter,
            ctx,
            stmt,
            functions,
            pending_calls,
            fn_name,
            ret_typ,
        )?;
    }
    let end_branch = emitter.emit_insn(aarch64::b(0));
    let else_offset = emitter.len() as i32 - else_branch as i32;
    emitter.patch_u32(else_branch, aarch64::b_cond(0, else_offset));
    for stmt in else_body {
        lower_stmt(
            emitter,
            ctx,
            stmt,
            functions,
            pending_calls,
            fn_name,
            ret_typ,
        )?;
    }
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b(end_offset));
    Ok(())
}

pub(crate) fn lower_loop(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    cond: Option<&Expr>,
    body: &[Stmt],
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
    ret_typ: &Typ,
) -> Result<(), String> {
    let head = emitter.len();
    let end_branch = if let Some(cond) = cond {
        lower_expr::lower_expr_into(emitter, ctx, cond, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::cmp_reg64(0, aarch64::REG_XZR));
        Some(emitter.emit_insn(aarch64::b_cond(0, 0)))
    } else {
        None
    };
    ctx.loop_branches.push(super::lower_ctx::LoopBranches {
        try_depth: ctx.active_try_flags.len(),
        ..Default::default()
    });
    for stmt in body {
        lower_stmt(
            emitter,
            ctx,
            stmt,
            functions,
            pending_calls,
            fn_name,
            ret_typ,
        )?;
    }
    let back_offset = head as i32 - emitter.len() as i32;
    emitter.emit_u32(aarch64::b(back_offset));
    if let Some(end_branch) = end_branch {
        let end_offset = emitter.len() as i32 - end_branch as i32;
        emitter.patch_u32(end_branch, aarch64::b_cond(0, end_offset));
    }
    patch_loop_branches(emitter, ctx, head)
}

pub(crate) fn patch_loop_branches(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    continue_target: u32,
) -> Result<(), String> {
    let Some(branches) = ctx.loop_branches.pop() else {
        return Err("native-lower: missing loop branch state".into());
    };
    let end = emitter.len();
    for site in branches.breaks {
        emitter.patch_u32(site, aarch64::b(end as i32 - site as i32));
    }
    for site in branches.continues {
        emitter.patch_u32(site, aarch64::b(continue_target as i32 - site as i32));
    }
    Ok(())
}

/// Emit the equality test for one string match arm.
///
/// `x2` holds the scrutinee pointer, so the test compares string *contents*:
/// equal literals are interned to one table entry, but a scrutinee that is
/// computed at runtime (concatenated, read from a field, passed as a parameter)
/// has its own allocation. An earlier version loaded the pattern's constant-pool
/// index into `x1` and compared it against the scrutinee pointer, so a string
/// pattern could never match and every string `match` fell through to its
/// default arm.
///
/// On return the flags say whether the arm matched: control reaches the arm body
/// only when the strings are equal, and each returned site is a `b.ne` that must
/// be patched to the instruction after the arm.
fn emit_string_match_test(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    string_index: i64,
) -> Vec<(u32, u8)> {
    // x1 = the pattern literal. x9/x12 walk the payloads; x13/x14 hold one byte
    // from each side. x3/x4 hold the lengths and x3 doubles as the loop counter.
    let adr_site = emitter.emit_insn(aarch64::adr(1, 0));
    ctx.pending_strings.push(super::PendingString {
        adr_site,
        string_index,
        rd: 1,
    });
    let mut not_equal = Vec::with_capacity(3);
    emitter.emit_u32(aarch64::cmp_reg64(2, aarch64::REG_XZR));
    not_equal.push((
        emitter.emit_insn(aarch64::b_cond(aarch64::COND_EQ, 0)),
        aarch64::COND_EQ,
    ));

    // Lengths must agree before the payloads are worth comparing, and reading a
    // length header is safe for every literal now that `""` is a real entry.
    emitter.emit_u32(aarch64::ldr64(3, 2, 0));
    emitter.emit_u32(aarch64::ldr64(4, 1, 0));
    emitter.emit_u32(aarch64::cmp_reg64(3, 4));
    not_equal.push((
        emitter.emit_insn(aarch64::b_cond(aarch64::COND_NE, 0)),
        aarch64::COND_NE,
    ));

    // Equal lengths: empty strings match without entering the loop.
    emitter.emit_u32(aarch64::cmp_reg64(3, aarch64::REG_XZR));
    let empty_branch = emitter.emit_insn(aarch64::b_cond(aarch64::COND_EQ, 0));

    emitter.emit_u32(aarch64::add_imm64(9, 2, 8));
    emitter.emit_u32(aarch64::add_imm64(12, 1, 8));
    let loop_head = emitter.len();
    emitter.emit_u32(aarch64::ldrb(13, 9, 0));
    emitter.emit_u32(aarch64::ldrb(14, 12, 0));
    emitter.emit_u32(aarch64::cmp_reg64(13, 14));
    not_equal.push((
        emitter.emit_insn(aarch64::b_cond(aarch64::COND_NE, 0)),
        aarch64::COND_NE,
    ));
    emitter.emit_u32(aarch64::add_imm64(9, 9, 1));
    emitter.emit_u32(aarch64::add_imm64(12, 12, 1));
    emitter.emit_u32(aarch64::sub_imm64(3, 3, 1));
    emitter.emit_u32(aarch64::cmp_reg64(3, aarch64::REG_XZR));
    let back_offset = loop_head as i32 - emitter.len() as i32;
    emitter.emit_u32(aarch64::b_cond(aarch64::COND_NE, back_offset));

    let matched_offset = emitter.len() as i32 - empty_branch as i32;
    emitter.patch_u32(
        empty_branch,
        aarch64::b_cond(aarch64::COND_EQ, matched_offset),
    );
    not_equal
}

pub(crate) fn lower_match(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    scrutinee: &Expr,
    arms: &[crate::core_ir::MatchArm],
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
    ret_typ: &Typ,
) -> Result<(), String> {
    lower_expr::lower_expr_into(
        emitter,
        ctx,
        scrutinee,
        2,
        functions,
        pending_calls,
        fn_name,
    )?;
    let mut end_branches = Vec::new();
    for arm in arms {
        let pattern = MatchPattern::parse(&arm.pattern)?;
        if let MatchPattern::IdentPat(name) = &pattern {
            let Some(LocalSlot::Scalar(offset)) = ctx.locals.get(name) else {
                return Err("native-lower: missing scalar match binding".into());
            };
            emitter.emit_u32(aarch64::str64(2, aarch64::REG_SP, *offset));
            if let Some(tag_offset) = ctx.error_types.get(name) {
                emit_error_type(emitter, ctx, scrutinee, functions)?;
                emitter.emit_u32(aarch64::str64(9, aarch64::REG_SP, *tag_offset));
            }
        }
        if matches!(pattern, MatchPattern::WildPat | MatchPattern::IdentPat(_)) {
            for stmt in &arm.body {
                lower_stmt(
                    emitter,
                    ctx,
                    stmt,
                    functions,
                    pending_calls,
                    fn_name,
                    ret_typ,
                )?;
            }
            break;
        }
        let scalar = match &pattern {
            MatchPattern::IntPat(value) => Some((*value, Typ::Int)),
            MatchPattern::BoolPat(value) => Some((i64::from(*value), Typ::Bool)),
            _ => None,
        };
        if let Some((value, typ)) = scalar {
            let type_miss = if let Expr::Ident(name) = scrutinee
                && ctx.error_types.contains_key(name)
            {
                emit_error_type(emitter, ctx, scrutinee, functions)?;
                emitter.emit_insns(&aarch64::load_i64(
                    1,
                    crate::core_effects::error_type_tag(&typ).unwrap(),
                ));
                emitter.emit_u32(aarch64::cmp_reg64(9, 1));
                Some(emitter.emit_insn(aarch64::b_cond(1, 0)))
            } else {
                if lower_expr::native_expr_type(scrutinee, ctx, functions)
                    .map(|typ| typ.canonical())
                    != Some(typ)
                {
                    return Err(
                        "native-lower: scalar pattern requires a compatible scrutinee".into(),
                    );
                }
                None
            };
            emitter.emit_insns(&aarch64::load_i64(1, value));
            emitter.emit_u32(aarch64::cmp_reg64(2, 1));
            let next_branch = emitter.emit_insn(aarch64::b_cond(1, 0));
            for stmt in &arm.body {
                lower_stmt(
                    emitter,
                    ctx,
                    stmt,
                    functions,
                    pending_calls,
                    fn_name,
                    ret_typ,
                )?;
            }
            end_branches.push(emitter.emit_insn(aarch64::b(0)));
            let next_offset = emitter.len() as i32 - next_branch as i32;
            emitter.patch_u32(next_branch, aarch64::b_cond(1, next_offset));
            if let Some(site) = type_miss {
                emitter.patch_u32(site, aarch64::b_cond(1, emitter.len() as i32 - site as i32));
            }
        } else if let MatchPattern::StringPat(value) = pattern {
            let type_miss = if let Expr::Ident(name) = scrutinee
                && ctx.error_types.contains_key(name)
            {
                emit_error_type(emitter, ctx, scrutinee, functions)?;
                emitter.emit_insns(&aarch64::load_i64(1, 4));
                emitter.emit_u32(aarch64::cmp_reg64(9, 1));
                Some((emitter.emit_insn(aarch64::b_cond(1, 0)), 1))
            } else {
                if lower_expr::native_expr_type(scrutinee, ctx, functions).map(|typ| typ.canonical()) != Some(Typ::String)
                {
                    return Err("native-lower: string pattern requires a String scrutinee".into());
                }
                None
            };
            let id = ctx.string_id(&value)?;
            let mut not_equal = emit_string_match_test(emitter, ctx, id);
            not_equal.extend(type_miss);
            for stmt in &arm.body {
                lower_stmt(
                    emitter,
                    ctx,
                    stmt,
                    functions,
                    pending_calls,
                    fn_name,
                    ret_typ,
                )?;
            }
            end_branches.push(emitter.emit_insn(aarch64::b(0)));
            for (site, condition) in not_equal {
                let next_offset = emitter.len() as i32 - site as i32;
                emitter.patch_u32(site, aarch64::b_cond(condition, next_offset));
            }
        } else {
            // A pattern the lowerer cannot test (variant, range, ...) used
            // to fall through to this arm's body unconditionally, which silently
            // returned the first arm's result for every input. Refusing lowers
            // the function to a trap that names the pattern instead.
            return Err(format!(
                "native-lower: unsupported match pattern `{}` in `{fn_name}`",
                arm.pattern.trim()
            ));
        }
    }
    for branch in end_branches {
        let offset = emitter.len() as i32 - branch as i32;
        emitter.patch_u32(branch, aarch64::b(offset));
    }
    Ok(())
}

fn stage_record_array_value(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    value: &Expr,
    typ: &str,
    fields: &[(String, Typ)],
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<u32, String> {
    if !matches!(lower_expr::native_expr_type(value, ctx, functions).map(|typ| typ.canonical()), Some(Typ::Named(actual)) if actual == typ)
    {
        return Err("native-lower: record array value type mismatch".into());
    }
    if let Expr::StructInit { fields: values, .. } = value {
        for (name, value) in values {
            let expected = fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, typ)| typ)
                .ok_or("native-lower: unknown record array initializer field")?;
            let actual = lower_expr::native_expr_type(value, ctx, functions)
                .ok_or("native-lower: unknown record array initializer value type")?;
            if !array_item_matches(expected, &actual) {
                return Err("native-lower: record array initializer field type mismatch".into());
            }
        }
    }
    let offset = ctx.acquire_record_array_scratch(fields.len(), fn_name)?;
    let slots: HashMap<_, _> = fields
        .iter()
        .enumerate()
        .map(|(index, (name, _))| (name.clone(), offset + (index as u32) * 8))
        .collect();
    let result = lower_struct_expr_into_slots(
        emitter,
        ctx,
        value,
        typ,
        &slots,
        functions,
        pending_calls,
        fn_name,
    );
    if let Err(error) = result {
        ctx.release_record_array_scratch(fields.len())?;
        return Err(error);
    }
    Ok(offset)
}

fn copy_record_stage(emitter: &mut CodeEmitter, offset: u32, words: usize, pointer: u8) {
    for word in 0..words {
        emitter.emit_u32(aarch64::ldr64(
            3,
            aarch64::REG_SP,
            offset + (word as u32) * 8,
        ));
        emitter.emit_u32(aarch64::str64(3, pointer, (word as u32) * 8));
    }
}

#[cfg(test)]
mod owned_word_storage_tests {
    use super::*;

    #[test]
    fn physical_storage_includes_header_without_changing_logical_word_count() {
        assert_eq!(owned_word_storage_size(0).unwrap(), (0, 16));
        assert_eq!(owned_word_storage_size(1).unwrap(), (1, 24));
        assert_eq!(owned_word_storage_size(65).unwrap(), (65, 536));
        assert!(owned_word_storage_size(usize::MAX).is_err());
        let overflowing = (i64::MAX as usize / 8) + 1;
        assert!(owned_word_storage_size(overflowing).is_err());
    }
}

#[cfg(test)]
mod declaration_storage_tests {
    use super::*;
    #[test]
    fn declaration_reserves_a_slot_without_emitting_a_store() {
        let structs = HashMap::new();
        let strings = HashMap::new();
        let globals = HashMap::new();
        let types = HashMap::new();
        let mut inrt = vec![];
        let mut pending_strings = vec![];
        let mut ctx = LowerCtx::new(
            &[],
            &structs,
            &strings,
            &globals,
            &types,
            &mut inrt,
            &mut pending_strings,
            "test",
        )
        .unwrap();
        let functions = HashMap::new();
        let mut pending = vec![];
        let mut emitter = CodeEmitter::new();
        let declaration = Stmt::Declare("value".into(), Typ::Int);
        lower_stmt(
            &mut emitter,
            &mut ctx,
            &declaration,
            &functions,
            &mut pending,
            "test",
            &Typ::Int,
        )
        .unwrap();
        assert!(ctx.locals.contains_key("value"));
        assert!(emitter.bytes.is_empty());
        lower_stmt(
            &mut emitter,
            &mut ctx,
            &Stmt::Assign("value".into(), Expr::IntLit(42)),
            &functions,
            &mut pending,
            "test",
            &Typ::Int,
        )
        .unwrap();
        assert!(!emitter.bytes.is_empty());
    }
}
