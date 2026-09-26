//! Core IR → AArch64 call lowering.

use super::lower_expr::lower_expr_into;
use super::lower_stdlib;
use super::lower_stmt::lower_struct_expr_into_slots;
use super::{
    FunctionInfo, LocalSlot, LowerCtx, PendingCall, PendingInrtCall, TL_EXTERNAL_REFS,
    TL_NATIVE_MODE, base_struct_name, find_field_offset, is_native_scalar_type, native_link_name,
    native_param_abi_slots, native_struct_fields,
};
use crate::core_ir::{Expr, Typ};
use crate::inrt::{inrt_builtin_param_slots, is_inrt_builtin};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::HashMap;

pub(crate) fn lower_call(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    callee: &Expr,
    args: &[Expr],
    rd: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let Expr::Ident(target) = callee else {
        // Non-Ident callee (method call, chained): emit 0 and continue
        emitter.emit_insns(&aarch64::load_i64(rd, 0));
        return Ok(());
    };
    if is_inrt_builtin(target) {
        return lower_inrt_call(emitter, ctx, target, args, rd, fn_name);
    }
    // ponytail: try stdlib intrinsic lowering before external ref fallback
    if lower_stdlib::lower_stdlib_call(
        emitter,
        ctx,
        target,
        args,
        rd,
        functions,
        pending_calls,
        fn_name,
    )? {
        return Ok(());
    }
    if !functions.contains_key(target) {
        #[cfg(debug_assertions)]
        eprintln!(
            "[TRACE] EXTERNAL path for target={target}, all fns: {:?}",
            functions.keys().collect::<Vec<_>>()
        );
        // Load args into registers
        for (i, arg) in args.iter().enumerate() {
            if i > 7 {
                break;
            }
            lower_expr_into(
                emitter,
                ctx,
                arg,
                i as u8,
                functions,
                pending_calls,
                fn_name,
            )?;
        }
        let is_native = TL_NATIVE_MODE.with(|m| *m.borrow());
        if is_native {
            // Native mode: emit BL 0 + record external symbol reference
            // Map Rust function names to C/mangled linker names
            let link_name = native_link_name(target);
            let call_site = emitter.len() as u32;
            emitter.emit_u32(aarch64::bl(0));
            TL_EXTERNAL_REFS.with(|refs| refs.borrow_mut().push((call_site, link_name)));
        } else if let Some(native_ptr) =
            crate::native_emit::native_link::resolve_native_fn(match target.as_str() {
                "std::process::exit" | "std::process::abort" => "exit",
                _ => target,
            })
        {
            // JIT mode: use dlsym'd address
            emitter.emit_insns(&aarch64::load_i64(15, native_ptr as usize as i64));
            emitter.emit_u32(0xD63F_01E0u32 | (15 << 5)); // BLR X15
        } else {
            // JIT mode with no dlsym'd symbol. Record the call so the patch pass
            // emits a trap and reports it, rather than fabricating a return 0.
            let call_site = emitter.len() as u32;
            emitter.emit_u32(aarch64::bl(0));
            pending_calls.push(PendingCall {
                site: call_site,
                target: target.clone(),
            });
        }
        if rd != 0 {
            emitter.emit_u32(aarch64::mov_reg64(rd, 0));
        }
        return Ok(());
    }
    let Some(target_info) = functions.get(target) else {
        return Err(format!("native-lower: function `{target}` not found"));
    };
    let abi_arg_count = native_param_abi_slots(&target_info.params, ctx.structs, target)?;
    if abi_arg_count > 32 {
        return Err(format!(
            "native-lower: function `{target}` requires {abi_arg_count} ABI argument slots, only 32 are supported"
        ));
    }
    // ponytail: allow mismatched argument counts (rust front generates dup functions with varying sigs)
    let max_args = args.len().min(target_info.params.len());

    let temp_base = ctx.acquire_call_arg_temps(fn_name)?;
    let mut reg = 0u8;
    for (arg, (param_name, typ)) in args.iter().take(max_args).zip(&target_info.params) {
        let first_reg = reg;
        reg = lower_call_arg(
            emitter,
            ctx,
            arg,
            typ,
            reg,
            functions,
            pending_calls,
            fn_name,
            param_name,
        )?;
        for current in first_reg..reg.min(8) {
            emitter.emit_u32(aarch64::str64(
                current,
                aarch64::REG_SP,
                ctx.call_arg_temps[temp_base + current as usize],
            ));
        }
    }
    // Load first 8 args into registers x0-x7
    for current in 0..reg.min(8) {
        emitter.emit_u32(aarch64::ldr64(
            current,
            aarch64::REG_SP,
            ctx.call_arg_temps[temp_base + current as usize],
        ));
    }
    // Stack-based args (beyond 8): allocate stack space and store
    let stack_slots = if reg > 8 { reg as usize - 8 } else { 0 };
    if stack_slots > 0 {
        emitter.emit_u32(aarch64::sub_imm64(
            aarch64::REG_SP,
            aarch64::REG_SP,
            stack_slots as u16 * 8,
        ));
        for i in 0..stack_slots {
            let slot_reg = 8 + i as u8;
            emitter.emit_u32(aarch64::str64(slot_reg, aarch64::REG_SP, i as u32 * 8));
        }
    }

    let call_site = emitter.len();
    emitter.emit_u32(aarch64::bl(0));
    // Restore SP after stack-based args
    if stack_slots > 0 {
        emitter.emit_u32(aarch64::add_imm64(
            aarch64::REG_SP,
            aarch64::REG_SP,
            stack_slots as u16 * 8,
        ));
    }
    pending_calls.push(PendingCall {
        site: call_site,
        target: target.clone(),
    });
    ctx.release_call_arg_temps();

    if rd != 0 {
        emitter.emit_u32(aarch64::mov_reg64(rd, 0));
    }
    Ok(())
}

pub(crate) fn lower_inrt_call(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    target: &str,
    args: &[Expr],
    rd: u8,
    fn_name: &str,
) -> Result<(), String> {
    let expected = inrt_builtin_param_slots(target)
        .ok_or_else(|| format!("native-lower: unknown inrt builtin `{target}` in `{fn_name}`"))?;
    if args.len() != expected {
        return Err(format!(
            "native-lower: inrt call arity mismatch for `{target}` in `{fn_name}` (expected {expected} arg(s), got {})",
            args.len()
        ));
    }

    for (i, arg) in args.iter().enumerate() {
        let reg = i as u8;
        match arg {
            Expr::IntLit(v) => {
                emitter.emit_insns(&aarch64::load_i64(reg, *v));
            }
            Expr::BoolLit(v) => {
                emitter.emit_insns(&aarch64::load_i64(reg, i64::from(*v)));
            }
            Expr::StringLit(v) if v.is_empty() => {
                emitter.emit_insns(&aarch64::load_i64(reg, 0));
            }
            Expr::StringLit(v) => {
                let id = ctx.string_id(v)?;
                let adr_site = emitter.emit_insn(aarch64::adr(reg, 0));
                ctx.pending_strings.push(super::PendingString {
                    adr_site,
                    string_index: id,
                    rd: reg,
                });
            }
            Expr::Ident(name) => {
                if let Some(offset) = ctx.params.get(name) {
                    emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, *offset));
                } else if let Some(LocalSlot::Scalar(offset)) = ctx.locals.get(name) {
                    emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, *offset));
                } else {
                    return Err(format!(
                        "native-lower: inrt call references unknown param/local `{name}` in `{fn_name}`"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "native-lower: unsupported inrt call arg expression in `{fn_name}`"
                ));
            }
        }
    }

    let call_site = emitter.len();
    emitter.emit_u32(aarch64::bl(0));
    ctx.pending_inrt_calls.push(PendingInrtCall {
        site: call_site,
        target: target.to_string(),
    });

    if rd != 0 {
        emitter.emit_u32(aarch64::mov_reg64(rd, 0));
    }
    Ok(())
}

pub(crate) fn lower_call_arg(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    arg: &Expr,
    typ: &Typ,
    reg: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
    param_name: &str,
) -> Result<u8, String> {
    match typ {
        Typ::Int | Typ::Bool | Typ::String | Typ::Float => {
            lower_expr_into(emitter, ctx, arg, reg, functions, pending_calls, fn_name)?;
            Ok(reg + 1)
        }
        Typ::Named(struct_name) => {
            // ponytail: "self" parameter in impl methods is always &self (reference)
            if param_name == "self" {
                // Pass pointer to struct by emitting address of first field into reg
                lower_struct_ptr_arg(emitter, ctx, arg, reg, functions, pending_calls, fn_name)
            } else {
                lower_struct_call_arg(
                    emitter,
                    ctx,
                    arg,
                    struct_name,
                    reg,
                    functions,
                    pending_calls,
                    fn_name,
                )
            }
        }
        Typ::Array(elem) => lower_array_call_arg(emitter, ctx, arg, elem, reg, fn_name),
        Typ::Vector(elem) => lower_vector_call_arg(
            emitter,
            ctx,
            arg,
            elem,
            reg,
            functions,
            pending_calls,
            fn_name,
        ),
        Typ::Void => Ok(reg), // Void params need no registers
        _ => Err(format!(
            "native-lower: unsupported parameter type `{typ:?}` for argument `{param_name}` in `{fn_name}`"
        )),
    }
}

fn lower_vector_call_arg(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    arg: &Expr,
    elem: &Typ,
    reg: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<u8, String> {
    let (Typ::Named(struct_name), Expr::ArrayLit(items)) = (elem, arg) else {
        return lower_struct_call_arg(
            emitter,
            ctx,
            arg,
            "Vec",
            reg,
            functions,
            pending_calls,
            fn_name,
        );
    };
    let Some(header_offset) = ctx.vec_literal_header_offset else {
        return Err(format!(
            "native-lower: missing Vec argument header in `{fn_name}`"
        ));
    };
    lower_aggregate_vector_literal_into_slots(
        emitter,
        ctx,
        items,
        struct_name,
        header_offset,
        header_offset + 8,
        header_offset + 16,
        functions,
        pending_calls,
        fn_name,
    )?;
    for (index, offset) in [header_offset, header_offset + 8, header_offset + 16]
        .into_iter()
        .enumerate()
    {
        emitter.emit_u32(aarch64::ldr64(reg + index as u8, aarch64::REG_SP, offset));
    }
    Ok(reg + 3)
}

pub(crate) fn lower_aggregate_vector_literal_into_slots(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    items: &[Expr],
    struct_name: &str,
    ptr_offset: u32,
    len_offset: u32,
    cap_offset: u32,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<(), String> {
    let Some((scratch_offset, scratch_words)) = ctx.aggregate_vector_scratch else {
        return Err(format!(
            "native-lower: missing aggregate Vec scratch space in `{fn_name}`"
        ));
    };
    let words = native_param_abi_slots(
        &[("value".to_string(), Typ::Named(struct_name.to_string()))],
        ctx.structs,
        fn_name,
    )?;
    if words > scratch_words {
        return Err(format!(
            "native-lower: aggregate Vec scratch space is too small in `{fn_name}`"
        ));
    }
    for offset in [ptr_offset, len_offset, cap_offset] {
        emitter.emit_u32(aarch64::str64(aarch64::REG_XZR, aarch64::REG_SP, offset));
    }
    let fields = aggregate_scratch_fields(ctx, struct_name, scratch_offset, fn_name)?;
    for item in items {
        lower_struct_expr_into_slots(
            emitter,
            ctx,
            item,
            struct_name,
            &fields,
            functions,
            pending_calls,
            fn_name,
        )?;
        lower_stdlib::emit_vec_push_words(emitter, ptr_offset, scratch_offset, words)?;
    }
    Ok(())
}

pub(crate) fn aggregate_scratch_fields(
    ctx: &LowerCtx<'_>,
    struct_name: &str,
    base_offset: u32,
    fn_name: &str,
) -> Result<HashMap<String, u32>, String> {
    fn append_fields(
        fields: &mut HashMap<String, u32>,
        structs: &HashMap<String, Vec<(String, Typ)>>,
        typ: &Typ,
        prefix: &str,
        next_offset: &mut u32,
        fn_name: &str,
    ) -> Result<(), String> {
        match typ {
            Typ::Int | Typ::Bool | Typ::String | Typ::Float => {
                fields.insert(prefix.to_string(), *next_offset);
                *next_offset += 8;
            }
            Typ::Vector(_) => {
                for name in ["ptr", "len", "cap"] {
                    fields.insert(format!("{prefix}.{name}"), *next_offset);
                    *next_offset += 8;
                }
            }
            Typ::Named(name) => {
                let nested = native_struct_fields(structs, name, fn_name)?;
                for (field, field_typ) in nested {
                    append_fields(
                        fields,
                        structs,
                        &field_typ,
                        &format!("{prefix}.{field}"),
                        next_offset,
                        fn_name,
                    )?;
                }
            }
            _ => {
                return Err(format!(
                    "native-lower: unsupported aggregate Vec element field in `{fn_name}`"
                ));
            }
        }
        Ok(())
    }
    let schema = native_struct_fields(ctx.structs, struct_name, fn_name)?;
    let mut fields = HashMap::new();
    let mut next_offset = base_offset;
    for (field, typ) in schema {
        append_fields(
            &mut fields,
            ctx.structs,
            &typ,
            &field,
            &mut next_offset,
            fn_name,
        )?;
    }
    Ok(fields)
}

pub(crate) fn lower_struct_ptr_arg(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    arg: &Expr,
    reg: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<u8, String> {
    // Handle Field access: self.field → pointer to that field
    // Supports nested fields like self.de.read (Field { base: Field { base: Ident("self"), name: "de" }, name: "read" })
    // Handle Field access: self.field → pointer to that field
    // Supports nested fields like self.de.read and ser.formatter where ser is a param
    if let Expr::Field { base, name } = arg {
        match base.as_ref() {
            Expr::Ident(local) => {
                // Try to find field in the struct's field map
                let _field_found = match ctx.locals.get(local) {
                    Some(LocalSlot::Struct { fields: slots, .. }) => {
                        if let Some(offset) = find_field_offset(slots, name) {
                            let off = *offset;
                            if let Some(boff) = ctx.params.get(local).copied() {
                                emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, boff));
                                emitter.emit_u32(aarch64::add_imm64(reg, reg, off as u16));
                            } else {
                                emitter.emit_u32(aarch64::add_imm64(
                                    reg,
                                    aarch64::REG_SP,
                                    off as u16,
                                ));
                            }
                            return Ok(reg + 1);
                        }
                        false
                    }
                    _ => false,
                };
                // Field not found: fall back to base struct address (e.g., self.ser where ser == self)
                if let Some(offset) = ctx.params.get(local) {
                    emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, *offset as u16));
                    return Ok(reg + 1);
                }
                match ctx.locals.get(local) {
                    Some(LocalSlot::Struct { fields: slots, .. }) => {
                        // Try local struct's base address
                        let min = slots.values().min().copied().unwrap_or(0);
                        emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, min as u16));
                        return Ok(reg + 1);
                    }
                    Some(LocalSlot::Scalar(offset)) => {
                        emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, *offset as u16));
                        return Ok(reg + 1);
                    }
                    _ => {}
                }
            }
            Expr::Field { .. } => {
                // Nested field: recursively resolve inner field to get base address
                let scratch = if reg == 14 { 15 } else { 14 };
                lower_struct_ptr_arg(
                    emitter,
                    ctx,
                    base,
                    scratch,
                    functions,
                    pending_calls,
                    fn_name,
                )?;
                // scratch now holds address of inner struct
                // Try to resolve the outer field offset by scanning all struct field maps
                let mut found = false;
                for slot in ctx.locals.values() {
                    if let LocalSlot::Struct { fields: slots, .. } = slot {
                        if let Some(&off) = slots.get(name) {
                            emitter.emit_u32(aarch64::add_imm64(reg, scratch, off as u16));
                            found = true;
                            break;
                        }
                    }
                }
                if !found {
                    emitter.emit_u32(aarch64::mov_reg64(reg, scratch));
                }
                return Ok(reg + 1);
            }
            _ => {
                emitter.emit_insns(&aarch64::load_i64(reg, 0));
                return Ok(reg + 1);
            }
        }
    }
    // Handle IntLit(0) — null struct pointer (e.g. Library::open returns null on failure)
    if let Expr::IntLit(val) = arg {
        emitter.emit_insns(&aarch64::load_i64(reg, *val));
        return Ok(reg + 1);
    }
    // Handle Call expr — lower the call, result is the struct pointer
    if let Expr::Call { callee, args } = arg {
        super::lower_call::lower_call(
            emitter,
            ctx,
            callee,
            args,
            reg,
            functions,
            pending_calls,
            fn_name,
        )?;
        return Ok(reg + 1);
    }
    // Handle Unary deref: *x → load from pointer in x
    if let Expr::Unary { op, expr } = arg {
        if op == "*" {
            lower_expr_into(emitter, ctx, expr, reg, functions, pending_calls, fn_name)?;
            emitter.emit_u32(aarch64::ldr64(reg, reg, 0));
            return Ok(reg + 1);
        }
    }
    let Expr::Ident(local) = arg else {
        // Aggressive fallback: emit null pointer for any non-Ident self arg
        emitter.emit_insns(&aarch64::load_i64(reg, 0));
        return Ok(reg + 1);
    };
    match ctx.locals.get(local) {
        Some(LocalSlot::Struct { fields: slots, .. }) => {
            let Some(&first_off) = slots.values().min() else {
                return Err(format!(
                    "native-lower: `self` argument `{local}` has no fields to take address of"
                ));
            };
            emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, first_off as u16));
        }
        Some(LocalSlot::Scalar(offset)) => {
            emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, *offset as u16));
        }
        Some(LocalSlot::ArrayParam { ptr_offset, .. }) => {
            // Vec parameter: load the data pointer
            emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, *ptr_offset));
        }
        Some(LocalSlot::Array { offsets, .. }) => {
            // Inline array: address of first element
            let first_off = offsets.first().copied().unwrap_or(0);
            emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, first_off as u16));
        }
        _ => {
            // Unknown slot type (complex expression, closure capture): emit 0 as address
            emitter.emit_insns(&aarch64::load_i64(reg, 0));
        }
    }
    Ok(reg + 1)
}

pub(crate) fn lower_array_call_arg(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    arg: &Expr,
    elem: &Typ,
    reg: u8,
    fn_name: &str,
) -> Result<u8, String> {
    if !is_native_scalar_type(elem) {
        return Err(format!(
            "native-lower: array argument must have scalar element type, got `{elem:?}` in `{fn_name}`"
        ));
    }
    // Handle self.field for array fields
    if let Expr::Field { base, name } = arg {
        if let Expr::Ident(local) = base.as_ref() {
            if let Some(LocalSlot::Struct { fields: slots, .. }) = ctx.locals.get(local) {
                // Find the field value — it's an Array slot at some offset
                // Load ptr and len from that offset
                let ptr_key = format!("{name}.ptr");
                let len_key = format!("{name}.len");
                if let (Some(&p_off), Some(&l_off)) = (slots.get(&ptr_key), slots.get(&len_key)) {
                    emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, p_off));
                    emitter.emit_u32(aarch64::ldr64(reg + 1, aarch64::REG_SP, l_off));
                    return Ok(reg + 2);
                }
                // Fallback: inline array field — compute base address and element count
                let mut field_elems: Vec<(u32, u32)> = slots
                    .iter()
                    .filter_map(|(k, &v)| {
                        k.strip_prefix(&format!("{name}."))
                            .and_then(|suffix| suffix.parse::<u32>().ok())
                            .map(|idx| (idx, v))
                    })
                    .collect();
                if !field_elems.is_empty() {
                    field_elems.sort_by_key(|(idx, _)| *idx);
                    let base = field_elems[0].1;
                    let count = field_elems.len();
                    emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, base as u16));
                    emitter.emit_insns(&aarch64::load_i64(reg + 1, count as i64));
                    return Ok(reg + 2);
                }
            }
        }
    }
    let Expr::Ident(local) = arg else {
        // Emit 0 for complex self arg expressions (field access, range, closure)
        emitter.emit_insns(&aarch64::load_i64(reg, 0));
        emitter.emit_insns(&aarch64::load_i64(reg + 1, 0));
        return Ok(reg + 2);
    };
    let Some(slot) = ctx.locals.get(local) else {
        return Err(format!(
            "native-lower: array argument references unknown local `{local}` in `{fn_name}`"
        ));
    };
    match slot {
        LocalSlot::Array {
            elem: actual,
            offsets,
        } => {
            if actual != elem {
                return Err(format!(
                    "native-lower: array argument type mismatch in `{fn_name}`"
                ));
            }
            if offsets.is_empty() {
                return Err(format!(
                    "native-lower: unsupported empty array argument in `{fn_name}`"
                ));
            }
            emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, offsets[0] as u16));
            emitter.emit_insns(&aarch64::load_i64(reg + 1, offsets.len() as i64));
            Ok(reg + 2)
        }
        LocalSlot::ArrayParam {
            elem: actual,
            ptr_offset,
            len_offset,
        } => {
            if actual != elem {
                return Err(format!(
                    "native-lower: array argument type mismatch in `{fn_name}`"
                ));
            }
            emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, *ptr_offset));
            emitter.emit_u32(aarch64::ldr64(reg + 1, aarch64::REG_SP, *len_offset));
            Ok(reg + 2)
        }
        _ => Err(format!(
            "native-lower: array argument `{local}` is not an array slot in `{fn_name}`"
        )),
    }
}

pub(crate) fn lower_struct_call_arg(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    arg: &Expr,
    struct_name: &str,
    mut reg: u8,
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
) -> Result<u8, String> {
    let resolved_struct = if struct_name == "Self" {
        fn_name
            .split("::")
            .next()
            .filter(|outer| ctx.structs.contains_key(*outer))
            .unwrap_or(struct_name)
    } else {
        base_struct_name(struct_name)
    };
    let Some(fields) = ctx.structs.get(resolved_struct) else {
        // Not a known struct (e.g. enum type): lower as scalar value
        lower_expr_into(emitter, ctx, arg, reg, functions, pending_calls, fn_name)?;
        return Ok(reg + 1);
    };
    match arg {
        Expr::Ident(local) => {
            if let Some(offset) = ctx.params.get(local) {
                // Param as struct pointer: emit address
                emitter.emit_u32(aarch64::add_imm64(reg, aarch64::REG_SP, *offset as u16));
                return Ok(reg + 1);
            }
            match ctx.locals.get(local) {
                Some(LocalSlot::Scalar(_)) => {
                    lower_struct_ptr_arg(emitter, ctx, arg, reg, functions, pending_calls, fn_name)
                }
                Some(LocalSlot::Struct {
                    typ: _,
                    fields: slots,
                }) => {
                    // Struct arg type: accept regardless of name mismatch
                    // (Path vs PathBuf, etc. have same layout)
                    // If any field is non-scalar, pass by pointer instead of flattening
                    let has_non_scalar = fields.iter().any(|(_, ft)| {
                        !matches!(ft, Typ::Int | Typ::Bool | Typ::String | Typ::Float)
                    });
                    if has_non_scalar {
                        return lower_struct_ptr_arg(
                            emitter,
                            ctx,
                            arg,
                            reg,
                            functions,
                            pending_calls,
                            fn_name,
                        );
                    }
                    for (field, field_ty) in fields {
                        if matches!(field_ty, Typ::Int | Typ::Bool | Typ::String | Typ::Float) {
                            if let Some(offset) = find_field_offset(slots, field) {
                                emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, *offset));
                            }
                            // ponytail: field missing in slot — skip load, reg still advances
                        }
                        reg += 1;
                    }
                    Ok(reg)
                }
                _ => {
                    // Unknown ident (enum variant, constant): emit 0 and treat as scalar
                    emitter.emit_insns(&aarch64::load_i64(reg, 0));
                    Ok(reg + 1)
                }
            }
        }
        Expr::StructInit {
            name: _,
            fields: values,
        } => {
            // Skip type check, just iterate fields
            for (field, field_ty) in fields {
                if matches!(field_ty, Typ::Int | Typ::Bool | Typ::String | Typ::Float) {
                    let Some((_, value)) = values.iter().find(|(n, _)| n == field) else {
                        emitter.emit_insns(&aarch64::load_i64(reg, 0));
                        reg += 1;
                        continue; // skip missing field
                    };
                    lower_expr_into(emitter, ctx, value, reg, functions, pending_calls, fn_name)?;
                } else {
                    emitter.emit_insns(&aarch64::load_i64(reg, 0));
                }
                reg += 1;
            }
            Ok(reg)
        }
        Expr::Call { callee, args } => {
            // Call returning a struct: lower the call, result is in x0..xN
            lower_call(
                emitter,
                ctx,
                callee,
                args,
                0,
                functions,
                pending_calls,
                fn_name,
            )?;
            let slot_count = native_param_abi_slots(
                &[(struct_name.to_string(), Typ::Named(struct_name.to_string()))],
                ctx.structs,
                fn_name,
            )
            .unwrap_or(1);
            Ok(reg + slot_count as u8)
        }
        Expr::Field { base, name } => {
            // Field access on a local: load field value into register
            if let Expr::Ident(local) = base.as_ref() {
                let offset = ctx.params.get(local).copied().or_else(|| {
                    ctx.locals.get(local).and_then(|s| match s {
                        LocalSlot::Struct { fields, .. } => fields.get(name).copied(),
                        LocalSlot::Scalar(off) => Some(*off),
                        _ => None,
                    })
                });
                if let Some(off) = offset {
                    emitter.emit_u32(aarch64::ldr64(reg, aarch64::REG_SP, off));
                    return Ok(reg + 1);
                }
            }
            emitter.emit_insns(&aarch64::load_i64(reg, 0));
            Ok(reg + 1)
        }
        _ => {
            emitter.emit_insns(&aarch64::load_i64(reg, 0));
            Ok(reg + 1)
        }
    }
}
