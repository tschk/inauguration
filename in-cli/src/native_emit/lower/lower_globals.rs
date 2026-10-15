use super::PendingGlobalLiteral;
use crate::core_ir::{Decl, Expr, Typ, UnifiedModule};
use crate::native_emit::aarch64::{self, CodeEmitter};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy)]
pub(crate) struct GlobalField {
    pub(crate) offset: u64,
    pub(crate) mutable: bool,
}

#[derive(Default)]
pub(crate) struct GlobalLayout {
    pub(crate) names: HashSet<String>,
    pub(crate) fields: HashMap<String, GlobalField>,
    pub(crate) structs: HashMap<String, ()>,
    pub(crate) data: Vec<u8>,
}

impl GlobalLayout {
    pub(crate) fn collect(
        module: &UnifiedModule,
        schemas: &HashMap<String, Vec<(String, Typ)>>,
    ) -> Result<Self, String> {
        let mut layout = Self::default();
        for decl in &module.decls {
            let Decl::Global { name, .. } = decl else {
                continue;
            };
            layout.names.insert(name.clone());
            let Decl::Global {
                name,
                typ: Typ::Named(struct_name),
                init,
                mutable,
            } = decl
            else {
                continue;
            };
            let Some(fields) = schemas.get(struct_name) else {
                continue;
            };
            layout.structs.insert(name.clone(), ());
            // This first AArch64 JIT path covers zero-initialized structs of
            // integer fields. Other shapes fail when a field is accessed.
            if !matches!(init.as_deref(), None | Some(Expr::IntLit(0)))
                || !fields
                    .iter()
                    .all(|(_, typ)| matches!(typ.canonical(), Typ::Int))
            {
                continue;
            }
            let base = layout.data.len() as u64;
            let additional = fields
                .len()
                .max(1)
                .checked_mul(8)
                .ok_or_else(|| "native-lower: global storage size overflow".to_string())?;
            let new_size = layout
                .data
                .len()
                .checked_add(additional)
                .ok_or_else(|| "native-lower: global storage size overflow".to_string())?;
            for (index, (field, _)) in fields.iter().enumerate() {
                layout.fields.insert(
                    format!("{name}.{field}"),
                    GlobalField {
                        offset: base + index as u64 * 8,
                        mutable: *mutable,
                    },
                );
            }
            layout.data.resize(new_size, 0);
        }
        Ok(layout)
    }
}

/// Append relocatable pointer literals after the function's final instruction.
/// The existing JIT loader writes each mapped data address into its 8-byte slot.
pub(crate) fn append_global_literals(
    emitter: &mut CodeEmitter,
    pending: Vec<PendingGlobalLiteral>,
) -> Result<Vec<(u32, u64)>, String> {
    let mut relocs = Vec::with_capacity(pending.len());
    if !pending.is_empty() {
        // A branch-heavy function can have a fallthrough even if another path
        // returned. Never execute pointer literal bytes as instructions.
        emitter.emit_u32(aarch64::brk(0));
    }
    for reference in pending {
        while !emitter.len().is_multiple_of(8) {
            emitter.bytes.push(0);
        }
        let literal = emitter.len();
        let delta = literal as i64 - reference.adr_site as i64;
        if !(-(1 << 20)..(1 << 20)).contains(&delta) {
            return Err("native-lower: global pointer literal exceeds ADR range".into());
        }
        emitter.patch_u32(
            reference.adr_site,
            aarch64::adr(reference.reg, delta as i32),
        );
        emitter.bytes.extend_from_slice(&0u64.to_le_bytes());
        relocs.push((literal, reference.data_offset));
    }
    Ok(relocs)
}
