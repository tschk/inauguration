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
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before UNIX_EPOCH")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "inauguration-driver-{}-{}-{}",
                std::process::id(),
                unique,
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_compile_in_file_success() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("test_module.in");
        fs::write(&source_path, "fn main() -> Int { return 42; }").unwrap();

        let spec = ComponentSpec::host_executable("test_module", Some("main"));

        let result = compile_in_file(&source_path, &spec, false);
        assert!(result.is_ok(), "compile_in_file failed: {:?}", result.err());

        let (output_path, metadata_path) = result.unwrap();
        assert!(output_path.exists(), "Output artifact was not created");
        assert!(metadata_path.is_none(), "Metadata should not be emitted");

        let output_data = fs::read(&output_path).unwrap();
        assert!(!output_data.is_empty(), "Output artifact is empty");
    }

    #[test]
    fn test_compile_in_file_with_metadata() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("test_module_meta.in");
        fs::write(&source_path, "fn main() -> Int { return 42; }").unwrap();

        let spec = ComponentSpec::host_executable("test_module_meta", Some("main"));

        let result = compile_in_file(&source_path, &spec, true);
        assert!(result.is_ok(), "compile_in_file failed: {:?}", result.err());

        let (output_path, metadata_path) = result.unwrap();
        assert!(output_path.exists(), "Output artifact was not created");

        let md_path = metadata_path.expect("Metadata should be emitted");
        assert!(md_path.exists(), "Metadata file was not created");

        let md_data = fs::read_to_string(&md_path).unwrap();
        assert!(
            md_data.contains("test_module_meta"),
            "Metadata should contain the module name"
        );
    }

    #[test]
    fn test_compile_in_file_read_error() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("nonexistent.in");

        let spec = ComponentSpec::host_executable("nonexistent", Some("main"));
        let result = compile_in_file(&source_path, &spec, false);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("read source"));
    }

    #[test]
    fn test_compile_in_file_compile_error() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("invalid.in");
        fs::write(&source_path, "this is not valid syntax").unwrap();

        let spec = ComponentSpec::host_executable("invalid", Some("main"));

        let result = compile_in_file(&source_path, &spec, false);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("compilation"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before UNIX_EPOCH")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "inauguration-driver-{}-{}-{}",
                std::process::id(),
                unique,
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_compile_in_file_success() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("test_module.in");
        fs::write(&source_path, "fn main() -> Int { return 42; }").unwrap();

        let spec = ComponentSpec::host_executable("test_module", Some("main"));

        let result = compile_in_file(&source_path, &spec, false);
        assert!(result.is_ok(), "compile_in_file failed: {:?}", result.err());

        let (output_path, metadata_path) = result.unwrap();
        assert!(output_path.exists(), "Output artifact was not created");
        assert!(metadata_path.is_none(), "Metadata should not be emitted");

        let output_data = fs::read(&output_path).unwrap();
        assert!(!output_data.is_empty(), "Output artifact is empty");
    }

    #[test]
    fn test_compile_in_file_with_metadata() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("test_module_meta.in");
        fs::write(&source_path, "fn main() -> Int { return 42; }").unwrap();

        let spec = ComponentSpec::host_executable("test_module_meta", Some("main"));

        let result = compile_in_file(&source_path, &spec, true);
        assert!(result.is_ok(), "compile_in_file failed: {:?}", result.err());

        let (output_path, metadata_path) = result.unwrap();
        assert!(output_path.exists(), "Output artifact was not created");

        let md_path = metadata_path.expect("Metadata should be emitted");
        assert!(md_path.exists(), "Metadata file was not created");

        let md_data = fs::read_to_string(&md_path).unwrap();
        assert!(
            md_data.contains("test_module_meta"),
            "Metadata should contain the module name"
        );
    }

    #[test]
    fn test_compile_in_file_read_error() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("nonexistent.in");

        let spec = ComponentSpec::host_executable("nonexistent", Some("main"));
        let result = compile_in_file(&source_path, &spec, false);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("read source"));
    }

    #[test]
    fn test_compile_in_file_compile_error() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("invalid.in");
        fs::write(&source_path, "this is not valid syntax").unwrap();

        let spec = ComponentSpec::host_executable("invalid", Some("main"));

        let result = compile_in_file(&source_path, &spec, false);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("compilation"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before UNIX_EPOCH")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "inauguration-driver-{}-{}-{}",
                std::process::id(),
                unique,
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn test_compile_in_file_success() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("test_module.in");
        fs::write(&source_path, "fn main() -> Int { return 42; }").unwrap();

        let spec = ComponentSpec::host_executable("test_module", Some("main"));

        let result = compile_in_file(&source_path, &spec, false);
        assert!(result.is_ok(), "compile_in_file failed: {:?}", result.err());

        let (output_path, metadata_path) = result.unwrap();
        assert!(output_path.exists(), "Output artifact was not created");
        assert!(metadata_path.is_none(), "Metadata should not be emitted");

        let output_data = fs::read(&output_path).unwrap();
        assert!(!output_data.is_empty(), "Output artifact is empty");
    }

    #[test]
    fn test_compile_in_file_with_metadata() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("test_module_meta.in");
        fs::write(&source_path, "fn main() -> Int { return 42; }").unwrap();

        let spec = ComponentSpec::host_executable("test_module_meta", Some("main"));

        let result = compile_in_file(&source_path, &spec, true);
        assert!(result.is_ok(), "compile_in_file failed: {:?}", result.err());

        let (output_path, metadata_path) = result.unwrap();
        assert!(output_path.exists(), "Output artifact was not created");

        let md_path = metadata_path.expect("Metadata should be emitted");
        assert!(md_path.exists(), "Metadata file was not created");

        let md_data = fs::read_to_string(&md_path).unwrap();
        assert!(
            md_data.contains("test_module_meta"),
            "Metadata should contain the module name"
        );
    }

    #[test]
    fn test_compile_in_file_read_error() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("nonexistent.in");

        let spec = ComponentSpec::host_executable("nonexistent", Some("main"));
        let result = compile_in_file(&source_path, &spec, false);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("read source"));
    }

    #[test]
    fn test_compile_in_file_compile_error() {
        let temp = TempDirGuard::new();
        let source_path = temp.path.join("invalid.in");
        fs::write(&source_path, "this is not valid syntax").unwrap();

        let spec = ComponentSpec::host_executable("invalid", Some("main"));

        let result = compile_in_file(&source_path, &spec, false);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("compilation"));
    }
}
