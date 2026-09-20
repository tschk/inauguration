use crate::boundary_emit;
use crate::boundary_ir::{BoundaryModule, IN_ABI_VERSION};
use crate::core_ir::UnifiedModule;
use crate::native_emit::NativeLinkage;
use crate::native_emit::coff::{COFF_WINDOWS_TRIPLE, write_exit_process_exe};
use crate::native_emit::elf::{
    AARCH64_LINUX_TRIPLE, ARMV7_LINUX_GNUEABIHF_TRIPLE, ELF_LINUX_TRIPLE, ElfExecutable, ElfObject,
    THUMBV8M_MAIN_NONE_EABI_TRIPLE, aarch64_linux_exit_code, aarch64_return_i32_object_code,
    arm32_linux_exit_code, arm32_return_i32_object_code, write_aarch64_executable,
    write_aarch64_relocatable_object, write_arm32_executable, write_arm32_relocatable_object,
    write_executable, write_i386_relocatable_object, write_x86_64_relocatable_object,
    x86_64_linux_exit_code, x86_64_return_i32_object_code,
};
use crate::native_emit::macho::{ExportSymbol, MachOImage, MachOLinkage, write_image};
use crate::native_emit::target::AARCH64_NONE_TRIPLE;
use crate::native_emit::thumb_lower;
use crate::native_emit::wasm::{WASM32_UNKNOWN_TRIPLE, WasmModule, write_scalar_i32_module};
use crate::native_emit::x86_64_lower::{X86_64_TRIPLE, lower_module, lower_module_with_bases};

pub const NATIVE_OBJECT_SUBSET: &str = "native-object-subset";

pub struct NativeObjectRequest<'a> {
    pub target_triple: &'a str,
    pub linkage: NativeLinkage,
    pub entry: &'a str,
    pub exit_code: u8,
    pub module: &'a UnifiedModule,
    pub module_id: &'a str,
    pub base: Option<u64>,
}

pub struct NativeObjectArtifact {
    pub bytes: Vec<u8>,
    pub artifact_kind: &'static str,
    pub backend_level: &'static str,
    pub runtime_level: &'static str,
    pub reason_code: &'static str,
    pub reason: &'static str,
    pub abi_manifest: Option<String>,
}

pub fn emit_native_object(request: &NativeObjectRequest<'_>) -> Option<NativeObjectArtifact> {
    match (request.target_triple, request.linkage) {
        (ELF_LINUX_TRIPLE, NativeLinkage::StaticLib) => emit_x86_64_elf_object(request),
        (ELF_LINUX_TRIPLE, NativeLinkage::Executable) => Some(emit_x86_64_elf_executable(request)),
        (COFF_WINDOWS_TRIPLE, NativeLinkage::Executable) => {
            Some(emit_x86_64_pe_executable(request))
        }
        (AARCH64_LINUX_TRIPLE, NativeLinkage::StaticLib) => Some(emit_aarch64_elf_object(request)),
        (AARCH64_LINUX_TRIPLE, NativeLinkage::Executable) => {
            Some(emit_aarch64_elf_executable(request))
        }
        ("aarch64-apple-darwin", NativeLinkage::StaticLib) => {
            Some(emit_aarch64_macho_archive(request))
        }
        (ARMV7_LINUX_GNUEABIHF_TRIPLE, NativeLinkage::StaticLib) => {
            Some(emit_arm32_elf_object(request))
        }
        (ARMV7_LINUX_GNUEABIHF_TRIPLE, NativeLinkage::Executable) => {
            Some(emit_arm32_elf_executable(request))
        }
        (WASM32_UNKNOWN_TRIPLE, NativeLinkage::StaticLib) => Some(emit_wasm32_module(request)),
        ("i386-unknown-none", NativeLinkage::StaticLib)
        | ("i686-unknown-none", NativeLinkage::StaticLib)
        | (X86_64_TRIPLE, NativeLinkage::StaticLib) => {
            Some(emit_x86_64_freestanding_object(request))
        }
        (AARCH64_NONE_TRIPLE, NativeLinkage::StaticLib) => {
            Some(emit_aarch64_freestanding_object(request))
        }
        (THUMBV8M_MAIN_NONE_EABI_TRIPLE, NativeLinkage::StaticLib) => {
            match emit_thumbv8m_freestanding_object(request) {
                Ok(artifact) => Some(artifact),
                Err(err) => {
                    eprintln!("thumb-lower error: {err}");
                    None
                }
            }
        }
        _ => None,
    }
}

fn emit_thumbv8m_freestanding_object(
    request: &NativeObjectRequest<'_>,
) -> Result<NativeObjectArtifact, String> {
    let result = thumb_lower::lower_module(request.module, request.entry)?;
    let object = ElfObject {
        code: result.code,
        export_name: request.entry.to_string(),
        exports: result
            .exports
            .into_iter()
            .filter(|(name, off)| name != request.entry || *off != 0)
            .collect(),
        undefs: result.externs,
        x86_plt_relocs: vec![],
        thumb_calls: result.relocations,
        thumb_addr_refs: result.addr_relocs,
    };
    let mut bytes = Vec::new();
    write_arm32_relocatable_object(&object, &mut bytes);
    Ok(NativeObjectArtifact {
        bytes,
        artifact_kind: "elf32-relocatable-object",
        backend_level: "owned-native-subset-freestanding-thumb",
        runtime_level: "freestanding-none",
        reason_code: "native-thumbv8m-main-none-eabi-freestanding",
        reason: "inauguration owns ELF32 Thumb-2 freestanding object emission with real Core IR lowering for the owned scalar subset on thumbv8m.main-none-eabi",
        abi_manifest: Some(object_abi_manifest(request)),
    })
}

fn emit_x86_64_pe_executable(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let mut bytes = Vec::new();
    write_exit_process_exe(request.exit_code, &mut bytes);
    NativeObjectArtifact {
        bytes,
        artifact_kind: "pe-executable",
        backend_level: "owned-native-subset-x86_64",
        runtime_level: "windows-exitprocess",
        reason_code: "native-x86_64-windows-exe-subset",
        reason: "inauguration owns PE32+ executable emission with kernel32 ExitProcess import for const-evaluable scalar entry functions on this target",
        abi_manifest: None,
    }
}

fn emit_x86_64_elf_object(request: &NativeObjectRequest<'_>) -> Option<NativeObjectArtifact> {
    // Real x86_64 lowering (hosted ABI: instring literals are rejected below
    // together with globals). External calls stay `call rel32 0` and are
    // recorded as R_X86_64_PLT32 relocations so `ld` binds them at link
    // time; every lowered function is exported.
    crate::native_emit::x86_64::set_32bit(false);
    let result = match lower_module(request.module, request.entry) {
        Ok(result) => result,
        Err(_err) => return None,
    };
    if !result.relocations.is_empty() {
        // Any absolute-address site (string/global/function references)
        // would embed a baked load address that hosted `ld` cannot fix up
        // (no R_X86_64_64 support in the writer yet). Refuse rather than
        // emit code that only works at one load address. Code without such
        // sites is position-independent: rel32 calls and stack access.
        return None;
    }
    let mut bytes = Vec::new();
    let object = ElfObject {
        code: result.code.clone(),
        export_name: request.entry.to_string(),
        // Export the entry globally only. Sibling bodies stay in .text but
        // out of the symbol table so a hosted link against `main` or other
        // host symbols cannot collide.
        exports: vec![],
        undefs: result.externs.clone(),
        x86_plt_relocs: result
            .extern_calls
            .iter()
            .map(|(symbol, site)| (site + 1, symbol.clone()))
            .collect(),
        thumb_calls: vec![],
        thumb_addr_refs: vec![],
    };
    write_x86_64_relocatable_object(&object, &mut bytes);
    Some(NativeObjectArtifact {
        bytes,
        artifact_kind: "elf-relocatable-object",
        backend_level: "owned-object-subset",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration lowers x86_64 ELF relocatable objects directly (real function bodies; extern calls carry R_X86_64_PLT32 relocations)",
        abi_manifest: Some(object_abi_manifest(request)),
    })
}

fn emit_aarch64_elf_object(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let object = ElfObject {
        code: aarch64_return_i32_object_code(request.exit_code),
        export_name: request.entry.to_string(),
        exports: vec![],
        undefs: vec![],
        thumb_calls: vec![],
        thumb_addr_refs: vec![],
        x86_plt_relocs: vec![],
    };
    let mut bytes = Vec::new();
    write_aarch64_relocatable_object(&object, &mut bytes);
    NativeObjectArtifact {
        bytes,
        artifact_kind: "elf-relocatable-object",
        backend_level: "owned-object-subset",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration owns ELF64 AArch64 relocatable object emission for const-evaluable scalar entry functions on this target",
        abi_manifest: Some(object_abi_manifest(request)),
    }
}

fn emit_aarch64_elf_executable(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let exe = ElfExecutable {
        code: aarch64_linux_exit_code(request.exit_code),
        entry_offset: 0,
    };
    let mut bytes = Vec::new();
    write_aarch64_executable(&exe, &mut bytes);
    NativeObjectArtifact {
        bytes,
        artifact_kind: "elf-executable",
        backend_level: "owned-native-subset-aarch64",
        runtime_level: "linux-syscall-exit",
        reason_code: "native-aarch64-linux-exit-subset",
        reason: "inauguration owns ELF64 AArch64 Linux executable emission for const-evaluable scalar entry functions on this target",
        abi_manifest: None,
    }
}

fn emit_arm32_elf_object(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let object = ElfObject {
        code: arm32_return_i32_object_code(request.exit_code),
        export_name: request.entry.to_string(),
        exports: vec![],
        undefs: vec![],
        thumb_calls: vec![],
        thumb_addr_refs: vec![],
        x86_plt_relocs: vec![],
    };
    let mut bytes = Vec::new();
    write_arm32_relocatable_object(&object, &mut bytes);
    NativeObjectArtifact {
        bytes,
        artifact_kind: "elf32-relocatable-object",
        backend_level: "owned-object-subset",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration owns ELF32 ARM relocatable object emission for const-evaluable scalar entry functions on this target",
        abi_manifest: Some(object_abi_manifest(request)),
    }
}

fn emit_arm32_elf_executable(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let exe = ElfExecutable {
        code: arm32_linux_exit_code(request.exit_code),
        entry_offset: 0,
    };
    let mut bytes = Vec::new();
    write_arm32_executable(&exe, &mut bytes);
    NativeObjectArtifact {
        bytes,
        artifact_kind: "elf32-executable",
        backend_level: "owned-native-subset-arm32",
        runtime_level: "linux-syscall-exit",
        reason_code: "native-armv7-linux-exit-subset",
        reason: "inauguration owns ELF32 ARM Linux executable emission for const-evaluable scalar entry functions on this target",
        abi_manifest: None,
    }
}

fn emit_aarch64_macho_archive(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let image = MachOImage {
        code: aarch64_return_i32_object_code(request.exit_code),
        entry_offset: None,
        exports: vec![ExportSymbol {
            name: request.entry.to_string(),
            offset: 0,
        }],
        external_refs: Vec::new(),
    };
    let mut bytes = Vec::new();
    write_image(&image, MachOLinkage::StaticLib, request.entry, &mut bytes);
    NativeObjectArtifact {
        bytes,
        artifact_kind: "mach-o-static-archive",
        backend_level: "owned-object-subset",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration owns Mach-O static archive emission for const-evaluable scalar entry functions on this target",
        abi_manifest: Some(object_abi_manifest(request)),
    }
}

fn emit_x86_64_elf_executable(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let exe = ElfExecutable {
        code: x86_64_linux_exit_code(request.exit_code),
        entry_offset: 0,
    };
    let mut bytes = Vec::new();
    write_executable(&exe, &mut bytes);
    NativeObjectArtifact {
        bytes,
        artifact_kind: "elf-executable",
        backend_level: "owned-native-subset-x86_64",
        runtime_level: "linux-syscall-exit",
        reason_code: "native-x86_64-linux-exit-subset",
        reason: "inauguration owns ELF64 Linux executable emission for const-evaluable scalar entry functions on this target",
        abi_manifest: None,
    }
}

fn emit_aarch64_freestanding_object(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    // Use real AArch64 lowering for freestanding targets
    match crate::native_emit::lower::lower_module(
        request.module,
        request.entry,
        NativeLinkage::StaticLib,
    ) {
        Ok(result) => {
            let mut bytes = Vec::new();
            let object = ElfObject {
                code: result.code,
                export_name: request.entry.to_string(),
                exports: vec![],
                undefs: vec![],
                x86_plt_relocs: vec![],
                thumb_calls: vec![],
                thumb_addr_refs: vec![],
            };
            write_aarch64_relocatable_object(&object, &mut bytes);
            NativeObjectArtifact {
                bytes,
                artifact_kind: "elf-relocatable-object",
                backend_level: "owned-native-subset-freestanding",
                runtime_level: "freestanding-none",
                reason_code: "native-aarch64-unknown-none-freestanding",
                reason: "inauguration owns ELF64 relocatable object emission for real AArch64 freestanding function bodies",
                abi_manifest: Some(object_abi_manifest(request)),
            }
        }
        Err(_err) => {
            // Fallback to const-evaluated exit code for trivial functions
            let object = ElfObject {
                code: aarch64_return_i32_object_code(request.exit_code),
                export_name: request.entry.to_string(),
                exports: vec![],
                undefs: vec![],
                x86_plt_relocs: vec![],
                thumb_calls: vec![],
                thumb_addr_refs: vec![],
            };
            let mut bytes = Vec::new();
            write_aarch64_relocatable_object(&object, &mut bytes);
            NativeObjectArtifact {
                bytes,
                artifact_kind: "elf-relocatable-object",
                backend_level: "owned-object-subset",
                runtime_level: "freestanding-none",
                reason_code: "native-object-subset",
                reason: "inauguration owns ELF64 AArch64 relocatable object emission for const-evaluable scalar entry functions on freestanding target",
                abi_manifest: Some(object_abi_manifest(request)),
            }
        }
    }
}

fn emit_x86_64_freestanding_object(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let is_i386 =
        request.target_triple.starts_with("i386-") || request.target_triple.starts_with("i686-");
    crate::native_emit::x86_64::set_32bit(is_i386);
    let align_up = |addr: u64, alignment: u64| -> u64 { (addr + alignment - 1) & !(alignment - 1) };
    // Use real x86_64 lowering for freestanding targets. If a base address was
    // supplied, lower the module so globals/strings are emitted at the correct
    // load address (enabling external ld.lld + linker-script workflows).
    let lower_result = match request.base {
        Some(code_base) => {
            match lower_module_with_bases(request.module, request.entry, code_base, code_base) {
                Ok(temp) => {
                    let code_size = temp.code.len();
                    let data_base = align_up(code_base + code_size as u64, 8);
                    lower_module_with_bases(request.module, request.entry, code_base, data_base)
                }
                Err(e) => Err(e),
            }
        }
        None => lower_module(request.module, request.entry),
    };
    match lower_result {
        Ok(result) => {
            let mut bytes = Vec::new();
            let object = ElfObject {
                code: result.code.clone(),
                export_name: request.entry.to_string(),
                exports: result.exports.clone(),
                undefs: result.externs.clone(),
                x86_plt_relocs: result
                    .extern_calls
                    .iter()
                    .map(|(symbol, site)| (site + 1, symbol.clone()))
                    .collect(),
                thumb_calls: vec![],
                thumb_addr_refs: vec![],
            };
            if is_i386 {
                write_i386_relocatable_object(&object, &mut bytes);
            } else {
                write_x86_64_relocatable_object(&object, &mut bytes);
            }
            NativeObjectArtifact {
                bytes,
                artifact_kind: "elf-relocatable-object",
                backend_level: "owned-native-subset-freestanding",
                runtime_level: "freestanding-none",
                reason_code: if is_i386 {
                    "native-i386-unknown-none-freestanding"
                } else {
                    "native-x86_64-unknown-none-freestanding"
                },
                reason: if is_i386 {
                    "inauguration owns ELF32 i386 relocatable object emission for real i386 freestanding function bodies"
                } else {
                    "inauguration owns ELF64 relocatable object emission for real x86_64 freestanding function bodies"
                },
                abi_manifest: Some(object_abi_manifest(request)),
            }
        }
        Err(_err) => {
            // Fallback to const-evaluated exit code for trivial functions
            let object = ElfObject {
                code: x86_64_return_i32_object_code(request.exit_code),
                export_name: request.entry.to_string(),
                exports: vec![],
                undefs: vec![],
                x86_plt_relocs: vec![],
                thumb_calls: vec![],
                thumb_addr_refs: vec![],
            };
            let mut bytes = Vec::new();
            if is_i386 {
                write_i386_relocatable_object(&object, &mut bytes);
            } else {
                write_x86_64_relocatable_object(&object, &mut bytes);
            }
            NativeObjectArtifact {
                bytes,
                artifact_kind: "elf-relocatable-object",
                backend_level: "owned-object-subset",
                runtime_level: "freestanding-none",
                reason_code: "native-object-subset",
                reason: if is_i386 {
                    "inauguration owns ELF32 i386 relocatable object emission for const-evaluable scalar entry functions on freestanding target"
                } else {
                    "inauguration owns ELF64 relocatable object emission for const-evaluable scalar entry functions on freestanding target"
                },
                abi_manifest: Some(object_abi_manifest(request)),
            }
        }
    }
}

fn emit_wasm32_module(request: &NativeObjectRequest<'_>) -> NativeObjectArtifact {
    let module = WasmModule {
        export_name: request.entry.to_string(),
        value: request.exit_code,
    };
    let mut bytes = Vec::new();
    write_scalar_i32_module(&module, &mut bytes);
    NativeObjectArtifact {
        bytes,
        artifact_kind: "wasm-module",
        backend_level: "owned-object-subset",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration owns WebAssembly module emission for const-evaluable scalar entry functions on this target",
        abi_manifest: None,
    }
}

fn object_abi_manifest(request: &NativeObjectRequest<'_>) -> String {
    boundary_emit::emit_abi_manifest_with_package(
        &BoundaryModule {
            abi_version: IN_ABI_VERSION,
            module: request
                .module
                .effective_module_id(request.module_id)
                .to_string(),
            layouts: Vec::new(),
            symbols: Vec::new(),
            allocators: Vec::new(),
            layout_hash: String::new(),
        }
        .with_layout_hash(),
        request.module.identity.package.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{CoreModuleIdentity, Decl, Expr, Stmt, Typ, UnifiedModule};

    fn scalar_answer_module() -> UnifiedModule {
        UnifiedModule::with_identity(
            vec![Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
                type_params: vec![],
            }],
            CoreModuleIdentity::default(),
        )
    }

    #[test]
    fn dispatches_x86_64_staticlib_object() {
        let module = scalar_answer_module();
        let request = NativeObjectRequest {
            target_triple: ELF_LINUX_TRIPLE,
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("object artifact");
        assert_eq!(artifact.artifact_kind, "elf-relocatable-object");
        assert_eq!(artifact.reason_code, NATIVE_OBJECT_SUBSET);
        assert!(artifact.bytes.windows(6).any(|window| window == b"answer"));
        // Real lowering: `mov rax, 42` inside a real prologue must appear.
        assert!(
            artifact
                .bytes
                .windows(8)
                .any(|w| w == [0x48, 0xB8, 42, 0, 0, 0, 0, 0])
        );
        assert!(artifact.abi_manifest.is_some());
    }

    #[test]
    fn x86_64_staticlib_object_rejects_absolute_globals() {
        let module = UnifiedModule::with_identity(
            vec![Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let("s".into(), None, Expr::StringLit("hi".into())),
                    Stmt::Return(Some(Expr::IntLit(42))),
                ],
                type_params: vec![],
            }],
            CoreModuleIdentity::default(),
        );
        let request = NativeObjectRequest {
            target_triple: ELF_LINUX_TRIPLE,
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        // Hosted objects cannot carry absolute string/global references yet;
        // the backend must refuse rather than emit baked addresses.
        assert!(emit_native_object(&request).is_none());
    }

    #[test]
    fn dispatches_wasm32_staticlib_module() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: WASM32_UNKNOWN_TRIPLE,
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("wasm artifact");
        assert_eq!(artifact.artifact_kind, "wasm-module");
        assert_eq!(artifact.reason_code, NATIVE_OBJECT_SUBSET);
        assert!(artifact.bytes.windows(6).any(|window| window == b"answer"));
        assert!(artifact.abi_manifest.is_none());
    }

    #[test]
    fn ignores_unsupported_target() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: "riscv64gc-unknown-none-elf",
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        assert!(emit_native_object(&request).is_none());
    }

    #[test]
    fn dispatches_aarch64_linux_staticlib_object() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: "aarch64-unknown-linux-gnu",
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("aarch64 object artifact");
        assert_eq!(artifact.artifact_kind, "elf-relocatable-object");
        assert_eq!(artifact.reason_code, NATIVE_OBJECT_SUBSET);
        assert_eq!(
            u16::from_le_bytes([artifact.bytes[18], artifact.bytes[19]]),
            183
        );
    }

    #[test]
    fn dispatches_aarch64_apple_darwin_staticlib_archive() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: "aarch64-apple-darwin",
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("macho archive artifact");
        assert_eq!(artifact.artifact_kind, "mach-o-static-archive");
        assert_eq!(artifact.reason_code, NATIVE_OBJECT_SUBSET);
        assert_eq!(&artifact.bytes[..8], b"!<arch>\n");
        assert!(artifact.bytes.windows(7).any(|window| window == b"_answer"));
    }

    #[test]
    fn dispatches_arm32_staticlib_object() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: "armv7-unknown-linux-gnueabihf",
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("arm32 object artifact");
        assert_eq!(artifact.artifact_kind, "elf32-relocatable-object");
        assert_eq!(artifact.reason_code, NATIVE_OBJECT_SUBSET);
        assert_eq!(artifact.bytes[4], 1);
        assert_eq!(
            u16::from_le_bytes([artifact.bytes[18], artifact.bytes[19]]),
            40
        );
    }

    #[test]
    fn dispatches_aarch64_freestanding_object() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: AARCH64_NONE_TRIPLE,
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("aarch64 freestanding artifact");
        assert_eq!(artifact.artifact_kind, "elf-relocatable-object");
        // Empty module falls back to const-evaluated exit code path
        assert_eq!(artifact.reason_code, NATIVE_OBJECT_SUBSET);
        assert_eq!(
            u16::from_le_bytes([artifact.bytes[18], artifact.bytes[19]]),
            183
        );
    }

    #[test]
    fn dispatches_x86_64_linux_executable() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: ELF_LINUX_TRIPLE,
            linkage: NativeLinkage::Executable,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("x86 executable artifact");
        assert_eq!(artifact.artifact_kind, "elf-executable");
        assert_eq!(artifact.reason_code, "native-x86_64-linux-exit-subset");
        assert_eq!(
            u16::from_le_bytes([artifact.bytes[16], artifact.bytes[17]]),
            2
        );
    }

    #[test]
    fn dispatches_aarch64_linux_executable() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: AARCH64_LINUX_TRIPLE,
            linkage: NativeLinkage::Executable,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("aarch64 executable artifact");
        assert_eq!(artifact.artifact_kind, "elf-executable");
        assert_eq!(artifact.reason_code, "native-aarch64-linux-exit-subset");
        assert_eq!(
            u16::from_le_bytes([artifact.bytes[18], artifact.bytes[19]]),
            183
        );
    }

    #[test]
    fn dispatches_arm32_linux_executable() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: ARMV7_LINUX_GNUEABIHF_TRIPLE,
            linkage: NativeLinkage::Executable,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("arm32 executable artifact");
        assert_eq!(artifact.artifact_kind, "elf32-executable");
        assert_eq!(artifact.reason_code, "native-armv7-linux-exit-subset");
        assert_eq!(artifact.bytes[4], 1);
        assert_eq!(
            u16::from_le_bytes([artifact.bytes[18], artifact.bytes[19]]),
            40
        );
    }

    #[test]
    fn dispatches_thumbv8m_freestanding_object() {
        let module = crate::in_lang_parse::parse_in_source(
            r#"
fn answer() -> Int { return 42 }
fn main() -> void { return }
"#,
        )
        .expect("parse");
        let request = NativeObjectRequest {
            target_triple: THUMBV8M_MAIN_NONE_EABI_TRIPLE,
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("thumbv8m freestanding artifact");
        assert_eq!(artifact.artifact_kind, "elf32-relocatable-object");
        assert_eq!(
            artifact.reason_code,
            "native-thumbv8m-main-none-eabi-freestanding"
        );
        assert_eq!(
            artifact.backend_level,
            "owned-native-subset-freestanding-thumb"
        );
        assert_eq!(artifact.bytes[4], 1);
        assert_eq!(
            u16::from_le_bytes([artifact.bytes[18], artifact.bytes[19]]),
            40
        );
        assert!(artifact.bytes.windows(6).any(|w| w == b"answer"));
        // Real lowering must emit movs r0,#42 (0x202A) somewhere in .text
        assert!(artifact.bytes.windows(2).any(|w| w == [0x2A, 0x20]));
    }

    #[test]
    fn thumbv8m_empty_module_fails_closed() {
        let module = UnifiedModule::new(Vec::new());
        let request = NativeObjectRequest {
            target_triple: THUMBV8M_MAIN_NONE_EABI_TRIPLE,
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 42,
            module: &module,
            module_id: "App",
            base: None,
        };
        assert!(emit_native_object(&request).is_none());
    }
}
