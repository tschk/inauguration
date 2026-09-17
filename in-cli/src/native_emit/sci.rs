//! Generic SCI (Simple Component Image) binary emitter for freestanding targets.
//!
//! Produces a raw, position-dependent binary with a 32-byte manifest at the start:
//!   [magic(8), required_caps(8), entry(8), image_size(8)]
//!
//! The body is x86_64 machine code lowered for the load address passed via
//! `--base`. Global variables and string literals are relocated into a data
//! section placed immediately after the code, so the emitted binary can be
//! loaded directly without an external linker.

use crate::core_ir::{Decl, UnifiedModule};
use crate::native_emit::x86_64_lower::{X86_64CompileResult, lower_module_with_bases};

pub const SCI_MAGIC: u64 = 0x5343490000000001;
/// SCI container whose payload is the private INISA (harden profile).
pub const SCI_INISA_MAGIC: u64 = 0x5343490000000049; // ...'I'
pub const SCI_MANIFEST_SIZE: usize = 32;

/// Emit a raw SCI binary for `module` with entry point `entry` loaded at `base`.
pub fn emit_sci_binary(module: &UnifiedModule, entry: &str, base: u64) -> Result<Vec<u8>, String> {
    let code_base = base;
    // First pass: determine code size so the data section can be placed after it.
    let temp = lower_module_with_bases(module, entry, code_base, code_base)?;
    let code_size = temp.code.len();
    let data_base = align_up(code_base + code_size as u64, 8);
    // Second pass: patch globals using the real data section base.
    let result = lower_module_with_bases(module, entry, code_base, data_base)?;
    let pad = (data_base - (code_base + result.code.len() as u64)) as usize;
    build_image(&result, base, required_capabilities_mask(module), pad)
}

fn build_image(
    result: &X86_64CompileResult,
    base: u64,
    required_caps: u64,
    data_pad: usize,
) -> Result<Vec<u8>, String> {
    let image_size = SCI_MANIFEST_SIZE + result.code.len() + data_pad + result.data.len();
    let mut image = Vec::with_capacity(image_size);
    image.extend_from_slice(&SCI_MAGIC.to_le_bytes());
    image.extend_from_slice(&required_caps.to_le_bytes());
    image.extend_from_slice(&base.to_le_bytes());
    image.extend_from_slice(&(image_size as u64).to_le_bytes());
    image.extend_from_slice(&result.code);
    image.resize(image.len() + data_pad, 0);
    image.extend_from_slice(&result.data);
    Ok(image)
}

fn required_capabilities_mask(module: &UnifiedModule) -> u64 {
    let mut mask = 0u64;
    let mut bit = 0u64;
    for decl in &module.decls {
        if let Decl::Component { capabilities, .. } = decl {
            for _cap in capabilities {
                if bit < 64 {
                    mask |= 1u64 << bit;
                    bit += 1;
                }
            }
        }
    }
    mask
}

fn align_up(addr: u64, alignment: u64) -> u64 {
    (addr + alignment - 1) & !(alignment - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{CoreModuleIdentity, Decl, Expr, Stmt, Typ};

    fn simple_module() -> UnifiedModule {
        let decls = vec![Decl::Function {
            name: "answer".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            type_params: vec![],
        }];
        UnifiedModule::with_identity(decls, CoreModuleIdentity::default())
    }

    #[test]
    fn sci_manifest_layout() {
        let module = simple_module();
        let binary = emit_sci_binary(&module, "answer", 0x40000020).expect("emit sci");
        let image_size = u64::from_le_bytes(binary[24..32].try_into().unwrap());
        assert_eq!(binary.len(), image_size as usize);
        assert_eq!(
            u64::from_le_bytes(binary[0..8].try_into().unwrap()),
            SCI_MAGIC
        );
        assert_eq!(
            u64::from_le_bytes(binary[16..24].try_into().unwrap()),
            0x40000020
        );
    }
}
