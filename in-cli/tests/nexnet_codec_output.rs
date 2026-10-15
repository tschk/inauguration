//! The encoded byte stream is part of the compiler contract, even when `main`
//! returns the expected scalar result.
#![cfg(all(unix, target_arch = "aarch64"))]

use std::{
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn nexnet_uint_42_emits_complete_canonical_bytes() {
    // Exact frozen Nexnet sources from chain/in-core; the probe imports
    // ../canonical.in relative to its own path.
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/nexnet_codec/probes/emit_uint_42.in");
    let mut child = Command::new(env!("CARGO_BIN_EXE_in"))
        .arg("execute")
        .arg("--verbose")
        .arg(&fixture)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn compiler CLI");
    let started = Instant::now();
    while child.try_wait().expect("poll compiler CLI").is_none() {
        if started.elapsed() > Duration::from_secs(5) {
            child.kill().expect("terminate timed-out compiler CLI");
            let _ = child.wait_with_output();
            panic!("Nexnet uint-42 compiler fixture exceeded five seconds");
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child
        .wait_with_output()
        .expect("collect compiler CLI output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.stdout, b"24,42,",
        "status {:?}; stderr: {stderr}",
        output.status
    );
    assert!(stderr.contains("Int(42)"), "{stderr}");
    assert_eq!(output.status.code(), Some(1), "{stderr}");
}
