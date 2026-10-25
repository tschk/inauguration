use crate::compiler::driver::*;
use crate::compiler::metadata::ArtifactKind;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_compile_in_file_success() {
    let dir = tempdir().unwrap();
    let source_path = dir.path().join("test_module.in");
    let source_code = "
package test.pkg;
module test.pkg.module;

fn main() -> void {
    return;
}
";
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
    assert!(result.is_ok());
    let (output_path, metadata_path) = result.unwrap();
    assert!(output_path.exists());
    assert!(metadata_path.is_some());
    assert!(metadata_path.unwrap().exists());
}

#[test]
fn test_compile_in_file_error_read_source() {
    let dir = tempdir().unwrap();
    let source_path = dir.path().join("non_existent.in");

    let spec = spec_from_cli(
        "test_module",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::StaticLibrary,
        "main",
        0,
        false,
        false,
    );

    let result = compile_in_file(&source_path, &spec, true);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("read source"));
}

#[test]
fn test_compile_in_file_error_compilation() {
    let dir = tempdir().unwrap();
    let source_path = dir.path().join("test_module_invalid.in");
    let source_code = "invalid syntax!!";
    fs::write(&source_path, source_code).unwrap();

    let spec = spec_from_cli(
        "test_module",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::StaticLibrary,
        "main",
        0,
        false,
        false,
    );

    let result = compile_in_file(&source_path, &spec, true);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("compilation"));
}

#[test]
fn test_emit_component_metadata_success() {
    let dir = tempdir().unwrap();
    let source_path = dir.path().join("test_module.in");
    let source_code = "
package test.pkg;
module test.pkg.module;

fn main() -> void {
    return;
}
";
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

    let result = emit_component_metadata(&source_path, &spec);
    assert!(result.is_ok());
    let metadata_path = result.unwrap();
    assert!(metadata_path.exists());
}

#[test]
fn test_emit_component_metadata_error_read_source() {
    let dir = tempdir().unwrap();
    let source_path = dir.path().join("non_existent.in");

    let spec = spec_from_cli(
        "test_module",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::StaticLibrary,
        "main",
        0,
        false,
        false,
    );

    let result = emit_component_metadata(&source_path, &spec);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("read source"));
}

#[test]
fn test_emit_component_metadata_error_parse() {
    let dir = tempdir().unwrap();
    let source_path = dir.path().join("test_module_invalid.in");
    let source_code = "invalid syntax!!";
    fs::write(&source_path, source_code).unwrap();

    let spec = spec_from_cli(
        "test_module",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::StaticLibrary,
        "main",
        0,
        false,
        false,
    );

    let result = emit_component_metadata(&source_path, &spec);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("parse"));
}
