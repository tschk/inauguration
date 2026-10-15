use super::PendingInrtCall;
use super::lower_util::{
    array_item_matches, base_struct_name, ensure_native_array_element,
    ensure_native_array_field_element, expr_type,
};
use crate::core_ir::{Expr, LoopKind, Stmt, Typ};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::HashMap;

fn expr_try_depth(expr: &Expr) -> usize {
    let mut depth = if let Expr::Block { body, .. } = expr {
        max_try_depth(body)
    } else {
        0
    };
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        depth = depth.max(expr_try_depth(child))
    });
    depth
}

pub(crate) fn max_try_depth(body: &[Stmt]) -> usize {
    body.iter()
        .map(|stmt| {
            let nested = match stmt {
                Stmt::Try { body, catches } => {
                    1 + max_try_depth(body).max(
                        catches
                            .iter()
                            .map(|catch| max_try_depth(&catch.body))
                            .max()
                            .unwrap_or(0),
                    )
                }
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => max_try_depth(then_body).max(max_try_depth(else_body)),
                Stmt::Loop { body, .. } => max_try_depth(body),
                Stmt::Match { arms, .. } => arms
                    .iter()
                    .map(|arm| max_try_depth(&arm.body))
                    .max()
                    .unwrap_or(0),
                _ => 0,
            };
            let expressions = match stmt {
                Stmt::Let(_, _, value)
                | Stmt::Assign(_, value)
                | Stmt::Expr(value)
                | Stmt::Return(Some(value))
                | Stmt::Throw(value) => expr_try_depth(value),
                Stmt::If { cond, .. } => expr_try_depth(cond),
                Stmt::Loop {
                    cond: Some(cond), ..
                } => expr_try_depth(cond),
                Stmt::Match { scrutinee, .. } => expr_try_depth(scrutinee),
                Stmt::FieldAssign { base, value, .. } => {
                    expr_try_depth(base).max(expr_try_depth(value))
                }
                Stmt::IndexAssign {
                    base, index, value, ..
                } => [base, index, value]
                    .into_iter()
                    .map(expr_try_depth)
                    .max()
                    .unwrap_or(0),
                _ => 0,
            };
            nested.max(expressions)
        })
        .max()
        .unwrap_or(0)
}

pub(crate) fn append_string_table(
    emitter: &mut CodeEmitter,
    strings: &HashMap<String, i64>,
    pending: Vec<super::PendingString>,
) -> Result<(), String> {
    if pending.is_empty() {
        return Ok(());
    }
    while !emitter.len().is_multiple_of(8) {
        emitter.bytes.push(0);
    }
    // Build an ordered list of (index, string) pairs so we can lay them out by index.
    let mut ordered: Vec<(i64, &String)> = strings.iter().map(|(s, &idx)| (idx, s)).collect();
    ordered.sort_by_key(|(idx, _)| *idx);

    // Build map from index -> offset of the string length header. JIT code will
    // load this address as the instring pointer; the runtime reads the 8-byte
    // length header and then the bytes that follow.
    let mut index_offsets: HashMap<i64, i64> = HashMap::new();
    for (idx, value) in ordered {
        assert!(
            emitter.len().is_multiple_of(8),
            "string table entry must be 8-byte aligned"
        );
        let header_offset = crate::native_emit::global_data::append_string_payload(
            &mut emitter.bytes,
            value,
            crate::native_emit::global_data::StringLayout::Instring,
        )?;
        index_offsets.insert(idx, header_offset as i64);
    }

    for p in pending {
        // Every `PendingString` comes from `LowerCtx::string_id`, which only
        // hands out indices present in the pool, so a miss here is a compiler
        // bug. Failing loudly beats leaving the `adr` unpatched: an unpatched
        // `adr rd, 0` yields a self-referential pointer and the program would
        // read garbage instead of the literal.
        let header_offset = index_offsets.get(&p.string_index).ok_or_else(|| {
            format!(
                "native-lower: string table is missing pool index {} (internal invariant)",
                p.string_index
            )
        })?;
        let adr_delta = (*header_offset - p.adr_site as i64) as i32;
        emitter.patch_u32(p.adr_site, aarch64::adr(p.rd, adr_delta));
    }
    Ok(())
}

fn alloc_expr_locals(
    ctx: &mut LowerCtx<'_>,
    expr: &Expr,
    fn_name: &str,
    functions: &HashMap<String, super::FunctionInfo>,
) -> Result<(), String> {
    if let Expr::MatchValue { scrutinee, arms } = expr {
        alloc_expr_locals(ctx, scrutinee, fn_name, functions)?;
        let typ = super::lower_expr::native_expr_type(scrutinee, ctx, functions)
            .ok_or("native-lower: unknown match scrutinee type")?;
        let mut index = ctx.locals.len();
        let name = loop {
            let name = format!("__native_match_value_{index}");
            if !ctx.locals.contains_key(&name)
                && !ctx.params.contains_key(&name)
                && !ctx.globals.contains_key(&name)
            {
                break name;
            }
            index += 1;
        };
        ctx.alloc_let_local(&name, Some(&typ), scrutinee, fn_name, functions)?;
        let mut planned = Vec::new();
        for arm in arms {
            let (test, bindings) = crate::lower_core::value_pattern_test(
                &arm.pattern,
                &typ,
                &|name| ctx.structs.get(name).map(Vec::as_slice),
                Expr::Ident(name.clone()),
            )?;
            alloc_declared_locals(ctx, &bindings, fn_name, functions)?;
            if let Some(guard) = &arm.guard {
                alloc_expr_locals(ctx, guard, fn_name, functions)?;
            }
            alloc_expr_locals(ctx, &arm.value, fn_name, functions)?;
            planned.push((test, bindings));
        }
        ctx.match_values
            .insert(expr as *const Expr as usize, (name, typ, planned));
        return Ok(());
    }
    if let Expr::Block { body, .. } = expr {
        alloc_declared_locals(ctx, body, fn_name, functions)?;
    }
    let mut result = Ok(());
    crate::core_ir::for_each_expr_child(expr, &mut |child| {
        if result.is_ok() {
            result = alloc_expr_locals(ctx, child, fn_name, functions);
        }
    });
    result
}

pub(crate) fn alloc_declared_locals(
    ctx: &mut LowerCtx<'_>,
    body: &[Stmt],
    fn_name: &str,
    functions: &HashMap<String, super::FunctionInfo>,
) -> Result<(), String> {
    for stmt in body {
        let mut expressions = Vec::new();
        match stmt {
            Stmt::Let(_, _, value)
            | Stmt::Assign(_, value)
            | Stmt::Expr(value)
            | Stmt::Return(Some(value))
            | Stmt::Throw(value) => expressions.push(value),
            Stmt::If { cond, .. } => expressions.push(cond),
            Stmt::Loop {
                cond: Some(cond), ..
            } => expressions.push(cond),
            Stmt::Match { scrutinee, .. } => expressions.push(scrutinee),
            Stmt::FieldAssign { base, value, .. } => expressions.extend([base, value]),
            Stmt::IndexAssign {
                base, index, value, ..
            } => expressions.extend([base, index, value]),
            _ => {}
        }
        for expr in expressions {
            alloc_expr_locals(ctx, expr, fn_name, functions)?;
        }

        match stmt {
            Stmt::Let(name, typ, expr) => {
                ctx.alloc_let_local(name, typ.as_ref(), expr, fn_name, functions)?
            }
            Stmt::Break | Stmt::Continue | Stmt::Propagate => {}
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                alloc_declared_locals(ctx, then_body, fn_name, functions)?;
                alloc_declared_locals(ctx, else_body, fn_name, functions)?;
            }
            Stmt::Loop {
                kind: LoopKind::For { binding },
                body,
                ..
            } => {
                ctx.alloc_local(binding, Some(&Typ::Int), fn_name)?;
                let ptr = ctx.alloc_slot();
                let len = ctx.alloc_slot();
                let index = ctx.alloc_slot();
                ctx.vec_for_slots.allocate(VecForSlots { ptr, len, index });
                alloc_declared_locals(ctx, body, fn_name, functions)?;
            }
            Stmt::Loop { body, .. } => alloc_declared_locals(ctx, body, fn_name, functions)?,
            Stmt::Match { scrutinee, arms } => {
                for arm in arms {
                    if let crate::core_ir::MatchPattern::IdentPat(name) =
                        crate::core_ir::MatchPattern::parse(&arm.pattern)?
                    {
                        let typ = super::lower_expr::native_expr_type(scrutinee, ctx, functions)
                            .map(|typ| typ.canonical())
                            .ok_or("native-lower: unknown match binding type")?;
                        if !matches!(typ, Typ::Int | Typ::Bool | Typ::Float | Typ::String) {
                            return Err("native-lower: unsupported aggregate match binding".into());
                        }
                        ctx.alloc_local(&name, Some(&typ), fn_name)?;
                    }
                    alloc_declared_locals(ctx, &arm.body, fn_name, functions)?;
                }
            }
            Stmt::Return(_)
            | Stmt::Assign(_, _)
            | Stmt::IndexAssign { .. }
            | Stmt::FieldAssign { .. }
            | Stmt::Expr(_) => {}
            Stmt::Throw(_) => {}
            Stmt::Try { body, catches, .. } => {
                alloc_declared_locals(ctx, body, fn_name, functions)?;
                for catch in catches {
                    if let crate::core_ir::MatchPattern::IdentPat(name) =
                        crate::core_ir::MatchPattern::parse(&catch.pattern)?
                    {
                        ctx.alloc_local(&name, Some(&Typ::Int), fn_name)?;
                    }
                    alloc_declared_locals(ctx, &catch.body, fn_name, functions)?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) enum LocalSlot {
    Scalar(u32),
    Array {
        elem: Typ,
        offsets: Vec<u32>,
    },
    ArrayParam {
        elem: Typ,
        ptr_offset: u32,
        len_offset: u32,
    },
    Struct {
        typ: String,
        fields: HashMap<String, u32>,
    },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct VecForSlots {
    pub(crate) ptr: u32,
    pub(crate) len: u32,
    pub(crate) index: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct IteratorMapSlots {
    pub(crate) ptr: u32,
    pub(crate) len: u32,
    pub(crate) index: u32,
    pub(crate) binding: u32,
}

#[derive(Default)]
pub(crate) struct VecForPlan {
    slots: Vec<VecForSlots>,
    next: usize,
}

impl VecForPlan {
    fn allocate(&mut self, slots: VecForSlots) {
        self.slots.push(slots);
    }

    fn next(&mut self, fn_name: &str) -> Result<VecForSlots, String> {
        let Some(slots) = self.slots.get(self.next).copied() else {
            return Err(format!(
                "native-lower: missing Vec iterator state in `{fn_name}`"
            ));
        };
        self.next += 1;
        Ok(slots)
    }

    fn assert_consumed(&self, fn_name: &str) -> Result<(), String> {
        if self.next == self.slots.len() {
            Ok(())
        } else {
            Err(format!(
                "native-lower: Vec iterator plan incomplete in `{fn_name}` ({}/{} loops lowered)",
                self.next,
                self.slots.len()
            ))
        }
    }
}

#[derive(Default)]
pub(crate) struct LoopBranches {
    pub(crate) breaks: Vec<u32>,
    pub(crate) continues: Vec<u32>,
    pub(crate) try_depth: usize,
}

pub(crate) struct LowerCtx<'a> {
    /// Parameter name → stack offset (params fully spilled, no register residency)
    pub(crate) params: HashMap<String, u32>,
    pub(crate) receiver_pointers: HashMap<String, u32>,
    pub(crate) param_types: HashMap<String, Typ>,
    pub(crate) param_stores: Vec<(u8, u32)>,
    /// Stack-based params: (incoming_stack_offset, local_stack_offset)
    pub(crate) stack_params: Vec<(u32, u32)>,
    pub(crate) locals: HashMap<String, LocalSlot>,
    pub(crate) match_values: HashMap<usize, (String, Typ, Vec<(Expr, Vec<Stmt>)>)>,
    pub(crate) globals: &'a HashMap<String, u64>,
    pub(crate) global_types: &'a HashMap<String, Typ>,
    pub(crate) pending_globals: Vec<(u32, u64)>,
    pub(crate) scalar_types: HashMap<String, Typ>,
    pub(crate) vec_for_slots: VecForPlan,
    pub(crate) structs: &'a HashMap<String, Vec<(String, Typ)>>,
    pub(crate) strings: &'a HashMap<String, i64>,
    pub(crate) pending_inrt_calls: &'a mut Vec<PendingInrtCall>,
    pub(crate) pending_strings: &'a mut Vec<super::PendingString>,
    pub(crate) stack_size: u32,
    pub(crate) emitted_return: bool,
    pub(crate) error_branches: Vec<Vec<u32>>,
    pub(crate) error_types: HashMap<String, u32>,
    pub(crate) ret_typ: Typ,
    arrays_in_aggregates: bool,
    pub(crate) loop_branches: Vec<LoopBranches>,
    pub(crate) _params_src: &'a [(String, Typ)],
    pub(crate) try_flag_slots: Vec<u32>,
    pub(crate) active_try_flags: Vec<u32>,
    pub(crate) prologue_stack_reserve: u32,
    /// Stack offset for saving binary operation lhs (preserved across rhs eval)
    pub(crate) binop_temp: u32,
    pub(crate) binop_temps: [u32; 64],
    pub(crate) binop_depth: usize,
    pub(crate) call_arg_temps: [u32; 64],
    pub(crate) call_arg_depth: usize,
    call_arg_frames: Vec<usize>,
    pub(crate) vec_literal_header_offset: Option<u32>,
    pub(crate) aggregate_vector_scratch: Option<(u32, usize)>,
    pub(crate) iterator_chain_header_offset: Option<u32>,
    pub(crate) iterator_map_slots: Option<IteratorMapSlots>,
}

#[allow(clippy::only_used_in_recursion)]
pub(crate) fn alloc_nested_struct_slots(
    ctx: &mut LowerCtx<'_>,
    struct_name: &str,
    fields: &[(String, Typ)],
    structs: &HashMap<String, Vec<(String, Typ)>>,
    abi_idx: &mut usize,
    fn_name: &str,
) -> Result<HashMap<String, u32>, String> {
    let mut slots = HashMap::new();
    alloc_nested_struct_slots_inner(
        ctx,
        struct_name,
        fields,
        structs,
        abi_idx,
        fn_name,
        &mut Vec::new(),
        &mut slots,
        true,
    )?;
    if slots.is_empty() {
        slots.insert("__base".to_string(), ctx.alloc_slot());
    }
    Ok(slots)
}

fn alloc_nested_struct_slots_inner(
    ctx: &mut LowerCtx<'_>,
    struct_name: &str,
    fields: &[(String, Typ)],
    structs: &HashMap<String, Vec<(String, Typ)>>,
    abi_idx: &mut usize,
    _fn_name: &str,
    visited: &mut Vec<String>,
    slots: &mut HashMap<String, u32>,
    store_params: bool,
) -> Result<(), String> {
    let base_name = base_struct_name(struct_name);
    if visited.contains(&base_name.to_string()) {
        slots.insert(format!("{base_name}.__cycle"), ctx.alloc_slot());
        return Ok(());
    }
    visited.push(base_name.to_string());
    for (field, field_ty) in fields {
        match field_ty {
            Typ::Void => {
                return Err(
                    "native-lower: unit aggregate fields require a zero-width layout".into(),
                );
            }
            Typ::Int | Typ::Bool | Typ::String | Typ::Float => {
                let offset = ctx.alloc_slot();
                if store_params {
                    if *abi_idx < 8 {
                        ctx.param_stores.push((*abi_idx as u8, offset));
                    } else {
                        ctx.stack_params.push(((*abi_idx - 8) as u32, offset));
                    }
                }
                slots.insert(field.clone(), offset);
                *abi_idx += 1;
            }
            Typ::Array(elem) => {
                ensure_native_array_field_element(elem, _fn_name)?;
                // Preserve the scalar array ABI order for aggregate copies and receivers.
                for suffix in ["ptr", "len"] {
                    let offset = ctx.alloc_slot();
                    if store_params {
                        if *abi_idx < 8 {
                            ctx.param_stores.push((*abi_idx as u8, offset));
                        } else {
                            ctx.stack_params.push(((*abi_idx - 8) as u32, offset));
                        }
                    }
                    slots.insert(format!("{field}.{suffix}"), offset);
                    *abi_idx += 1;
                }
            }
            Typ::Named(_) | Typ::Vector(_) => {
                let inner_name = match field_ty {
                    Typ::Named(name) => name.as_str(),
                    Typ::Vector(_) => "Vec",
                    _ => {
                        return Err(format!(
                            "Internal error: expected Named or Vector type, got {:?}",
                            field_ty
                        ));
                    }
                };
                let base = base_struct_name(inner_name);
                let Some(inner_fields) = structs.get(base) else {
                    slots.insert(format!("{field}.__opaque"), ctx.alloc_slot());
                    continue;
                };
                let mut inner_slots = HashMap::new();
                alloc_nested_struct_slots_inner(
                    ctx,
                    inner_name,
                    inner_fields,
                    structs,
                    abi_idx,
                    _fn_name,
                    visited,
                    &mut inner_slots,
                    store_params,
                )?;
                // Flatten: nested struct fields go into parent's slot map with <field>.<subfield> keys
                for (sub_field, sub_offset) in inner_slots {
                    slots.insert(format!("{field}.{sub_field}"), sub_offset);
                }
            }
            _ => {
                // Unknown field type: allocate as opaque scalar slot
                slots.insert(field.clone(), ctx.alloc_slot());
            }
        }
    }
    visited.pop();
    if slots.is_empty() {
        slots.insert("__base".to_string(), ctx.alloc_slot());
    }
    Ok(())
}

pub(crate) fn alloc_local_struct_fields(
    slots: &mut HashMap<String, u32>,
    _struct_name: &str,
    fields: &[(String, Typ)],
    all_structs: &HashMap<String, Vec<(String, Typ)>>,
    ctx: &mut LowerCtx<'_>,
    _fn_name: &str,
) -> Result<(), String> {
    for (field, field_ty) in fields {
        match field_ty {
            Typ::Void => {
                return Err(
                    "native-lower: unit aggregate fields require a zero-width layout".into(),
                );
            }
            Typ::Int | Typ::Bool | Typ::String | Typ::Float => {
                slots.insert(field.clone(), ctx.alloc_slot());
            }
            Typ::Array(elem) => {
                ensure_native_array_field_element(elem, _fn_name)?;
                slots.insert(format!("{field}.ptr"), ctx.alloc_slot());
                slots.insert(format!("{field}.len"), ctx.alloc_slot());
            }
            Typ::Named(_) | Typ::Vector(_) => {
                let inner_name = match field_ty {
                    Typ::Named(name) => name.as_str(),
                    Typ::Vector(_) => "Vec",
                    _ => {
                        return Err(format!(
                            "Internal error: expected Named or Vector type, got {:?}",
                            field_ty
                        ));
                    }
                };
                let Some(inner_fields) = all_structs.get(inner_name) else {
                    slots.insert(field.clone(), ctx.alloc_slot());
                    continue;
                };
                let mut inner_slots = HashMap::new();
                alloc_local_struct_fields(
                    &mut inner_slots,
                    inner_name,
                    inner_fields,
                    all_structs,
                    ctx,
                    _fn_name,
                )?;
                for (sub_field, sub_offset) in inner_slots {
                    slots.insert(format!("{field}.{sub_field}"), sub_offset);
                }
            }
            _ => {
                slots.insert(field.clone(), ctx.alloc_slot());
            }
        }
    }
    if slots.is_empty() {
        slots.insert("__base".to_string(), ctx.alloc_slot());
    }
    Ok(())
}

impl<'a> LowerCtx<'a> {
    pub(crate) fn new(
        params: &'a [(String, Typ)],
        structs: &'a HashMap<String, Vec<(String, Typ)>>,
        strings: &'a HashMap<String, i64>,
        globals: &'a HashMap<String, u64>,
        global_types: &'a HashMap<String, Typ>,
        pending_inrt_calls: &'a mut Vec<PendingInrtCall>,
        pending_strings: &'a mut Vec<super::PendingString>,
        fn_name: &str,
    ) -> Result<Self, String> {
        let mut ctx = Self {
            params: HashMap::new(),
            receiver_pointers: HashMap::new(),
            param_types: HashMap::new(),
            param_stores: Vec::new(),
            stack_params: Vec::new(),
            locals: HashMap::new(),
            match_values: HashMap::new(),
            globals,
            global_types,
            pending_globals: Vec::new(),
            scalar_types: HashMap::new(),
            vec_for_slots: VecForPlan::default(),
            structs,
            strings,
            pending_inrt_calls,
            pending_strings,
            stack_size: 0,
            emitted_return: false,
            error_branches: Vec::new(),
            error_types: HashMap::new(),
            ret_typ: Typ::Int,
            arrays_in_aggregates: structs
                .values()
                .flatten()
                .any(|(_, typ)| matches!(typ.canonical(), Typ::Array(_))),
            loop_branches: Vec::new(),
            _params_src: params,
            try_flag_slots: Vec::new(),
            active_try_flags: Vec::new(),
            prologue_stack_reserve: 0,
            binop_temp: 0,
            binop_temps: [0; 64],
            binop_depth: 0,
            call_arg_temps: [0; 64],
            call_arg_depth: 0,
            call_arg_frames: Vec::new(),
            vec_literal_header_offset: None,
            aggregate_vector_scratch: None,
            iterator_chain_header_offset: None,
            iterator_map_slots: None,
        };
        let mut abi_idx = 0usize;
        for (name, typ) in params {
            match typ {
                Typ::Int | Typ::Bool | Typ::String | Typ::Float => {
                    let offset = ctx.alloc_slot();
                    if abi_idx < 8 {
                        ctx.param_stores.push((abi_idx as u8, offset));
                    } else {
                        // Stack-based param: load from caller's stack later
                        ctx.stack_params.push(((abi_idx - 8) as u32, offset));
                    }
                    ctx.params.insert(name.clone(), offset);
                    ctx.param_types.insert(name.clone(), typ.clone());
                    abi_idx += 1;
                }
                Typ::Named(struct_name) => {
                    if struct_name == "String[]" {
                        // Java `String[] args` is emitted as a named type by the Java front.
                        let elem = Typ::String;
                        ensure_native_array_element(&elem, fn_name, "parameter")?;
                        let ptr_offset = ctx.alloc_slot();
                        let len_offset = ctx.alloc_slot();
                        if abi_idx + 1 < 8 {
                            ctx.param_stores.push((abi_idx as u8, ptr_offset));
                            ctx.param_stores.push(((abi_idx + 1) as u8, len_offset));
                        } else if abi_idx >= 8 {
                            ctx.stack_params.push(((abi_idx - 8) as u32, ptr_offset));
                            ctx.stack_params
                                .push(((abi_idx + 1 - 8) as u32, len_offset));
                        } else {
                            return Err(format!(
                                "native-lower: array param straddles register/stack boundary in `{fn_name}`"
                            ));
                        }
                        ctx.locals.insert(
                            name.clone(),
                            LocalSlot::ArrayParam {
                                elem,
                                ptr_offset,
                                len_offset,
                            },
                        );
                        abi_idx += 2;
                        continue;
                    }
                    let base = base_struct_name(struct_name);
                    let Some(fields) = structs.get(base) else {
                        let offset = ctx.alloc_slot();
                        if abi_idx < 8 {
                            ctx.param_stores.push((abi_idx as u8, offset));
                        } else {
                            ctx.stack_params.push(((abi_idx - 8) as u32, offset));
                        }
                        ctx.locals.insert(name.clone(), LocalSlot::Scalar(offset));
                        ctx.param_types.insert(name.clone(), typ.clone());
                        abi_idx += 1;
                        continue;
                    };
                    let slots = if name == "self" {
                        let pointer_slot = ctx.alloc_slot();
                        if abi_idx < 8 {
                            ctx.param_stores.push((abi_idx as u8, pointer_slot));
                        } else {
                            ctx.stack_params.push(((abi_idx - 8) as u32, pointer_slot));
                        }
                        ctx.receiver_pointers.insert(name.clone(), pointer_slot);
                        ctx.param_types.insert(name.clone(), typ.clone());
                        abi_idx += 1;
                        let mut slots = HashMap::new();
                        alloc_nested_struct_slots_inner(
                            &mut ctx,
                            struct_name,
                            fields,
                            structs,
                            &mut 0,
                            fn_name,
                            &mut Vec::new(),
                            &mut slots,
                            false,
                        )?;
                        slots
                    } else {
                        alloc_nested_struct_slots(
                            &mut ctx,
                            struct_name,
                            fields,
                            structs,
                            &mut abi_idx,
                            fn_name,
                        )?
                    };
                    ctx.locals.insert(
                        name.clone(),
                        LocalSlot::Struct {
                            typ: struct_name.clone(),
                            fields: slots,
                        },
                    );
                }
                Typ::Array(elem) => {
                    ensure_native_array_element(elem, fn_name, "parameter")?;
                    let ptr_offset = ctx.alloc_slot();
                    let len_offset = ctx.alloc_slot();
                    if abi_idx + 1 < 8 {
                        ctx.param_stores.push((abi_idx as u8, ptr_offset));
                        ctx.param_stores.push(((abi_idx + 1) as u8, len_offset));
                    } else if abi_idx >= 8 {
                        ctx.stack_params.push(((abi_idx - 8) as u32, ptr_offset));
                        ctx.stack_params
                            .push(((abi_idx + 1 - 8) as u32, len_offset));
                    } else {
                        return Err(format!(
                            "native-lower: array param straddles register/stack boundary in `{fn_name}`"
                        ));
                    }
                    ctx.locals.insert(
                        name.clone(),
                        LocalSlot::ArrayParam {
                            elem: elem.as_ref().clone(),
                            ptr_offset,
                            len_offset,
                        },
                    );
                    abi_idx += 2;
                }
                Typ::Vector(_) => {
                    super::lower_util::ensure_unit_sequence_layout(typ)?;
                    ctx.param_types.insert(name.clone(), typ.clone());
                    let Some(fields) = structs.get("Vec") else {
                        return Err(format!("native-lower: missing Vec ABI type in `{fn_name}`"));
                    };
                    let slots = alloc_nested_struct_slots(
                        &mut ctx,
                        "Vec",
                        fields,
                        structs,
                        &mut abi_idx,
                        fn_name,
                    )?;
                    ctx.locals.insert(
                        name.clone(),
                        LocalSlot::Struct {
                            typ: "Vec".to_string(),
                            fields: slots,
                        },
                    );
                }
                Typ::Void => {
                    let offset = ctx.alloc_slot();
                    ctx.params.insert(name.clone(), offset);
                    ctx.param_types.insert(name.clone(), Typ::Void);
                    // Void param takes no register slots
                }
                _ => {
                    return Err(format!(
                        "native-lower: unsupported parameter type `{typ:?}` for `{name}` in `{fn_name}`"
                    ));
                }
            }
        }
        Ok(ctx)
    }

    pub(crate) fn alloc_local(
        &mut self,
        name: &str,
        typ: Option<&Typ>,
        fn_name: &str,
    ) -> Result<(), String> {
        if let Some(typ) = typ {
            super::lower_util::ensure_unit_sequence_layout(typ)?;
        }
        if self.locals.contains_key(name) {
            return Ok(());
        }
        match typ {
            None => {
                let offset = self.alloc_slot();
                self.locals
                    .insert(name.to_string(), LocalSlot::Scalar(offset));
                self.scalar_types.insert(name.to_string(), Typ::Int);
                Ok(())
            }
            Some(Typ::Int | Typ::Bool | Typ::String | Typ::Float | Typ::Void) => {
                let offset = self.alloc_slot();
                self.locals
                    .insert(name.to_string(), LocalSlot::Scalar(offset));
                self.scalar_types
                    .insert(name.to_string(), typ.expect("primitive type").clone());
                Ok(())
            }
            Some(Typ::Array(_)) => Err(format!(
                "native-lower: unsupported let binding type in `{fn_name}` (array locals require literal initializers)"
            )),
            Some(Typ::Named(struct_name)) => {
                let resolved = if struct_name == "Self" {
                    fn_name
                        .split("::")
                        .next()
                        .filter(|outer| self.structs.contains_key(*outer))
                        .map(|outer| outer.to_string())
                        .unwrap_or_else(|| struct_name.to_string())
                } else {
                    struct_name.clone()
                };
                if let Some(fields) = self.structs.get(&resolved) {
                    let mut slots = HashMap::new();
                    alloc_local_struct_fields(
                        &mut slots,
                        &resolved,
                        fields,
                        self.structs,
                        self,
                        fn_name,
                    )?;
                    self.locals.insert(
                        name.to_string(),
                        LocalSlot::Struct {
                            typ: resolved.to_string(),
                            fields: slots,
                        },
                    );
                } else {
                    let offset = self.alloc_slot();
                    self.locals
                        .insert(name.to_string(), LocalSlot::Scalar(offset));
                    self.scalar_types.insert(name.to_string(), Typ::Int);
                }
                Ok(())
            }
            Some(vector @ Typ::Vector(_)) => {
                self.scalar_types.insert(name.to_string(), vector.clone());
                let Some(fields) = self.structs.get("Vec") else {
                    return Err(format!("native-lower: missing Vec ABI type in `{fn_name}`"));
                };
                let mut slots = HashMap::new();
                alloc_local_struct_fields(&mut slots, "Vec", fields, self.structs, self, fn_name)?;
                self.locals.insert(
                    name.to_string(),
                    LocalSlot::Struct {
                        typ: "Vec".to_string(),
                        fields: slots,
                    },
                );
                Ok(())
            }
            _ => Err(format!(
                "native-lower: unsupported let binding type in `{fn_name}` ({typ:?})"
            )),
        }
    }

    pub(crate) fn alloc_let_local(
        &mut self,
        name: &str,
        typ: Option<&Typ>,
        expr: &Expr,
        fn_name: &str,
        functions: &HashMap<String, super::FunctionInfo>,
    ) -> Result<(), String> {
        if self.locals.contains_key(name) {
            return Ok(());
        }
        let resolved = typ
            .cloned()
            .or_else(|| super::lower_expr::native_expr_type(expr, self, functions));
        if let Some(typ) = &resolved {
            super::lower_util::ensure_unit_sequence_layout(typ)?;
        }
        if let Some(Typ::Array(elem)) = resolved.as_ref() {
            ensure_native_array_element(elem, fn_name, "local")?;
            // Local aliases share inline array metadata rather than pretending
            // an inline array is a returned pointer/length pair.
            if let Expr::Ident(source) = expr
                && let Some(slot @ LocalSlot::Array { elem: actual, .. }) = self.locals.get(source)
                && actual == elem.as_ref()
            {
                self.locals.insert(name.to_string(), slot.clone());
                return Ok(());
            }
            // Array-returning functions can forward locals through other calls.
            // Heap-backed aggregate fields must preserve aliases before and after a frame exits.
            // shortcut: any array-bearing schema disables inline locals; refine with escape analysis.
            let inline_items = match expr {
                Expr::ArrayLit(items)
                    if !self.arrays_in_aggregates && !matches!(self.ret_typ, Typ::Array(_)) =>
                {
                    Some(items)
                }
                _ => None,
            };
            let Some(items) = inline_items else {
                let ptr_offset = self.alloc_slot();
                let len_offset = self.alloc_slot();
                self.locals.insert(
                    name.to_string(),
                    LocalSlot::ArrayParam {
                        elem: elem.as_ref().clone(),
                        ptr_offset,
                        len_offset,
                    },
                );
                return Ok(());
            };
            let mut offsets = Vec::with_capacity(items.len());
            for item in items {
                if let Some(item_ty) = expr_type(item)
                    && !array_item_matches(elem, &item_ty)
                {
                    return Err(format!(
                        "native-lower: array item type mismatch in `{fn_name}`"
                    ));
                }
                offsets.push(self.alloc_slot());
            }
            self.locals.insert(
                name.to_string(),
                LocalSlot::Array {
                    elem: elem.as_ref().clone(),
                    offsets,
                },
            );
            return Ok(());
        }
        self.alloc_local(name, resolved.as_ref(), fn_name)
    }

    pub(crate) fn receiver_field_type(&self, name: &str, field: &str) -> Option<Typ> {
        let LocalSlot::Struct { typ, .. } = self.locals.get(name)? else {
            return None;
        };
        let mut typ = Typ::Named(typ.clone());
        for part in field.split('.') {
            let schema = match typ.canonical() {
                Typ::Named(name) => self.structs.get(base_struct_name(&name))?,
                Typ::Vector(_) => self.structs.get("Vec")?,
                _ => return None,
            };
            typ = schema.iter().find(|(name, _)| name == part)?.1.clone();
        }
        Some(typ.canonical())
    }

    pub(crate) fn receiver_field(&self, name: &str, field: &str) -> Option<(u32, u32)> {
        let pointer = *self.receiver_pointers.get(name)?;
        let LocalSlot::Struct { typ, fields } = self.locals.get(name)? else {
            return None;
        };
        if self
            .structs
            .get(base_struct_name(typ))
            .is_some_and(|fields| fields.is_empty())
        {
            return None;
        }
        let offset = *super::lower_util::find_field_offset(fields, field)?;
        let base = *fields.values().min()?;
        Some((pointer, offset - base))
    }

    pub(crate) fn validate_global_struct(&self, global: &str) -> Result<(), String> {
        if let Some(Typ::Named(name)) = self.global_types.get(global)
            && self.structs.get(name).is_some_and(|fields| {
                fields
                    .iter()
                    .all(|(_, typ)| super::lower_util::is_native_scalar_type(typ))
            })
        {
            return Ok(());
        }
        Err("native-lower: global struct values require a known scalar-field layout".into())
    }

    pub(crate) fn load_global_struct_field(
        &mut self,
        emitter: &mut CodeEmitter,
        global: &str,
        field: &str,
        reg: u8,
        fn_name: &str,
    ) -> Result<(), String> {
        let key = format!("{global}.{field}");
        let offset = self.globals.get(&key).copied().ok_or_else(|| {
            format!("native-lower: missing global struct field `{key}` in `{fn_name}`")
        })?;
        self.emit_global_address(emitter, reg, offset);
        emitter.emit_u32(aarch64::ldr64(reg, reg, 0));
        Ok(())
    }

    pub(crate) fn emit_global_address(
        &mut self,
        emitter: &mut crate::native_emit::aarch64::CodeEmitter,
        rd: u8,
        offset: u64,
    ) {
        // LDR literal, then branch past its embedded relocatable address.
        emitter.emit_u32(0x5800_0040 | u32::from(rd));
        emitter.emit_u32(crate::native_emit::aarch64::b(12));
        self.pending_globals.push((emitter.len(), offset));
        emitter.bytes.extend_from_slice(&0_u64.to_le_bytes());
    }

    pub(crate) fn alloc_slot(&mut self) -> u32 {
        let offset = self.stack_size;
        self.stack_size += 8;
        offset
    }

    pub(crate) fn acquire_binop_temp(&mut self, fn_name: &str) -> Result<u32, String> {
        let Some(offset) = self.binop_temps.get(self.binop_depth).copied() else {
            return Err(format!(
                "native-lower: binary expression nesting is too deep in `{fn_name}`"
            ));
        };
        self.binop_depth += 1;
        Ok(offset)
    }

    pub(crate) fn release_binop_temp(&mut self) {
        self.binop_depth -= 1;
    }

    pub(crate) fn acquire_call_arg_temps(&mut self, fn_name: &str) -> Result<usize, String> {
        self.acquire_call_arg_slots(fn_name, 8)
    }

    pub(crate) fn acquire_call_arg_slots(
        &mut self,
        fn_name: &str,
        slots: usize,
    ) -> Result<usize, String> {
        let base = self.call_arg_depth;
        if base + slots > self.call_arg_temps.len() {
            return Err(format!(
                "native-lower: call nesting is too deep in `{fn_name}`"
            ));
        }
        self.call_arg_depth += slots;
        self.call_arg_frames.push(slots);
        Ok(base)
    }

    pub(crate) fn release_call_arg_temps(&mut self) {
        self.call_arg_depth -= self
            .call_arg_frames
            .pop()
            .expect("balanced call argument slots");
    }

    pub(crate) fn next_vec_for_slots(&mut self, fn_name: &str) -> Result<VecForSlots, String> {
        self.vec_for_slots.next(fn_name)
    }

    pub(crate) fn assert_vec_for_slots_consumed(&self, fn_name: &str) -> Result<(), String> {
        self.vec_for_slots.assert_consumed(fn_name)
    }

    pub(crate) fn stack_reserve(&self) -> u32 {
        self.stack_size.next_multiple_of(16)
    }

    pub(crate) fn scalar_type(&self, name: &str) -> Option<Typ> {
        self.param_types
            .get(name)
            .or_else(|| self.scalar_types.get(name))
            .or_else(|| self.global_types.get(name))
            .cloned()
    }

    pub(crate) fn string_id(&self, value: &str) -> Result<i64, String> {
        // The empty string is a real pool entry at index 0, not a null-pointer
        // sentinel: code that reads a length header or compares payloads must be
        // able to treat every literal uniformly.
        self.strings.get(value).copied().ok_or_else(|| {
            format!("native-lower: string literal not found in constant pool: `{value}`")
        })
    }
}
