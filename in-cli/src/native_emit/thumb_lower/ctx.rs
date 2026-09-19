use crate::core_ir::{Stmt, Typ};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub(crate) struct FunctionInfo {
    pub(crate) name: String,
    pub(crate) params: Vec<(String, Typ)>,
    pub(crate) ret: Typ,
    pub(crate) body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub(crate) enum LocalSlot {
    /// Scalar local: SP offset of the 4-byte slot.
    Scalar(u32),
    /// Struct local: flattened field name → SP offset.
    Struct { fields: HashMap<String, u32> },
    /// Fixed-size array: SP base offset, element size, and element count.
    Array {
        base: u32,
        elem_size: u32,
        len: usize,
    },
}

pub(crate) struct LowerCtx<'a> {
    /// local name → slot descriptor
    pub(crate) locals: HashMap<String, LocalSlot>,
    pub(crate) frame_size: u32,
    pub(crate) emitted_return: bool,
    /// stack of break-site lists, one per enclosing loop
    pub(crate) break_sites: Vec<Vec<u32>>,
    pub(crate) functions: &'a HashMap<String, FunctionInfo>,
    pub(crate) structs: &'a HashMap<String, Vec<(String, Typ)>>,
    pub(crate) fn_name: String,
    pub(crate) ret_typ: Typ,
    /// SP offsets for call-argument temps. Indexed by [depth * chunk + i].
    pub(crate) call_arg_temps: Vec<u32>,
    pub(crate) call_arg_depth: usize,
    pub(crate) call_arg_chunk: usize,
    /// Scratch-slot pairs for binary-op operand evaluation. Indexed by
    /// [depth * 2] and [depth * 2 + 1]: each nested lower_binary acquires its
    /// own pair, so a binary operand of a binary (e.g. `a - b * c`) cannot
    /// clobber the outer operand's stashed value.
    pub(crate) scratch_temps: Vec<u32>,
    pub(crate) scratch_depth: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RelocKind {
    /// R_ARM_THM_CALL on a BL at `site`.
    Call,
    /// R_ARM_THM_MOVW_ABS_NC + R_ARM_THM_MOVT_ABS on the movw/movt pair that
    /// loads a function's address. `site` = movw offset; `site2` = movt offset.
    MovwAbs,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingCall {
    /// offset of the first halfword of the 32-bit BL encoding
    pub(crate) site: u32,
    /// second relocation site (movt for MovwAbs), 0 otherwise
    pub(crate) site2: u32,
    pub(crate) target: String,
    pub(crate) is_extern: bool,
    pub(crate) kind: RelocKind,
}

impl<'a> LowerCtx<'a> {
    pub(crate) fn new(
        fn_name: &str,
        params: &[(String, Typ)],
        functions: &'a HashMap<String, FunctionInfo>,
        structs: &'a HashMap<String, Vec<(String, Typ)>>,
    ) -> Self {
        let mut ctx = Self {
            locals: HashMap::new(),
            frame_size: 0,
            emitted_return: false,
            break_sites: Vec::new(),
            functions,
            structs,
            fn_name: fn_name.to_string(),
            ret_typ: Typ::Int,
            call_arg_temps: Vec::new(),
            call_arg_depth: 0,
            call_arg_chunk: 4,
            scratch_temps: Vec::new(),
            scratch_depth: 0,
        };
        for (name, typ) in params {
            match typ.canonical() {
                Typ::Int | Typ::Bool => {
                    let off = ctx.alloc_slot();
                    ctx.locals.insert(name.clone(), LocalSlot::Scalar(off));
                }
                other => {
                    // validated later
                    let _ = other;
                    let off = ctx.alloc_slot();
                    ctx.locals.insert(name.clone(), LocalSlot::Scalar(off));
                }
            }
        }
        ctx
    }

    pub(crate) fn alloc_slot(&mut self) -> u32 {
        let off = self.frame_size;
        self.frame_size += 4;
        off
    }

    pub(crate) fn alloc_struct(
        &mut self,
        name: &str,
    ) -> Result<(u32, HashMap<String, u32>), String> {
        let base = self.frame_size;
        let layout = build_struct_layout(self.structs, name, base, &mut Vec::new())?;
        self.frame_size += layout.size;
        Ok((base, layout.fields))
    }

    pub(crate) fn acquire_call_arg_temps(&mut self, n: usize) -> Result<usize, String> {
        let base = self.call_arg_depth * self.call_arg_chunk;
        if base + n > self.call_arg_temps.len() {
            return Err(format!(
                "thumb-lower: call arg temp pool exhausted in `{}`",
                self.fn_name
            ));
        }
        self.call_arg_depth += 1;
        Ok(base)
    }

    pub(crate) fn release_call_arg_temps(&mut self) {
        self.call_arg_depth = self.call_arg_depth.saturating_sub(1);
    }

    /// Acquire the scratch-slot pair for one lower_binary evaluation. Nested
    /// binary evaluation acquires the next depth's pair, so operand stash
    /// slots are never shared between an outer and an inner binary.
    pub(crate) fn acquire_scratch_temps(&mut self) -> Result<(u32, u32), String> {
        let base = self.scratch_depth * 2;
        if base + 2 > self.scratch_temps.len() {
            return Err(format!(
                "thumb-lower: binary scratch temp pool exhausted in `{}`",
                self.fn_name
            ));
        }
        let pair = (self.scratch_temps[base], self.scratch_temps[base + 1]);
        self.scratch_depth += 1;
        Ok(pair)
    }

    pub(crate) fn release_scratch_temps(&mut self) {
        self.scratch_depth = self.scratch_depth.saturating_sub(1);
    }

    pub(crate) fn frame_reserve(&self) -> u32 {
        // keep 8-byte alignment for AAPCS
        (self.frame_size + 7) & !7
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StructLayout {
    pub(crate) size: u32,
    pub(crate) align: u32,
    pub(crate) fields: HashMap<String, u32>, // dotted field name -> byte offset
}

pub(crate) fn align_up(off: u32, align: u32) -> u32 {
    (off + align - 1) & !(align - 1)
}

pub(crate) fn scalar_size_align(t: &Typ) -> (u32, u32) {
    match t.canonical() {
        Typ::Int | Typ::Float => (4, 4),
        Typ::Bool => (4, 4),
        _ => (4, 4),
    }
}

pub(crate) fn type_size(t: &Typ) -> Result<u32, String> {
    match t.canonical() {
        Typ::Int | Typ::Bool => Ok(4),
        other => Err(format!("thumb-lower: unsupported element type {other:?}")),
    }
}

/// Flatten a struct (including nested structs) into dotted field names with
/// absolute byte offsets. Also returns the total aligned size.
pub(crate) fn build_struct_layout(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
    base: u32,
    visited: &mut Vec<String>,
) -> Result<StructLayout, String> {
    let fields = structs
        .get(name)
        .ok_or_else(|| format!("thumb-lower: unknown struct `{name}`"))?;
    if visited.contains(&name.to_string()) {
        return Err(format!("thumb-lower: recursive struct `{name}`"));
    }
    visited.push(name.to_string());
    let mut layout = StructLayout {
        size: 0,
        align: 1,
        fields: HashMap::new(),
    };
    for (field, ty) in fields {
        let (size, falign) = match ty.canonical() {
            Typ::Named(inner) if structs.contains_key(&inner) => {
                let inner =
                    build_struct_layout(structs, &inner, base + align_up(layout.size, 4), visited)?;
                for (k, off) in inner.fields {
                    layout.fields.insert(format!("{field}.{k}"), off);
                }
                (inner.size, inner.align)
            }
            _ => scalar_size_align(ty),
        };
        let off = align_up(layout.size, falign);
        if !matches!(ty.canonical(), Typ::Named(inner) if structs.contains_key(&inner)) {
            layout.fields.insert(field.clone(), base + off);
        }
        layout.size = off + size;
        layout.align = layout.align.max(falign);
    }
    layout.size = align_up(layout.size, layout.align);
    visited.pop();
    Ok(layout)
}
