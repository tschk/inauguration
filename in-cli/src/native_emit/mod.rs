//! Owned native code generation for Apple ARM64 (Mach-O executable subset).

pub mod aarch64;
pub mod antidecomp;
pub mod c_backend;
pub mod coff;
pub mod elf;
pub mod linker_layout;
pub mod lower;
mod macho;
pub mod native_link;
pub mod object;
pub mod qemu_harness;
pub mod raw;
pub mod sci;
pub mod source_emit;
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
    NativeLinkage, TARGET_TRIPLE, compile_native_artifact, compile_native_artifact_for_host,
    compile_native_executable, compile_native_executable_for_host, host_supports_native_subset,
};
pub use macho::{ExportSymbol, MachOLinkage, write_relocatable_object};
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
