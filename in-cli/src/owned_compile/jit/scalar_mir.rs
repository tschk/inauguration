//! Opportunistic scalar MIR admission. After admission, failures are errors.
use crate::compiler::{mir_emit, mir_emit_x86, mir_lower, mir_scalar_lower};
use crate::core_ir::{Decl, UnifiedModule};
use crate::native_emit::lower::LoweredModule;

pub(super) fn try_lower(
    module: &UnifiedModule,
    entry: &str,
) -> Result<Option<LoweredModule>, String> {
    if !module.decls.iter().any(|decl| {
        matches!(decl,
        Decl::Function { name, params, body, .. }
            if name == entry && params.is_empty() && !body.is_empty())
    }) {
        return Ok(None);
    }
    // These declaration shapes are unconditionally outside the producer's
    // subset. Skip its full Core verification without copying its type rules.
    if !module.decls.iter().all(|decl| {
        matches!(decl,
        Decl::Function { type_params, .. } if type_params.is_empty())
    }) {
        return Ok(None);
    }
    let target = if cfg!(target_arch = "aarch64") {
        mir_scalar_lower::ScalarTarget::Aarch64
    } else if cfg!(target_arch = "x86_64") {
        mir_scalar_lower::ScalarTarget::X86_64
    } else {
        return Ok(None);
    };
    let Ok(typed) = mir_lower::lower_to_mir(module) else {
        return Ok(None);
    };
    let Ok(machine) = mir_scalar_lower::lower_scalar_typed(&typed, target) else {
        return Ok(None);
    };
    // Machine admission is the fallback boundary: emission failures must remain
    // observable, rather than rerunning the source through another backend.
    let (code, exports) = match target {
        mir_scalar_lower::ScalarTarget::Aarch64 => mir_emit::emit_jit(&machine),
        mir_scalar_lower::ScalarTarget::X86_64 => mir_emit_x86::emit_boot(&machine),
    }
    .map_err(|error| format!("scalar MIR emission failed: {error}"))?;
    let entry_offset = exports
        .iter()
        .find(|(name, _, _)| name == entry)
        .map(|(_, offset, _)| *offset)
        .ok_or_else(|| format!("scalar MIR entry missing: {entry}"))?;
    for (name, offset, size) in &exports {
        if *size == 0
            || (*offset as usize)
                .checked_add(*size as usize)
                .is_none_or(|end| end > code.len())
        {
            return Err(format!("scalar MIR invalid function extent: {name}"));
        }
    }
    Ok(Some(LoweredModule {
        code,
        entry_offset: Some(entry_offset),
        exports: Vec::new(),
        function_offsets: exports
            .into_iter()
            .map(|(name, offset, _)| (name, offset))
            .collect(),
        relocations: Vec::new(),
        data: Vec::new(),
        data_relocations: Vec::new(),
        global_relocs: Vec::new(),
        external_refs: Vec::new(),
        degradations: Vec::new(),
        error_slot_refs: Vec::new(),
    }))
}
