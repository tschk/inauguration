use super::lower_util::{
    array_item_matches, base_struct_name, ensure_native_array_element, expr_type,
};
use super::{PendingInrtCall, PendingStaticArray};
use crate::core_ir::{Expr, LoopKind, Stmt, Typ};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::HashMap;

pub(crate) fn append_static_arrays(emitter: &mut CodeEmitter, arrays: Vec<PendingStaticArray>) {
    for array in arrays {
        while !emitter.len().is_multiple_of(8) {
            emitter.bytes.push(0);
        }
        let data_offset = emitter.len();
        let adr_delta = data_offset as i32 - array.adr_site as i32;
        emitter.patch_u32(array.adr_site, aarch64::adr(0, adr_delta));
        for value in array.values {
            emitter.bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
}

pub(crate) fn append_string_table(
    emitter: &mut CodeEmitter,
    strings: &HashMap<String, i64>,
    pending: Vec<super::PendingString>,
) {
    if pending.is_empty() {
        return;
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
        let header_offset = emitter.len() as i64;
        index_offsets.insert(idx, header_offset);
        emitter
            .bytes
            .extend_from_slice(&(value.len() as u64).to_le_bytes());
        emitter.bytes.extend_from_slice(value.as_bytes());
        // Pad to 8-byte alignment for the next entry.
        while !emitter.len().is_multiple_of(8) {
            emitter.bytes.push(0);
        }
    }

    for p in pending {
        let Some(header_offset) = index_offsets.get(&p.string_index) else {
            continue;
        };
        let adr_delta = (*header_offset - p.adr_site as i64) as i32;
        emitter.patch_u32(p.adr_site, aarch64::adr(p.rd, adr_delta));
    }
}

pub(crate) fn alloc_declared_locals(
    ctx: &mut LowerCtx<'_>,
    body: &[Stmt],
    fn_name: &str,
    functions: &HashMap<String, super::FunctionInfo>,
) -> Result<(), String> {
    for stmt in body {
        match stmt {
            Stmt::Let(name, typ, expr) => {
                ctx.alloc_let_local(name, typ.as_ref(), expr, fn_name, functions)?
            }
            Stmt::Break | Stmt::Propagate => {}
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
            Stmt::Match { arms, .. } => {
                for arm in arms {
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
                    ctx.alloc_local(&catch.pattern, Some(&Typ::Int), fn_name)?;
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

pub(crate) struct LowerCtx<'a> {
    /// Parameter name → stack offset (params fully spilled, no register residency)
    pub(crate) params: HashMap<String, u32>,
    pub(crate) param_types: HashMap<String, Typ>,
    pub(crate) param_stores: Vec<(u8, u32)>,
    /// Stack-based params: (incoming_stack_offset, local_stack_offset)
    pub(crate) stack_params: Vec<(u32, u32)>,
    pub(crate) locals: HashMap<String, LocalSlot>,
    pub(crate) scalar_types: HashMap<String, Typ>,
    pub(crate) vec_for_slots: VecForPlan,
    pub(crate) structs: &'a HashMap<String, Vec<(String, Typ)>>,
    pub(crate) strings: &'a HashMap<String, i64>,
    pub(crate) pending_static_arrays: &'a mut Vec<PendingStaticArray>,
    pub(crate) pending_inrt_calls: &'a mut Vec<PendingInrtCall>,
    pub(crate) pending_strings: &'a mut Vec<super::PendingString>,
    pub(crate) stack_size: u32,
    pub(crate) emitted_return: bool,
    pub(crate) _params_src: &'a [(String, Typ)],
    pub(crate) saved_flag_offset: u32,
    pub(crate) prologue_stack_reserve: u32,
    /// Stack offset for saving binary operation lhs (preserved across rhs eval)
    pub(crate) binop_temp: u32,
    pub(crate) binop_temps: [u32; 64],
    pub(crate) binop_depth: usize,
    pub(crate) call_arg_temps: [u32; 64],
    pub(crate) call_arg_depth: usize,
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
) -> Result<(), String> {
    let base_name = base_struct_name(struct_name);
    if visited.contains(&base_name.to_string()) {
        slots.insert(format!("{base_name}.__cycle"), ctx.alloc_slot());
        return Ok(());
    }
    visited.push(base_name.to_string());
    for (field, field_ty) in fields {
        match field_ty {
            Typ::Int | Typ::Bool | Typ::String | Typ::Float => {
                let offset = ctx.alloc_slot();
                if *abi_idx < 8 {
                    ctx.param_stores.push((*abi_idx as u8, offset));
                } else {
                    ctx.stack_params.push(((*abi_idx - 8) as u32, offset));
                }
                slots.insert(field.clone(), offset);
                *abi_idx += 1;
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
            Typ::Int | Typ::Bool | Typ::String | Typ::Float => {
                slots.insert(field.clone(), ctx.alloc_slot());
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
        pending_static_arrays: &'a mut Vec<PendingStaticArray>,
        pending_inrt_calls: &'a mut Vec<PendingInrtCall>,
        pending_strings: &'a mut Vec<super::PendingString>,
        fn_name: &str,
    ) -> Result<Self, String> {
        let mut ctx = Self {
            params: HashMap::new(),
            param_types: HashMap::new(),
            param_stores: Vec::new(),
            stack_params: Vec::new(),
            locals: HashMap::new(),
            scalar_types: HashMap::new(),
            vec_for_slots: VecForPlan::default(),
            structs,
            strings,
            pending_static_arrays,
            pending_inrt_calls,
            pending_strings,
            stack_size: 0,
            emitted_return: false,
            _params_src: params,
            saved_flag_offset: 0,
            prologue_stack_reserve: 0,
            binop_temp: 0,
            binop_temps: [0; 64],
            binop_depth: 0,
            call_arg_temps: [0; 64],
            call_arg_depth: 0,
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
                    let slots = alloc_nested_struct_slots(
                        &mut ctx,
                        struct_name,
                        fields,
                        structs,
                        &mut abi_idx,
                        fn_name,
                    )?;
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
            Some(Typ::Int | Typ::Bool | Typ::String | Typ::Float) => {
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
            Some(Typ::Vector(_)) => {
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
        let resolved = typ.cloned().or_else(|| match expr {
            Expr::Ident(source) => self.locals.get(source).and_then(|slot| match slot {
                LocalSlot::Struct { typ, .. } => Some(Typ::Named(typ.clone())),
                _ => None,
            }),
            Expr::Call { callee, .. } => {
                if let Expr::Ident(target) = callee.as_ref() {
                    functions
                        .get(target)
                        .map(|func| func.ret.clone())
                        .or_else(|| {
                            if let Some(idx) = target.rfind("::") {
                                let last = &target[idx + 2..];
                                functions.get(last).map(|func| func.ret.clone())
                            } else {
                                None
                            }
                        })
                } else {
                    None
                }
            }
            _ => expr_type(expr),
        });
        if let Some(Typ::Array(elem)) = resolved.as_ref() {
            ensure_native_array_element(elem, fn_name, "local")?;
            let Expr::ArrayLit(items) = expr else {
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
        let base = self.call_arg_depth * 8;
        if base + 8 > self.call_arg_temps.len() {
            return Err(format!(
                "native-lower: call nesting is too deep in `{fn_name}`"
            ));
        }
        self.call_arg_depth += 1;
        Ok(base)
    }

    pub(crate) fn release_call_arg_temps(&mut self) {
        self.call_arg_depth -= 1;
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
            .cloned()
    }

    pub(crate) fn string_id(&self, value: &str) -> Result<i64, String> {
        if value.is_empty() {
            return Ok(0);
        }
        self.strings.get(value).copied().ok_or_else(|| {
            format!("native-lower: string literal not found in constant pool: `{value}`")
        })
    }
}
