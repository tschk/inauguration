use std::fs;

use crate::compiler::driver::{compile_in_file, spec_from_cli};
use crate::compiler::metadata::ArtifactKind;

#[test]
fn test_compile_in_file_success() {
    let dir = std::env::temp_dir().join("in_compile_test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let source_path = dir.join("test.in");
    fs::write(&source_path, "fn main() -> Int { return 42 }").unwrap();

    let spec = spec_from_cli(
        "test",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::Executable,
        "main",
        0,
        false,
        false,
    );

    let result = compile_in_file(&source_path, &spec, true);
    if let Err(e) = &result {
        println!("Error compiling: {}", e);
    }
    assert!(result.is_ok(), "compile_in_file should succeed");

    let (output_path, metadata_path) = result.unwrap();
    assert!(output_path.exists(), "output path should exist");
    assert!(metadata_path.is_some(), "metadata path should be some");
    assert!(
        metadata_path.unwrap().exists(),
        "metadata path should exist"
    );

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_compile_in_file_missing_source() {
    let dir = std::env::temp_dir().join("in_compile_test_missing");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let source_path = dir.join("missing.in");

    let spec = spec_from_cli(
        "test",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::Executable,
        "main",
        0,
        false,
        false,
    );

    let result = compile_in_file(&source_path, &spec, true);
    assert!(
        result.is_err(),
        "compile_in_file should fail for missing source"
    );
    assert!(result.unwrap_err().contains("read source"));

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_compile_in_file_invalid_syntax() {
    let dir = std::env::temp_dir().join("in_compile_test_invalid");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let source_path = dir.join("invalid.in");
    fs::write(&source_path, "fn main() -> Int { return ").unwrap();

    let spec = spec_from_cli(
        "test",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::Executable,
        "main",
        0,
        false,
        false,
    );

    let result = compile_in_file(&source_path, &spec, true);
    assert!(
        result.is_err(),
        "compile_in_file should fail for invalid syntax"
    );
    let err = result.unwrap_err();
    assert!(err.contains("compilation") || err.contains("parse"));

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_compile_in_file_without_metadata() {
    let dir = std::env::temp_dir().join("in_compile_test_no_meta");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let source_path = dir.join("test.in");
    fs::write(&source_path, "fn main() -> Int { return 42 }").unwrap();

    let spec = spec_from_cli(
        "test",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::Executable,
        "main",
        0,
        false,
        false,
    );

    let result = compile_in_file(&source_path, &spec, false);
    assert!(result.is_ok(), "compile_in_file should succeed");

    let (output_path, metadata_path) = result.unwrap();
    assert!(output_path.exists(), "output path should exist");
    assert!(metadata_path.is_none(), "metadata path should be none");

    fs::remove_dir_all(&dir).unwrap();
}
