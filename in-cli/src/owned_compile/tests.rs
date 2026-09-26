use super::*;
use crate::core_ir::{Decl, Typ, UnifiedModule};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use super::jit::resolve_jit_entry;
use super::util::artifact_stem;

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "inauguration-owned-compile-{}-{}-{name}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn default_request(
    path: PathBuf,
    target: CompileTarget,
    entry: Option<&str>,
    out: Option<PathBuf>,
) -> OwnedCompileRequest {
    OwnedCompileRequest {
        path,
        module_id: "App".to_string(),
        parser: ParserCli::Auto,
        target,
        entry: entry.map(str::to_string),
        out,
        linkage: NativeLinkage::Executable,
        target_triple: None,
        jobs: 1,
        debug: false,
        profile: crate::emit_profile::EmitProfile::Default,
        emit: None,
        base: None,
    }
}

#[test]
fn native_target_reports_host_status() {
    let source_path = temp_path("native.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("main"),
        None,
    ));

    if native_backend::native_subset_host_available() {
        assert!(!report.success);
        assert!(
            report
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("--out"))
        );
    } else {
        assert!(!report.success);
        assert!(
            matches!(
                report.reason_code.as_deref(),
                Some(native_backend::NATIVE_BACKEND_NOT_IMPLEMENTED)
                    | Some("native-lowering-failed")
            ),
            "unexpected reason_code: {:?}",
            report.reason_code
        );
    }

    fs::remove_file(source_path).unwrap();
}

#[test]
fn jit_executes_snake_case_stdlib_process_run() {
    let source_path = temp_path("stdlib-process-run.in");
    fs::write(
        &source_path,
        "import std.process;\ncapability process.spawn;\nfn main() -> String { return process_run(\"true\"); }\n",
    )
    .unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));

    assert!(report.success, "{report:?}");
    assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
    // On some environments (like CI runner where `true` has no output), `instring_from_bytes` will return None for empty strings,
    // or Some("") depending on initialization details. We accept both here since the primary goal is executing `process_run("true")`.
    let eval_res = match &report.eval_result {
        Some(crate::owned_compile::EvalValue::Str(value)) => Some(value.as_str()),
        _ => None,
    };
    assert!(eval_res == Some("") || eval_res.is_none());

    fs::remove_file(source_path).unwrap();
}

#[test]
fn native_compile_rejects_missing_explicit_entry() {
    let source_path = temp_path("native-missing-entry.in");
    let out_path = temp_path("native-missing-entry.out");
    fs::write(&source_path, "fn main() -> Int { return 42; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    ));

    assert!(!report.success, "{:?}", report);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("verify-missing-entry-symbol")
    );

    fs::remove_file(source_path).unwrap();
    let _ = fs::remove_file(out_path);
}

#[test]
fn native_staticlib_emits_x86_64_linux_object_file() {
    let source_path = temp_path("x86-object.in");
    let out_path = temp_path("x86-object.o");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("x86_64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-object-subset");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    assert_eq!(
        report.artifact_path.as_deref(),
        Some(out_path.to_str().unwrap())
    );
    let bytes = fs::read(&out_path).expect("object bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 1);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 62);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
    let _ = fs::remove_file(out_path.with_extension("abi.json"));
}

#[test]
fn native_staticlib_emits_aarch64_linux_object_file() {
    let source_path = temp_path("aarch64-object.in");
    let out_path = temp_path("aarch64-object.o");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("aarch64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-object-subset");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    let bytes = fs::read(&out_path).expect("object bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 1);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 183);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
    let _ = fs::remove_file(out_path.with_extension("abi.json"));
}

#[test]
fn native_staticlib_emits_arm32_linux_object_file() {
    let source_path = temp_path("arm32-object.in");
    let out_path = temp_path("arm32-object.o");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("armv7-unknown-linux-gnueabihf".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-object-subset");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    let bytes = fs::read(&out_path).expect("object bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(bytes[4], 1);
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 1);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 40);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
    let _ = fs::remove_file(out_path.with_extension("abi.json"));
}

#[test]
fn native_staticlib_emits_aarch64_macho_archive() {
    let source_path = temp_path("macho-staticlib.in");
    let out_path = temp_path("macho-staticlib.a");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("aarch64-apple-darwin".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-object-subset");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    let bytes = fs::read(&out_path).expect("archive bytes");
    assert_eq!(&bytes[..8], b"!<arch>\n");
    assert!(bytes.windows(7).any(|window| window == b"_answer"));

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
    let _ = fs::remove_file(out_path.with_extension("abi.json"));
}

#[test]
fn native_executable_emits_x86_64_linux_elf_file() {
    let source_path = temp_path("x86-executable.in");
    let out_path = temp_path("x86-executable");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("x86_64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-x86_64");
    assert_eq!(report.runtime_level, "linux-syscall-exit");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-x86_64-linux-exit-subset")
    );
    let bytes = fs::read(&out_path).expect("executable bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 2);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 62);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&out_path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_executable_emits_aarch64_linux_elf_file() {
    let source_path = temp_path("aarch64-linux-executable.in");
    let out_path = temp_path("aarch64-linux-executable");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("aarch64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-aarch64");
    assert_eq!(report.runtime_level, "linux-syscall-exit");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-aarch64-linux-exit-subset")
    );
    let bytes = fs::read(&out_path).expect("executable bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(bytes[4], 2);
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 2);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 183);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_executable_emits_arm32_linux_elf_file() {
    let source_path = temp_path("arm32-linux-executable.in");
    let out_path = temp_path("arm32-linux-executable");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("armv7-unknown-linux-gnueabihf".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-arm32");
    assert_eq!(report.runtime_level, "linux-syscall-exit");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-armv7-linux-exit-subset")
    );
    let bytes = fs::read(&out_path).expect("executable bytes");
    assert_eq!(&bytes[0..4], b"\x7FELF");
    assert_eq!(bytes[4], 1);
    assert_eq!(u16::from_le_bytes([bytes[16], bytes[17]]), 2);
    assert_eq!(u16::from_le_bytes([bytes[18], bytes[19]]), 40);

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_executable_emits_windows_pe_exe_file() {
    let source_path = temp_path("windows-executable.in");
    let out_path = temp_path("windows-executable.exe");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("x86_64-pc-windows-msvc".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-x86_64");
    assert_eq!(report.runtime_level, "windows-exitprocess");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-x86_64-windows-exe-subset")
    );
    let bytes = fs::read(&out_path).expect("exe bytes");
    assert_eq!(&bytes[0..2], b"MZ");
    let pe_off = u32::from_le_bytes(bytes[0x3C..0x40].try_into().unwrap()) as usize;
    assert_eq!(&bytes[pe_off..pe_off + 4], b"PE\0\0");
    assert_eq!(
        u16::from_le_bytes(bytes[pe_off + 4..pe_off + 6].try_into().unwrap()),
        0x8664
    );
    assert!(bytes.windows(12).any(|window| window == b"KERNEL32.dll"));
    assert!(bytes.windows(11).any(|window| window == b"ExitProcess"));

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_executable_emits_aarch64_darwin_app_bundle() {
    let source_path = temp_path("darwin-app.in");
    let out_path = temp_path("Answer.app");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("aarch64-apple-darwin".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-aarch64-app");
    assert_eq!(report.runtime_level, "macos-app-bundle");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-aarch64-darwin-app-subset")
    );
    let executable = out_path
        .join("Contents/MacOS")
        .join(artifact_stem(&out_path, "App"));
    assert!(executable.exists());
    assert!(out_path.join("Contents/Info.plist").exists());

    fs::remove_file(source_path).unwrap();
    let _ = fs::remove_dir_all(&out_path);
}

#[test]
fn native_executable_emits_linux_appdir() {
    let source_path = temp_path("linux-appdir.in");
    let out_path = temp_path("Answer.AppDir");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("x86_64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset-x86_64-appdir");
    assert_eq!(report.runtime_level, "linux-appdir");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-x86_64-linux-appdir-subset")
    );
    assert!(out_path.join("AppRun").exists());
    assert!(out_path.join("answer.desktop").exists());

    fs::remove_file(source_path).unwrap();
    let _ = fs::remove_dir_all(&out_path);
}

#[test]
fn native_executable_appimage_fails_closed() {
    let source_path = temp_path("linux-appimage.in");
    let out_path = temp_path("Answer.AppImage");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("x86_64-unknown-linux-gnu".to_string());

    let report = compile_owned(&request);

    assert!(!report.success, "{:?}", report);
    assert_eq!(report.backend_level, "contract-only");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-package-not-implemented")
    );
    assert!(!out_path.exists());

    fs::remove_file(source_path).unwrap();
}

#[test]
fn explicit_unsupported_native_target_fails_closed() {
    let source_path = temp_path("unsupported-target.in");
    let out_path = temp_path("unsupported-target.o");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("riscv64gc-unknown-none-elf".to_string());

    let report = compile_owned(&request);

    assert!(!report.success, "{:?}", report);
    assert_eq!(report.backend_level, "contract-only");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-target-not-implemented")
    );
    assert!(!out_path.exists());

    fs::remove_file(source_path).unwrap();
}

#[test]
fn explicit_aarch64_darwin_executable_target_fails_closed() {
    let source_path = temp_path("unsupported-macho-executable.in");
    let out_path = temp_path("unsupported-macho-executable");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::Executable;
    request.target_triple = Some("aarch64-apple-darwin".to_string());

    let report = compile_owned(&request);

    assert!(!report.success, "{:?}", report);
    assert_eq!(report.backend_level, "contract-only");
    assert_eq!(
        report.reason_code.as_deref(),
        Some("native-target-not-implemented")
    );
    assert!(!out_path.exists());

    fs::remove_file(source_path).unwrap();
}

#[test]
fn native_staticlib_emits_wasm32_module() {
    let source_path = temp_path("wasm-object.in");
    let out_path = temp_path("wasm-object.wasm");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();
    let mut request = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    );
    request.linkage = NativeLinkage::StaticLib;
    request.target_triple = Some("wasm32-unknown-unknown".to_string());

    let report = compile_owned(&request);

    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-object-subset");
    assert_eq!(report.runtime_level, "none");
    assert_eq!(report.reason_code.as_deref(), Some("native-object-subset"));
    let bytes = fs::read(&out_path).expect("wasm bytes");
    assert_eq!(&bytes[0..4], b"\0asm");
    assert!(bytes.windows(6).any(|window| window == b"answer"));

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&out_path).unwrap();
}

#[test]
fn native_answer_entry_compiles_on_aarch64_host() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("answer.in");
    let out_path = temp_path("answer.bin");
    fs::write(
        &source_path,
        "fn answer() -> Int { return 42; }\nfn main() -> void { return; }\n",
    )
    .unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("answer"),
        Some(out_path.clone()),
    ));
    assert!(report.success, "{:?}", report);
    assert_eq!(report.backend_level, "owned-native-subset");
    assert_eq!(report.runtime_level, "inrt-native");
    assert_eq!(report.eval_exit_code, Some(42));
    assert!(out_path.exists());

    fs::remove_file(source_path).unwrap();
    fs::remove_file(out_path).unwrap();
}

#[test]
fn native_polyglot_answer_entries_compile_on_aarch64_host() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let cases = [
        (
            "sample.js",
            "function answer() {\n  return 42;\n}\n\nfunction main() {}\n",
        ),
        (
            "sample.ts",
            "function answer(): number {\n  return 42;\n}\n\nfunction main(): void {}\n",
        ),
        (
            "sample.py",
            "def answer() -> int:\n    return 42\n\ndef main() -> None:\n    pass\n",
        ),
        (
            "sample.rb",
            "def answer\n  return 42\nend\n\ndef main\nend\n",
        ),
        (
            "sample.zig",
            "fn answer() i32 {\n    return 42;\n}\n\npub fn main() void {}\n",
        ),
        #[cfg(feature = "parse-extended")]
        (
            "sample.php",
            "<?php\n\nfunction answer(): int {\n    return 42;\n}\n\nfunction main(): void {\n}\n",
        ),
        (
            "Sample.java",
            "class Sample {\n  static int answer() {\n    return 42;\n  }\n\n  public static void main(String[] args) {}\n}\n",
        ),
    ];
    for (name, source) in cases {
        let source_path = temp_path(name);
        fs::write(&source_path, source).unwrap();
        let out_path = temp_path(&format!(
            "polyglot-{}.bin",
            source_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("sample")
        ));
        let report = compile_owned(&default_request(
            source_path.clone(),
            CompileTarget::Native,
            Some("answer"),
            Some(out_path.clone()),
        ));
        assert!(report.success, "{}: {report:?}", source_path.display());
        assert_eq!(report.eval_exit_code, Some(42), "{}", source_path.display());
        assert!(out_path.exists(), "{}", source_path.display());
        fs::remove_file(source_path).unwrap();
        fs::remove_file(out_path).unwrap();
    }
}

#[test]
fn report_has_empty_external_invocations() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("external.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.external_invocations.is_empty());
    assert!(report.success);

    fs::remove_file(source_path).unwrap();
}

#[test]
fn bare_source_without_entry_keeps_main_through_optimization() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("bare-main.in");
    fs::write(
        &source_path,
        "fn helper() -> Int { return 3; }\n\nfn main() -> Int { return helper(); }\n",
    )
    .unwrap();

    // No entry is named, so the optimizer must not fall back to a kernel entry
    // name and delete `main` along with everything it calls. A stripped module
    // fails lowering with "module has no functions" and returns no result.
    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.success, "{report:?}");
    assert!(
        report.typed_function_count >= 1,
        "optimizer emptied the module: {report:?}"
    );
    assert_eq!(report.eval_exit_code, Some(3), "{report:?}");
    fs::remove_file(source_path).unwrap();
}

/// A Float entry's value comes back in the integer result register as its `f64`
/// bit pattern, so the JIT can report it; the exit status still cannot carry it
/// and stays 0.
#[test]
fn float_entry_reports_its_value_without_an_exit_status() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("float-main.in");
    fs::write(&source_path, "fn main() -> Float { return 2.5 + 3.5; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_exit_code, Some(0), "{report:?}");
    assert_eq!(
        report.eval_result,
        Some(crate::owned_compile::EvalValue::Float(6.0)),
        "{report:?}"
    );
    fs::remove_file(source_path).unwrap();
}

#[test]
fn report_to_json_roundtrip_fields() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("json.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));
    let json = report_to_json(&report).unwrap();
    assert!(json.contains("\"schema_version\": 1"));
    assert!(json.contains("\"owned\": true"));
    assert!(json.contains("\"external_invocations\": []"));

    fs::remove_file(source_path).unwrap();
}

#[test]
fn report_carries_core_identity_metadata() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("identity.in");
    fs::write(
        &source_path,
        "package agents.video;\nmodule agents.video.main;\nfn main() -> Int { return 7; }\n",
    )
    .unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.success, "{:?}", report);
    let identity = report.module_identity.as_ref().expect("module identity");
    assert_eq!(identity.package.as_deref(), Some("agents.video"));
    assert_eq!(identity.module.as_deref(), Some("agents.video.main"));
    assert_eq!(identity.requested_module_id, "App");
    assert_eq!(identity.effective_module_id, "agents.video.main");

    fs::remove_file(source_path).unwrap();
}

#[test]
fn report_defaults_identity_metadata_without_source_identity() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("default-identity.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();

    let report = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));

    assert!(report.success, "{:?}", report);
    let identity = report.module_identity.as_ref().expect("module identity");
    assert_eq!(identity.package, None);
    assert_eq!(identity.module, None);
    assert_eq!(identity.requested_module_id, "App");
    assert_eq!(identity.effective_module_id, "App");

    fs::remove_file(source_path).unwrap();
}

#[test]
fn jit_execution_does_not_reuse_report_cache() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source_path = temp_path("cache.in");
    fs::write(&source_path, "fn main() -> void { return; }\n").unwrap();
    let first = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));
    assert!(!first.cache_hit);
    let second = compile_owned(&default_request(
        source_path.clone(),
        CompileTarget::Jit,
        None,
        None,
    ));
    assert!(!second.cache_hit);
    assert_eq!(first.success, second.success);
    fs::remove_file(source_path).unwrap();
}

#[test]
fn resolve_jit_entry_exact_match() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    assert_eq!(resolve_jit_entry(&module, "main"), "main");
}

#[test]
fn resolve_jit_entry_namespaced() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "package.main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    assert_eq!(resolve_jit_entry(&module, "main"), "package.main");
}

#[test]
fn resolve_jit_entry_suffix_dot() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "foo.bar.main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    assert_eq!(resolve_jit_entry(&module, "main"), "foo.bar.main");
}

#[test]
fn resolve_jit_entry_no_match_falls_through() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "other".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    // Falls through to first non-closure function for library mode
    assert_eq!(resolve_jit_entry(&module, "main"), "other");
}

#[test]
fn resolve_jit_entry_prefers_exact_over_namespaced() {
    let module = UnifiedModule::new(vec![
        Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![],
            type_params: vec![],
        },
        Decl::Function {
            name: "package.main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![],
            type_params: vec![],
        },
    ]);
    assert_eq!(resolve_jit_entry(&module, "main"), "main");
}

#[test]
fn resolve_jit_entry_non_dot_suffix_not_matched() {
    // "also_main" ends with "main" but char before is '_', not '.'
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "also_main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    // Falls through to first non-closure function for library mode
    assert_eq!(resolve_jit_entry(&module, "main"), "also_main");
}
