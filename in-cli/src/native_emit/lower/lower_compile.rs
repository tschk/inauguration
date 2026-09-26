//! Native executable / artifact compilation helpers.

use super::{
    LoweringDegradation, NativeLinkage, TL_NATIVE_MODE, boundary_from_module, build_assembly,
    find_sdk_root, lower_module, native_link_name,
};
use crate::boundary_emit;
use crate::core_ir::UnifiedModule;
use crate::native_emit::macho::{self, MachOImage};
use std::path::{Path, PathBuf};

pub fn compile_native_executable(
    module: &UnifiedModule,
    entry: &str,
    out_path: &Path,
) -> Result<(), String> {
    compile_native_artifact(module, "App", entry, NativeLinkage::Executable, out_path).map(|_| ())
}

pub fn compile_native_executable_for_host(
    module: &UnifiedModule,
    entry: &str,
    out_path: &Path,
) -> Result<(), String> {
    if !host_supports_native_subset() {
        return Err("native-host-unsupported".to_string());
    }
    compile_native_executable(module, entry, out_path)
}

pub fn compile_native_artifact_for_host(
    module: &UnifiedModule,
    module_id: &str,
    entry: &str,
    linkage: NativeLinkage,
    out_path: &Path,
) -> Result<Option<PathBuf>, String> {
    if !host_supports_native_subset() {
        return Err("native-host-unsupported".to_string());
    }
    compile_native_artifact(module, module_id, entry, linkage, out_path)
}

pub fn compile_native_artifact_for_host_with_report(
    module: &UnifiedModule,
    module_id: &str,
    entry: &str,
    linkage: NativeLinkage,
    out_path: &Path,
) -> Result<NativeArtifactOutcome, String> {
    if !host_supports_native_subset() {
        return Err("native-host-unsupported".to_string());
    }
    compile_native_artifact_with_report(module, module_id, entry, linkage, out_path)
}

/// A native artifact plus anything the lowerer could not compile.
#[derive(Debug)]
pub struct NativeArtifactOutcome {
    pub abi_path: Option<PathBuf>,
    pub degradations: Vec<LoweringDegradation>,
}

pub fn compile_native_artifact(
    module: &UnifiedModule,
    module_id: &str,
    entry: &str,
    linkage: NativeLinkage,
    out_path: &Path,
) -> Result<Option<PathBuf>, String> {
    compile_native_artifact_with_report(module, module_id, entry, linkage, out_path)
        .map(|outcome| outcome.abi_path)
}

/// Like [`compile_native_artifact`], but also reports code the lowerer replaced
/// with a runtime trap. Callers that can surface that to the user should prefer
/// this entry point.
pub fn compile_native_artifact_with_report(
    module: &UnifiedModule,
    module_id: &str,
    entry: &str,
    linkage: NativeLinkage,
    out_path: &Path,
) -> Result<NativeArtifactOutcome, String> {
    TL_NATIVE_MODE.with(|m| *m.borrow_mut() = true);
    let lowered = lower_module(module, entry, linkage)?;
    let degradations = lowered.degradations;

    if linkage == NativeLinkage::Executable && cfg!(target_os = "macos") {
        eprintln!(
            "[native] as+ld path: {} external refs",
            lowered.external_refs.len()
        );
        for (site, name) in &lowered.external_refs {
            let link_name = native_link_name(name);
            eprintln!("[native]   ref at offset {site}: '{name}' → '{link_name}'");
        }
        // Write assembly source and assemble+link with system tools
        let mapped_exports: Vec<(String, u32)> = lowered
            .exports
            .iter()
            .map(|e| (native_link_name(&e.name), e.offset))
            .collect();
        let mapped_external_refs: Vec<(u32, String)> = lowered
            .external_refs
            .iter()
            .map(|(site, name)| (*site, native_link_name(name)))
            .collect();

        // Build assembly source from lowered code
        let asm = build_assembly(
            &lowered.code,
            &mapped_exports,
            &mapped_external_refs,
            &lowered.error_slot_refs,
            entry,
        );
        let asm_path = out_path.with_extension("s");
        std::fs::write(&asm_path, &asm).map_err(|e| format!("write assembly: {e}"))?;

        // Assemble with system assembler
        let obj_path = out_path.with_extension("o");
        let as_status = std::process::Command::new("as")
            .arg("-arch")
            .arg("arm64")
            .arg("-o")
            .arg(&obj_path)
            .arg(&asm_path)
            .status()
            .map_err(|e| format!("as invocation failed: {e}"))?;
        if !as_status.success() {
            return Err("assembly failed".to_string());
        }

        // Link with system linker
        let sdk_root = find_sdk_root().unwrap_or_else(|| "/".to_string());
        let entry_name = native_link_name(entry);
        let ld_status = std::process::Command::new("ld")
            .arg("-o")
            .arg(out_path)
            .arg(&obj_path)
            .arg("-lSystem")
            .arg("-L")
            .arg(format!("{sdk_root}/usr/lib"))
            .arg("-arch")
            .arg("arm64")
            .arg("-macos_version_min")
            .arg("14.0")
            .arg("-e")
            .arg(&entry_name)
            .status()
            .map_err(|e| format!("ld invocation failed: {e}"))?;
        if !ld_status.success() {
            return Err("ld link failed".to_string());
        }

        // Keep .s and .o for debugging; clean up on success
        return Ok(NativeArtifactOutcome {
            abi_path: None,
            degradations,
        });
    }

    // No external refs: write standalone Mach-O executable
    let install_name = out_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("libin.dylib");
    let install = match linkage {
        NativeLinkage::Dylib => format!("@rpath/{install_name}"),
        NativeLinkage::Executable | NativeLinkage::StaticLib => install_name.to_string(),
    };
    let abi_path = match linkage {
        NativeLinkage::Dylib | NativeLinkage::StaticLib => {
            let boundary = boundary_from_module(module, module_id, &lowered.exports);
            let abi_json = boundary_emit::emit_abi_manifest_with_package(
                &boundary,
                module.identity.package.as_deref(),
            );
            let abi_path = out_path.with_extension("abi.json");
            std::fs::write(&abi_path, abi_json)
                .map_err(|err| format!("write abi manifest `{}`: {err}", abi_path.display()))?;
            Some(abi_path)
        }
        NativeLinkage::Executable => None,
    };
    let image = MachOImage {
        code: lowered.code,
        entry_offset: lowered.entry_offset,
        exports: match linkage {
            NativeLinkage::Executable => Vec::new(),
            NativeLinkage::Dylib | NativeLinkage::StaticLib => lowered.exports,
        },
        external_refs: lowered.external_refs,
    };
    let mut file_bytes = Vec::new();
    macho::write_image(&image, linkage.into(), &install, &mut file_bytes);
    std::fs::write(out_path, &file_bytes)
        .map_err(|err| format!("write native artifact `{}`: {err}", out_path.display()))?;
    Ok(NativeArtifactOutcome {
        abi_path,
        degradations,
    })
}

pub fn host_supports_native_subset() -> bool {
    cfg!(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
    ))
}
