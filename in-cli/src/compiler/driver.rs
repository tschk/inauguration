//! Compiler invocations — entry points for `in build`, `in compile`, and `in agent`.
//!
//! These functions wire the full compiler pipeline:
//!
//! 1. Parse `.in` source via `in_lang_parse` → `UnifiedModule`
//! 2. Lower to `IrModule` (the compiler's SSA-like IR)
//! 3. Run type checker
//! 4. Run optimization passes
//! 5. Emit machine code via `CodegenBackend` + `native_emit/*`
//! 6. Produce component metadata JSON sidecar

use std::path::{Path, PathBuf};

use super::backend::{self, BackendKind};
use super::metadata::{ArtifactKind, ComponentMetadata, ComponentSpec, OptimizationLevel};
use super::pipeline::Compiler;

/// Lower a unified module to the same textual SIL shape as `.in` / native subset emitters.
#[must_use]
pub fn lower_unified_module(module: &crate::core_ir::UnifiedModule, module_id: &str) -> String {
    crate::lower_core::lower_to_textual_sil(module.clone(), module_id)
}

/// Compile a `.in` source file into an artifact with metadata sidecar.
///
/// This is the main entry point for `in build --compile <path>`.
pub fn compile_in_file(
    source_path: &Path,
    spec: &ComponentSpec,
    emit_metadata: bool,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let source = std::fs::read_to_string(source_path).map_err(|e| format!("read source: {e}"))?;

    let source_name = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("module");

    // Build and run the compiler
    let mut compiler = Compiler::new(spec.clone()).map_err(|e| format!("compiler setup: {e}"))?;
    let result = compiler
        .compile(&source)
        .map_err(|e| format!("compilation: {e}"))?;

    // Write the output artifact
    let ext = result.output.extension;
    let output_path = source_path.with_extension(ext);
    std::fs::write(&output_path, &result.output.data).map_err(|e| format!("write output: {e}"))?;

    if spec.artifact_kind == ArtifactKind::Executable {
        crate::owned_compile::set_native_artifact_permissions(
            &output_path,
            crate::native_emit::NativeLinkage::Executable,
        )?;
    }

    println!(
        "  Compiled `{}` → {} ({} bytes, target: {})",
        source_name,
        output_path.display(),
        result.output.data.len(),
        spec.target,
    );

    // Write metadata sidecar
    let metadata_path = if emit_metadata {
        let md_path = output_path.with_extension("component.json");
        let json = serde_json::to_string_pretty(&result.metadata)
            .map_err(|e| format!("serialize metadata: {e}"))?;
        let code_bytes: u64 = result.metadata.code_sections.iter().map(|s| s.size).sum();
        std::fs::write(&md_path, &json).map_err(|e| format!("write metadata: {e}"))?;
        println!(
            "  Metadata → {} ({}b code, {} imports, {} exports, {}+{} capabilities)",
            md_path.display(),
            code_bytes,
            result.metadata.imports.len(),
            result.metadata.exports.len(),
            result.metadata.capabilities_required.len(),
            result.metadata.capabilities_exported.len(),
        );
        Some(md_path)
    } else {
        None
    };

    Ok((output_path, metadata_path))
}

/// Emit only the component metadata for a `.in` source file (no codegen).
pub fn emit_component_metadata(
    source_path: &Path,
    spec: &ComponentSpec,
) -> Result<PathBuf, String> {
    let source = std::fs::read_to_string(source_path).map_err(|e| format!("read source: {e}"))?;

    let mut compiler = Compiler::new(spec.clone()).map_err(|e| format!("compiler setup: {e}"))?;
    let module = compiler
        .parse_source(&source)
        .map_err(|e| format!("parse: {e}"))?;

    let empty_output = super::backend::BackendOutput {
        data: Vec::new(),
        extension: "",
        entry_offset: None,
        symbol_table: vec![],
    };
    let metadata = ComponentMetadata::build(spec, &module, &empty_output, &source);
    let md_path = source_path.with_extension("component.json");
    let json =
        serde_json::to_string_pretty(&metadata).map_err(|e| format!("serialize metadata: {e}"))?;
    std::fs::write(&md_path, &json).map_err(|e| format!("write metadata: {e}"))?;

    println!(
        "  Metadata `{}` → {}",
        source_path.display(),
        md_path.display()
    );
    Ok(md_path)
}

/// Build CLI flags into a [`ComponentSpec`].
pub fn spec_from_cli(
    name: &str,
    target: &str,
    kind: ArtifactKind,
    entry: &str,
    opt: u8,
    deterministic: bool,
    freestanding: bool,
) -> ComponentSpec {
    let target = if freestanding {
        if target.contains("aarch64") {
            "aarch64-unknown-none".to_string()
        } else {
            "x86_64-unknown-none".to_string()
        }
    } else {
        target.to_string()
    };

    let opt_level = match opt {
        0 => OptimizationLevel::None,
        1 => OptimizationLevel::Less,
        2 => OptimizationLevel::Default,
        _ => OptimizationLevel::Aggressive,
    };

    ComponentSpec {
        name: name.to_string(),
        target,
        artifact_kind: kind,
        deterministic,
        checkpoint: String::new(),
        optimization_level: opt_level,
        debug_info: false,
        entry_point: Some(entry.to_string()),
        imports: vec![],
        exports: vec![],
        capabilities: vec![],
        capabilities_exported: vec![],
    }
}

/// Print available backends (replaces `llc --version` / `clang --print-supported-targets`).
pub fn print_backends() {
    println!("Available codegen backends (replacing LLVM):");
    println!();
    for kind in BackendKind::ALL {
        let triple = match kind {
            BackendKind::AArch64MachO => "aarch64-apple-darwin",
            BackendKind::AArch64Elf => "aarch64-unknown-linux-gnu",
            BackendKind::X86_64Elf => "x86_64-unknown-linux-gnu",
            BackendKind::Arm32Elf => "armv7-unknown-linux-gnueabihf",
            BackendKind::X86_64Coff => "x86_64-pc-windows-msvc",
            BackendKind::AArch64Coff => "aarch64-pc-windows-msvc",
            BackendKind::Wasm32 => "wasm32-unknown-unknown",
            BackendKind::RawBinary => "raw",
        };
        let caps = backend::backend_capabilities(triple);
        let status = if caps.implemented {
            "implemented"
        } else {
            "contract"
        };
        println!("  {triple:40} {status:15} format={}", caps.object_format);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDirWrapper(pub std::path::PathBuf);
    impl Drop for TempDirWrapper {
        fn drop(&mut self) {
            if self.0.exists() {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn test_compile_in_file_success() {
        let pid = std::process::id();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let temp_dir_path =
            std::env::temp_dir().join(format!("in_cli_test_compile_success_{}_{}", pid, timestamp));
        fs::create_dir_all(&temp_dir_path).unwrap();
        let _cleanup = TempDirWrapper(temp_dir_path.clone());

        let source_path = temp_dir_path.join("test_module.in");
        let source_code = "fn main() { }";
        fs::write(&source_path, source_code).unwrap();

        let spec = spec_from_cli(
            "test_module",
            "x86_64-unknown-linux-gnu",
            ArtifactKind::Executable,
            "main",
            0,
            false,
            false,
        );

        let result = compile_in_file(&source_path, &spec, true);
        assert!(result.is_ok(), "compilation failed: {:?}", result.err());

        let (out_path, meta_path) = result.unwrap();
        assert!(out_path.exists());
        assert!(meta_path.is_some());
        assert!(meta_path.unwrap().exists());
    }

    #[test]
    fn test_compile_in_file_read_error() {
        let pid = std::process::id();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let temp_dir_path =
            std::env::temp_dir().join(format!("in_cli_test_read_err_{}_{}", pid, timestamp));
        fs::create_dir_all(&temp_dir_path).unwrap();
        let _cleanup = TempDirWrapper(temp_dir_path.clone());

        let source_path = temp_dir_path.join("does_not_exist.in");

        let spec = spec_from_cli(
            "test_module",
            "x86_64-unknown-linux-gnu",
            ArtifactKind::Executable,
            "main",
            0,
            false,
            false,
        );

        let result = compile_in_file(&source_path, &spec, true);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(err_msg.contains("read source"));
    }

    #[test]
    fn test_compile_in_file_compile_error() {
        let pid = std::process::id();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let temp_dir_path =
            std::env::temp_dir().join(format!("in_cli_test_compile_err_{}_{}", pid, timestamp));
        fs::create_dir_all(&temp_dir_path).unwrap();
        let _cleanup = TempDirWrapper(temp_dir_path.clone());

        let source_path = temp_dir_path.join("test_module_err.in");
        let source_code = "fn main() -> i32 { return \"not an int\"; }";
        fs::write(&source_path, source_code).unwrap();

        let spec = spec_from_cli(
            "test_module",
            "x86_64-unknown-linux-gnu",
            ArtifactKind::Executable,
            "main",
            0,
            false,
            false,
        );

        let result = compile_in_file(&source_path, &spec, true);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(err_msg.contains("compilation"));
    }
}
