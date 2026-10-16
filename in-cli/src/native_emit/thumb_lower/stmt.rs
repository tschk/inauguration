use crate::core_ir::{Expr, LoopKind, Stmt, Typ};
use crate::native_emit::thumb::{self, COND_EQ, CodeEmitter, R0, R1, R2, R3, R4, REG_RET};
use std::collections::HashMap;

use super::ctx::{FunctionInfo, LocalSlot, LowerCtx, PendingCall};
use super::expr::{
    lower_expr_into, lower_field_assign, lower_index_assign, lower_struct_init_into, patch_b,
    patch_b_cond,
};

pub(crate) fn lower_function(
    emitter: &mut CodeEmitter,
    func: &FunctionInfo,
    functions: &HashMap<String, FunctionInfo>,
    structs: &HashMap<String, Vec<(String, Typ)>>,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    match func.ret.canonical() {
        Typ::Int | Typ::Bool | Typ::Void => {}
        other => {
            return Err(format!(
                "thumb-lower: unsupported return type {:?} in `{}`",
                other, func.name
            ));
        }
    }
    for (name, typ) in &func.params {
        match typ.canonical() {
            Typ::Int | Typ::Bool => {}
            other => {
                return Err(format!(
                    "thumb-lower: unsupported param `{name}` type {:?} in `{}`",
                    other, func.name
                ));
            }
        }
    }

    let mut ctx = LowerCtx::new(&func.name, &func.params, functions, structs);
    ctx.ret_typ = func.ret.canonical();
    alloc_declared_locals(&mut ctx, &func.body)?;

    // Reserve exactly the pairs simultaneously live during binary evaluation.
    // Nested binary operands (e.g. the `b * c` in `a - b * c`) evaluate into
    // the next depth's pair so they cannot clobber the outer operand stash.
    for _ in 0..max_binary_depth(&func.body) * 2 {
        let off = ctx.alloc_slot();
        ctx.scratch_temps.push(off);
    }

    // Call-argument temp pool: chunk = max arity, depth = 8 nested calls.
    let max_arity = max_call_arity(&func.body);
    ctx.call_arg_chunk = max_arity;
    let slots_needed = ctx.call_arg_chunk * 8;
    for _ in 0..slots_needed {
        let off = ctx.alloc_slot();
        ctx.call_arg_temps.push(off);
    }

    thumb::emit_prologue(emitter);
    let frame = ctx.frame_reserve();
    if frame > 0x1FC {
        return Err(format!(
            "thumb-lower: frame {} too large for sub sp imm7 in `{}`",
            frame, func.name
        ));
    }
    thumb::emit_frame(emitter, frame)?;

    // Store AAPCS params into their local slots. r0-r3 are live; extras are
    // on the caller's stack above the saved r4-r7/lr.
    let param_regs = [R0, R1, R2, R3];
    for (i, (name, _)) in func.params.iter().enumerate() {
        let Some(LocalSlot::Scalar(off)) = ctx.locals.get(name) else {
            continue;
        };
        if i < 4 {
            emitter.emit_u16(thumb::str_sp(param_regs[i], *off)?);
        } else {
            let caller_off = frame + 20 + ((i - 4) as u32) * 4;
            emitter.emit_u16(thumb::ldr_sp(R4, caller_off)?);
            emitter.emit_u16(thumb::str_sp(R4, *off)?);
        }
    }

    for stmt in &func.body {
        lower_stmt(emitter, &mut ctx, stmt, pending, frame)?;
    }

    if !ctx.emitted_return {
        if matches!(ctx.ret_typ, Typ::Void) {
            thumb::load_i32(emitter, REG_RET, 0);
        }
        thumb::emit_epilogue(emitter, frame)?;
    }
    Ok(())
}

pub(crate) fn alloc_declared_locals(ctx: &mut LowerCtx<'_>, body: &[Stmt]) -> Result<(), String> {
    for stmt in body {
        match stmt {
            Stmt::Let(name, typ, expr) => {
                if ctx.locals.contains_key(name) {
                    continue;
                }
                let t = typ.as_ref().map(Typ::canonical).unwrap_or(Typ::Int);
                match t {
                    Typ::Int | Typ::Bool => {
                        let off = ctx.alloc_slot();
                        ctx.locals.insert(name.clone(), LocalSlot::Scalar(off));
                    }
                    Typ::Named(s) if ctx.structs.contains_key(&s) => {
                        let (_, fields) = ctx.alloc_struct(&s)?;
                        ctx.locals
                            .insert(name.clone(), LocalSlot::Struct { fields });
                    }
                    Typ::Array(elem) => {
                        let esz = super::ctx::type_size(&elem)?;
                        let Expr::ArrayLit(items) = expr else {
                            return Err(format!("thumb-lower: `{name}` needs an array literal"));
                        };
                        let len = items.len();
                        let base = ctx.frame_size;
                        ctx.frame_size += esz * len as u32;
                        ctx.locals.insert(
                            name.clone(),
                            LocalSlot::Array {
                                base,
                                elem_size: esz,
                                len,
                            },
                        );
                    }
                    other => {
                        return Err(format!(
                            "thumb-lower: unsupported local `{name}` type {:?} in `{}`",
                            other, ctx.fn_name
                        ));
                    }
                }
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                alloc_declared_locals(ctx, then_body)?;
                alloc_declared_locals(ctx, else_body)?;
            }
            Stmt::Loop { body, .. } => alloc_declared_locals(ctx, body)?,
            _ => {}
        }
    }
    Ok(())
}

// Match lower_binary's lifetime: ordinary binaries hold a pair while either
// operand executes; lazy logical operands execute without an enclosing pair.
fn max_binary_depth(body: &[Stmt]) -> usize {
    body.iter().map(stmt_binary_depth).max().unwrap_or(0)
}

fn stmt_binary_depth(stmt: &Stmt) -> usize {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Expr(expr)
        | Stmt::Return(Some(expr))
        | Stmt::Throw(expr) => expr_binary_depth(expr),
        Stmt::FieldAssign { base, value, .. } => {
            expr_binary_depth(base).max(expr_binary_depth(value))
        }
        Stmt::IndexAssign { base, index, value } => expr_binary_depth(base)
            .max(expr_binary_depth(index))
            .max(expr_binary_depth(value)),
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => expr_binary_depth(cond)
            .max(max_binary_depth(then_body))
            .max(max_binary_depth(else_body)),
        Stmt::Loop { cond, body, .. } => cond
            .as_ref()
            .map(expr_binary_depth)
            .unwrap_or(0)
            .max(max_binary_depth(body)),
        Stmt::Match { scrutinee, arms } => expr_binary_depth(scrutinee).max(
            arms.iter()
                .map(|arm| {
                    arm.guard
                        .as_ref()
                        .map(expr_binary_depth)
                        .unwrap_or(0)
                        .max(max_binary_depth(&arm.body))
                })
                .max()
                .unwrap_or(0),
        ),
        Stmt::Try { body, catches } => max_binary_depth(body).max(
            catches
                .iter()
                .map(|arm| max_binary_depth(&arm.body))
                .max()
                .unwrap_or(0),
        ),
        Stmt::Return(None) | Stmt::Propagate | Stmt::Break | Stmt::Continue => 0,
    }
}

fn expr_binary_depth(expr: &Expr) -> usize {
    // Casts are expanded inside lower_expr_into, after frame allocation.
    if let Expr::Unary { op, expr: value } = expr
        && op.starts_with("int-cast-")
        && let Some(expanded) = crate::core_ir::integer_cast_expr((**value).clone(), op)
    {
        return expr_binary_depth(&expanded);
    }
    let mut depth = if let Expr::Block { body, .. } = expr {
        max_binary_depth(body)
    } else {
        0
    };
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        depth = depth.max(expr_binary_depth(child));
    });
    if matches!(expr, Expr::Binary { op, .. } if op != "&&" && op != "||") {
        depth += 1;
    }
    depth
}

pub(crate) fn max_call_arity(body: &[Stmt]) -> usize {
    body.iter().map(max_call_arity_stmt).max().unwrap_or(0)
}

pub(crate) fn max_call_arity_stmt(s: &Stmt) -> usize {
    match s {
        Stmt::Let(_, _, e)
        | Stmt::Assign(_, e)
        | Stmt::Expr(e)
        | Stmt::Return(Some(e))
        | Stmt::FieldAssign { value: e, .. } => max_call_arity_expr(e),
        Stmt::Return(None) => 0,
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => max_call_arity_expr(cond)
            .max(max_call_arity(then_body))
            .max(max_call_arity(else_body)),
        Stmt::Loop { cond, body, .. } => cond
            .as_ref()
            .map(max_call_arity_expr)
            .unwrap_or(0)
            .max(max_call_arity(body)),
        _ => 0,
    }
}

pub(crate) fn max_call_arity_expr(e: &Expr) -> usize {
    match e {
        Expr::Call { args, .. } => {
            let here = args.len();
            args.iter()
                .map(max_call_arity_expr)
                .max()
                .unwrap_or(0)
                .max(here)
        }
        Expr::Unary { expr, .. } => max_call_arity_expr(expr),
        Expr::Binary { lhs, rhs, .. } => max_call_arity_expr(lhs).max(max_call_arity_expr(rhs)),
        Expr::StructInit { fields, .. } => fields
            .iter()
            .map(|(_, e)| max_call_arity_expr(e))
            .max()
            .unwrap_or(0),
        Expr::ArrayLit(items) => items.iter().map(max_call_arity_expr).max().unwrap_or(0),
        Expr::Index { index, .. } => max_call_arity_expr(index),
        Expr::Field { base, .. } => max_call_arity_expr(base),
        _ => 0,
    }
}

pub(crate) fn lower_stmt(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    stmt: &Stmt,
    pending: &mut Vec<PendingCall>,
    frame: u32,
) -> Result<(), String> {
    match stmt {
        Stmt::Return(expr) => {
            if let Some(expr) = expr {
                lower_expr_into(emitter, ctx, expr, REG_RET, pending)?;
            } else {
                thumb::load_i32(emitter, REG_RET, 0);
            }
            thumb::emit_epilogue(emitter, frame)?;
            ctx.emitted_return = true;
            Ok(())
        }
        Stmt::Let(name, annotation, expr) => {
            let typ = annotation.clone().or_else(|| {
                crate::typecheck::TypeChecker::new().expr_type(expr, &ctx.facts, &ctx.local_types)
            });
            lower_store_local(emitter, ctx, name, expr, pending)?;
            if let Some(typ) = typ {
                ctx.local_types.insert(name.clone(), typ);
            } else {
                ctx.local_types.remove(name);
            }
            Ok(())
        }
        Stmt::Assign(name, expr) => lower_store_local(emitter, ctx, name, expr, pending),
        Stmt::FieldAssign { base, name, value } => {
            lower_field_assign(emitter, ctx, base, name, value, pending)
        }
        Stmt::IndexAssign { base, index, value } => {
            lower_index_assign(emitter, ctx, base, index, value, pending)
        }
        Stmt::Expr(expr) => {
            lower_expr_into(emitter, ctx, expr, R0, pending)?;
            Ok(())
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => lower_if(emitter, ctx, cond, then_body, else_body, pending, frame),
        Stmt::Loop {
            kind: LoopKind::While,
            cond: Some(cond),
            body,
        } => lower_while(emitter, ctx, cond, body, pending, frame),
        Stmt::Loop { kind, .. } => Err(format!(
            "thumb-lower: unsupported loop {:?} in `{}`",
            kind, ctx.fn_name
        )),
        Stmt::Continue => {
            let Some(&head) = ctx.loop_heads.last() else {
                return Err(format!(
                    "thumb-lower: continue outside loop in `{}`",
                    ctx.fn_name
                ));
            };
            let site = emitter.len();
            emitter.emit_u32_thumb(thumb::b_wide(0));
            patch_b(emitter, site, head)
        }
        Stmt::Break => {
            let Some(sites) = ctx.break_sites.last_mut() else {
                return Err(format!(
                    "thumb-lower: break outside loop in `{}`",
                    ctx.fn_name
                ));
            };
            let site = emitter.len();
            emitter.emit_u32_thumb(thumb::b_wide(0));
            sites.push(site);
            Ok(())
        }
        other => Err(format!(
            "thumb-lower: unsupported stmt {:?} in `{}`",
            std::mem::discriminant(other),
            ctx.fn_name
        )),
    }
}

pub(crate) fn lower_store_local(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    name: &str,
    expr: &Expr,
    pending: &mut Vec<PendingCall>,
) -> Result<(), String> {
    match ctx.locals.get(name).cloned() {
        Some(LocalSlot::Scalar(off)) => {
            lower_expr_into(emitter, ctx, expr, R0, pending)?;
            emitter.emit_u16(thumb::str_sp(R0, off)?);
            Ok(())
        }
        Some(LocalSlot::Struct { fields }) => {
            lower_struct_init_into(emitter, ctx, expr, &fields, pending)
        }
        Some(LocalSlot::Array {
            base,
            elem_size,
            len,
        }) => {
            let Expr::ArrayLit(items) = expr else {
                return Err(format!("thumb-lower: `{name}` needs an array literal"));
            };
            if items.len() != len {
                return Err(format!("thumb-lower: array length mismatch for `{name}`"));
            }
            for (i, item) in items.iter().enumerate() {
                lower_expr_into(emitter, ctx, item, R0, pending)?;
                emitter.emit_u16(thumb::str_sp(R0, base + i as u32 * elem_size)?);
            }
            Ok(())
        }
        None => Err(format!(
            "thumb-lower: unknown local `{name}` in `{}`",
            ctx.fn_name
        )),
    }
}

fn lower_if(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    cond: &Expr,
    then_body: &[Stmt],
    else_body: &[Stmt],
    pending: &mut Vec<PendingCall>,
    frame: u32,
) -> Result<(), String> {
    let outer_types = ctx.local_types.clone();
    // evaluate cond → r0; cmp r0, #0; beq else
    lower_expr_into(emitter, ctx, cond, R0, pending)?;
    emitter.emit_u16(thumb::cmp_imm8(R0, 0));
    let beq_site = emitter.len();
    emitter.emit_u32_thumb(thumb::b_cond_wide(COND_EQ, 0)); // patch later

    for stmt in then_body {
        if ctx.emitted_return {
            break;
        }
        lower_stmt(emitter, ctx, stmt, pending, frame)?;
    }
    ctx.local_types = outer_types.clone();
    let then_returned = ctx.emitted_return;
    ctx.emitted_return = false;

    let mut b_end_site = None;
    if !else_body.is_empty() && !then_returned {
        b_end_site = Some(emitter.len());
        emitter.emit_u32_thumb(thumb::b_wide(0));
    }

    let else_start = emitter.len();
    // patch beq: rel from next insn after beq (beq_site+2) to else_start
    patch_b_cond(emitter, beq_site, else_start)?;

    if !else_body.is_empty() {
        for stmt in else_body {
            if ctx.emitted_return {
                break;
            }
            lower_stmt(emitter, ctx, stmt, pending, frame)?;
        }
        let else_returned = ctx.emitted_return;
        if let Some(site) = b_end_site {
            let end = emitter.len();
            patch_b(emitter, site, end)?;
        }
        // if both branches returned, keep emitted_return true only if both did
        ctx.emitted_return = then_returned && else_returned;
    } else {
        ctx.emitted_return = false;
    }
    ctx.local_types = outer_types;
    Ok(())
}

fn lower_while(
    emitter: &mut CodeEmitter,
    ctx: &mut LowerCtx<'_>,
    cond: &Expr,
    body: &[Stmt],
    pending: &mut Vec<PendingCall>,
    frame: u32,
) -> Result<(), String> {
    let outer_types = ctx.local_types.clone();
    let loop_head = emitter.len();
    lower_expr_into(emitter, ctx, cond, R0, pending)?;
    emitter.emit_u16(thumb::cmp_imm8(R0, 0));
    let beq_site = emitter.len();
    emitter.emit_u32_thumb(thumb::b_cond_wide(COND_EQ, 0));

    ctx.break_sites.push(Vec::new());
    ctx.loop_heads.push(loop_head);
    for stmt in body {
        lower_stmt(emitter, ctx, stmt, pending, frame)?;
        ctx.emitted_return = false; // returns inside loop don't end the function for us
    }

    ctx.local_types = outer_types;
    let b_back = emitter.len();
    emitter.emit_u32_thumb(thumb::b_wide(0));
    patch_b(emitter, b_back, loop_head)?;

    let end = emitter.len();
    patch_b_cond(emitter, beq_site, end)?;

    ctx.loop_heads.pop();
    let breaks = ctx.break_sites.pop().expect("break site stack");
    for site in breaks {
        patch_b(emitter, site, end)?;
    }
    Ok(())
}

#[cfg(test)]
mod scratch_depth_tests {
    use super::*;

    fn binary(op: &str, lhs: Expr, rhs: Expr) -> Expr {
        Expr::Binary {
            op: op.into(),
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    fn nested(mut expr: Expr, depth: usize) -> Expr {
        for _ in 0..depth {
            expr = binary("+", Expr::IntLit(0), expr);
        }
        expr
    }

    fn function(expr: Expr) -> FunctionInfo {
        FunctionInfo {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(expr))],
        }
    }

    fn lower(func: &FunctionInfo) -> Result<CodeEmitter, String> {
        let functions = HashMap::from([(func.name.clone(), func.clone())]);
        let mut emitter = CodeEmitter::new();
        lower_function(&mut emitter, func, &functions, &HashMap::new(), &mut vec![])?;
        Ok(emitter)
    }

    #[test]
    fn scratch_depth_counts_lazy_cast_and_block_evaluation() {
        let deep = nested(Expr::IntLit(42), 12);
        assert_eq!(expr_binary_depth(&deep), 12);
        assert_eq!(
            expr_binary_depth(&binary("&&", deep.clone(), deep.clone())),
            12
        );
        let cast = Expr::Unary {
            op: "int-cast-i8".into(),
            expr: Box::new(deep.clone()),
        };
        assert_eq!(expr_binary_depth(&cast), 15);
        let block = Expr::Block {
            body: vec![Stmt::Expr(cast)],
            result: Some(Box::new(Expr::IntLit(42))),
        };
        assert_eq!(expr_binary_depth(&binary("+", deep, block)), 16);
    }

    #[test]
    fn scratch_frames_fit_actual_depth_and_keep_instruction_bound() {
        let small = lower(&function(Expr::IntLit(42))).unwrap();
        assert_eq!(
            &small.bytes[4..6],
            &thumb::sub_sp_imm(4).unwrap().to_le_bytes()
        );
        let deep = lower(&function(nested(Expr::IntLit(42), 12))).unwrap();
        // Twelve pairs = 96 bytes, plus four to align the saved 20-byte frame.
        assert_eq!(
            &deep.bytes[4..6],
            &thumb::sub_sp_imm(100).unwrap().to_le_bytes()
        );
        let cast = lower(&function(Expr::Unary {
            op: "int-cast-i8".into(),
            expr: Box::new(nested(Expr::IntLit(42), 12)),
        }))
        .unwrap();
        assert_eq!(
            &cast.bytes[4..6],
            &thumb::sub_sp_imm(124).unwrap().to_le_bytes()
        );
        let error = lower(&function(nested(Expr::IntLit(42), 70)))
            .err()
            .unwrap();
        assert!(error.contains("frame 564 too large"), "{error}");
    }

    #[test]
    fn deep_operand_calls_remain_in_source_order() {
        let call = |name: &str| Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args: vec![],
        };
        let func = function(binary("+", call("left"), nested(call("right"), 12)));
        let mut functions = HashMap::from([(func.name.clone(), func.clone())]);
        for name in ["left", "right"] {
            functions.insert(
                name.into(),
                FunctionInfo {
                    name: name.into(),
                    params: vec![],
                    ret: Typ::Int,
                    body: vec![],
                },
            );
        }
        let mut pending = vec![];
        lower_function(
            &mut CodeEmitter::new(),
            &func,
            &functions,
            &HashMap::new(),
            &mut pending,
        )
        .unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|call| call.target.as_str())
                .collect::<Vec<_>>(),
            vec!["left", "right"]
        );
    }
}
