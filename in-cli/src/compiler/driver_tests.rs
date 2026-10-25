use super::driver::{compile_in_file, spec_from_cli};
use super::metadata::ArtifactKind;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn get_temp_path(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("{}_{}.in", name, timestamp));
    path
}

#[test]
fn test_compile_in_file_success() {
    let source_path = get_temp_path("test_module");
    let source_code = "fn main() -> Int {\n    return 42;\n}\n";
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
    assert!(result.is_ok(), "compile failed: {:?}", result);

    let (out_path, meta_path) = result.unwrap();
    assert!(out_path.exists());
    assert!(meta_path.is_some());
    assert!(meta_path.as_ref().unwrap().exists());

    // cleanup
    let _ = fs::remove_file(&source_path);
    let _ = fs::remove_file(&out_path);
    if let Some(m) = meta_path {
        let _ = fs::remove_file(&m);
    }
}

#[test]
fn test_compile_in_file_missing_source() {
    let spec = spec_from_cli(
        "test_module",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::Executable,
        "main",
        0,
        false,
        false,
    );
    let result = compile_in_file(
        &PathBuf::from("does_not_exist_in_compiler_driver_tests.in"),
        &spec,
        false,
    );
    assert!(result.is_err());
    let err_msg = result.unwrap_err();
    assert!(err_msg.contains("read source:"));
}

#[test]
fn test_compile_in_file_invalid_code() {
    let source_path = get_temp_path("invalid_module");
    fs::write(&source_path, "fn main() { // missing return type and body").unwrap();

    let spec = spec_from_cli(
        "invalid",
        "x86_64-unknown-linux-gnu",
        ArtifactKind::Executable,
        "main",
        0,
        false,
        false,
    );

    let result = compile_in_file(&source_path, &spec, false);
    assert!(result.is_err());
    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("compilation:"),
        "expected compilation error, got: {}",
        err_msg
    );

    // cleanup
    let _ = fs::remove_file(&source_path);
}
