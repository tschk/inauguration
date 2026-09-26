//! Core IR → AArch64 stdlib intrinsic lowering.
//!
//! Recognizes std::env, std::fs, String/Path, Vec, Option, Result, and std::mem
//! helpers and emits inline AArch64 sequences or calls to the C-ABI wrappers
//! in `native_stdlib`.

use super::lower_expr::lower_expr_into;
use super::lower_stmt::lower_struct_expr_into_slots;
use super::{
    FunctionInfo, LocalSlot, LowerCtx, PendingCall, TL_NATIVE_MODE,
    find_field_offset, lower_comparison_result, native_param_abi_slots, pick_scratch,
};
use crate::core_ir::{Expr, Stmt, Typ};
use crate::native_emit::aarch64::{self, CodeEmitter, REG_SP, REG_XZR};

/// Emit a call to a C-ABI wrapper exposed by `native_stdlib`.
///
/// For native binaries this emits a BL placeholder that is resolved by the
/// system linker; for JIT it looks up the wrapper address and calls via BLR.
fn emit_stdlib_wrapper_call(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    wrapper: &str,
    args: &[Expr],
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let temp_base = ctx.acquire_call_arg_temps(fn_name)?;
    for (i, arg) in args.iter().enumerate() {
        if i >= 8 {
            break;
        }
        lower_expr_into(emitter, ctx, arg, 0, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::str64(0, REG_SP, ctx.call_arg_temps[temp_base + i]));
    }
    for i in 0..args.len().min(8) {
        emitter.emit_u32(aarch64::ldr64(
            i as u8,
            REG_SP,
            ctx.call_arg_temps[temp_base + i],
        ));
    }
    let result = emit_stdlib_wrapper_register_call(emitter, ctx, wrapper, rd);
    ctx.release_call_arg_temps();
    result
}

/// Runtime-blob builtin that stands in for a Rust-side wrapper in a native
/// artifact.
///
/// The JIT resolves these wrappers with dlsym, but a native executable links
/// only against the system libraries, so the wrapper symbol does not exist there.
/// Wrappers not listed here have no native implementation yet; the caller
/// refuses them with a coded message instead of emitting a `bl` to a symbol the
/// linker cannot resolve.
fn native_inrt_builtin(wrapper: &str) -> Option<&'static str> {
    match wrapper {
        "in_str_concat" => Some(crate::inrt::INRT_STR_CONCAT),
        "in_str_eq" => Some(crate::inrt::INRT_STR_EQ),
        "in_str_contains" => Some(crate::inrt::INRT_STR_CONTAINS),
        _ => None,
    }
}

fn emit_stdlib_wrapper_register_call(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    wrapper: &str,
    rd: u8,
) -> Result<(), String> {
    let is_native = TL_NATIVE_MODE.with(|m| *m.borrow());
    if is_native {
        let Some(builtin) = native_inrt_builtin(wrapper) else {
            return Err(format!(
                "native-lower: `{wrapper}` has no native runtime yet; it is only \
                 available under the JIT"
            ));
        };
        // Arguments already sit in x0..x7 in the wrapper's order, which is the
        // builtin's order too.
        let call_site = emitter.len();
        emitter.emit_u32(aarch64::bl(0));
        ctx.pending_inrt_calls.push(super::PendingInrtCall {
            site: call_site,
            target: builtin.to_string(),
        });
    } else {
        crate::native_emit::native_link::bootstrap_jit_native();
        if let Some(native_ptr) = crate::native_emit::native_link::resolve_native_fn(wrapper) {
            emitter.emit_insns(&aarch64::load_i64(15, native_ptr as usize as i64));
            emitter.emit_u32(0xD63F_01E0u32 | (15 << 5)); // BLR X15
        } else {
            return Err(format!("native-lower: missing {wrapper} runtime"));
        }
    }
    if rd != 0 {
        emitter.emit_u32(aarch64::mov_reg64(rd, 0));
    }
    Ok(())
}

pub(crate) fn lower_vec_literal_into_slots(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    items: &[Expr],
    ptr_offset: u32,
    len_offset: u32,
    cap_offset: u32,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    emitter.emit_u32(aarch64::str64(
        aarch64::REG_XZR,
        aarch64::REG_SP,
        ptr_offset,
    ));
    emitter.emit_u32(aarch64::str64(
        aarch64::REG_XZR,
        aarch64::REG_SP,
        len_offset,
    ));
    emitter.emit_u32(aarch64::str64(
        aarch64::REG_XZR,
        aarch64::REG_SP,
        cap_offset,
    ));
    for item in items {
        lower_expr_into(emitter, ctx, item, 1, functions, pending_calls, fn_name)?;
        emitter.emit_u32(aarch64::add_imm64(0, aarch64::REG_SP, ptr_offset as u16));
        emit_stdlib_wrapper_register_call(emitter, ctx, "in_vec_push", 0)?;
    }
    Ok(())
}

pub(crate) fn emit_vec_push_words(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    header_offset: u32,
    source_offset: u32,
    words: usize,
) -> Result<(), String> {
    emitter.emit_u32(aarch64::add_imm64(0, aarch64::REG_SP, header_offset as u16));
    emitter.emit_u32(aarch64::add_imm64(1, aarch64::REG_SP, source_offset as u16));
    emitter.emit_insns(&aarch64::load_i64(2, words as i64));
    emit_stdlib_wrapper_register_call(emitter, ctx, "in_vec_push_words", 0)
}

fn lower_string_push_str(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    args: &[Expr],
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    lower_expr_into(
        emitter,
        ctx,
        &args[0],
        rd,
        functions,
        pending_calls,
        fn_name,
    )?;
    lower_expr_into(
        emitter,
        ctx,
        &args[1],
        rd + 1,
        functions,
        pending_calls,
        fn_name,
    )?;

    let self_reg = rd;
    let arg_reg = rd + 1;
    let self_ptr = pick_scratch(&[self_reg, arg_reg]);
    let self_len = pick_scratch(&[self_reg, arg_reg, self_ptr]);
    let self_cap = pick_scratch(&[self_reg, arg_reg, self_ptr, self_len]);
    let arg_ptr = pick_scratch(&[self_reg, arg_reg, self_ptr, self_len, self_cap]);
    let arg_len = pick_scratch(&[self_reg, arg_reg, self_ptr, self_len, self_cap, arg_ptr]);
    let new_len = pick_scratch(&[
        self_reg, arg_reg, self_ptr, self_len, self_cap, arg_ptr, arg_len,
    ]);
    let i = pick_scratch(&[
        self_reg, arg_reg, self_ptr, self_len, self_cap, arg_ptr, arg_len, new_len,
    ]);
    let byte = pick_scratch(&[
        self_reg, arg_reg, self_ptr, self_len, self_cap, arg_ptr, arg_len, new_len, i,
    ]);
    let dst_addr = pick_scratch(&[
        self_reg, arg_reg, self_ptr, self_len, self_cap, arg_ptr, arg_len, new_len, i, byte,
    ]);

    emitter.emit_u32(aarch64::ldr64(self_ptr, self_reg, 0));
    emitter.emit_u32(aarch64::ldr64(self_len, self_reg, 8));
    emitter.emit_u32(aarch64::ldr64(self_cap, self_reg, 16));
    emitter.emit_u32(aarch64::ldr64(arg_ptr, arg_reg, 0));
    emitter.emit_u32(aarch64::ldr64(arg_len, arg_reg, 8));
    emitter.emit_u32(aarch64::add_reg64(new_len, self_len, arg_len));
    emitter.emit_u32(aarch64::cmp_reg64(new_len, self_cap));
    let overflow_branch = emitter.emit_insn(aarch64::b_cond(12, 0)); // B.GT -> overflow
    emitter.emit_insns(&aarch64::load_i64(i, 0));
    let loop_head = emitter.len();
    emitter.emit_u32(aarch64::cmp_reg64(i, arg_len));
    let end_branch = emitter.emit_insn(aarch64::b_cond(0, 0));
    emitter.emit_u32(aarch64::add_reg64(dst_addr, arg_ptr, i));
    emitter.emit_u32(aarch64::ldrb(byte, dst_addr, 0));
    emitter.emit_u32(aarch64::add_reg64(dst_addr, self_ptr, self_len));
    emitter.emit_u32(aarch64::add_reg64(dst_addr, dst_addr, i));
    emitter.emit_u32(aarch64::strb(byte, dst_addr, 0));
    emitter.emit_u32(aarch64::add_imm64(i, i, 1));
    let back_offset = loop_head as i32 - emitter.len() as i32;
    emitter.emit_u32(aarch64::b(back_offset));
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b_cond(0, end_offset));
    emitter.emit_u32(aarch64::str64(new_len, self_reg, 8));
    let end_branch = emitter.emit_insn(aarch64::b(0)); // jump to epilogue
    let overflow_offset = emitter.len() as i32 - overflow_branch as i32;
    emitter.patch_u32(overflow_branch, aarch64::b_cond(12, overflow_offset));
    emitter.emit_u32(aarch64::brk(0)); // overflow trap
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b(end_offset));
    emitter.emit_insns(&aarch64::load_i64(rd, 0));
    Ok(())
}

fn lower_vec_extend(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    args: &[Expr],
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let Expr::Ident(destination) = &args[0] else {
        return Err(format!(
            "native-lower: Vec::extend destination must be a local in `{fn_name}`"
        ));
    };
    let Some(LocalSlot::Struct { typ, fields }) = ctx.locals.get(destination) else {
        return Err(format!(
            "native-lower: Vec::extend destination `{destination}` is not a Vec local in `{fn_name}`"
        ));
    };
    if typ != "Vec" {
        return Err(format!(
            "native-lower: Vec::extend destination `{destination}` has type `{typ}` in `{fn_name}`"
        ));
    }
    let offset = *find_field_offset(fields, "ptr").ok_or_else(|| {
        format!("native-lower: Vec local `{destination}` is missing `ptr` in `{fn_name}`")
    })?;
    match &args[1] {
        Expr::Ident(source) => {
            let Some(LocalSlot::Struct { typ, fields }) = ctx.locals.get(source) else {
                return Err(format!(
                    "native-lower: Vec::extend source `{source}` is not a Vec local in `{fn_name}`"
                ));
            };
            if typ != "Vec" {
                return Err(format!(
                    "native-lower: Vec::extend source `{source}` has type `{typ}` in `{fn_name}`"
                ));
            }
            for (register, field) in [(1, "ptr"), (2, "len"), (3, "cap")] {
                let field_offset = find_field_offset(fields, field).ok_or_else(|| {
                    format!(
                        "native-lower: Vec local `{source}` is missing `{field}` in `{fn_name}`"
                    )
                })?;
                emitter.emit_u32(aarch64::ldr64(register, aarch64::REG_SP, *field_offset));
            }
        }
        Expr::Call { callee, args } => {
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
            emitter.emit_u32(aarch64::mov_reg64(3, 2));
            emitter.emit_u32(aarch64::mov_reg64(2, 1));
            emitter.emit_u32(aarch64::mov_reg64(1, 0));
        }
        _ => {
            return Err(format!(
                "native-lower: Vec::extend source must be a Vec local or call in `{fn_name}`"
            ));
        }
    }
    emitter.emit_u32(aarch64::add_imm64(0, aarch64::REG_SP, offset as u16));
    let is_native = TL_NATIVE_MODE.with(|m| *m.borrow());
    if is_native {
        // No runtime-blob builtin covers `in_vec_extend`, so a native artifact
        // cannot call it: refuse instead of emitting an unresolvable `bl`.
        return Err(
            "native-lower: `in_vec_extend` has no native runtime yet; it is only \
             available under the JIT"
                .to_string(),
        );
    } else if let Some(native_ptr) =
        crate::native_emit::native_link::resolve_native_fn("in_vec_extend")
    {
        emitter.emit_insns(&aarch64::load_i64(15, native_ptr as usize as i64));
        emitter.emit_u32(0xD63F_01E0u32 | (15 << 5));
    } else {
        return Err("native-lower: missing in_vec_extend runtime".to_string());
    }
    if rd != 0 {
        emitter.emit_u32(aarch64::mov_reg64(rd, 0));
    }
    Ok(())
}

fn lower_iter_once(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    args: &[Expr],
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let Some(header_offset) = ctx.vec_literal_header_offset else {
        return Err(format!(
            "native-lower: missing iterator vector header in `{fn_name}`"
        ));
    };
    for offset in [header_offset, header_offset + 8, header_offset + 16] {
        emitter.emit_u32(aarch64::str64(aarch64::REG_XZR, REG_SP, offset));
    }
    lower_expr_into(emitter, ctx, &args[0], 1, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::add_imm64(0, REG_SP, header_offset as u16));
    emit_stdlib_wrapper_register_call(emitter, ctx, "in_vec_push", 0)?;
    for (index, offset) in [header_offset, header_offset + 8, header_offset + 16]
        .into_iter()
        .enumerate()
    {
        emitter.emit_u32(aarch64::ldr64(rd + index as u8, REG_SP, offset));
    }
    Ok(())
}

fn lower_array_into_iter(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    source: &Expr,
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    match source {
        Expr::Call { callee, args } => super::lower_call::lower_call(
            emitter,
            ctx,
            callee,
            args,
            rd,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::Ident(name) => match ctx.locals.get(name) {
            Some(LocalSlot::Array { offsets, .. }) if !offsets.is_empty() => {
                emitter.emit_u32(aarch64::add_imm64(rd, REG_SP, offsets[0] as u16));
                emitter.emit_insns(&aarch64::load_i64(rd + 1, offsets.len() as i64));
                Ok(())
            }
            Some(LocalSlot::ArrayParam {
                ptr_offset,
                len_offset,
                ..
            }) => {
                emitter.emit_u32(aarch64::ldr64(rd, REG_SP, *ptr_offset));
                emitter.emit_u32(aarch64::ldr64(rd + 1, REG_SP, *len_offset));
                Ok(())
            }
            _ => Err(format!(
                "native-lower: into_iter source `{name}` is not an array in `{fn_name}`"
            )),
        },
        _ => {
            // Unsupported into_iter source — emit empty iterator
            emitter.emit_insns(&aarch64::load_i64(rd, 0));
            emitter.emit_insns(&aarch64::load_i64(rd + 1, 0));
            Ok(())
        }
    }
}

fn lower_iter_chain(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    args: &[Expr],
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let Some(header_offset) = ctx.iterator_chain_header_offset else {
        return Err(format!(
            "native-lower: missing chain vector header in `{fn_name}"
        ));
    };
    for offset in [header_offset, header_offset + 8, header_offset + 16] {
        emitter.emit_u32(aarch64::str64(aarch64::REG_XZR, REG_SP, offset));
    }
    for source in args {
        super::lower_call::lower_call(
            emitter,
            ctx,
            &Expr::Ident("collect".to_string()),
            std::slice::from_ref(source),
            0,
            functions,
            pending_calls,
            fn_name,
        )?;
        emitter.emit_u32(aarch64::mov_reg64(3, 2));
        emitter.emit_u32(aarch64::mov_reg64(2, 1));
        emitter.emit_u32(aarch64::mov_reg64(1, 0));
        emitter.emit_u32(aarch64::add_imm64(0, REG_SP, header_offset as u16));
        emit_stdlib_wrapper_register_call(emitter, ctx, "in_vec_extend", 0)?;
    }
    for (index, offset) in [header_offset, header_offset + 8, header_offset + 16]
        .into_iter()
        .enumerate()
    {
        emitter.emit_u32(aarch64::ldr64(rd + index as u8, REG_SP, offset));
    }
    Ok(())
}

fn lower_array_map(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    source: &Expr,
    closure: &Expr,
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let Expr::Closure { params, body, .. } = closure else {
        // Map with non-closure arg: emit empty result
        emitter.emit_insns(&aarch64::load_i64(0, 0));
        emitter.emit_insns(&aarch64::load_i64(1, 0));
        emitter.emit_insns(&aarch64::load_i64(2, 0));
        return Ok(());
    };
    let [(binding, _)] = params.as_slice() else {
        return Err(format!(
            "native-lower: map closure requires one parameter in `{fn_name}`"
        ));
    };
    let Some(Stmt::Return(Some(item @ Expr::StructInit { name, .. }))) = body.last() else {
        // map closure doesn't return a struct — emit empty result
        emitter.emit_insns(&aarch64::load_i64(0, 0));
        emitter.emit_insns(&aarch64::load_i64(1, 0));
        emitter.emit_insns(&aarch64::load_i64(2, 0));
        return Ok(());
    };
    let Some(header_offset) = ctx.vec_literal_header_offset else {
        return Err(format!(
            "native-lower: missing map vector header in `{fn_name}`"
        ));
    };
    let Some(slots) = ctx.iterator_map_slots else {
        return Err(format!(
            "native-lower: missing map iterator state in `{fn_name}`"
        ));
    };
    let Some((scratch_offset, scratch_words)) = ctx.aggregate_vector_scratch else {
        return Err(format!(
            "native-lower: missing aggregate map scratch space in `{fn_name}`"
        ));
    };
    let words = native_param_abi_slots(
        &[("value".to_string(), Typ::Named(name.clone()))],
        ctx.structs,
        fn_name,
    )?;
    if words > scratch_words {
        return Err(format!(
            "native-lower: aggregate map scratch space is too small in `{fn_name}`"
        ));
    }
    lower_array_into_iter(emitter, ctx, source, 0, functions, pending_calls, fn_name)?;
    emitter.emit_u32(aarch64::str64(0, REG_SP, slots.ptr));
    emitter.emit_u32(aarch64::str64(1, REG_SP, slots.len));
    emitter.emit_u32(aarch64::str64(aarch64::REG_XZR, REG_SP, slots.index));
    for offset in [header_offset, header_offset + 8, header_offset + 16] {
        emitter.emit_u32(aarch64::str64(aarch64::REG_XZR, REG_SP, offset));
    }
    let previous = ctx
        .locals
        .insert(binding.clone(), LocalSlot::Scalar(slots.binding));
    let fields = super::lower_call::aggregate_scratch_fields(ctx, name, scratch_offset, fn_name)?;
    let head = emitter.len();
    emitter.emit_u32(aarch64::ldr64(0, REG_SP, slots.ptr));
    emitter.emit_u32(aarch64::ldr64(1, REG_SP, slots.len));
    emitter.emit_u32(aarch64::ldr64(2, REG_SP, slots.index));
    emitter.emit_u32(aarch64::cmp_reg64(2, 1));
    let end_branch = emitter.emit_insn(aarch64::b_cond(10, 0));
    emitter.emit_u32(aarch64::add_reg64(3, 2, 2));
    emitter.emit_u32(aarch64::add_reg64(3, 3, 3));
    emitter.emit_u32(aarch64::add_reg64(3, 3, 3));
    emitter.emit_u32(aarch64::add_reg64(4, 0, 3));
    emitter.emit_u32(aarch64::ldr64(5, 4, 0));
    emitter.emit_u32(aarch64::str64(5, REG_SP, slots.binding));
    emitter.emit_u32(aarch64::add_imm64(2, 2, 1));
    emitter.emit_u32(aarch64::str64(2, REG_SP, slots.index));
    let result = lower_struct_expr_into_slots(
        emitter,
        ctx,
        item,
        name,
        &fields,
        functions,
        pending_calls,
        fn_name,
    )
    .and_then(|()| {
        super::lower_stdlib::emit_vec_push_words(emitter, ctx, header_offset, scratch_offset, words)
    });
    if let Some(previous) = previous {
        ctx.locals.insert(binding.clone(), previous);
    } else {
        ctx.locals.remove(binding);
    }
    result?;
    let back_offset = head as i32 - emitter.len() as i32;
    emitter.emit_u32(aarch64::b(back_offset));
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b_cond(10, end_offset));
    for (index, offset) in [header_offset, header_offset + 8, header_offset + 16]
        .into_iter()
        .enumerate()
    {
        emitter.emit_u32(aarch64::ldr64(rd + index as u8, REG_SP, offset));
    }
    Ok(())
}

fn lower_iter_collect(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    args: &[Expr],
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    match &args[0] {
        Expr::Call { callee, args }
            if matches!(callee.as_ref(), Expr::Ident(name) if name == "map") && args.len() == 2 =>
        {
            lower_array_map(
                emitter,
                ctx,
                &args[0],
                &args[1],
                rd,
                functions,
                pending_calls,
                fn_name,
            )
        }
        Expr::Call { callee, args } => super::lower_call::lower_call(
            emitter,
            ctx,
            callee,
            args,
            rd,
            functions,
            pending_calls,
            fn_name,
        ),
        Expr::Field { base, name } => {
            let Expr::Ident(local) = base.as_ref() else {
                return Err(format!(
                    "native-lower: collect field source unsupported in `{fn_name}`"
                ));
            };
            let Some(LocalSlot::Struct { fields, .. }) = ctx.locals.get(local) else {
                return Err(format!(
                    "native-lower: collect source `{local}` is not a struct in `{fn_name}"
                ));
            };
            for (index, suffix) in ["ptr", "len", "cap"].into_iter().enumerate() {
                let key = format!("{name}.{suffix}");
                let Some(offset) = find_field_offset(fields, &key) else {
                    return Err(format!(
                        "native-lower: collect source `{key}` missing in `{fn_name}"
                    ));
                };
                emitter.emit_u32(aarch64::ldr64(rd + index as u8, REG_SP, *offset));
            }
            Ok(())
        }
        Expr::Ident(local) => {
            let Some(LocalSlot::Struct { typ, fields }) = ctx.locals.get(local) else {
                return Err(format!(
                    "native-lower: collect source `{local}` is not a Vec in `{fn_name}"
                ));
            };
            if typ != "Vec" {
                return Err(format!(
                    "native-lower: collect source `{local}` is not a Vec in `{fn_name}"
                ));
            }
            for (index, name) in ["ptr", "len", "cap"].into_iter().enumerate() {
                let Some(offset) = find_field_offset(fields, name) else {
                    return Err(format!(
                        "native-lower: collect source `{local}` missing `{name}` in `{fn_name}"
                    ));
                };
                emitter.emit_u32(aarch64::ldr64(rd + index as u8, REG_SP, *offset));
            }
            Ok(())
        }
        _ => Err(format!(
            "native-lower: collect source unsupported in `{fn_name}"
        )),
    }
}

fn lower_clone(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    source: &Expr,
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    if let Expr::Field { base, name } = source
        && let Expr::Ident(local) = base.as_ref()
        && let Some(LocalSlot::Struct { fields, .. }) = ctx.locals.get(local)
        && find_field_offset(fields, &format!("{name}.ptr")).is_some()
    {
        for (index, suffix) in ["ptr", "len", "cap"].into_iter().enumerate() {
            let key = format!("{name}.{suffix}");
            let offset = find_field_offset(fields, &key).ok_or_else(|| {
                format!("native-lower: clone source `{key}` missing in `{fn_name}`")
            })?;
            emitter.emit_u32(aarch64::ldr64(rd + index as u8, REG_SP, *offset));
        }
        return Ok(());
    }
    lower_expr_into(emitter, ctx, source, rd, functions, pending_calls, fn_name)
}

fn lower_vec_join(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    source: &Expr,
    separator: &Expr,
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    match source {
        Expr::Ident(local) => {
            let Some(LocalSlot::Struct { typ, fields }) = ctx.locals.get(local) else {
                // Not a Vec — emit empty result (ptr=0, len=0)
                emitter.emit_insns(&aarch64::load_i64(rd, 0));
                emitter.emit_insns(&aarch64::load_i64(rd + 1, 0));
                return Ok(());
            };
            if typ != "Vec" {
                // Not a Vec — emit empty result
                emitter.emit_insns(&aarch64::load_i64(rd, 0));
                emitter.emit_insns(&aarch64::load_i64(rd + 1, 0));
                return Ok(());
            }
            for (register, field) in [(0, "ptr"), (1, "len")] {
                let offset = find_field_offset(fields, field).ok_or_else(|| {
                    format!("native-lower: join source `{local}` missing `{field}` in `{fn_name}`")
                })?;
                emitter.emit_u32(aarch64::ldr64(register, REG_SP, *offset));
            }
        }
        Expr::Field { base, name } => {
            let Expr::Ident(local) = base.as_ref() else {
                return Err(format!(
                    "native-lower: join field source unsupported in `{fn_name}`"
                ));
            };
            let Some(LocalSlot::Struct { fields, .. }) = ctx.locals.get(local) else {
                return Err(format!(
                    "native-lower: join source `{local}` is not a struct in `{fn_name}`"
                ));
            };
            for (register, suffix) in [(0, "ptr"), (1, "len")] {
                let key = format!("{name}.{suffix}");
                let offset = find_field_offset(fields, &key).ok_or_else(|| {
                    format!("native-lower: join source `{key}` missing in `{fn_name}`")
                })?;
                emitter.emit_u32(aarch64::ldr64(register, REG_SP, *offset));
            }
        }
        Expr::Call { callee, args } => super::lower_call::lower_call(
            emitter,
            ctx,
            callee,
            args,
            0,
            functions,
            pending_calls,
            fn_name,
        )?,
        _ => {
            return Err(format!(
                "native-lower: join source unsupported in `{fn_name}`"
            ));
        }
    }
    lower_expr_into(
        emitter,
        ctx,
        separator,
        2,
        functions,
        pending_calls,
        fn_name,
    )?;
    emit_stdlib_wrapper_register_call(emitter, ctx, "in_vec_join", rd)
}

fn lower_array_len(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    source: &Expr,
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    match source {
        Expr::Ident(local) => match ctx.locals.get(local) {
            Some(LocalSlot::Array { offsets, .. }) => {
                emitter.emit_insns(&aarch64::load_i64(rd, offsets.len() as i64));
                Ok(())
            }
            Some(LocalSlot::ArrayParam { len_offset, .. }) => {
                emitter.emit_u32(aarch64::ldr64(rd, REG_SP, *len_offset));
                Ok(())
            }
            _ => Err(format!(
                "native-lower: array_len source `{local}` is not an array in `{fn_name}`"
            )),
        },
        Expr::Call { callee, args } => super::lower_call::lower_call(
            emitter,
            ctx,
            callee,
            args,
            rd,
            functions,
            pending_calls,
            fn_name,
        ),
        _ => Err(format!(
            "native-lower: array_len source unsupported in `{fn_name}`"
        )),
    }
}

pub(crate) fn resolve_function_name(
    name: &str,
    functions: &std::collections::HashMap<String, FunctionInfo>,
) -> Option<String> {
    if functions.contains_key(name) {
        return Some(name.to_string());
    }
    name.rfind("::").and_then(|idx| {
        let last = name[idx + 2..].to_string();
        if functions.contains_key(&last) {
            Some(last)
        } else {
            None
        }
    })
}

pub(crate) fn is_resolvable_function_ref(
    expr: &Expr,
    functions: &std::collections::HashMap<String, FunctionInfo>,
) -> bool {
    let Expr::Ident(name) = expr else {
        return false;
    };
    resolve_function_name(name, functions).is_some()
}

pub(crate) fn try_emit_closure_call(
    emitter: &mut CodeEmitter,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    closure_expr: &Expr,
    arg_reg: Option<u8>,
) -> Result<bool, String> {
    let Expr::Ident(name) = closure_expr else {
        return Ok(false);
    };
    let Some(target) = resolve_function_name(name, functions) else {
        return Ok(false);
    };
    if let Some(arg) = arg_reg {
        if arg != 0 {
            emitter.emit_u32(aarch64::mov_reg64(0, arg));
        }
    }
    let site = emitter.len();
    emitter.emit_u32(aarch64::bl(0));
    pending_calls.push(PendingCall { site, target });
    Ok(true)
}

/// Try to lower a recognized stdlib function as an inline intrinsic.
/// Returns Ok(true) if handled, Ok(false) if not recognized (caller should fall back).
pub(crate) fn lower_stdlib_call(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target: &str,
    args: &[Expr],
    rd: u8,
    functions: &std::collections::HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<bool, String> {
    // Recognize std::env / std::fs wrappers first, before generic suffix matching.
    let cleaned: String = target.chars().filter(|&c| c != ' ').collect();
    // Inlang uses kebab-case; Rust/polyglot fronts still emit snake_case method
    // names. Normalize bare surface names (not `std::…` paths) for matching.
    let cleaned_kebab = if cleaned.contains("::") {
        cleaned.clone()
    } else {
        cleaned.replace('_', "-")
    };
    match cleaned_kebab.as_str() {
        "display" if args.len() == 1 => {
            lower_expr_into(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "join" | "Vec::join" if args.len() == 2 => {
            lower_vec_join(
                emitter,
                ctx,
                &args[0],
                &args[1],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "clone" if args.len() == 1 => {
            lower_clone(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "into-iter" if args.len() == 1 => {
            lower_array_into_iter(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "collect" if args.len() == 1 => {
            lower_iter_collect(emitter, ctx, args, rd, functions, pending_calls, fn_name)?;
            return Ok(true);
        }
        "chain" if args.len() == 2 => {
            lower_iter_chain(emitter, ctx, args, rd, functions, pending_calls, fn_name)?;
            return Ok(true);
        }
        "std::iter::once" if args.len() == 1 => {
            lower_iter_once(emitter, ctx, args, rd, functions, pending_calls, fn_name)?;
            return Ok(true);
        }
        "std::env::var" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_var",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::env::temp_dir" if args.is_empty() => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_temp_dir",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::env::current_dir" if args.is_empty() => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_current_dir",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::fs::read_to_string" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_read_to_string",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::fs::exists" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_exists",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::fs::write" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_write",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::fs::create_dir" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_create_dir",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::fs::remove_file" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_remove_file",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::env::set_var" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_set_var",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "std::env::remove_var" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_remove_var",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        // Built-in print: dispatch to string or int wrapper based on the argument.
        "print" if args.len() == 1 => {
            let wrapper = if matches!(&args[0], Expr::IntLit(_) | Expr::BoolLit(_)) {
                "in_print_int"
            } else {
                "in_print"
            };
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                wrapper,
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        _ => {}
    }

    // Bare function names used by the .in compiler bootstrap and stdlib imports.
    // These are matched before the suffix-based method dispatch below.
    // `cleaned_kebab` accepts both Inlang kebab-case and Rust snake_case.
    match cleaned_kebab.as_str() {
        "read-file" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_read_to_string",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "write-file" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_write",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "fs-exists" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_exists",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "create-dir" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_create_dir",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "remove-file" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_fs_remove_file",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "process-run" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_process_run",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "env-get" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_var",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "env-set" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_set_var",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "env-has" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_has",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "env-temp-dir" if args.is_empty() => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_temp_dir",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "env-current-dir" if args.is_empty() => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_env_current_dir",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "path-join" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_path_join",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "path-dirname" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_path_dirname",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "path-basename" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_path_basename",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "path-extname" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_path_extname",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "path-normalize" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_path_normalize",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-concat" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_concat",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-eq" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_eq",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "json-stringify" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_json_stringify",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-table-has" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_table_has",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-table-get-int" if args.len() == 3 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_table_get_int",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-contains" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_contains",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-starts-with" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_starts_with",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-ends-with" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_ends_with",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-len" if args.len() == 1 => {
            // An instring's byte length is the first word of its header, so this
            // needs no runtime call: load the pointer, then the length.
            lower_expr_into(emitter, ctx, &args[0], rd, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
            return Ok(true);
        }
        "str-trim" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_trim",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-split-lines" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_split_lines",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-split-spaces" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_split_spaces",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-tokenize-expr" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_tokenize_expr",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-to-int" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_to_int",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-is-int" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_is_int",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-index-of" if args.len() == 2 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_index_of",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "str-slice" if args.len() == 3 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_str_slice",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "to-string" if args.len() == 1 => {
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                "in_int_to_string",
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "array-len" if args.len() == 1 => {
            lower_array_len(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            return Ok(true);
        }
        "array-push" if args.len() == 2 => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            lower_expr_into(emitter, ctx, &args[1], 1, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(2, 0, 0));
            emitter.emit_u32(aarch64::add_imm64(3, 0, 16));
            emitter.emit_u32(aarch64::str64_reg_offset(1, 3, 2));
            emitter.emit_u32(aarch64::add_imm64(2, 2, 1));
            emitter.emit_u32(aarch64::str64(2, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(0, 0));
            return Ok(true);
        }
        _ => {}
    }

    // Strip type qualifier prefix if present (e.g., "Option::unwrap" → "unwrap").
    // Normalize snake_case method names from Rust front to kebab-case.
    let base_owned = target
        .rsplit("::")
        .next()
        .unwrap_or(target)
        .replace('_', "-");
    match base_owned.as_str() {
        // String/Path/str::len → length is at offset 8 (after ptr at offset 0)
        "len"
            if args.len() == 1
                && (target.contains("String")
                    || target.contains("Path")
                    || target.contains("str")) =>
        {
            lower_expr_into(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            emitter.emit_u32(aarch64::ldr64(rd, rd, 8));
            Ok(true)
        }
        // String/Path/str::is_empty → compare len at offset 8 with 0
        "is-empty"
            if args.len() == 1
                && (target.contains("String")
                    || target.contains("Path")
                    || target.contains("str")) =>
        {
            lower_expr_into(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            emitter.emit_u32(aarch64::ldr64(rd, rd, 8));
            emitter.emit_u32(aarch64::cmp_reg64(rd, REG_XZR));
            lower_comparison_result(emitter, rd, "==", false)?;
            Ok(true)
        }
        // String/Path/str::to_string → return the receiver as a slice
        "to-string"
            if args.len() == 1
                && (target.contains("String")
                    || target.contains("Path")
                    || target.contains("str")) =>
        {
            lower_expr_into(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            emitter.emit_u32(aarch64::ldr64(rd + 1, rd, 8));
            emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
            Ok(true)
        }
        "to-path-buf" if args.len() == 1 && target.contains("Path") => {
            let Expr::Ident(name) = &args[0] else {
                return Err(format!(
                    "native-lower: Path::to_path_buf receiver must be a local in `{fn_name}`"
                ));
            };
            let Some(LocalSlot::Struct { typ, fields }) = ctx.locals.get(name) else {
                return Err(format!(
                    "native-lower: Path::to_path_buf receiver `{name}` is not a Path local in `{fn_name}`"
                ));
            };
            if typ != "Path" {
                return Err(format!(
                    "native-lower: Path::to_path_buf receiver `{name}` has type `{typ}` in `{fn_name}`"
                ));
            }
            let Some(&ptr_offset) = find_field_offset(fields, "ptr") else {
                return Err(format!(
                    "native-lower: Path local `{name}` is missing `ptr` in `{fn_name}`"
                ));
            };
            let Some(&len_offset) = find_field_offset(fields, "len") else {
                return Err(format!(
                    "native-lower: Path local `{name}` is missing `len` in `{fn_name}`"
                ));
            };
            emitter.emit_u32(aarch64::ldr64(rd, REG_SP, ptr_offset));
            emitter.emit_u32(aarch64::ldr64(rd + 1, REG_SP, len_offset));
            emitter.emit_u32(aarch64::ldr64(rd + 2, REG_SP, len_offset));
            Ok(true)
        }
        // String/str::as_str → return slice {ptr, len}
        "as-str" if args.len() == 1 && (target.contains("String") || target.contains("str")) => {
            lower_expr_into(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            emitter.emit_u32(aarch64::ldr64(rd + 1, rd, 8));
            emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
            Ok(true)
        }
        // PathBuf::as_path / Path::as_path → return slice {ptr, len}
        "as-path" if args.len() == 1 && target.contains("Path") => {
            lower_expr_into(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            emitter.emit_u32(aarch64::ldr64(rd + 1, rd, 8));
            emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
            Ok(true)
        }
        // Path::to_string_lossy → return slice {ptr, len} as a Cow<str> pass-through
        "to-string-lossy" if args.len() == 1 && target.contains("Path") => {
            lower_expr_into(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            emitter.emit_u32(aarch64::ldr64(rd + 1, rd, 8));
            emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
            Ok(true)
        }
        // String::from_utf8_lossy → return slice {ptr, len} as a Cow<str> pass-through
        "from-utf8-lossy" if args.len() == 1 && target.contains("String") => {
            lower_expr_into(
                emitter,
                ctx,
                &args[0],
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            emitter.emit_u32(aarch64::ldr64(rd + 1, rd, 8));
            emitter.emit_u32(aarch64::ldr64(rd, rd, 0));
            Ok(true)
        }
        // String::push_str → fast-path append when len + arg_len <= cap
        "push-str" if args.len() == 2 && target.contains("String") => {
            lower_string_push_str(emitter, ctx, args, rd, functions, pending_calls, fn_name)?;
            Ok(true)
        }
        // String::contains / starts_with / ends_with → delegate to native_stdlib wrapper
        "contains" | "starts-with" | "ends-with"
            if args.len() == 2 && target.contains("String") =>
        {
            let wrapper = match base_owned.as_str() {
                "contains" => "in_str_contains",
                "starts-with" => "in_str_starts_with",
                "ends-with" => "in_str_ends_with",
                _ => return Err(format!("Unrecognized string method: {}", base_owned)),
            };
            emit_stdlib_wrapper_call(
                emitter,
                ctx,
                wrapper,
                args,
                rd,
                functions,
                pending_calls,
                fn_name,
            )?;
            Ok(true)
        }
        // Vec::len → ldr x0, [x0]  (length is at offset 0)
        "len" if args.len() == 1 => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(0, 0, 0));
            Ok(true)
        }
        // Vec::is_empty → ldr x0, [x0]; cmp x0, #0; cset x0, eq
        "is-empty" if args.len() == 1 => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(0, 0, 0));
            emitter.emit_u32(aarch64::cmp_reg64(0, REG_XZR));
            let tb = emitter.emit_insn(aarch64::b_cond(0, 0)); // B.EQ → true
            emitter.emit_insns(&aarch64::load_i64(0, 0));
            let eb = emitter.emit_insn(aarch64::b(0));
            let to = emitter.len() as i32 - tb as i32;
            emitter.patch_u32(tb, aarch64::b_cond(0, to));
            emitter.emit_insns(&aarch64::load_i64(0, 1));
            let eo = emitter.len() as i32 - eb as i32;
            emitter.patch_u32(eb, aarch64::b(eo));
            Ok(true)
        }
        // Vec::new → return pointer to empty vec (0 for ptr, 0 for len, 0 for cap)
        "new" if args.is_empty() && (target.contains("Vec") || target.contains("vec")) => {
            emitter.emit_insns(&aarch64::load_i64(0, 0));
            emitter.emit_insns(&aarch64::load_i64(1, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 0));
            Ok(true)
        }
        "extend" if args.len() == 2 && target.contains("Vec") => {
            lower_vec_extend(emitter, ctx, args, rd, functions, pending_calls, fn_name)?;
            Ok(true)
        }
        // Vec::with_capacity(n) → return empty vec like Vec::new (push handles allocation)
        "with-capacity"
            if args.len() == 1 && (target.contains("Vec") || target.contains("vec")) =>
        {
            emitter.emit_insns(&aarch64::load_i64(0, 0));
            emitter.emit_insns(&aarch64::load_i64(1, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 0));
            Ok(true)
        }
        // Vec::push → store value at [vec + 2 + len]
        "push" if args.len() == 2 && (target.contains("Vec") || target.contains("vec")) => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?; // ptr
            lower_expr_into(emitter, ctx, &args[1], 1, functions, pending_calls, fn_name)?; // val
            emitter.emit_u32(aarch64::ldr64(2, 0, 0)); // len
            emitter.emit_u32(aarch64::add_imm64(3, 0, 16)); // &vec.data[0]
            emitter.emit_u32(aarch64::str64_reg_offset(1, 3, 2)); // data[len] = val
            emitter.emit_u32(aarch64::add_imm64(2, 2, 1)); // len += 1
            emitter.emit_u32(aarch64::str64(2, 0, 0)); // store len
            emitter.emit_insns(&aarch64::load_i64(0, 0)); // return old len
            Ok(true)
        }
        // Option::is_some → cmp tag, #1; cset x0, eq
        "is-some" if args.len() == 1 => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            // Option's tag is at offset 0 (first field)
            emitter.emit_u32(aarch64::ldr64(0, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(1, 1));
            emitter.emit_u32(aarch64::cmp_reg64(0, 1));
            let tb = emitter.emit_insn(aarch64::b_cond(0, 0)); // B.EQ → cmp result == 1 → is_some
            emitter.emit_insns(&aarch64::load_i64(0, 0));
            let eb = emitter.emit_insn(aarch64::b(0));
            let to = emitter.len() as i32 - tb as i32;
            emitter.patch_u32(tb, aarch64::b_cond(0, to));
            emitter.emit_insns(&aarch64::load_i64(0, 1));
            let eo = emitter.len() as i32 - eb as i32;
            emitter.patch_u32(eb, aarch64::b(eo));
            Ok(true)
        }
        // Option::is_none → cmp tag, #0; cset x0, eq
        "is-none" if args.len() == 1 => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(0, 0, 0));
            emitter.emit_u32(aarch64::cmp_reg64(0, REG_XZR));
            let tb = emitter.emit_insn(aarch64::b_cond(0, 0));
            emitter.emit_insns(&aarch64::load_i64(0, 0));
            let eb = emitter.emit_insn(aarch64::b(0));
            let to = emitter.len() as i32 - tb as i32;
            emitter.patch_u32(tb, aarch64::b_cond(0, to));
            emitter.emit_insns(&aarch64::load_i64(0, 1));
            let eo = emitter.len() as i32 - eb as i32;
            emitter.patch_u32(eb, aarch64::b(eo));
            Ok(true)
        }
        // Option::unwrap → if tag != 1, panic; return value
        "unwrap" if target.contains("Option") => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            // Load tag at offset 0
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 1));
            emitter.emit_u32(aarch64::cmp_reg64(1, 2));
            // If tag != 1 (Some), jump to panic stub
            let pb = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE → panic
            // Return value at offset 8 (for Option<T> = {tag, value})
            emitter.emit_u32(aarch64::ldr64(0, 0, 8));
            let end = emitter.emit_insn(aarch64::b(0));
            // Panic path: load 0
            let po = emitter.len() as i32 - pb as i32;
            emitter.patch_u32(pb, aarch64::b_cond(1, po));
            emitter.emit_insns(&aarch64::load_i64(0, 0));
            let eo = emitter.len() as i32 - end as i32;
            emitter.patch_u32(end, aarch64::b(eo));
            Ok(true)
        }
        // Result::is_ok → cmp tag, #0; cset x0, eq
        "is-ok" if args.len() == 1 && target.contains("Result") => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(0, 0, 0));
            emitter.emit_u32(aarch64::cmp_reg64(0, REG_XZR));
            let tb = emitter.emit_insn(aarch64::b_cond(0, 0));
            emitter.emit_insns(&aarch64::load_i64(0, 0));
            let eb = emitter.emit_insn(aarch64::b(0));
            let to = emitter.len() as i32 - tb as i32;
            emitter.patch_u32(tb, aarch64::b_cond(0, to));
            emitter.emit_insns(&aarch64::load_i64(0, 1));
            let eo = emitter.len() as i32 - eb as i32;
            emitter.patch_u32(eb, aarch64::b(eo));
            Ok(true)
        }
        // Option::map → if Some, apply closure to value and wrap in Some; else None
        "map" if args.len() == 2 && target.contains("Option") => {
            if !is_resolvable_function_ref(&args[1], functions) {
                return Ok(false);
            }
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 1));
            emitter.emit_u32(aarch64::cmp_reg64(1, 2));
            let none_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Some path
            emitter.emit_u32(aarch64::ldr64(3, 0, 8)); // value
            emitter.emit_u32(aarch64::str64(0, REG_SP, ctx.binop_temp)); // save pointer
            if !try_emit_closure_call(emitter, functions, pending_calls, &args[1], Some(3))? {
                return Ok(false);
            }
            emitter.emit_u32(aarch64::ldr64(2, REG_SP, ctx.binop_temp)); // load pointer
            emitter.emit_u32(aarch64::str64(0, 2, 8)); // store result
            emitter.emit_insns(&aarch64::load_i64(1, 1));
            emitter.emit_u32(aarch64::str64(1, 2, 0)); // tag = Some
            emitter.emit_u32(aarch64::mov_reg64(0, 2));
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let none_offset = emitter.len() as i32 - none_branch as i32;
            emitter.patch_u32(none_branch, aarch64::b_cond(1, none_offset));
            // None path
            emitter.emit_insns(&aarch64::load_i64(1, 0));
            emitter.emit_u32(aarch64::str64(1, 0, 0)); // tag = None
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Option::and_then → if Some, call closure with value and return its result; else None
        "and-then" if args.len() == 2 && target.contains("Option") => {
            if !is_resolvable_function_ref(&args[1], functions) {
                return Ok(false);
            }
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 1));
            emitter.emit_u32(aarch64::cmp_reg64(1, 2));
            let none_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Some path
            emitter.emit_u32(aarch64::ldr64(0, 0, 8)); // value
            if !try_emit_closure_call(emitter, functions, pending_calls, &args[1], Some(0))? {
                return Ok(false);
            }
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let none_offset = emitter.len() as i32 - none_branch as i32;
            emitter.patch_u32(none_branch, aarch64::b_cond(1, none_offset));
            // None path: X0 is already the original pointer
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Option::unwrap_or → return value if Some, else default
        "unwrap-or" if args.len() == 2 && target.contains("Option") => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 1));
            emitter.emit_u32(aarch64::cmp_reg64(1, 2));
            let none_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Some path
            emitter.emit_u32(aarch64::ldr64(0, 0, 8));
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let none_offset = emitter.len() as i32 - none_branch as i32;
            emitter.patch_u32(none_branch, aarch64::b_cond(1, none_offset));
            // None path
            lower_expr_into(emitter, ctx, &args[1], 0, functions, pending_calls, fn_name)?;
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Option::unwrap_or_else → return value if Some, else call closure
        "unwrap-or-else" if args.len() == 2 && target.contains("Option") => {
            if !is_resolvable_function_ref(&args[1], functions) {
                return Ok(false);
            }
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 1));
            emitter.emit_u32(aarch64::cmp_reg64(1, 2));
            let none_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Some path
            emitter.emit_u32(aarch64::ldr64(0, 0, 8));
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let none_offset = emitter.len() as i32 - none_branch as i32;
            emitter.patch_u32(none_branch, aarch64::b_cond(1, none_offset));
            // None path
            if !try_emit_closure_call(emitter, functions, pending_calls, &args[1], None)? {
                return Ok(false);
            }
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Option::ok_or_else → Some → Ok; None → Err(closure())
        "ok-or-else" if args.len() == 2 && target.contains("Option") => {
            if !is_resolvable_function_ref(&args[1], functions) {
                return Ok(false);
            }
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 1));
            emitter.emit_u32(aarch64::cmp_reg64(1, 2));
            let none_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Some path
            emitter.emit_u32(aarch64::str64(REG_XZR, 0, 0)); // tag = Ok
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let none_offset = emitter.len() as i32 - none_branch as i32;
            emitter.patch_u32(none_branch, aarch64::b_cond(1, none_offset));
            // None path
            emitter.emit_u32(aarch64::str64(0, REG_SP, ctx.binop_temp)); // save pointer
            if !try_emit_closure_call(emitter, functions, pending_calls, &args[1], None)? {
                return Ok(false);
            }
            emitter.emit_u32(aarch64::ldr64(2, REG_SP, ctx.binop_temp)); // load pointer
            emitter.emit_u32(aarch64::str64(0, 2, 8)); // store error
            emitter.emit_insns(&aarch64::load_i64(1, 1));
            emitter.emit_u32(aarch64::str64(1, 2, 0)); // tag = Err
            emitter.emit_u32(aarch64::mov_reg64(0, 2));
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Result::map → if Ok, apply closure to value and keep Ok; else Err
        "map" if args.len() == 2 && target.contains("Result") => {
            if !is_resolvable_function_ref(&args[1], functions) {
                return Ok(false);
            }
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_u32(aarch64::cmp_reg64(1, REG_XZR));
            let err_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Ok path
            emitter.emit_u32(aarch64::ldr64(3, 0, 8)); // value
            emitter.emit_u32(aarch64::str64(0, REG_SP, ctx.binop_temp)); // save pointer
            if !try_emit_closure_call(emitter, functions, pending_calls, &args[1], Some(3))? {
                return Ok(false);
            }
            emitter.emit_u32(aarch64::ldr64(2, REG_SP, ctx.binop_temp)); // load pointer
            emitter.emit_u32(aarch64::str64(0, 2, 8)); // store result
            emitter.emit_u32(aarch64::str64(REG_XZR, 2, 0)); // tag = Ok
            emitter.emit_u32(aarch64::mov_reg64(0, 2));
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let err_offset = emitter.len() as i32 - err_branch as i32;
            emitter.patch_u32(err_branch, aarch64::b_cond(1, err_offset));
            // Err path
            emitter.emit_insns(&aarch64::load_i64(1, 1));
            emitter.emit_u32(aarch64::str64(1, 0, 0)); // tag = Err
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Result::map_err → if Err, apply closure to error and keep Err; else Ok
        "map-err" if args.len() == 2 && target.contains("Result") => {
            if !is_resolvable_function_ref(&args[1], functions) {
                return Ok(false);
            }
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_insns(&aarch64::load_i64(2, 1));
            emitter.emit_u32(aarch64::cmp_reg64(1, 2));
            let err_branch = emitter.emit_insn(aarch64::b_cond(0, 0)); // B.EQ
            // Ok path
            emitter.emit_u32(aarch64::str64(REG_XZR, 0, 0)); // tag = Ok
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let err_offset = emitter.len() as i32 - err_branch as i32;
            emitter.patch_u32(err_branch, aarch64::b_cond(0, err_offset));
            // Err path
            emitter.emit_u32(aarch64::ldr64(3, 0, 8)); // error value
            emitter.emit_u32(aarch64::str64(0, REG_SP, ctx.binop_temp)); // save pointer
            if !try_emit_closure_call(emitter, functions, pending_calls, &args[1], Some(3))? {
                return Ok(false);
            }
            emitter.emit_u32(aarch64::ldr64(2, REG_SP, ctx.binop_temp)); // load pointer
            emitter.emit_u32(aarch64::str64(0, 2, 8)); // store error
            emitter.emit_insns(&aarch64::load_i64(1, 1));
            emitter.emit_u32(aarch64::str64(1, 2, 0)); // tag = Err
            emitter.emit_u32(aarch64::mov_reg64(0, 2));
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Result::and_then → if Ok, call closure with value and return its result; else Err
        "and-then" if args.len() == 2 && target.contains("Result") => {
            if !is_resolvable_function_ref(&args[1], functions) {
                return Ok(false);
            }
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_u32(aarch64::cmp_reg64(1, REG_XZR));
            let err_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Ok path
            emitter.emit_u32(aarch64::ldr64(0, 0, 8)); // value
            if !try_emit_closure_call(emitter, functions, pending_calls, &args[1], Some(0))? {
                return Ok(false);
            }
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let err_offset = emitter.len() as i32 - err_branch as i32;
            emitter.patch_u32(err_branch, aarch64::b_cond(1, err_offset));
            // Err path: X0 already pointer
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Result::unwrap_or → return value if Ok, else default
        "unwrap-or" if args.len() == 2 && target.contains("Result") => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_u32(aarch64::cmp_reg64(1, REG_XZR));
            let err_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Ok path
            emitter.emit_u32(aarch64::ldr64(0, 0, 8));
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let err_offset = emitter.len() as i32 - err_branch as i32;
            emitter.patch_u32(err_branch, aarch64::b_cond(1, err_offset));
            // Err path
            lower_expr_into(emitter, ctx, &args[1], 0, functions, pending_calls, fn_name)?;
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // Result::unwrap_or_else → return value if Ok, else call closure
        "unwrap-or-else" if args.len() == 2 && target.contains("Result") => {
            if !is_resolvable_function_ref(&args[1], functions) {
                return Ok(false);
            }
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_u32(aarch64::cmp_reg64(1, REG_XZR));
            let err_branch = emitter.emit_insn(aarch64::b_cond(1, 0)); // B.NE
            // Ok path
            emitter.emit_u32(aarch64::ldr64(0, 0, 8));
            let end_branch = emitter.emit_insn(aarch64::b(0));
            let err_offset = emitter.len() as i32 - err_branch as i32;
            emitter.patch_u32(err_branch, aarch64::b_cond(1, err_offset));
            // Err path
            if !try_emit_closure_call(emitter, functions, pending_calls, &args[1], None)? {
                return Ok(false);
            }
            let end_offset = emitter.len() as i32 - end_branch as i32;
            emitter.patch_u32(end_branch, aarch64::b(end_offset));
            Ok(true)
        }
        // std::mem::take → swap with 0
        "take" if args.len() == 1 && target.contains("mem") => {
            lower_expr_into(emitter, ctx, &args[0], 0, functions, pending_calls, fn_name)?;
            // Load old value, store 0 in its place
            emitter.emit_u32(aarch64::ldr64(1, 0, 0));
            emitter.emit_u32(aarch64::str64(REG_XZR, 0, 0));
            emitter.emit_u32(aarch64::mov_reg64(0, 1));
            Ok(true)
        }
        // Default: not a recognized stdlib intrinsic
        _ => Ok(false),
    }
}
