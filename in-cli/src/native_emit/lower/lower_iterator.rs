use super::lower_stmt::{lower_stmt, lower_struct_expr_into_regs};
use super::{FunctionInfo, LocalSlot, LowerCtx, PendingCall};
use crate::core_ir::{Expr, Stmt, Typ};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::HashMap;

#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_vec_for(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    binding: &str,
    iterator: &Expr,
    body: &[Stmt],
    functions: &HashMap<String, FunctionInfo>,
    pending_calls: &mut Vec<PendingCall>,
    fn_name: &str,
    ret_typ: &Typ,
) -> Result<(), String> {
    let binding = binding.trim();
    // Parse the binding pattern into (element_names, offsets, stride_per_element)
    let (elem_names, offsets, stride): (Vec<&str>, Vec<u32>, u32) = if binding.starts_with('(')
        && binding.ends_with(')')
    {
        // Tuple pattern: "(x , y)" or "(w , p)"
        let inner = &binding[1..binding.len() - 1];
        let names: Vec<&str> = inner
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        let n = names.len() as u32;
        let mut offs = Vec::new();
        for name in names.iter().copied() {
            if name == "_" {
                offs.push(0);
            } else if let Some(&off) = ctx.locals.get(name).and_then(|s| match s {
                LocalSlot::Scalar(o) => Some(o),
                _ => None,
            }) {
                offs.push(off);
            } else {
                // Element not in locals (Rust front didn't allocate it): treat as wildcard
                offs.push(0);
            }
        }
        (names, offs, n * 8)
    } else if binding == "_" {
        // Wildcard: iterate but don't store anything
        (vec![], vec![], 0)
    } else {
        // Simple identifier or reference pattern "& x"
        let name = binding.strip_prefix("& ").unwrap_or(binding);
        if !name.chars().enumerate().all(|(i, c)| match i {
            0 => c.is_ascii_alphabetic(),
            _ => c.is_ascii_alphanumeric() || c == '-',
        }) {
            return Err(format!(
                "native-lower[vec-iterator-pattern-unsupported]: unsupported for pattern `{binding}` in `{fn_name}`"
            ));
        }
        let off = match ctx.locals.get(name) {
            Some(LocalSlot::Scalar(offset)) => *offset,
            _ => 0, // binding not found: treat as wildcard
        };
        (vec![name], vec![off], 8)
    };

    let slots = ctx.next_vec_for_slots(fn_name)?;
    lower_struct_expr_into_regs(
        emitter,
        ctx,
        iterator,
        "Vec",
        functions,
        pending_calls,
        fn_name,
    )?;
    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, slots.ptr));
    emitter.emit_u32(aarch64::str64(1, aarch64::REG_SP, slots.len));
    emitter.emit_insns(&aarch64::load_i64(0, 0));
    emitter.emit_u32(aarch64::str64(0, aarch64::REG_SP, slots.index));

    let head = emitter.len();
    emitter.emit_u32(aarch64::ldr64(0, aarch64::REG_SP, slots.ptr));
    emitter.emit_u32(aarch64::ldr64(1, aarch64::REG_SP, slots.len));
    emitter.emit_u32(aarch64::ldr64(2, aarch64::REG_SP, slots.index));
    emitter.emit_u32(aarch64::cmp_reg64(2, 1));
    let end_branch = emitter.emit_insn(aarch64::b_cond(10, 0));
    // Compute element address: ptr + index * stride
    if stride <= 8 {
        // Single element: current code path (index * 8)
        emitter.emit_u32(aarch64::add_reg64(3, 2, 2));
        emitter.emit_u32(aarch64::add_reg64(3, 3, 3));
        emitter.emit_u32(aarch64::add_reg64(3, 3, 3));
        emitter.emit_u32(aarch64::add_reg64(4, 0, 3));
    } else {
        // Multi-element tuple: compute ptr + index * stride
        emitter.emit_insns(&aarch64::load_i64(3, stride as i64));
        emitter.emit_u32(aarch64::mul64(3, 2, 3)); // x3 = index * stride
        emitter.emit_u32(aarch64::add_reg64(4, 0, 3)); // x4 = ptr + index * stride
    }
    // Store each field value from the element into its local slot
    for (i, &off) in offsets.iter().enumerate() {
        if elem_names[i] != "_" {
            emitter.emit_u32(aarch64::ldr64(5, 4, (i as u32) * 8));
            emitter.emit_u32(aarch64::str64(5, aarch64::REG_SP, off));
        }
    }
    emitter.emit_u32(aarch64::add_imm64(2, 2, 1));
    emitter.emit_u32(aarch64::str64(2, aarch64::REG_SP, slots.index));
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
    let end_offset = emitter.len() as i32 - end_branch as i32;
    emitter.patch_u32(end_branch, aarch64::b_cond(10, end_offset));
    super::lower_stmt::patch_loop_branches(emitter, ctx, head)
}
