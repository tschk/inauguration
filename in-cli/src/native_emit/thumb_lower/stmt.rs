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

    // Reserve only argument slots simultaneously live while nested calls stage.
    let slots_needed = body_expr_demand(&func.body, &|expr| call_arg_demand(expr, functions));
    if slots_needed > (0x1FC / 4) as usize {
        return Err(format!(
            "thumb-lower: call argument frame too large in `{}`",
            func.name
        ));
    }
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
    body_expr_demand(body, &expr_binary_depth)
}

fn body_expr_demand(body: &[Stmt], demand: &impl Fn(&Expr) -> usize) -> usize {
    body.iter()
        .map(|stmt| stmt_expr_demand(stmt, demand))
        .max()
        .unwrap_or(0)
}

fn stmt_expr_demand(stmt: &Stmt, demand: &impl Fn(&Expr) -> usize) -> usize {
    match stmt {
        Stmt::Let(_, _, expr)
        | Stmt::Assign(_, expr)
        | Stmt::Expr(expr)
        | Stmt::Return(Some(expr))
        | Stmt::Throw(expr) => demand(expr),
        Stmt::FieldAssign { base, value, .. } => demand(base).max(demand(value)),
        Stmt::IndexAssign { base, index, value } => {
            demand(base).max(demand(index)).max(demand(value))
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => demand(cond)
            .max(body_expr_demand(then_body, demand))
            .max(body_expr_demand(else_body, demand)),
        Stmt::Loop { cond, body, .. } => cond
            .as_ref()
            .map(demand)
            .unwrap_or(0)
            .max(body_expr_demand(body, demand)),
        Stmt::Match { scrutinee, arms } => demand(scrutinee).max(
            arms.iter()
                .map(|arm| {
                    arm.guard
                        .as_ref()
                        .map(demand)
                        .unwrap_or(0)
                        .max(body_expr_demand(&arm.body, demand))
                })
                .max()
                .unwrap_or(0),
        ),
        Stmt::Try { body, catches } => body_expr_demand(body, demand).max(
            catches
                .iter()
                .map(|arm| body_expr_demand(&arm.body, demand))
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

fn call_arg_demand(expr: &Expr, functions: &HashMap<String, FunctionInfo>) -> usize {
    let mut nested = if let Expr::Block { body, .. } = expr {
        body_expr_demand(body, &|child| call_arg_demand(child, functions))
    } else {
        0
    };
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        nested = nested.max(call_arg_demand(child, functions));
    });
    let reserved = if let Expr::Call { callee, args } = expr {
        if let Expr::Ident(name) = callee.as_ref() {
            let defined = functions
                .get(name)
                .is_some_and(|function| !function.body.is_empty());
            let mmio = matches!(
                name.as_str(),
                "load8"
                    | "load16"
                    | "load32"
                    | "load64"
                    | "store8"
                    | "store16"
                    | "store32"
                    | "store64"
            );
            if defined || !mmio { args.len() } else { 0 }
        } else {
            0
        }
    } else {
        0
    };
    // AST nodes and argument vectors already consume at least this much memory.
    // Saturation forces the existing frame-size refusal instead of wrapping.
    reserved.saturating_add(nested)
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

#[cfg(test)]
mod call_temp_tests {
    use super::*;

    fn call(name: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::Ident(name.into())),
            args,
        }
    }
    fn nested(depth: usize) -> Expr {
        (0..depth).fold(Expr::IntLit(42), |value, _| call("step", vec![value]))
    }
    fn function(value: Expr) -> FunctionInfo {
        FunctionInfo {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(value))],
        }
    }
    fn lower(value: Expr) -> Result<CodeEmitter, String> {
        let main = function(value);
        let step = FunctionInfo {
            name: "step".into(),
            params: vec![("value".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![],
        };
        let functions = HashMap::from([("main".into(), main.clone()), ("step".into(), step)]);
        let mut emitter = CodeEmitter::new();
        lower_function(
            &mut emitter,
            &main,
            &functions,
            &HashMap::new(),
            &mut vec![],
        )?;
        Ok(emitter)
    }
    #[test]
    fn deep_calls_reserve_exact_live_slots_and_keep_frame_limit() {
        let emitter = lower(nested(12)).unwrap();
        // Twelve argument words plus the alignment word, no fixed nesting cap.
        assert_eq!(
            &emitter.bytes[4..6],
            &thumb::sub_sp_imm(52).unwrap().to_le_bytes()
        );
        assert!(
            lower(nested(128))
                .err()
                .unwrap()
                .contains("call argument frame too large")
        );
    }
    #[test]
    fn demand_tracks_mixed_arities_blocks_casts_and_intrinsic_overrides() {
        let mut functions = HashMap::new();
        let mixed = call(
            "many",
            vec![
                nested(12),
                Expr::IntLit(1),
                Expr::IntLit(2),
                Expr::IntLit(3),
                Expr::IntLit(4),
            ],
        );
        assert_eq!(call_arg_demand(&mixed, &functions), 17);
        let block = Expr::Block {
            body: vec![Stmt::IndexAssign {
                base: Expr::Ident("items".into()),
                index: nested(9),
                value: mixed,
            }],
            result: Some(Box::new(nested(10))),
        };
        let cast = Expr::Unary {
            op: "int-cast-i8".into(),
            expr: Box::new(block),
        };
        assert_eq!(call_arg_demand(&cast, &functions), 17);
        let mmio = call("load32", vec![nested(12)]);
        assert_eq!(call_arg_demand(&mmio, &functions), 12);
        let mut defined = function(Expr::IntLit(42));
        defined.name = "load32".into();
        functions.insert("load32".into(), defined);
        assert_eq!(call_arg_demand(&mmio, &functions), 13);
        assert_eq!(
            call_arg_demand(
                &call("invoke-native1", vec![Expr::IntLit(1), nested(12)]),
                &functions
            ),
            14
        );
    }
    #[test]
    fn reservations_restore_cursor_and_reject_imbalance() {
        let functions = HashMap::new();
        let structs = HashMap::new();
        let mut ctx = LowerCtx::new("test", &[], &functions, &structs);
        ctx.call_arg_temps = vec![0, 4, 8];
        assert_eq!(ctx.acquire_call_arg_temps(2).unwrap(), 0);
        assert_eq!(ctx.acquire_call_arg_temps(1).unwrap(), 2);
        assert!(ctx.acquire_call_arg_temps(1).is_err());
        ctx.release_call_arg_temps(1).unwrap();
        ctx.release_call_arg_temps(2).unwrap();
        assert_eq!(ctx.call_arg_used, 0);
        assert!(ctx.release_call_arg_temps(1).is_err());
    }
}
