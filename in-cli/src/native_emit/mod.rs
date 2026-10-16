//! Owned native code generation for Apple ARM64 (Mach-O executable subset).

pub mod aarch64;
pub mod antidecomp;
pub mod c_backend;
pub mod coff;
pub mod elf;
pub(crate) mod global_data;
pub mod inisa;
pub mod inisa_bundle;
pub mod linker_layout;
pub mod lower;
mod macho;
pub mod native_link;
pub mod object;
pub mod qemu_harness;
pub mod raw;
pub mod sci;
pub mod source_emit;
mod stdlib_wrappers;
pub mod target;
pub mod thumb;
pub mod thumb_lower;
pub mod uf2;
pub mod wasm;
pub mod x86_64;
pub mod x86_64_lower;

pub use coff::{COFF_WINDOWS_TRIPLE, CoffDll, CoffExe, write_dll, write_exe};
pub use elf::{
    ELF_LINUX_TRIPLE, ElfExecutable, ElfObject, THUMBV8M_MAIN_NONE_EABI_TRIPLE,
    write_executable as write_elf_executable, write_x86_64_relocatable_object,
    x86_64_linux_exit_code, x86_64_return_i32_object_code,
};
pub use linker_layout::{LinkerLayout, MemoryRegion};
pub use lower::{
    NativeArtifactOutcome, NativeLinkage, TARGET_TRIPLE, compile_native_artifact,
    compile_native_artifact_for_host, compile_native_artifact_for_host_with_report,
    compile_native_artifact_with_report, compile_native_executable,
    compile_native_executable_for_host, host_supports_native_subset,
};
pub use macho::{ExportSymbol, MachOLinkage, write_relocatable_object};
pub(crate) use object::try_emit_native_object;
pub use object::{
    NATIVE_OBJECT_SUBSET as NATIVE_OBJECT_REASON, NativeObjectArtifact, NativeObjectRequest,
    emit_native_object,
};
pub use qemu_harness::QemuRunSpec;
pub use raw::write_raw_binary;
pub use target::{
    NATIVE_EMIT_CONTRACT, NATIVE_EMIT_IMPLEMENTED, NATIVE_EMIT_SKELETON, NativeEmitTargetStatus,
    NativeTarget, NativeTargetKind, all_native_emit_targets, elf_linux_target_status,
    freestanding_supported, macho_target_status, resolve_native_target,
};
pub use thumb_lower::{THUMB_TRIPLE, ThumbCompileResult, lower_module as lower_thumb_module};
pub use uf2::{UF2_FAMILY_RP2350_ARM_S, Uf2Options, encode_uf2, write_uf2};
pub use wasm::{WASM32_UNKNOWN_TRIPLE, WasmModule, write_scalar_i32_module};

/// Section-relative targets retained for relocatable native objects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeSection {
    Code,
    Data,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsoluteReference {
    pub site: u32,
    pub width: u8,
    pub target_section: NativeSection,
    pub target_offset: u64,
}

pub(crate) mod struct_layout;

/// Scalar Float parameters/results need a 64-bit owned carrier. This checks an
/// emitted definition or selected call, never an unused declaration/address.
pub(crate) fn validate_word32_scalar_signature(
    params: &[(String, crate::core_ir::Typ)],
    ret: &crate::core_ir::Typ,
) -> Result<(), String> {
    use crate::core_ir::Typ;
    if ret.canonical() == Typ::Float || params.iter().any(|(_, typ)| typ.canonical() == Typ::Float)
    {
        return Err("Float requires 64-bit mode (scalar function ABI)".into());
    }
    Ok(())
}

#[cfg(test)]
mod word32_signature_tests {
    use super::validate_word32_scalar_signature;
    use crate::core_ir::Typ;

    #[test]
    fn rejects_only_canonical_scalar_float_abi() {
        assert!(validate_word32_scalar_signature(&[], &Typ::Float).is_err());
        assert!(validate_word32_scalar_signature(&[], &Typ::Named("f64".into())).is_err());
        assert!(
            validate_word32_scalar_signature(
                &[("items".into(), Typ::Array(Box::new(Typ::Float)))],
                &Typ::Int
            )
            .is_ok()
        );
        assert!(validate_word32_scalar_signature(&[("x".into(), Typ::Float)], &Typ::Int).is_err());
        for typ in [
            Typ::Int,
            Typ::Bool,
            Typ::Void,
            Typ::Named("Row".into()),
            Typ::Array(Box::new(Typ::Float)),
            Typ::Vector(Box::new(Typ::Float)),
        ] {
            assert!(validate_word32_scalar_signature(&[], &typ).is_ok());
        }
    }
}
