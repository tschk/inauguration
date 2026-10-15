use crate::boundary_emit;
use crate::boundary_ir::{BoundaryModule, IN_ABI_VERSION};
use crate::core_ir::UnifiedModule;
use crate::native_emit::NativeLinkage;
use crate::native_emit::coff::{COFF_WINDOWS_TRIPLE, write_exit_process_exe};
use crate::native_emit::elf::{
    AARCH64_LINUX_TRIPLE, ARMV7_LINUX_GNUEABIHF_TRIPLE, ELF_LINUX_TRIPLE, ElfExecutable, ElfObject,
    THUMBV8M_MAIN_NONE_EABI_TRIPLE, X86RelocatableObject, aarch64_linux_exit_code,
    arm32_linux_exit_code, arm32_return_i32_object_code, write_aarch64_executable,
    write_aarch64_relocatable_object, write_arm32_executable, write_arm32_relocatable_object,
    write_executable, write_thumb_relocatable_object, write_x86_data_object,
    x86_64_linux_exit_code,
};
use crate::native_emit::macho::{MachOImage, MachOLinkage, write_image};
use crate::native_emit::target::AARCH64_NONE_TRIPLE;
use crate::native_emit::thumb_lower;
use crate::native_emit::wasm::{WASM32_UNKNOWN_TRIPLE, WasmModule, write_scalar_i32_module};
use crate::native_emit::x86_64_lower::{
    X86_64_TRIPLE, lower_module_with_bases, lower_native_module,
};

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
    struct WidthGuard(bool);
    impl Drop for WidthGuard {
        fn drop(&mut self) {
            crate::native_emit::x86_64::set_32bit(self.0);
        }
    }
    let _width = WidthGuard(crate::native_emit::x86_64::is_32bit());
    match (request.target_triple, request.linkage) {
        (ELF_LINUX_TRIPLE, NativeLinkage::StaticLib) => emit_x86_64_elf_object(request),
        (ELF_LINUX_TRIPLE, NativeLinkage::Executable) => Some(emit_x86_64_elf_executable(request)),
        (COFF_WINDOWS_TRIPLE, NativeLinkage::Executable) => {
            Some(emit_x86_64_pe_executable(request))
        }
        (AARCH64_LINUX_TRIPLE, NativeLinkage::StaticLib) => emit_aarch64_elf_object(request),
        (AARCH64_LINUX_TRIPLE, NativeLinkage::Executable) => {
            Some(emit_aarch64_elf_executable(request))
        }
        ("aarch64-apple-darwin", NativeLinkage::StaticLib) => emit_aarch64_macho_archive(request),
        (ARMV7_LINUX_GNUEABIHF_TRIPLE, NativeLinkage::StaticLib) => emit_arm32_elf_object(request),
        (ARMV7_LINUX_GNUEABIHF_TRIPLE, NativeLinkage::Executable) => {
            Some(emit_arm32_elf_executable(request))
        }
        (WASM32_UNKNOWN_TRIPLE, NativeLinkage::StaticLib) => emit_wasm32_module(request),
        ("i386-unknown-none", NativeLinkage::StaticLib)
        | ("i686-unknown-none", NativeLinkage::StaticLib)
        | (X86_64_TRIPLE, NativeLinkage::StaticLib) => emit_x86_64_freestanding_object(request),
        (AARCH64_NONE_TRIPLE, NativeLinkage::StaticLib) => emit_aarch64_elf_object(request),
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
    write_thumb_relocatable_object(&object, &mut bytes);
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

fn x86_object_bytes(
    result: &crate::native_emit::x86_64_lower::X86_64CompileResult,
    exports: &[(String, u32)],
    is_64: bool,
) -> Result<Vec<u8>, String> {
    let mut legacy_sites = result.relocations.clone();
    let mut typed_sites = result
        .absolute_refs
        .iter()
        .map(|reference| reference.site)
        .collect::<Vec<_>>();
    legacy_sites.sort_unstable();
    typed_sites.sort_unstable();
    if legacy_sites != typed_sites {
        return Err("x86 object is missing absolute relocation metadata".into());
    }
    let calls = result
        .extern_calls
        .iter()
        .map(|(symbol, site)| {
            site.checked_add(1)
                .map(|site| (site, symbol.clone()))
                .ok_or_else(|| "x86 call relocation offset overflow".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    write_x86_data_object(
        &X86RelocatableObject {
            code: &result.code,
            data: &result.data,
            exports,
            undefs: &result.externs,
            calls: &calls,
            absolute_refs: &result.absolute_refs,
            data_relocations: &result.data_relocations,
        },
        is_64,
    )
}

fn emit_x86_64_elf_object(request: &NativeObjectRequest<'_>) -> Option<NativeObjectArtifact> {
    crate::native_emit::x86_64::set_32bit(false);
    let result = lower_native_module(request.module, request.entry).ok()?;
    // Keep hosted sibling bodies local to avoid collisions with host symbols.
    let exports = vec![(request.entry.to_string(), result.entry_offset)];
    let bytes = match x86_object_bytes(&result, &exports, true) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("x86 ELF object error: {error}");
            return None;
        }
    };
    Some(NativeObjectArtifact {
        bytes,
        artifact_kind: "elf-relocatable-object",
        backend_level: "owned-object-subset",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration lowers x86_64 ELF objects with writable data, section-relative absolute references and external call relocations",
        abi_manifest: Some(object_abi_manifest(request)),
    })
}

fn emit_aarch64_elf_object(request: &NativeObjectRequest<'_>) -> Option<NativeObjectArtifact> {
    let mut lowered = crate::native_emit::lower::lower_native_module(
        request.module,
        request.entry,
        NativeLinkage::StaticLib,
    )
    .ok()?;
    // ponytail: add data and CALL26 relocations before accepting globals or external calls.
    if !lowered.global_relocs.is_empty()
        || !lowered.data_relocations.is_empty()
        || !lowered.external_refs.is_empty()
        || !lowered.degradations.is_empty()
    {
        return None;
    }
    let first = lowered.exports.iter().min_by_key(|export| export.offset)?;
    let base = first.offset;
    let export_name = first.name.clone();
    // ELF's primary export starts at zero; remove the unused executable-entry prefix.
    // All remaining references are PC-relative, so a uniform shift preserves them.
    let object = ElfObject {
        code: lowered.code.split_off(base as usize),
        export_name: export_name.clone(),
        exports: lowered
            .exports
            .into_iter()
            .filter(|export| export.name != export_name)
            .map(|export| (export.name, export.offset - base))
            .collect(),
        undefs: vec![],
        thumb_calls: vec![],
        thumb_addr_refs: vec![],
        x86_plt_relocs: vec![],
    };
    let mut bytes = Vec::new();
    write_aarch64_relocatable_object(&object, &mut bytes);
    let freestanding = request.target_triple == AARCH64_NONE_TRIPLE;
    Some(NativeObjectArtifact {
        bytes,
        artifact_kind: "elf-relocatable-object",
        backend_level: if freestanding {
            "owned-native-subset-freestanding"
        } else {
            "owned-native-subset-aarch64-static"
        },
        runtime_level: if freestanding {
            "freestanding-none"
        } else {
            "none"
        },
        reason_code: if freestanding {
            "native-aarch64-unknown-none-freestanding"
        } else {
            NATIVE_OBJECT_SUBSET
        },
        reason: "inauguration emits ELF64 AArch64 relocatable objects with real Core IR function bodies and every function exported",
        abi_manifest: Some(object_abi_manifest(request)),
    })
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

fn emit_arm32_elf_object(request: &NativeObjectRequest<'_>) -> Option<NativeObjectArtifact> {
    // ponytail: ARM32 objects only support pure i32 entries until general body lowering exists.
    let value = i32::try_from(crate::owned_compile::pure_entry_scalar_value(
        request.module,
        request.entry,
    )?)
    .ok()?;
    let object = ElfObject {
        code: arm32_return_i32_object_code(value),
        export_name: request.entry.to_string(),
        exports: vec![],
        undefs: vec![],
        thumb_calls: vec![],
        thumb_addr_refs: vec![],
        x86_plt_relocs: vec![],
    };
    let mut bytes = Vec::new();
    write_arm32_relocatable_object(&object, &mut bytes);
    Some(NativeObjectArtifact {
        bytes,
        artifact_kind: "elf32-relocatable-object",
        backend_level: "owned-object-subset",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration owns ELF32 ARM relocatable object emission for const-evaluable scalar entry functions on this target",
        abi_manifest: Some(object_abi_manifest(request)),
    })
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

fn emit_aarch64_macho_archive(request: &NativeObjectRequest<'_>) -> Option<NativeObjectArtifact> {
    let lowered = crate::native_emit::lower::lower_native_module(
        request.module,
        request.entry,
        NativeLinkage::StaticLib,
    )
    .ok()?;
    // The object writer has no writable data section; every export must be fully lowered.
    if lowered.exports.is_empty()
        || !lowered.global_relocs.is_empty()
        || !lowered.data_relocations.is_empty()
        || !lowered.degradations.is_empty()
    {
        return None;
    }
    let image = MachOImage {
        code: lowered.code,
        entry_offset: None,
        exports: lowered.exports,
        external_refs: lowered.external_refs,
    };
    let mut bytes = Vec::new();
    write_image(&image, MachOLinkage::StaticLib, request.entry, &mut bytes);
    Some(NativeObjectArtifact {
        bytes,
        artifact_kind: "mach-o-static-archive",
        backend_level: "owned-native-subset-aarch64-static",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration emits Mach-O static archives containing real AArch64 Core IR function bodies and external call relocations",
        abi_manifest: Some(object_abi_manifest(request)),
    })
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

fn emit_x86_64_freestanding_object(
    request: &NativeObjectRequest<'_>,
) -> Option<NativeObjectArtifact> {
    let is_i386 =
        request.target_triple.starts_with("i386-") || request.target_triple.starts_with("i686-");
    crate::native_emit::x86_64::set_32bit(is_i386);
    // ET_REL section relocations determine final placement, independently of --base.
    let result =
        lower_module_with_bases(request.module, request.entry, request.base.unwrap_or(0), 0)
            .ok()?;
    let bytes = match x86_object_bytes(&result, &result.exports, !is_i386) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("x86 freestanding object error: {error}");
            return None;
        }
    };
    Some(NativeObjectArtifact {
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
            "inauguration owns ELF32 i386 relocatable function bodies, writable data and code/data relocations"
        } else {
            "inauguration owns ELF64 x86_64 relocatable function bodies, writable data and code/data relocations"
        },
        abi_manifest: Some(object_abi_manifest(request)),
    })
}

fn emit_wasm32_module(request: &NativeObjectRequest<'_>) -> Option<NativeObjectArtifact> {
    // ponytail: this writer only lowers a pure scalar entry until it gains general body lowering.
    let value = i32::try_from(crate::owned_compile::pure_entry_scalar_value(
        request.module,
        request.entry,
    )?)
    .ok()?;
    let module = WasmModule {
        export_name: request.entry.to_string(),
        value,
    };
    let mut bytes = Vec::new();
    write_scalar_i32_module(&module, &mut bytes);
    Some(NativeObjectArtifact {
        bytes,
        artifact_kind: "wasm-module",
        backend_level: "owned-object-subset",
        runtime_level: "none",
        reason_code: NATIVE_OBJECT_SUBSET,
        reason: "inauguration owns WebAssembly module emission for const-evaluable scalar entry functions on this target",
        abi_manifest: None,
    })
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

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn macho_archive_links_and_executes_its_function_body() {
        for (module, caller) in [
            (crate::in_lang_parse::parse_in_library_source("fn _answer() -> Int { return 42; }").unwrap(), "long long _answer(void); int main(void) { return (int)_answer(); }"),
            (scalar_answer_module(), "long long answer(void); int main(void) { return (int)answer(); }"),
            (crate::in_lang_parse::parse_in_library_source("fn answer(base: Int) -> Int { base[1] = 7; return (base + 16)[-1] + 35; }").unwrap(), "long long answer(long long); int main(void) { long long words[] = {10, 20, 30, 40}; long long result = answer((long long)words); if (result != 42 || words[0] != 10 || words[1] != 7 || words[2] != 30 || words[3] != 40) return 99; return 42; }"),
            (crate::in_lang_parse::parse_in_library_source("fn answer() -> Int { return _add(5, 37); }").unwrap(), "long long _add(long long a, long long b) { return a + b; } long long answer(void); int main(void) { return (int)answer(); }"),
            (crate::in_lang_parse::parse_in_library_source("fn add(a: Int, b: Int) -> Int { return a + b; }\nfn answer() -> Int { return add(5, 37); }").unwrap(), "long long add(long long, long long); int main(void) { return (int)add(-5, 47); }"),
            (crate::in_lang_parse::parse_in_library_source("fn answer() -> Int { return labs(-42); }").unwrap(), "long long answer(void); int main(void) { return (int)answer(); }"),
            (crate::in_lang_parse::parse_in_library_source("fn helper(value: Int) -> Int { return labs(value); }\nfn answer() -> Int { return helper(-42); }").unwrap(), "long long answer(void); int main(void) { return (int)answer(); }"),
        ] {
        let request = NativeObjectRequest {
            target_triple: "aarch64-apple-darwin",
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 0,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("archive");
        let dir = std::env::temp_dir().join(format!(
            "in-archive-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let lib = dir.join("libanswer.a");
        let c = dir.join("caller.c");
        let exe = dir.join("caller");
        std::fs::write(&lib, artifact.bytes).unwrap();
        std::fs::write(&c, caller).unwrap();
        let output = std::process::Command::new("cc")
            .args(["-arch", "arm64"])
            .arg(&c)
            .arg(&lib)
            .arg("-o")
            .arg(&exe)
            .output()
            .unwrap();
        let status = output
            .status
            .success()
            .then(|| std::process::Command::new(&exe).status().unwrap());
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(status.unwrap().code(), Some(42));
        }
    }

    #[test]
    fn aarch64_static_objects_reject_empty_degraded_and_writable_global_modules() {
        let mut degraded = scalar_answer_module();
        degraded.decls.push(Decl::Function {
            name: "unsupported".into(),
            params: vec![],
            ret: Typ::Array(Box::new(Typ::Array(Box::new(Typ::Int)))),
            body: vec![Stmt::Return(Some(Expr::ArrayLit(vec![])))],
            type_params: vec![],
        });
        for module in [
            UnifiedModule::new(Vec::new()),
            degraded,
            crate::in_lang_parse::parse_in_library_source(
                "var count: Int = 3;\nfn answer() -> Int { return count; }",
            )
            .unwrap(),
        ] {
            for target in [
                "aarch64-apple-darwin",
                AARCH64_LINUX_TRIPLE,
                AARCH64_NONE_TRIPLE,
            ] {
                let request = NativeObjectRequest {
                    target_triple: target,
                    linkage: NativeLinkage::StaticLib,
                    entry: "answer",
                    exit_code: 0,
                    module: &module,
                    module_id: "App",
                    base: None,
                };
                assert!(emit_native_object(&request).is_none(), "{target}");
            }
        }
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
    fn x86_object_adapter_refuses_missing_absolute_reference_metadata() {
        let module = crate::in_lang_parse::parse_in_source(
            "var count:Int=41;\nfn main()->Int{count=count+1;return count;}",
        )
        .unwrap();
        crate::native_emit::x86_64::set_32bit(false);
        let mut result = lower_native_module(&module, "main").unwrap();
        assert!(!result.data.is_empty());
        assert!(!result.absolute_refs.is_empty());
        let bytes = x86_object_bytes(&result, &result.exports, true).unwrap();
        let repeated = lower_native_module(&module, "main").unwrap();
        assert_eq!(
            bytes,
            x86_object_bytes(&repeated, &repeated.exports, true).unwrap()
        );
        result.absolute_refs.clear();
        assert!(x86_object_bytes(&result, &result.exports, true).is_err());
    }

    #[test]
    fn x86_64_staticlib_object_relocates_absolute_string_references() {
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
        let artifact = emit_native_object(&request).expect("relocatable string reference");
        assert!(
            artifact
                .bytes
                .windows(10)
                .any(|bytes| bytes == b".rela.text")
        );
        assert!(artifact.bytes.windows(2).any(|bytes| bytes == b"hi"));
    }

    #[test]
    fn dispatches_wasm32_staticlib_module() {
        let module = scalar_answer_module();
        let request = NativeObjectRequest {
            target_triple: WASM32_UNKNOWN_TRIPLE,
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 0,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("wasm artifact");
        assert_eq!(artifact.artifact_kind, "wasm-module");
        assert_eq!(artifact.reason_code, NATIVE_OBJECT_SUBSET);
        assert!(artifact.bytes.windows(6).any(|window| window == b"answer"));
        assert!(artifact.abi_manifest.is_none());
        assert!(
            artifact
                .bytes
                .windows(3)
                .any(|bytes| bytes == [0x41, 42, 0x0b])
        );
    }

    #[test]
    fn wasm_constant_writer_rejects_missing_effectful_and_out_of_range_entries() {
        for module in [
            UnifiedModule::new(vec![]),
            crate::in_lang_parse::parse_in_library_source(
                "fn answer() -> Int { print(\"effect\"); return 42; }",
            )
            .unwrap(),
            crate::in_lang_parse::parse_in_library_source(
                "fn answer(x: Int) -> Int { return 42; }",
            )
            .unwrap(),
            crate::in_lang_parse::parse_in_library_source(
                "fn answer() -> Int { return 2147483648; }",
            )
            .unwrap(),
        ] {
            let request = NativeObjectRequest {
                target_triple: WASM32_UNKNOWN_TRIPLE,
                linkage: NativeLinkage::StaticLib,
                entry: "answer",
                exit_code: 0,
                module: &module,
                module_id: "App",
                base: None,
            };
            assert!(emit_native_object(&request).is_none());
        }
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
    fn aarch64_elf_objects_reject_modules_needing_external_or_data_relocations() {
        for source in [
            "fn answer() -> Int { return labs(-42); }",
            "var count: Int = 3;\nfn answer() -> Int { return count; }",
        ] {
            let module = crate::in_lang_parse::parse_in_library_source(source).unwrap();
            for target in [AARCH64_LINUX_TRIPLE, AARCH64_NONE_TRIPLE] {
                let request = NativeObjectRequest {
                    target_triple: target,
                    linkage: NativeLinkage::StaticLib,
                    entry: "answer",
                    exit_code: 0,
                    module: &module,
                    module_id: "App",
                    base: None,
                };
                assert!(emit_native_object(&request).is_none(), "{target}: {source}");
            }
        }
    }

    #[test]
    fn aarch64_elf_objects_export_real_function_bodies() {
        let module = crate::in_lang_parse::parse_in_library_source(
            "fn a_add(a: Int, b: Int) -> Int { return a + b; }\nfn z_answer() -> Int { return a_add(5, 37); }",
        ).unwrap();
        for target in [AARCH64_LINUX_TRIPLE, AARCH64_NONE_TRIPLE] {
            let request = NativeObjectRequest {
                target_triple: target,
                linkage: NativeLinkage::StaticLib,
                entry: "z_answer",
                exit_code: 0,
                module: &module,
                module_id: "App",
                base: None,
            };
            let artifact = emit_native_object(&request).expect("real object");
            let bytes = artifact.bytes;
            let u64_at = |offset: usize| {
                u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize
            };
            let shoff = u64_at(40);
            let text_offset = u64_at(shoff + 64 + 24);
            let text_size = u64_at(shoff + 64 + 32);
            let symtab_offset = u64_at(shoff + 128 + 24);
            let symtab_size = u64_at(shoff + 128 + 32);
            let strtab_offset = u64_at(shoff + 192 + 24);
            let mut exports = Vec::new();
            for symbol in (symtab_offset + 24..symtab_offset + symtab_size).step_by(24) {
                let name_offset =
                    u32::from_le_bytes(bytes[symbol..symbol + 4].try_into().unwrap()) as usize;
                let name = bytes[strtab_offset + name_offset..]
                    .split(|byte| *byte == 0)
                    .next()
                    .unwrap();
                exports.push((
                    std::str::from_utf8(name).unwrap().to_string(),
                    u64_at(symbol + 8) as u32,
                    0,
                ));
            }
            assert_eq!(
                exports.len(),
                2,
                "{target}: every function must be exported"
            );
            assert_eq!(exports[0].0, "a_add");
            assert_eq!(exports[0].1, 0);
            assert_eq!(exports[1].0, "z_answer");
            assert!(exports[1].1 > 0);
            let text = &bytes[text_offset..text_offset + text_size];
            assert_ne!(
                &text[..4],
                &[0, 0, 0, 0],
                "export points at the unused entry prefix"
            );
            #[cfg(target_arch = "aarch64")]
            {
                // Execute the emitted ISA on the host; this does not claim Linux runtime coverage.
                let mut rt = crate::jit_runtime::JitRuntime::new();
                rt.load(text, &exports, &[]).unwrap();
                assert_eq!(unsafe { rt.invoke("a_add", &[-5, 47]).unwrap() }, 42);
                assert_eq!(unsafe { rt.invoke("z_answer", &[]).unwrap() }, 42);
            }
        }
    }

    #[test]
    fn dispatches_aarch64_linux_staticlib_object() {
        let module = scalar_answer_module();
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
        let module = scalar_answer_module();
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
    fn object_emission_restores_instruction_width_on_success_and_failure() {
        let original = crate::native_emit::x86_64::is_32bit();
        for previous in [false, true] {
            for target in ["i386-unknown-none", X86_64_TRIPLE, ELF_LINUX_TRIPLE] {
                for module in [scalar_answer_module(), UnifiedModule::new(vec![])] {
                    crate::native_emit::x86_64::set_32bit(previous);
                    let request = NativeObjectRequest {
                        target_triple: target,
                        linkage: NativeLinkage::StaticLib,
                        entry: "answer",
                        exit_code: 0,
                        module: &module,
                        module_id: "App",
                        base: None,
                    };
                    let _ = emit_native_object(&request);
                    let actual = crate::native_emit::x86_64::is_32bit();
                    crate::native_emit::x86_64::set_32bit(original);
                    assert_eq!(actual, previous, "{target}");
                }
            }
        }
    }

    #[test]
    fn constant_and_freestanding_writers_refuse_unlowered_bodies() {
        for module in [
            UnifiedModule::new(vec![]),
            UnifiedModule::new(vec![Decl::Function {
                name: "answer".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Binary {
                    op: "**".into(),
                    lhs: Box::new(Expr::IntLit(2)),
                    rhs: Box::new(Expr::IntLit(3)),
                }))],
                type_params: vec![],
            }]),
        ] {
            for target in [
                ARMV7_LINUX_GNUEABIHF_TRIPLE,
                "i386-unknown-none",
                "i686-unknown-none",
                X86_64_TRIPLE,
            ] {
                let request = NativeObjectRequest {
                    target_triple: target,
                    linkage: NativeLinkage::StaticLib,
                    entry: "answer",
                    exit_code: 0,
                    module: &module,
                    module_id: "App",
                    base: None,
                };
                assert!(
                    emit_native_object(&request).is_none(),
                    "{target} fabricated a body"
                );
            }
        }
    }

    #[test]
    fn dispatches_arm32_staticlib_object() {
        let module = scalar_answer_module();
        let request = NativeObjectRequest {
            target_triple: "armv7-unknown-linux-gnueabihf",
            linkage: NativeLinkage::StaticLib,
            entry: "answer",
            exit_code: 0,
            module: &module,
            module_id: "App",
            base: None,
        };
        let artifact = emit_native_object(&request).expect("arm32 object artifact");
        assert_eq!(artifact.artifact_kind, "elf32-relocatable-object");
        assert_eq!(artifact.reason_code, NATIVE_OBJECT_SUBSET);
        assert!(
            artifact
                .bytes
                .windows(4)
                .any(|bytes| bytes == 0xe300_002au32.to_le_bytes())
        );
        assert_eq!(artifact.bytes[4], 1);
        assert_eq!(
            u16::from_le_bytes([artifact.bytes[18], artifact.bytes[19]]),
            40
        );
    }

    #[test]
    fn dispatches_aarch64_freestanding_object() {
        let module = scalar_answer_module();
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
        assert_eq!(
            artifact.reason_code,
            "native-aarch64-unknown-none-freestanding"
        );
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
