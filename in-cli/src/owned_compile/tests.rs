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
            .is_some_and(|error| error.contains("unresolved identifier")),
        "{report:?}"
    );
    assert_eq!(report.eval_exit_code, None);
}

#[test]
fn owned_rust_jit_inclusive_ranges_include_the_endpoint() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> i64 { let mut sum = 0; for i in 1..=3 { sum = sum + i; } return sum; }",
            6,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for i in 7..=3 { count = count + 1; } return count; }",
            0,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for i in 9223372036854775807..=9223372036854775807 { count = count + 1; } return count; }",
            1,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for i in 9223372036854775806..=9223372036854775807 { count = count + 1; continue; } return count; }",
            2,
        ),
        (
            "fn main() -> i64 { let mut end = 3; let mut sum = 0; for mut i in 1..=end { sum = sum + i; end = 0; i = 99; } return sum; }",
            6,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for i in 1..=2 { for i in 1..=2 { count = count + 1; continue; } } return count; }",
            4,
        ),
    ] {
        let path = temp_path("rust-inclusive.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_rust_jit_loop_temporaries_preserve_user_names() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> i64 { let __for_items_0 = 100; let mut sum = 0; for item in [1, 2, 3] { sum = sum + item; } return __for_items_0 + sum; }",
            106,
        ),
        (
            "fn main() -> i64 { let __for_0 = 100; let __for_end_0 = 200; let mut sum = 0; for item in 1..3 { sum = sum + item; } return __for_0 + __for_end_0 + sum; }",
            303,
        ),
        (
            "fn sum(__for_0: i64) -> i64 { let mut result = 0; for item in 1..=3 { result = result + item; } return __for_0 + result; } fn main() -> i64 { return sum(100); }",
            106,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for mut __for_0 in 1..=3 { sum = sum + __for_0; __for_0 = 99; } return sum; }",
            6,
        ),
        (
            "fn __for_0() -> i64 { return 100; } fn main() -> i64 { let mut sum = 0; for item in 1..3 { sum = sum + item; } return __for_0() + sum; }",
            103,
        ),
    ] {
        let path = temp_path("rust-loop-names.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_rust_jit_complement_preserves_operand_type() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> bool { let len = true; return !len; }",
            EvalValue::Bool(false),
        ),
        ("fn main() -> i64 { return !1_i64; }", EvalValue::Int(-2)),
        (
            "fn main() -> i64 { let value = 3; return !value; }",
            EvalValue::Int(-4),
        ),
        (
            "fn invert(value: i64) -> i64 { return !value; } fn main() -> i64 { return invert(3); }",
            EvalValue::Int(-4),
        ),
        (
            "fn number(value: i64) -> i64 { if value == 0 { return 1; } else { return 2; } } fn main() -> i64 { return !number(0); }",
            EvalValue::Int(-2),
        ),
        ("fn main() -> i64 { return !(1 + 2); }", EvalValue::Int(-4)),
        ("fn main() -> i64 { return !!1; }", EvalValue::Int(1)),
        (
            "fn main() -> bool { return !true; }",
            EvalValue::Bool(false),
        ),
        (
            "fn invert(value: bool) -> bool { return !value; } fn main() -> bool { return invert(false); }",
            EvalValue::Bool(true),
        ),
        (
            "fn main() -> bool { return !(1 < 2); }",
            EvalValue::Bool(false),
        ),
        (
            "fn main() -> i64 { let values = [1, 2]; return !values[0]; }",
            EvalValue::Int(-2),
        ),
    ] {
        let path = temp_path("rust-complement.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(expected), "{source}");
    }
}

#[test]
fn owned_rust_jit_complement_refuses_nonintegral_operands() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> f64 { return !1.25; }",
        "fn main() -> String { return !\"text\"; }",
    ] {
        let path = temp_path("rust-invalid-complement.rs");
        fs::write(&path, source).unwrap();
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
                .is_some_and(|error| error.contains("complement operand")),
            "{report:?}"
        );
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_in_jit_bitwise_complement_handles_negative_operands() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/types/bitwise-complement.in");
    let report = compile_owned(&default_request(
        path,
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_rust_jit_preserves_conditional_returns_and_tail_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn answer(value: bool) -> i64 { if value { return 41; } return 42; } fn main() -> i64 { return answer(true) + answer(false); }",
        "fn answer(value: bool) -> i64 { if value { return 41; } else { let ignored = 99; } return 42; } fn main() -> i64 { return answer(true) + answer(false); }",
        "fn answer(value: bool) -> i64 { if value { 41 } else { 42 } } fn main() -> i64 { answer(true) + answer(false) }",
        "fn answer(value: bool) -> i64 { return if value { 41 } else { 42 }; } fn main() -> i64 { answer(true) + answer(false) }",
        "fn answer(value: bool) -> i64 { match value { true => 41, false => 42 } } fn main() -> i64 { answer(true) + answer(false) }",
        "fn answer(value: bool) -> i64 { return match value { true => 41, false => 42 }; } fn main() -> i64 { answer(true) + answer(false) }",
    ] {
        let path = temp_path("rust-return-flow.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(EvalValue::Int(83)), "{source}");
    }
    for source in [
        "fn main() -> i64 { { 99; } return 42; }",
        "fn main() -> i64 { { let value = 42; value } }",
        "fn main() -> i64 { match true { true => { let ignored = 1; }, false => { let ignored = 2; } }; return 42; }",
        "fn main() -> i64 { if false { return 1; } else if false { return 2; } else { let ignored = 3; } return 42; }",
    ] {
        let path = temp_path("rust-statement-flow.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_rust_jit_refuses_missing_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { 42; }",
        "fn answer(value: i64) -> i64 { match value { 1 => 41, 2 => 42 } } fn main() -> i64 { answer(3) }",
    ] {
        let path = temp_path("rust-missing-value.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_rust_jit_unit_tails_execute_in_the_enclosing_function() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() { for item in 0..3 { let ignored = item; } }",
        "fn main() { while false { } }",
        "fn main() { loop { break; } }",
        "fn main() { let mut value = 0; value = 1 }",
        "fn main() { let mut value = 0; value += 1 }",
    ] {
        let path = temp_path("rust-unit-tail.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, None);
        assert_eq!(report.eval_exit_code, Some(0));
    }
}

#[test]
fn owned_rust_jit_tail_branches_preserve_primitive_result_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn answer(value: bool) -> f64 { if value { 1.25 } else { 2.75 } } fn main() -> f64 { answer(true) + answer(false) }",
            EvalValue::Float(4.0),
        ),
        (
            "fn answer(value: bool) -> bool { if value { false } else { true } } fn main() -> bool { answer(false) }",
            EvalValue::Bool(true),
        ),
        (
            "fn answer(value: bool) -> &'static str { if value { \"left\" } else { \"right\" } } fn main() -> &'static str { answer(false) }",
            EvalValue::Str("right".into()),
        ),
    ] {
        let path = temp_path("rust-tail-types.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(expected));
    }
}

#[test]
fn owned_rust_jit_match_guards_select_the_first_successful_arm() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { match true { true if false => 1, _ => 42 } }",
        "fn accepted(value: i64) -> bool { if value == 3 { return true; } else { return false; } } fn main() -> i64 { match 3 { value if value < 0 => 1, value if accepted(value) => value + 39, _ => 2 } }",
        "fn main() -> i64 { let value = 42; match 3 { value if false => 1, _ => value } }",
        "fn main() -> i64 { let __match_value_0 = 40; let __match_done_0 = 2; match 3 { value if value > 0 => __match_value_0 + __match_done_0, _ => 0 } }",
    ] {
        let path = temp_path("rust-match-guards.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_guarded_matches_share_lowering_across_frontends() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for fixture in ["match-guard-effects.in", "match-guard.icore"] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../conformance/control-flow")
            .join(fixture);
        let report = compile_owned(&default_request(
            path,
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        assert!(report.success, "{report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{fixture}");
    }
}

#[test]
fn owned_match_guards_require_boolean_conditions() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (suffix, source) in [
        (
            "rs",
            "fn bad() -> bool { return 1; } fn main() -> i64 { match true { true if bad() => 1, _ => 42 } }",
        ),
        ("rs", "fn main() -> i64 { if 1 { 42 } else { 0 } }"),
        (
            "rs",
            "fn main() -> i64 { match 3 { value if 1 => 1, _ => 42 } }",
        ),
        (
            "in",
            "fn main() -> Int { match 3 { value if 1 { return 1; } _ { return 42; } } }",
        ),
    ] {
        let path = temp_path(&format!("invalid-guard.{suffix}"));
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn raw_native_guard_lowering_preserves_effect_order() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source = include_str!("../../../conformance/control-flow/match-guard-effects.in");
    let module = crate::in_lang_parse::parse_in_source(source).unwrap();
    let request = default_request(
        PathBuf::from("direct-guard.in"),
        CompileTarget::Jit,
        Some("main"),
        None,
    );
    let result = super::jit::compile_jit(&module, "GuardEffects", &request).unwrap();
    assert_eq!(result.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_rust_user_range_is_not_replaced_by_numeric_iteration() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-user-range.rs");
    fs::write(&path, "fn range(start: i64, end: i64) -> [i64; 2] { return [10, 20]; } fn main() -> i64 { let mut sum = 0; for item in range(1, 2) { sum = sum + item; } return sum; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.reason_code.as_deref(), Some("jit-executed"));
    assert_eq!(report.eval_result, Some(EvalValue::Int(30)));
}

#[test]
fn owned_rust_range_syntax_does_not_call_a_user_range_function() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-range-syntax.rs");
    fs::write(&path, "fn range(start: i64, end: i64) -> i64 { return 99; } fn main() -> i64 { let mut sum = 0; for item in 1..4 { sum = sum + item; } return sum; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(6)));
}

#[test]
fn owned_rust_noninlined_range_call_retains_its_array_abi() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("rust-range-array-abi.rs");
    fs::write(&path, "fn range(start: i64, end: i64) -> [i64; 2] { if start == 1 { return [10, 20]; } else { return [30, 40]; } } fn main() -> i64 { let mut sum = 0; for item in range(1, 2) { sum = sum + item; } return sum; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.reason_code.as_deref(), Some("jit-compiled-only"));
    assert_eq!(report.eval_result, None);
    assert_eq!(report.eval_exit_code, None);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn raw_x86_user_range_iterator_executes_the_function_result() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source = "fn range(start: i64, end: i64) -> [i64; 2] { if start == 1 { return [10, 20]; } else { return [30, 40]; } } fn main() -> i64 { let mut sum = 0; for item in range(1, 2) { sum = sum + item; } return sum; }";
    let module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
    let request = default_request(
        PathBuf::from("direct-range.in"),
        CompileTarget::Jit,
        Some("main"),
        None,
    );
    let result = super::jit::compile_jit(&module, "RangeCall", &request).unwrap();
    assert_eq!(result.eval_result, Some(EvalValue::Int(30)));
}

#[test]
fn owned_rust_open_ranges_iterate_from_their_start() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> i64 { for item in 1.. { return item; } return 2; }",
            1,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for mut item in 1.. { sum = sum + item; if item == 3 { break; } item = 99; } return sum; }",
            6,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for item in 1.. { if item < 3 { continue; } sum = sum + item; if item == 4 { break; } } return sum; }",
            7,
        ),
        (
            "fn main() -> i64 { let mut count = 0; for item in 9223372036854775807.. { count = count + 1; if count == 2 { return item; } } return 0; }",
            i64::MIN,
        ),
    ] {
        let path = temp_path("rust-open-range.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_in_open_ranges_evaluate_the_start_once() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/control-flow/range-open-effects.in");
    let report = compile_owned(&default_request(
        path,
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(8)));
}

#[test]
fn owned_range_iterators_do_not_invent_missing_bounds() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (suffix, source) in [
        (
            "rs",
            "fn main() -> i64 { for item in ..3 { return item; } return 42; }",
        ),
        (
            "rs",
            "fn main() -> i64 { for item in ..=3 { return item; } return 42; }",
        ),
        (
            "rs",
            "fn main() -> i64 { for item in .. { return item; } return 42; }",
        ),
        (
            "in",
            "fn main() -> Int { for item in ..3 { return item; } return 42; }",
        ),
        (
            "in",
            "fn main() -> Int { for item in 1..= { return item; } return 42; }",
        ),
    ] {
        let path = temp_path(&format!("invalid-range.{suffix}"));
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_rust_array_repeats_preserve_count() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "fn main() -> i64 { let mut sum = 0; for value in [7; 3] { sum = sum + value; } return sum; }",
            21,
        ),
        (
            "fn main() -> i64 { let values = [7; 3]; let mut sum = 0; for value in values { sum = sum + value; } return sum; }",
            21,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for value in [7; 0] { sum = sum + value; } return sum; }",
            0,
        ),
        (
            "const COUNT: usize = 1 + 2; fn main() -> i64 { let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } return sum; }",
            21,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for value in [true; 3] { if value { sum = sum + 1; } } return sum; }",
            3,
        ),
        ("const N: i64 = 42; fn main() -> i64 { N }", 42),
        (
            "fn main() -> i64 { let __array_repeat_0 = 100; let mut sum = 0; for value in [7; 3] { sum = sum + value; } return sum + __array_repeat_0; }",
            121,
        ),
        (
            "fn main() -> i64 { let mut sum = 0; for value in [\"x\"; 3] { if value == \"x\" { sum = sum + 1; } } return sum; }",
            3,
        ),
        (
            "const COUNT: usize = 3; fn main() -> i64 { let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } let COUNT = 0; return sum + COUNT; }",
            21,
        ),
    ] {
        let path = temp_path("rust-array-repeat.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_array_repeats_preserve_effect_order() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn main() -> Int { var sum: Int = 0; for value in array-repeat(step(), 3) { sum += value; } return sum + calls * 10; }",
            13,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn main() -> Int { var sum: Int = 0; for value in array-repeat(step(), 0) { sum += value; } return calls; }",
            1,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn main() -> Int { if false && array-len(array-repeat(step(), 3)) > 0 { return 99; } return calls; }",
            0,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn main() -> Int { while array-repeat(step(), 1)[0] < 3 { continue; } return calls; }",
            3,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; return calls; } fn combine(a: Int, b: Int, c: Int) -> Int { return a + b * 10 + c * 100; } fn main() -> Int { return combine(step(), array-len(array-repeat(step(), 3)), step()) + calls * 1000; }",
            3331,
        ),
        (
            "var calls: Int = 0; fn step() -> Int { calls += 1; throw 5; } fn main() -> Int { try { let values: [Int] = array-repeat(step(), 0); return 9; } catch e { return calls + 41; } }",
            42,
        ),
        (
            "fn main() -> Int { var count: Int = 0; for item in [\"..\", \"x\"] { count += 1; } return count; }",
            2,
        ),
        (
            "var calls: Int = 0; fn index() -> Int { calls += 1; return 0; } fn value() -> Int { calls += 1; return calls; } fn main() -> Int { let values: [Int] = [0]; index-assign-value-first(values, index(), value()); return values[0] * 10 + calls; }",
            12,
        ),
        (
            "var calls: Int = 0; fn index() -> Int { calls += 1; return 0; } fn value() -> Int { throw 5; } fn main() -> Int { let values: [Int] = [0]; try { index-assign-value-first(values, index(), value()); return 9; } catch e { return calls + 42; } }",
            42,
        ),
    ] {
        let path = temp_path("array-repeat-effects.in");
        fs::write(
            &path,
            source.replace("; fn", ";\nfn").replace("} fn", "}\nfn"),
        )
        .unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_array_repeat_counts_require_supported_constants() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { let count = 3; let mut sum = 0; for value in [7; count] { sum = sum + value; } return sum; }",
        "const COUNT: usize = 3; fn main() -> i64 { let COUNT = 1; let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } return sum; }",
        "fn main() -> i64 { let mut sum = 0; for value in [7; -1] { sum = sum + value; } return sum; }",
        "fn main() -> i64 { let mut sum = 0; for value in [7; 4097] { sum = sum + value; } return sum; }",
    ] {
        let path = temp_path("unsupported-repeat-count.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_class_array_repeats_use_field_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../conformance/classes/class-array-repeat.in");
    let report = compile_owned(&default_request(
        path,
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_scalar_struct_parameters_preserve_layout_and_stack_arguments() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "struct Pair {\n Int a\n Int b\n}\nfn score(pair: Pair, z: Int) -> Int { var sum: Int = 0; for i in 0..2 { sum += pair.a + pair.b; } return sum + z; }\nfn main() -> Int { let pair = Pair { a: 3, b: 4 }; return score(pair, 28); }",
        "struct Pair {\n Int a\n Int b\n}\nfn score(pair: Pair, z: Int) -> Int { var sum: Int = 0; for i in 0..2 { sum += pair.a + pair.b; } return sum + z; }\nfn main() -> Int { return score(Pair { a: 3, b: 4 }, 28); }",
        "struct Pair {\n Int a\n Int b\n}\nfn score(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int, pair: Pair) -> Int { var sum: Int = 0; for i in 0..1 { sum += a + b + c + d + e + f + pair.a + pair.b; } return sum; }\nfn main() -> Int { let pair = Pair { a: 10, b: 11 }; return score(1, 2, 3, 4, 5, 6, pair); }",
    ] {
        let path = temp_path("scalar-struct-arguments.in");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_rust_compile_time_constants_copy_values_at_use() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "const TEXT: &str = \"constant\"; fn main() -> &str { TEXT }",
            EvalValue::Str("constant".into()),
        ),
        (
            "const VALUES: [i64; 2] = [7, 14]; fn main() -> i64 { let mut first = VALUES; first[0] = 100; let second = VALUES; first[1] + second[0] + second[1] + 7 }",
            EvalValue::Int(42),
        ),
        (
            "mod first { pub const COUNT: usize = 3; pub fn calc() -> i64 { let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } return sum; } } mod second { pub const COUNT: usize = 2; pub fn calc() -> i64 { let mut sum = 0; for value in [7; COUNT] { sum = sum + value; } return sum; } } fn main() -> i64 { first::calc() + second::calc() + 7 }",
            EvalValue::Int(42),
        ),
    ] {
        let path = temp_path("compile-time-constants.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(&path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(expected), "{source}");
    }
}

#[test]
fn owned_large_repeat_respects_native_frame_limits() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("large-repeat.rs");
    fs::write(&path, "fn main() -> i64 { let mut sum = 0; for value in [7; 600] { sum = sum + value; } return sum; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    if cfg!(target_arch = "aarch64") {
        assert!(!report.success, "{report:?}");
        assert!(
            report
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("stack frame")),
            "{report:?}"
        );
        assert_eq!(report.eval_result, None);
    } else {
        assert!(report.success, "{report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(4200)));
    }
}

#[test]
fn owned_rust_indexed_compound_assignments_store_results() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, initial, rhs, expected) in [
        ("+", 7, 21, 28),
        ("-", 7, 2, 5),
        ("*", 7, 3, 21),
        ("/", 21, 3, 7),
        ("%", 21, 4, 1),
        ("&", 7, 3, 3),
        ("|", 4, 3, 7),
        ("^", 7, 3, 4),
        ("<<", 3, 2, 12),
        (">>", 12, 2, 3),
    ] {
        let path = temp_path("indexed-compound-store.rs");
        let source = format!(
            "fn main() -> i64 {{ let mut values = [{initial}, 14]; values[0] {op}= {rhs}; values[0] + values[1] }}"
        );
        fs::write(&path, &source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected + 14)),
            "{source}"
        );
    }
}

#[test]
fn owned_indexed_compound_stores_preserve_value_first_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        (
            "var calls: Int = 0;\nfn index() -> Int { calls += 1; return 0; }\nfn value() -> Int { calls += 1; return calls; }\nfn main() -> Int { let values: [Int] = [10]; index-modify-value-first(values, index(), value(), \"+\"); return values[0] * 10 + calls; }",
            112,
        ),
        (
            "var calls: Int = 0;\nfn index() -> Int { calls += 1; return 0; }\nfn value() -> Int { throw 5; }\nfn main() -> Int { let values: [Int] = [10]; try { index-modify-value-first(values, index(), value(), \"+\"); return 9; } catch e { return calls + 42; } }",
            42,
        ),
        (
            "fn value(values: [Int]) -> Int { values[0] = 100; return 1; }\nfn main() -> Int { let values: [Int] = [10]; index-modify-value-first(values, 0, value(values), \"+\"); return values[0]; }",
            101,
        ),
        (
            "fn main() -> Int { let values: [Int] = [0, 0, 0]; values[1 + 1] = 7; return values[2]; }",
            7,
        ),
        (
            "fn write(values: [Int]) -> Int { values[1] = 99; return 7; }\nfn main() -> Int { let values: [Int] = [10]; return write(values) + values[0]; }",
            11,
        ),
    ] {
        let path = temp_path("compound-store-effects.in");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_rust_float_indexed_compound_assignment() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("float-indexed-compound.rs");
    fs::write(
        &path,
        "fn main() -> f64 { let mut values = [1.5]; values[0] += 2.0; values[0] }",
    )
    .unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Float(3.5)));
}

#[test]
fn owned_rust_field_compound_assignments_update_original_storage() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, initial, rhs, expected) in [
        ("+", 21, 21, 42),
        ("-", 21, 7, 14),
        ("*", 7, 3, 21),
        ("/", 21, 3, 7),
        ("%", 21, 4, 1),
        ("&", 7, 3, 3),
        ("|", 4, 3, 7),
        ("^", 7, 3, 4),
        ("<<", 3, 2, 12),
        (">>", 12, 2, 3),
    ] {
        let path = temp_path("field-compound.rs");
        let source = format!(
            "struct Counter {{ value: i64 }} fn main() -> i64 {{ let mut counter = Counter {{ value: {initial} }}; counter.value {op}= {rhs}; counter.value }}"
        );
        fs::write(&path, &source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_field_compounds_read_after_rhs_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("field-compound-effects.in");
    fs::write(&path, "struct Counter {\n Int value\n}\nvar counter: Counter = 0;\nfn value() -> Int { counter.value = 100; return 1; }\nfn main() -> Int { counter.value = 21; field-modify-value-first(counter, \"value\", value(), \"+\"); return counter.value; }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(101)));
}

#[test]
fn owned_rust_float_field_compound_assignment() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("float-field-compound.rs");
    fs::write(&path, "struct Counter { value: f64 } fn main() -> f64 { let mut counter = Counter { value: 1.5 }; counter.value += 2.0; counter.value }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Float(3.5)));
}

#[test]
fn owned_field_compound_throw_preserves_original_field() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let path = temp_path("field-compound-throw.in");
    fs::write(&path, "struct Counter {\n Int value\n}\nvar counter: Counter = 0;\nfn value() -> Int { throw 5; }\nfn main() -> Int { counter.value = 21; try { field-modify-value-first(counter, \"value\", value(), \"+\"); return 9; } catch e { return counter.value + 21; } }").unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_rust_boolean_bitwise_operations_preserve_boolean_results() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (source, expected) in [
        ("fn main() -> bool { true & false }", false),
        ("fn main() -> bool { true | false }", true),
        ("fn main() -> bool { true ^ true }", false),
        (
            "fn combine(a: bool, b: bool) -> bool { for i in 0..1 { if i == 0 { return a ^ b; } } return false; } fn main() -> bool { combine(true, false) }",
            true,
        ),
        (
            "fn combine(a: bool) -> bool { for i in 0..1 { if i == 0 { return a ^ a; } } return true; } fn main() -> bool { combine(true) }",
            false,
        ),
        (
            "fn main() -> bool { let mut values = [true]; values[0] &= false; values[0] }",
            false,
        ),
        (
            "struct Flag { value: bool } fn main() -> bool { let mut flag = Flag { value: false }; flag.value |= true; flag.value }",
            true,
        ),
    ] {
        let path = temp_path("bool-bitwise.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Bool(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_boolean_bitwise_operators_evaluate_both_operands() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, left, right, negate) in [
        ("&", "false", "true", "!"),
        ("|", "true", "false", ""),
        ("^", "true", "false", ""),
    ] {
        let source = format!(
            "var calls: Int = 0;\nfn left() -> Bool {{ calls = calls * 10 + 1; return {left}; }}\nfn right() -> Bool {{ calls = calls * 10 + 2; return {right}; }}\nfn main() -> Int {{ let result: Bool = left() {op} right(); if {negate}result {{ return calls + 30; }} return 99; }}"
        );
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("bool-bitwise-effects.in");
            fs::write(&path, &source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{source}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{source}: {profile:?}"
            );
        }
    }
}

#[test]
fn owned_bitwise_operands_do_not_mix_boolean_float_and_integer_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> bool { true & 1 }",
        "fn main() -> i64 { 1 | false }",
        "fn main() -> i64 { 1.5 ^ 2.0 }",
        "fn main() -> i64 { true << false }",
    ] {
        let path = temp_path("invalid-bitwise.rs");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
    for (source, expected) in [
        ("int main() { return true & false; }", 0),
        ("int main() { return true | 4; }", 5),
        ("int main() { return true << 2; }", 4),
        (
            "int combine(bool left, bool right) { return left | right; } int main() { return combine(true, false); }",
            1,
        ),
    ] {
        let path = temp_path("cpp-bool-bitwise.cpp");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_c_for_loops_preserve_updates_and_control_flow() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for (source, expected) in [
            (
                "int main() { for (int i = 0; i < 1; i++) { return 42; } return 0; }",
                42,
            ),
            (
                "int main() { int sum = 0; for (int i = 0; i < 5; ++i) { if (i == 2) continue; sum = sum + i; } return sum; }",
                8,
            ),
            (
                "int main() { int sum = 0; for (int i = 0; i < 7; i += 2) { sum += i; } return sum; }",
                12,
            ),
            (
                "int main() { int i = 42; for (int i = 0; i < 3; i++) {} return i; }",
                42,
            ),
            (
                "int main() { int sum = 0; for (int i = 4; i > 0; i--) { if (i == 2) break; sum = sum + i; } return sum; }",
                7,
            ),
            (
                "int main() { int i = 0; for (;;) { i = i + 1; if (i == 42) return i; } }",
                42,
            ),
            (
                "int main() { int sum = 0; for (int i = 0; i < 3; i = i + 1) { int j = 0; while (j < 2) { j = j + 1; continue; } sum = sum + i; } return sum; }",
                3,
            ),
            (
                "int main() { int i = 0; for (i = 2; i < 4; i++) i = i + 1; return i; }",
                4,
            ),
            (
                "int combine(bool left, bool right) { for (int i = 0; i < 1; i++) { return left | right; } return 0; } int main() { return combine(true, false); }",
                1,
            ),
        ] {
            let path = temp_path(&format!("c-for.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected)),
                "{extension}: {source}"
            );
        }
    }
}

#[test]
fn owned_c_unsupported_loop_parts_are_refused_instead_of_dropped() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { for (int i = 0; i < 1; i++) { switch (i) { case 0: return 42; } } return 0; }",
            "int main() { int values[1]; for (int i = 0; i < 1; values[i]++) { return 42; } return 0; }",
        ] {
            let path = temp_path(&format!("unsupported-c-for.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{extension}: {source}: {report:?}");
            assert_eq!(report.eval_result, None);
        }
    }
}

#[test]
fn owned_c_global_storage_is_shared_and_zero_initialized() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int value = 20; int helper() { value = 40; return 2; } int main() { helper(); return value + 2; }",
            "extern int value = 42; int main() { return value; }",
            "int /* const */ value = 41; int main() { value++; return value; }",
            "int value; int main() { return value + 42; }",
            "int first = 20, second = 22; int main() { return first + second; }",
            "static int value = 21; int helper() { value = value * 2; return value; } int main() { helper(); return value; }",
            "const int value = 42; int main() { return value; }",
            "int value = 42; int main() { { int value = 0; value++; } return value; }",
        ] {
            let path = temp_path(&format!("c-global.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {source}"
            );
        }
    }
}

#[test]
fn owned_c_global_definitions_preserve_refusal_boundaries() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "const int value = 42; int main() { value = 0; return value; }",
            "extern int value; int main() { return value; }",
            "int helper() { return 42; } int value = helper(); int main() { return value; }",
            "int values[2]; int main() { return 42; }",
        ] {
            let path = temp_path(&format!("unsupported-c-global.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{extension}: {source}: {report:?}");
            assert_eq!(report.eval_result, None);
        }
    }
}

#[test]
fn owned_cpp_compound_assignment_reads_target_after_rhs_once() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (op, expected) in [
        ("+", 43),
        ("-", 37),
        ("*", 120),
        ("/", 13),
        ("%", 1),
        ("&", 0),
        ("|", 43),
        ("^", 43),
        ("<<", 320),
        (">>", 5),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = format!(
                "int value = 21; int calls = 0; int helper() {{ calls++; value = 40; return 3; }} int main() {{ value {op}= helper(); return value + calls * 1000; }}"
            );
            let path = temp_path("cpp-compound-order.cpp");
            fs::write(&path, &source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{source}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(expected + 1000)),
                "{source}: {profile:?}"
            );
        }
    }
    for (source, expected) in [
        (
            "int value = 20; int helper() { value = 40; return 2; } int main() { value += helper(); return value; }",
            42,
        ),
        (
            "int value = 40; int main() { int result = 0; { int value = 20; value += 2; result = value; } return result + value; }",
            62,
        ),
        (
            "int __array_repeat_0 = 40; int main() { int value = 20; value += 2; return value + __array_repeat_0; }",
            62,
        ),
        (
            "int step = 0; int calls = 0; int helper() { calls++; step = 2; return 2; } int main() { for (; step < 4; step += helper()) {} return step + calls * 1000; }",
            1004,
        ),
    ] {
        let path = temp_path("cpp-compound-place.cpp");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(
            report.eval_result,
            Some(EvalValue::Int(expected)),
            "{source}"
        );
    }
}

#[test]
fn owned_c_integer_literal_radices_reach_native_unchanged() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for literal in [
            "0x2A",
            "0X2a",
            "052",
            "0b101010",
            "42ULL",
            "0x2Aul",
            "0xFF - 0xD5",
        ] {
            let path = temp_path(&format!("c-radix.{extension}"));
            fs::write(
                &path,
                format!("int value = {literal}; int main() {{ return value; }}"),
            )
            .unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {literal}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {literal}"
            );
        }
    }
    for literal in ["4'2", "0'52", "0x2'A", "0b10'1010"] {
        let path = temp_path("cpp-separated-int.cpp");
        fs::write(&path, format!("int main() {{ return {literal}; }}")).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{literal}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{literal}");
    }
}

#[test]
fn owned_c_local_declarators_preserve_order_dependencies_and_scope() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int value = 0; int helper() { value = 42; return 1; } int main() { int first = 0, ignored = helper(); return value; }",
            "int calls = 0; int first() { calls = calls * 10 + 1; return 0; } int second() { calls = calls * 10 + 2; return 0; } int main() { int a = first(), b = second(); return calls + 30; }",
            "int main() { int first = 20, second = first + 2; return first + second; }",
            "int value = 99; int main() { int value = 20, second = value + 2; return value + second; }",
            "int main() { for (int i = 0, j = 40; i < 2; i++) { if (i == 1) return j + i + 1; } return 0; }",
            "int main() { int i = 42; for (int i = 0, j = 0; i < 1; i++) { j++; } return i; }",
            "int calls = 0; int first() { calls = calls * 10 + 1; return 0; } int second() { calls = calls * 10 + 2; return 0; } int main() { for (int a = first(), b = second(); a < 1; a++) { return calls + 30; } return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-local-declarators.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_c_unsupported_later_declarators_are_not_discarded() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { int first = 42, *pointer = 0; return first; }",
            "int main() { int first = 42, second; return first; }",
            "int main() { int first = 42, values[2] = {1, 2}; return first; }",
        ] {
            let path = temp_path(&format!("unsupported-c-local-declarators.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{extension}: {source}: {report:?}");
            assert_eq!(report.eval_result, None);
        }
    }
}

#[test]
fn owned_c_numeric_boolean_conversions_preserve_control_flow_and_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { if (42) return 42; return 0; }",
            "int main() { if (0) return 0; return 42; }",
            "int main() { int count = 2, sum = 0; while (count) { count--; sum += 21; } return sum; }",
            "int main() { int sum = 0; for (int i = 2; i; i--) { sum += 21; } return sum; }",
            "int main() { int result = !0; result = result && 7; return result * 42; }",
            "int main() { return (!0) * 42; }",
            "int main() { return (7 && 9) * 42; }",
            "bool *identity(bool *value) { return value; } int main() { if (identity(0)) return 0; return 42; }",
            "int main() { bool value = 2; return value * 42; }",
            "bool value = 2; int main() { value = 7; return value * 42; }",
            "int convert(bool value) { return value; } int main() { return convert(7) * 42; }",
            "bool convert(int value) { return value; } int main() { return convert(7) * 42; }",
            "double value; int main() { if (value) return 0; return 42; }",
            "double value; int main() { bool result = value; if (!result) return 42; return 0; }",
            "int calls = 0; int left() { calls = calls * 10 + 1; return 0; } int right() { calls = calls * 10 + 2; return 7; } int main() { if (left() && right()) return 0; return calls + 41; }",
            "int calls = 0; int left() { calls = calls * 10 + 1; return 7; } int right() { calls = calls * 10 + 2; return 9; } int main() { if (left() || right()) return calls + 41; return 0; }",
            "int calls = 0; int left() { calls = calls * 10 + 1; return 7; } int right() { calls = calls * 10 + 2; return 9; } int main() { if (left() && right()) return calls + 30; return 0; }",
            "int calls = 0; int left() { calls = calls * 10 + 1; return 0; } int right() { calls = calls * 10 + 2; return 9; } int main() { if (left() || right()) return calls + 30; return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-numeric-bool.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_non_c_frontends_keep_strict_boolean_operands() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (extension, source) in [
        ("rs", "fn main() -> i64 { if 42 { return 42; } return 0; }"),
        (
            "rs",
            "fn main() -> i64 { if 1 && 2 { return 42; } return 0; }",
        ),
        ("in", "fn main() -> Int { if 42 { return 42; } return 0; }"),
        (
            "in",
            "fn main() -> Int { let value: Bool = 2; if value { return 42; } return 0; }",
        ),
    ] {
        let path = temp_path(&format!("strict-bool.{extension}"));
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{extension}: {source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn raw_c_floating_conditions_preserve_zero_and_nan_truth_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (value, expected) in [
        (0.0, 0),
        (-0.0, 0),
        (1.0, 42),
        (-1.0, 42),
        (f64::INFINITY, 42),
        (f64::NEG_INFINITY, 42),
        (f64::NAN, 42),
    ] {
        let mut module = crate::in_lang_parse::parse_in_source(
            "var value: Float = 0.0;\nfn main() -> Int { if value { return 42; } return 0; }",
        )
        .unwrap();
        let crate::core_ir::Decl::Global { init, .. } = &mut module.decls[0] else {
            panic!("missing global");
        };
        *init = Some(Box::new(crate::core_ir::Expr::FloatLit(
            crate::core_ir::FloatVal(value),
        )));
        let module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::C, &module);
        let request = default_request(
            PathBuf::from("raw-c-float-condition.icore"),
            CompileTarget::Jit,
            Some("main"),
            None,
        );
        let result = super::jit::compile_jit(&module, "FloatingConditions", &request).unwrap();
        assert_eq!(
            result.eval_result,
            Some(EvalValue::Int(expected)),
            "{value:?}"
        );
    }
}

#[test]
fn raw_c_boolean_stores_use_field_and_element_types() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "struct Flag { Bool value }\nfn main() -> Int { let flag: Flag = Flag { value: false }; flag.value = 2; if flag.value { return 42; } return 0; }",
        "fn main() -> Int { let flags = [false]; flags[0] = 2; if flags[0] { return 42; } return 0; }",
    ] {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        let module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::C, &module);
        let request = default_request(
            PathBuf::from("raw-c-bool-store.icore"),
            CompileTarget::Jit,
            Some("main"),
            None,
        );
        let result = super::jit::compile_jit(&module, "BooleanStores", &request).unwrap();
        assert_eq!(result.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_cpp_condition_initializers_keep_effects_bindings_and_scope() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "int value = 0; int helper() { value = 42; return 1; } int main() { if (int ignored = helper(); true) return value; return 0; }",
        "int value = 0; int helper() { value = 42; return 1; } int main() { if (helper(); 7) return value; return 0; }",
        "int value = 0; int helper() { value = 42; return 0; } int main() { if (int result = helper(); result) return 0; else return value; }",
        "int result = 42; int main() { if (int result = 0; result) return 0; return result; }",
        "int main() { if (int result = 42) return result; return 0; }",
        "int main() { if (bool result = 7) return result * 42; return 0; }",
        "int value = 0; int helper() { value = 42; return 1; } int main() { if (/* comment */ int ignored = helper(); true) return value; return 0; }",
    ] {
        let path = temp_path("cpp-condition-init.cpp");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_cpp_scalar_conversion_does_not_guess_objects_or_references() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "struct Painting { operator bool() { return true; } }; Painting value; int main() { if (value) return 42; return 0; }",
        "struct Double { operator bool() { return true; } }; Double value; int main() { if (value) return 42; return 0; }",
        "struct Bool { operator bool() { return true; } }; Bool value; int main() { if (value) return 42; return 0; }",
        "void clear(bool &value) { value = false; } int main() { bool value = true; clear(value); return value; }",
        "long double value = 1; int main() { if (value / 2) return 42; return 0; }",
    ] {
        let path = temp_path("cpp-non-scalar-conversion.cpp");
        fs::write(&path, source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(!report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, None);
    }
}

#[test]
fn owned_c_parameter_assignment_reuses_the_parameter_frame_slot() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "void reset(int value) { value = 0; } int main() { reset(1); return 42; }",
            "int reset(int value) { value = 42; return value; } int main() { return reset(1); }",
            "bool reset(bool value) { value = false; return value; } int main() { if (reset(true)) return 0; return 42; }",
            "int last(int a, int b, int c, int d, int e, int f, int g, int h, int value) { value = 42; return value; } int main() { return last(0,0,0,0,0,0,0,0,1); }",
        ] {
            let path = temp_path(&format!("c-param-store.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {source}"
            );
        }
    }
}

#[test]
fn owned_stack_arguments_survive_later_argument_calls() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let source = r#"int helper(int a, int b, int c, int d, int e, int f, int g, int h, int i) {
    for (int n = 0; n < 1; n++) { return 2; }
    return 0;
}
int combine(int a, int b, int c, int d, int e, int f, int g, int h, int i, int j) {
    return i * 10 + j;
}
int main() { return combine(0,0,0,0,0,0,0,0,4,helper(0,0,0,0,0,0,0,0,99)); }
"#;
    for extension in ["c", "cpp"] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path(&format!("stack-argument-effects.{extension}"));
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {profile:?}"
            );
        }
    }
}

#[test]
fn owned_native_calls_stage_many_scalar_and_aggregate_words() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    let params = (0..32)
        .map(|index| format!("int p{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let args = (0..32)
        .map(|index| if index == 0 || index == 31 { "21" } else { "0" })
        .collect::<Vec<_>>()
        .join(",");
    let scalar = format!(
        "int combine({params}) {{ for (int i = 0; i < 1; i++) {{ return p0 + p31; }} return 0; }} int main() {{ return combine({args}); }}"
    );
    let fields = (0..10)
        .map(|index| format!("Int f{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let values = (0..10)
        .map(|index| {
            format!(
                "f{index}: {}",
                if index == 0 || index == 9 { 21 } else { 0 }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let aggregate = format!(
        "struct Wide {{\n{fields}\n}}\nfn combine(value: Wide) -> Int {{ for i in 0..1 {{ return value.f0 + value.f9; }} return 0; }}\nfn main() -> Int {{ let value: Wide = Wide {{ {values} }}; return combine(value); }}"
    );
    for (extension, source) in [("c", scalar), ("in", aggregate)] {
        let path = temp_path(&format!("many-argument-words.{extension}"));
        fs::write(&path, &source).unwrap();
        let report = compile_owned(&default_request(
            path.clone(),
            CompileTarget::Jit,
            Some("main"),
            None,
        ));
        fs::remove_file(path).unwrap();
        assert!(report.success, "{source}: {report:?}");
        assert_eq!(report.eval_result, Some(EvalValue::Int(42)), "{source}");
    }
}

#[test]
fn owned_c_decimal_floating_literals_reach_native_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "double value = 1.5; int main() { if (value > 1.0) return 42; return 0; }",
            "double value = 1.25e1; int main() { if (value == 12.5) return 42; return 0; }",
            "double value = .5; int main() { if (value + .5 == 1.) return 42; return 0; }",
            "double value = 1e-3; int main() { if (value == 0.001) return 42; return 0; }",
            "float value = 0.1f; int main() { if (value > 0.1) return 42; return 0; }",
            "double twice(double value) { for (int i = 0; i < 1; i++) return value * 2.0; return 0.0; } int main() { if (twice(1.5) == 3.0) return 42; return 0; }",
            "bool value = .5; int main() { if (value) return 42; return 0; }",
        ] {
            let path = temp_path(&format!("c-float-literal.{extension}"));
            fs::write(&path, source).unwrap();
            let report = compile_owned(&default_request(
                path.clone(),
                CompileTarget::Jit,
                Some("main"),
                None,
            ));
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {source}"
            );
        }
    }
    let path = temp_path("cpp-float-separator.cpp");
    fs::write(
        &path,
        "double value = 1'2.5; int main() { if (value == 12.5) return 42; return 0; }",
    )
    .unwrap();
    let report = compile_owned(&default_request(
        path.clone(),
        CompileTarget::Jit,
        Some("main"),
        None,
    ));
    fs::remove_file(path).unwrap();
    assert!(report.success, "{report:?}");
    assert_eq!(report.eval_result, Some(EvalValue::Int(42)));
}

#[test]
fn owned_cpp_while_declarations_reevaluate_and_keep_scope() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "int calls = 0; int next() { calls++; if (calls < 3) return 1; return 0; } int main() { int sum = 0; while (int value = next()) { sum += 21; if (sum > 42) return 99; } return sum; }",
        "int calls = 0; int next() { calls++; if (calls < 3) return 1; return 0; } int main() { int iterations = 0; while (int value = next()) { iterations++; if (iterations > 2) return 99; continue; } return calls + 39; }",
        "int value = 42; int main() { while (int value = 0) { return 0; } return value; }",
        "int main() { while (bool value = 2) { return value * 42; } return 0; }",
        "int calls = 0; int next() { calls++; return 1; } int main() { while (int value = next()) { break; } return calls + 41; }",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("cpp-while-decl.cpp");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{profile:?}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {source}"
            );
        }
    }
}

#[test]
fn owned_c_do_while_keeps_posttest_effects_continue_and_scope() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { int value = 0; do { value++; } while (0); return value + 41; }",
            "int calls = 0; int condition() { calls++; return 0; } int main() { int value = 0; do { value = 1; continue; } while (condition()); return value + calls + 40; }",
            "int calls = 0; int condition() { calls++; return 1; } int main() { do { break; } while (condition()); return calls + 42; }",
            "int value = 0; int main() { int iterations = 0; do { int value = 7; iterations++; if (iterations > 1) return 99; continue; } while (value); return iterations + 41; }",
            "int main() { int value = 0; do { value++; int inner = 0; while (inner < 2) { inner++; continue; } } while (value < 2); return value + 40; }",
            "int main() { int value = 0; do value++; while (value < 2); return value + 40; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-do-while.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_c_numeric_conversions_preserve_effects_and_truncate() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "double widen(int value) { return value; } int narrow(double value) { return value; } int main() { return narrow(widen(40) + 2.75); }",
            "int main() { double value = 41; value = value + 1.9; int result = value; return result; }",
            "int main() { double value = -42.9; return -value; }",
            "int main() { double value = true; return value + 41; }",
            "int calls = 0; double next() { calls++; return 41.9; } int take(int value) { return value; } int main() { return take(next()) + calls; }",
            "int main() { double value = 40.75; return value + 1.5; }",
            "double value = 42; int main() { return value; }",
            "int main() { double value = -0.5; return -value * 84; }",
            "int narrow(double value) { return value; } int main() { return narrow(-42.9) + 84; }",
            "int main() { double value = 42.75; return (value > 42) + 41; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-numeric-conversion.{extension}"));
                fs::write(&path, source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_c_numeric_casts_preserve_value_and_effects() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { double value = 42.9; return (int)value; }",
            "int main() { int value = 21; return (int)((double)value * 2.0); }",
            "int main() { double value = -0.5; return (int)((double)(int)value + 42.0); }",
            "int main() { double value = -42.9; return (int)value + 84; }",
            "int calls = 0; double next() { calls++; return 41.9; } int main() { return (int)next() + calls; }",
            "int main() { double value = 2.5; return (int)((_Bool)value) + 41; }",
            "int main() { int value = 42; return (const int)value; }",
            "int calls = 0; double next() { calls++; return 2.5; } int main() { return (int)(_Bool)next() + calls + 40; }",
            "int main() { double value = -0.0; return (int)(_Bool)value + 42; }",
        ] {
            let source = source.replace("_Bool", if extension == "cpp" { "bool" } else { "_Bool" });
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let path = temp_path(&format!("c-numeric-cast.{extension}"));
                fs::write(&path, &source).unwrap();
                let mut request =
                    default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
                request.profile = profile;
                let report = compile_owned(&request);
                fs::remove_file(path).unwrap();
                assert!(
                    report.success,
                    "{extension}: {profile:?}: {source}: {report:?}"
                );
                assert_eq!(
                    report.eval_result,
                    Some(EvalValue::Int(42)),
                    "{extension}: {profile:?}: {source}"
                );
            }
        }
    }
}

#[test]
fn owned_c_numeric_casts_refuse_pointer_object_and_extended_types() {
    for extension in ["c", "cpp"] {
        for source in [
            "int main() { int value = 42; return (int*)value; }",
            "int main() { int value = 42; return (long double)value; }",
            "int main() { int value = 42; return (Unknown)value; }",
        ] {
            let path = temp_path(&format!("c-unsupported-cast.{extension}"));
            fs::write(&path, source).unwrap();
            let request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(!report.success, "{extension}: {source}: {report:?}");
            assert_eq!(report.eval_result, None);
        }
    }
}

#[test]
fn owned_rust_integer_casts_truncate_and_extend() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { let value: i64 = 298; return (value as u8) as i64; }",
        "fn main() -> i64 { let value: i64 = 170; return (value as i8) as i64 + 128; }",
        "fn main() -> i64 { let value: i64 = 65578; return (value as u16) as i64; }",
        "fn main() -> i64 { let value: i64 = 32768; return (value as i16) as i64 + 32810; }",
        "fn main() -> i64 { let value: i64 = 4294967338; return (value as u32) as i64; }",
        "fn main() -> i64 { let value: i64 = 2147483648; return (value as i32) as i64 + 2147483690; }",
        "fn main() -> i64 { let value: i64 = -214; return (value as u8) as i64; }",
        "fn main() -> i64 { let value: bool = true; return (value as u8) as i64 + 41; }",
        "fn main() -> i64 { let value: bool = false; return (value as i64) + 42; }",
        "fn main() -> i64 { let value: i64 = 42; return (value as usize) as isize as i64; }",
        "fn main() -> i64 { let value: i64 = 298; return (value as core::primitive::u8) as i64; }",
        "fn main() -> i64 { let value: i64 = 298; return (value as ::std::primitive::u8) as i64; }",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("rust-integer-cast.rs");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{profile:?}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {source}"
            );
        }
    }
}

#[test]
fn owned_rust_floating_width_casts_round_and_keep_values() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for source in [
        "fn main() -> i64 { let value = 16777217.0; if (value as f32) as f64 == 16777216.0 { return 42; } return 0; }",
        "fn main() -> i64 { let value: f64 = 16777217.0; if (value as f32) as f64 == 16777216.0 { return 42; } return 0; }",
        "fn value() -> f64 { return 16777217.0; } fn main() -> i64 { if (value() as f32) as f64 == 16777216.0 { return 42; } return 0; }",
        "fn main() -> i64 { let value = 0.1f32; if (value as f64) > 0.1f64 { return 42; } return 0; }",
        "fn main() -> i64 { let value = 42f32; if value as f64 == 42f64 { return 42; } return 0; }",
        "fn main() -> i64 { let value: f64 = 42.5; if value as ::core::primitive::f64 == 42.5 { return 42; } return 0; }",
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path("rust-float-width.rs");
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{profile:?}: {source}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{profile:?}: {source}"
            );
        }
    }
}

#[test]
fn owned_signed_shift_sources_preserve_sign_and_compound_updates() {
    if !native_backend::native_subset_host_available() {
        return;
    }
    for (extension, source) in [
        (
            "in",
            include_str!("../../../conformance/types/signed-shift-effects.in"),
        ),
        (
            "rs",
            "fn half(value: i64, count: i64) -> i64 { return value >> count; } fn main() -> i64 { let mut value: i64 = -168; value >>= 1; return -half(value, 1); }",
        ),
        (
            "c",
            "long long value = -84; long long ones = -1; long long minimum = -9223372036854775807LL - 1; int main() { int count = 1; if ((ones >> count) != -1) return 99; count = 63; if ((minimum >> count) != -1) return 99; count = 1; value >>= count; return -value; }",
        ),
        (
            "cpp",
            "long long value = -84; long long ones = -1; long long minimum = -9223372036854775807LL - 1; int main() { int count = 1; if ((ones >> count) != -1) return 99; count = 63; if ((minimum >> count) != -1) return 99; count = 1; value >>= count; return -value; }",
        ),
    ] {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let path = temp_path(&format!("signed-shift.{extension}"));
            fs::write(&path, source).unwrap();
            let mut request = default_request(path.clone(), CompileTarget::Jit, Some("main"), None);
            request.profile = profile;
            let report = compile_owned(&request);
            fs::remove_file(path).unwrap();
            assert!(report.success, "{extension}: {profile:?}: {report:?}");
            assert_eq!(
                report.eval_result,
                Some(EvalValue::Int(42)),
                "{extension}: {profile:?}"
            );
        }
    }
}
