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
fn jit_executes_stdlib_process_run_with_string_result() {
    for (imports, name) in [
        ("", "process-run"),
        ("", "process_run"),
        (
            "import std.process;\ncapability process.spawn;\n",
            "process-run",
        ),
        (
            "import std.process;\ncapability process.spawn;\n",
            "process_run",
        ),
    ] {
        let source_path = temp_path("stdlib-process-run.in");
        fs::write(
            &source_path,
            format!("{imports}fn main() -> String {{ return {name}(\"printf hello\"); }}\n"),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source_path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(source_path).unwrap();
        if !crate::native_emit::lower::host_supports_native_subset() {
            assert!(!report.success, "{report:?}");
            assert!(
                report
                    .error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("jit-host-unsupported:")),
                "{report:?}"
            );
            continue;
        }
        assert!(report.success, "{imports}/{name}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(EvalValue::Str("hello".into())));
    }
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
fn dual_emit_writes_runtime_and_harden_artifacts() {
    let source_path = temp_path("dual-emit.in");
    let runtime_out = temp_path("dual-runtime.o");
    let harden_out = temp_path("dual-harden.o");
    let sample = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/compile/antidecomp_sample.in");
    let source = fs::read_to_string(&sample).unwrap_or_else(|_| {
        concat!(
            "fn mix(a: Int, b: Int) -> Int {\n",
            "  let s: Int = a + b;\n",
            "  let t: Int = s * 3;\n",
            "  return t - a;\n",
            "}\n",
            "fn gate(n: Int) -> Int {\n",
            "  if n > 0 {\n",
            "    return mix(n, 2);\n",
            "  } else {\n",
            "    return mix(1, n);\n",
            "  }\n",
            "}\n",
            "fn main() -> Int {\n",
            "  return gate(7);\n",
            "}\n",
        )
        .to_string()
    });
    fs::write(&source_path, source).unwrap();

    let mut runtime_req = default_request(
        source_path.clone(),
        CompileTarget::Native,
        Some("main"),
        Some(runtime_out.clone()),
    );
    runtime_req.linkage = NativeLinkage::StaticLib;
    runtime_req.target_triple = Some("x86_64-unknown-none".to_string());
    runtime_req.profile = crate::emit_profile::EmitProfile::Default;

    let mut harden_req = runtime_req.clone();
    harden_req.out = Some(harden_out.clone());
    harden_req.profile = crate::emit_profile::EmitProfile::Harden;

    let runtime_report = compile_owned(&runtime_req);
    let harden_report = compile_owned(&harden_req);

    if !runtime_report.success || !harden_report.success {
        let msg = format!("{runtime_report:?} / {harden_report:?}");
        if msg.contains("not implemented")
            || msg.contains("NATIVE_BACKEND")
            || msg.contains("native-lowering-failed")
        {
            eprintln!("skip dual_emit_writes_runtime_and_harden_artifacts: {msg}");
            let _ = fs::remove_file(&source_path);
            let _ = fs::remove_file(&runtime_out);
            let _ = fs::remove_file(&harden_out);
            return;
        }
        panic!("dual-emit owned compile failed: {msg}");
    }

    assert!(runtime_out.is_file(), "runtime artifact missing");
    assert!(harden_out.is_file(), "harden artifact missing");
    let runtime_bytes = fs::read(&runtime_out).expect("read runtime");
    let harden_bytes = fs::read(&harden_out).expect("read harden");
    assert_ne!(
        runtime_bytes, harden_bytes,
        "runtime and harden artifacts must differ"
    );
    let has_hashed = |bytes: &[u8]| {
        bytes
            .windows(3)
            .any(|w| w[0] == b'_' && w[1] == b'H' && w[2].is_ascii_hexdigit())
    };
    assert!(
        !has_hashed(&runtime_bytes),
        "runtime artifact must not contain _H hashed symbols"
    );
    assert!(
        has_hashed(&harden_bytes),
        "harden artifact should contain _H hashed symbols"
    );
    assert_eq!(
        &harden_bytes[..4.min(harden_bytes.len())],
        b"\x7fELF",
        "harden respects the requested ELF object target"
    );

    assert_eq!(
        u16::from_le_bytes(harden_bytes[16..18].try_into().unwrap()),
        1,
        "harden must preserve static-library linkage"
    );
    assert_eq!(harden_report.runtime_level, "freestanding-none");

    fs::remove_file(source_path).unwrap();
    fs::remove_file(&runtime_out).unwrap();
    fs::remove_file(&harden_out).unwrap();
    let _ = fs::remove_file(runtime_out.with_extension("abi.json"));
    let _ = fs::remove_file(harden_out.with_extension("abi.json"));
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
    assert_eq!(report.backend_level, "owned-native-subset-aarch64-static");
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
    assert_eq!(report.backend_level, "owned-native-subset-aarch64-static");
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

#[test]
fn compilation_restores_the_callers_emission_profile() {
    use crate::emit_profile::EmitProfile;
    use crate::native_emit::antidecomp;
    let module = crate::in_lang_parse::parse_in_source(
        "fn add(a: Int, b: Int) -> Int { return a + b; }\nfn main() -> Int { return add(5, 37); }",
    )
    .unwrap();
    let previous = antidecomp::current_profile();
    antidecomp::set_profile(EmitProfile::Lean);
    let request = default_request(
        PathBuf::from("profile.in"),
        CompileTarget::Native,
        Some("main"),
        None,
    );
    // Missing output fails without executing the entry.
    assert!(super::native::compile_native(&module, "App", &request).is_err());
    let actual = antidecomp::current_profile();
    antidecomp::set_profile(previous);
    assert_eq!(actual, EmitProfile::Lean);
    if crate::native_emit::lower::host_supports_native_subset() {
        for entry in ["main", "absent"] {
            antidecomp::set_profile(EmitProfile::Lean);
            let request = default_request(
                PathBuf::from("profile.in"),
                CompileTarget::Jit,
                Some(entry),
                None,
            );
            let target_module = if entry == "main" {
                module.clone()
            } else {
                UnifiedModule::new(Vec::new())
            };
            let result = super::jit::compile_jit(&target_module, "App", &request);
            let actual = antidecomp::current_profile();
            antidecomp::set_profile(previous);
            assert_eq!(result.is_ok(), entry == "main");
            if entry == "main" {
                assert_eq!(result.unwrap().eval_exit_code, Some(42));
            }
            assert_eq!(actual, EmitProfile::Lean, "{entry}");
        }
    }
}

#[test]
fn cross_target_exit_stubs_reject_observable_entry_bodies() {
    let module = crate::in_lang_parse::parse_in_source(
        "fn main() -> Int { print(\"runtime-only\"); return 42; }",
    )
    .unwrap();
    for target in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "armv7-unknown-linux-gnueabihf",
        "x86_64-pc-windows-msvc",
    ] {
        let out = temp_path("side-effect-native");
        let mut request = default_request(
            PathBuf::from("effects.in"),
            CompileTarget::Native,
            Some("main"),
            Some(out.clone()),
        );
        request.target_triple = Some(target.into());
        let result = super::native::compile_native(&module, "App", &request);
        assert!(
            matches!(result, Err(ref error) if error.contains("pure scalar exit entry")),
            "{target}"
        );
        assert!(!out.exists(), "{target} wrote an incomplete artifact");
    }
}

#[test]
fn resolve_jit_entry_empty_name_uses_library_fallback() {
    let module = UnifiedModule::new(vec![Decl::Function {
        name: "answer".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![],
        type_params: vec![],
    }]);
    assert_eq!(resolve_jit_entry(&module, ""), "answer");
    assert_eq!(resolve_jit_entry(&UnifiedModule::new(vec![]), ""), "");
}

#[test]
fn native_backends_reject_aggregate_catches() {
    for pattern in ["[value]", "(value, other)"] {
        let mut module = crate::in_lang_parse::parse_in_source(
            "fn main() -> Int { try { throw 42; } catch error { return 1; } return 2; }",
        )
        .unwrap();
        if let Decl::Function { body, .. } = &mut module.decls[0]
            && let crate::core_ir::Stmt::Try { catches, .. } = &mut body[0]
        {
            catches[0].pattern = pattern.into();
        }
        for result in [
            crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
                .map(|_| ()),
            crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
        ] {
            let error = result.expect_err("unsupported catch must not run unconditionally");
            assert!(error.contains("unsupported catch pattern"), "{error}");
        }
    }
}

#[test]
fn native_matches_reject_unsupported_and_incompatible_patterns() {
    for body in [
        "match 5 { [value] { return 1; } _ { return 2; } } return 3;",
        "match false { 0 { return 1; } _ { return 2; } } return 3;",
    ] {
        let mut module =
            crate::in_lang_parse::parse_in_source(&format!("fn main() -> Int {{ {body} }}"))
                .unwrap();
        for optimized in [false, true] {
            if optimized {
                crate::core_opt::optimize_with_profile(
                    &mut module.decls,
                    Some("main"),
                    crate::emit_profile::EmitProfile::Default,
                );
            }
            for result in [
                crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
                    .map(|_| ()),
                crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
            ] {
                let error = result.unwrap_err();
                assert!(
                    error.contains("unsupported match pattern")
                        || error.contains("compatible scrutinee"),
                    "{error}"
                );
            }
        }
    }
}

#[test]
fn native_string_patterns_reject_non_string_scrutinees() {
    let module = crate::in_lang_parse::parse_in_source(
        "fn main() -> Int { match 123 { \"text\" { return 1; } _ { return 2; } } return 3; }",
    )
    .unwrap();
    for result in [
        crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
            .map(|_| ()),
        crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
    ] {
        assert!(
            result
                .unwrap_err()
                .contains("string pattern requires a String scrutinee")
        );
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn substring_builtin_copies_bytes_and_checks_ranges() {
    for (text, start, count, expected) in [
        ("abcdefghijk", 1, 9, "bcdefghij"),
        ("éabc", 2, 2, "ab"),
        ("abc", 3, 0, ""),
        ("abc", 0, 3, "abc"),
        ("abc", -1, 1, ""),
        ("abc", 0, -1, ""),
        ("abc", 4, 0, ""),
        ("abc", 2, 2, ""),
        ("abc", i64::MAX, 1, ""),
        ("abc", 1, i64::MAX, ""),
    ] {
        let source = temp_path("substring-range.in");
        fs::write(
            &source,
            format!(
                "fn main() -> String {{ return __inrt_str_substr({text:?}, {start}, {count}); }}"
            ),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(source).unwrap();
        assert!(report.success, "{text:?}/{start}/{count}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Str(expected.into())),
            "{text:?}/{start}/{count}"
        );
    }
    let null_source = temp_path("substring-null.in");
    fs::write(
        &null_source,
        "fn main() -> String { return __inrt_str_substr(0, 0, 0); }",
    )
    .unwrap();
    let report = compile_owned(&default_request(
        null_source.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(null_source).unwrap();
    assert!(!report.success, "{report:?}");
    assert_eq!(report.reason_code.as_deref(), Some("verify-type-mismatch"));
    let source = temp_path("substring-native.in");
    let binary = temp_path("substring-native");
    fs::write(&source, "fn main() -> Int { let value = __inrt_str_substr(\"abcdefghijk\", 1, 9); if __inrt_str_eq(value, \"bcdefghij\") { return 42; } return 1; }").unwrap();
    let report = compile_owned(&default_request(
        source.clone(),
        CompileTarget::Native,
        Some("main"),
        Some(binary.clone()),
    ));
    assert!(report.success, "{report:?}");
    assert_eq!(
        std::process::Command::new(&binary).status().unwrap().code(),
        Some(42)
    );
    fs::remove_file(source).unwrap();
    fs::remove_file(binary).unwrap();
}

#[test]
fn owned_jit_uses_runtime_builtin_result_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (typ, expression, expected) in [
        ("String", "\"\"", EvalValue::Str(String::new())),
        ("String", "\"é🙂\"", EvalValue::Str("é🙂".into())),
        (
            "String",
            "path_join(\"/tmp\", \"file\")",
            EvalValue::Str("/tmp/file".into()),
        ),
        (
            "String",
            "__inrt_str_concat(\"a\", \"b\")",
            EvalValue::Str("ab".into()),
        ),
        (
            "String",
            "__inrt_str_substr(\"abcd\", 1, 2)",
            EvalValue::Str("bc".into()),
        ),
        ("Bool", "__inrt_str_eq(\"a\", \"a\")", EvalValue::Bool(true)),
        (
            "Bool",
            "__inrt_str_contains(\"abcd\", \"bc\")",
            EvalValue::Bool(true),
        ),
    ] {
        let source = temp_path("builtin-result.in");
        fs::write(
            &source,
            format!("fn main() -> {typ} {{ return {expression}; }}"),
        )
        .unwrap();
        for out in [None, Some(temp_path("builtin-result.jit"))] {
            let report = compile_owned(&default_request(
                source.clone(),
                CompileTarget::Jit,
                Some("main"),
                out,
            ));
            assert!(report.success, "{expression}: {report:?}");
            assert_eq!(report.eval_result, Some(expected.clone()), "{expression}");
        }
        fs::remove_file(source).unwrap();
    }
}

#[test]
fn owned_jit_preserves_caught_scalar_payload_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (typ, value, expected) in [
        ("Int", "7", EvalValue::Int(7)),
        ("Bool", "true", EvalValue::Bool(true)),
        ("Float", "3.75", EvalValue::Float(3.75)),
        ("String", "\"value\"", EvalValue::Str("value".into())),
    ] {
        let source = temp_path("catch-payload.in");
        fs::write(
            &source,
            format!(
                "fn main() -> {typ} {{ try {{ throw {value}; }} catch error {{ return error; }} }}"
            ),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(source).unwrap();
        assert!(report.success, "{typ}: {report:?}");
        assert_eq!(report.eval_result, Some(expected), "{typ}");
    }
}

#[test]
fn owned_jit_rejects_constant_writes_in_exception_bodies() {
    for body in [
        "try { count = 7; } catch error { return 9; } return count;",
        "try { throw 42; } catch error { count = 7; } return count;",
    ] {
        let source = temp_path("try-constant.in");
        fs::write(
            &source,
            format!("const count: Int = 0;\nfn main() -> Int {{ {body} }}"),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            source.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(source).unwrap();
        assert!(!report.success, "{body}: {report:?}");
        assert!(
            report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("cannot assign to constant global")),
            "{body}: {report:?}"
        );
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_jit_preserves_error_type_through_calls_and_rethrows() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for body in [
            "try { fail(); } catch 0 { return 1; } catch false { return 2; } return 3;",
            "try { try { fail(); } catch value { let alias = value; throw alias; } } catch 0 { return 1; } catch false { return 2; } return 3;",
            "try { throw 0 == 1; } catch 0 { return 1; } catch false { return 2; } return 3;",
        ] {
            let source = temp_path("typed-error.in");
            fs::write(
                &source,
                format!(
                    "fn fail() -> Int {{ throw false; return 0; }}\nfn main() -> Int {{ {body} }}"
                ),
            )
            .unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(report.success, "{profile:?}: {body}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(2)),
                "{profile:?}: {body}"
            );
        }
    }
}

#[test]
fn owned_jit_selects_catches_and_reports_unmatched_errors() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (catches, handled) in [
            ("catch 41 { return 1; } catch 42 { return 2; }", true),
            ("catch 41 { return 1; }", false),
        ] {
            let source = temp_path("catch-dispatch.in");
            fs::write(&source, format!("fn fail() -> Int {{ throw 42; return 0; }}\nfn main() -> Int {{ try {{ fail(); }} {catches} return 3; }}")).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert_eq!(report.success, handled, "{profile:?}: {report:?}");
            if handled {
                assert_eq!(report.eval_result, Some(EvalValue::Int(2)));
            } else {
                assert_eq!(
                    report.error.as_deref(),
                    Some("jit-uncaught-error: entry `main` threw 42")
                );
                assert_eq!(report.eval_result, None);
            }
        }
    }
}

#[test]
fn native_backends_reject_dynamic_call_targets() {
    use crate::core_ir::{Expr, Stmt};
    for callee in [
        Expr::Field {
            base: Box::new(Expr::IntLit(0)),
            name: "method".into(),
        },
        Expr::Call {
            callee: Box::new(Expr::Ident("make".into())),
            args: vec![],
        },
        Expr::IntLit(42),
    ] {
        let mut module =
            crate::in_lang_parse::parse_in_source("fn main() -> Int { return 0; }").unwrap();
        if let Decl::Function { body, .. } = &mut module.decls[0] {
            *body = vec![Stmt::Return(Some(Expr::Call {
                callee: Box::new(callee),
                args: vec![],
            }))];
        }
        for result in [
            crate::native_emit::lower::lower_module(&module, "main", NativeLinkage::Executable)
                .map(|_| ()),
            crate::native_emit::x86_64_lower::lower_module(&module, "main").map(|_| ()),
        ] {
            let error = result.expect_err("dynamic call cannot silently return zero");
            assert!(error.contains("dynamic call target unsupported"), "{error}");
        }
        if native_backend::native_subset_host_available() {
            let request = default_request(
                PathBuf::from("dynamic-call.in"),
                CompileTarget::Jit,
                Some("main"),
                None,
            );
            let error = super::jit::compile_jit(&module, "App", &request)
                .err()
                .expect("dynamic call must fail");
            assert!(error.starts_with("jit-lowering-failed:"), "{error}");
            assert!(error.contains("dynamic call target unsupported"), "{error}");
        }
    }
}

#[test]
fn owned_jit_reports_uncaught_errors_before_decoding_results() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (typ, tail) in [
            ("Int", "return 7;"),
            ("Bool", "return true;"),
            ("Float", "return 1.5;"),
            ("String", "return \"value\";"),
            ("void", "return;"),
        ] {
            let source = temp_path("uncaught-error.in");
            fs::write(&source, format!("fn fail() -> Int {{ throw 42; return 0; }}\nfn main() -> {typ} {{ fail(); {tail} }}")).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(!report.success, "{profile:?}/{typ}: {report:?}");
            assert_eq!(
                report.error.as_deref(),
                Some("jit-uncaught-error: entry `main` threw 42"),
                "{profile:?}/{typ}: {report:?}"
            );
            assert_eq!(report.eval_result, None);
            assert_eq!(report.eval_exit_code, None);
        }
    }
}

#[test]
fn owned_jit_stops_effects_after_throwing_calls() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for main in [
            "try { throw 42; count = 7; } catch error { return count; } return -1;",
            "try { fail(); count = 8; } catch error { return count; } return -1;",
        ] {
            let source = temp_path("throw-flow.in");
            fs::write(&source, format!("var count: Int = 0;\nfn fail() -> Int {{ throw 42; count = 7; return 0; }}\nfn main() -> Int {{ {main} }}")).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(report.success, "{profile:?}: {main}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(0)),
                "{profile:?}: {main}"
            );
        }
    }
}

#[test]
fn owned_jit_preserves_shadowed_bindings() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (text, expected) in [
            (
                "var count: Int = 0;\nfn read() -> Int { return count; }\nfn main() -> Int { count = 42; let count = 7; return read(); }",
                42,
            ),
            (
                "fn shadow(x: Int) -> Int { let x = x + 1; return x; }\nfn main() -> Int { return shadow(41); }",
                42,
            ),
            (
                "fn main() -> Int { let x = 1; let before = x; let x = 7; return before + x; }",
                8,
            ),
            (
                "fn main() -> Int { var x = 1; if true { let x = 7; x = x + 1; } return x; }",
                1,
            ),
        ] {
            let source = temp_path("shadowed-bindings.in");
            fs::write(&source, text).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(report.success, "{profile:?}: {text}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{profile:?}: {text}"
            );
        }
    }
}

#[test]
fn inlining_preserves_argument_call_evaluation() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for profile in [
        crate::emit_profile::EmitProfile::Default,
        crate::emit_profile::EmitProfile::Lean,
        crate::emit_profile::EmitProfile::Harden,
    ] {
        for (callee, main, expected) in [
            (
                "fn twice(x: Int) -> Int { return x + x; }",
                "return twice(next());",
                2,
            ),
            (
                "fn twice(x: Int) -> Int { return x + x; }",
                "let result = twice(next()); return result;",
                2,
            ),
            (
                "fn unused(x: Int) -> Int { return 7; }",
                "let result = unused(next()); return count;",
                1,
            ),
            (
                "fn unused(x: Int) -> Void { return; }",
                "unused(next()); return count;",
                1,
            ),
            (
                "fn combine(x: Int) -> Int { return next() + x; }",
                "return combine(count);",
                1,
            ),
            (
                "fn combine() -> Int { let snapshot = count; return next() + snapshot; }",
                "return combine();",
                1,
            ),
            (
                "fn addtwice(x: Int) -> Void { count = count + x; count = count + x; }",
                "count = 1; addtwice(count); return count;",
                3,
            ),
            (
                "fn helper() -> Void { let temp = 7; count = temp; }",
                "let temp = 42; helper(); return count + temp;",
                49,
            ),
            (
                "fn helper() -> Void { return; count = 2; }",
                "count = 1; helper(); return count;",
                1,
            ),
            (
                "var shared: Int = 42;\nfn read() -> Int { return shared; }",
                "let shared = 7; return read();",
                42,
            ),
            (
                "var shared: Int = 42;\nfn read() -> Int { let copy = shared; return copy; }",
                "let shared = 7; return read();",
                42,
            ),
        ] {
            let source = temp_path("inline-argument-effects.in");
            fs::write(&source, format!("var count: Int = 0;\nfn next() -> Int {{ count = count + 1; return count; }}\n{callee}\nfn main() -> Int {{ {main} }}\n")).unwrap();
            let mut request =
                default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(source).unwrap();
            assert!(report.success, "{profile:?}: {main}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{profile:?}: {main}: {report:?}"
            );
        }
    }
}

#[test]
fn harden_jit_preserves_typed_float_results() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source = temp_path("harden-float.in");
    fs::write(
        &source,
        "var x: Float = 1.25;\nfn main() -> Float { x = 1.25; return x + 2.5; }\n",
    )
    .unwrap();
    let mut request = default_request(source.clone(), CompileTarget::Jit, Some("main"), None);
    request.profile = crate::emit_profile::EmitProfile::Harden;
    let report = compile_owned(&request);
    fs::remove_file(source).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(
        report.eval_result,
        Some(EvalValue::Float(3.75)),
        "{report:?}"
    );
    assert_eq!(report.runtime_level, "inrt-jit");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn harden_native_preserves_host_target_and_executes_at_runtime() {
    let source = temp_path("harden-native.in");
    let out = temp_path("harden-native");
    fs::write(&source, "fn main() -> Int { return 42; }\n").unwrap();
    let mut request = default_request(
        source.clone(),
        CompileTarget::Native,
        Some("main"),
        Some(out.clone()),
    );
    request.profile = crate::emit_profile::EmitProfile::Harden;
    let report = compile_owned(&request);
    fs::remove_file(source).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.runtime_level, "inrt-native");
    assert_eq!(&fs::read(&out).unwrap()[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    let status = std::process::Command::new(&out).status().unwrap();
    fs::remove_file(out).unwrap();
    assert_eq!(status.code(), Some(42));
}

#[test]
fn owned_rust_jit_executes_primitive_entries() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        ("fn main() -> i64 { return 42; }", EvalValue::Int(42)),
        (
            "fn main() -> f64 { return 1.25 + 2.5; }",
            EvalValue::Float(3.75),
        ),
        ("fn main() -> bool { return true; }", EvalValue::Bool(true)),
        (
            "fn main() -> i64 { let mut sum = 0; for mut item in 1..4 { sum = sum + item; item = 99; } return sum; }",
            EvalValue::Int(6),
        ),
        (
            "struct Point { x: i64 } fn unused(point: Point) -> Point { return point; } fn main() -> i64 { return 42; }",
            EvalValue::Int(42),
        ),
    ] {
        let path = temp_path("rust-jit.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(
            report.reason_code.as_deref(),
            Some("jit-executed"),
            "{report:?}"
        );
        assert_eq!(report.eval_result, Some(expected));
    }
}

#[test]
fn owned_rust_jit_retains_compile_only_for_uninvocable_entries() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "struct Point { x: i64 } fn main() -> Point { return Point { x: 42 }; }",
        "fn main(value: i64) -> i64 { return value; }",
        "fn helper() -> i64 { return 42; }",
        "type Result = i64; fn Result(value: i64) -> i64 { if value == 0 { return 42; } return Result(value - 1); } fn main() -> i64 { return Result(2); }",
    ] {
        let path = temp_path("rust-abi.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            None,
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-compiled-only"));
        assert_eq!(report.eval_exit_code, None);
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_rust_jit_verifies_before_execution() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-unresolved.rs");
    fs::write(&path, "fn main() -> i64 { return missing_value; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(&path).unwrap();
    assert!(!report.success, "{report:?}");
    assert!(
        report
            .error
            .as_deref()
            .is_some_and(|error| error.starts_with("jit-verification-failed:")),
        "{report:?}"
    );
    assert_eq!(report.eval_exit_code, None);
}
