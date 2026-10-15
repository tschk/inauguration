//! Native lowering traces must stay bounded without hiding runtime degradation.
#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

use inauguration::{
    in_lang_parse::parse_in_source,
    jit_runtime::JitRuntime,
    native_emit::{NativeLinkage, lower},
};
use std::{
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const TARGET: &str = "codex_missing_native_target";
const TRACE: &str = "[TRACE] external call candidate `codex_missing_native_target` in `main`";

#[test]
fn unresolved_call_child() {
    let Ok(case) = std::env::var("IN_TRACE_CHILD") else {
        return;
    };
    let calls = if case == "many" { 70 } else { 1 };
    let mut source = String::from("fn main() -> Int {\n");
    for _ in 0..calls - 1 {
        source.push_str("  codex_missing_native_target(7);\n");
    }
    source.push_str("  return codex_missing_native_target(8);\n}\n");

    let module = parse_in_source(&source).expect("parse unresolved-call fixture");
    let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect("lower unresolved-call fixture");
    assert!(
        lowered.degradations.iter().any(|d| {
            d.code == "IN3002"
                && d.function == "main"
                && d.target.as_deref() == Some(TARGET)
                && d.reachable
        }),
        "missing reachable unresolved-call degradation: {:?}",
        lowered.degradations
    );
    let functions = lowered
        .function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset, lowered.code.len() as u32 - *offset))
        .collect::<Vec<_>>();
    let mut runtime = JitRuntime::new();
    runtime
        .load(&lowered.code, &functions, &lowered.relocations)
        .expect("load unresolved-call fixture");
    let result = unsafe { runtime.invoke("main", &[]) };
    panic!("unresolved call returned {result:?} instead of trapping");
}

fn run_child(debug: Option<&str>, case: &str) -> Output {
    let mut command = Command::new(std::env::current_exe().expect("find test binary"));
    command
        .arg("--exact")
        .arg("unresolved_call_child")
        .arg("--nocapture")
        .env("IN_TRACE_CHILD", case)
        .env_remove("IN_DEBUG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(value) = debug {
        command.env("IN_DEBUG", value);
    }
    let mut child = command.spawn().expect("spawn unresolved-call child");
    let started = Instant::now();
    while child
        .try_wait()
        .expect("poll unresolved-call child")
        .is_none()
    {
        if started.elapsed() > Duration::from_secs(10) {
            child
                .kill()
                .expect("terminate timed-out unresolved-call child");
            let _ = child.wait_with_output();
            panic!("unresolved-call child exceeded ten seconds");
        }
        thread::sleep(Duration::from_millis(10));
    }
    child
        .wait_with_output()
        .expect("collect unresolved-call child")
}

#[test]
fn traces_are_opt_in_and_bounded_while_traps_remain_visible() {
    for (debug, case, expected_traces) in [
        (None, "one", 0),
        (Some("0"), "one", 0),
        (Some("1"), "one", 1),
        (Some("1"), "many", 64),
    ] {
        let output = run_child(debug, case);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(70), "stderr: {stderr}");
        assert!(stderr.contains("IN3002"), "stderr: {stderr}");
        assert!(stderr.contains(TARGET), "stderr: {stderr}");
        assert!(!stderr.contains("all fns:"), "stderr: {stderr}");
        assert_eq!(
            stderr.matches(TRACE).count(),
            expected_traces,
            "stderr: {stderr}"
        );
        assert_eq!(
            stderr.contains("further external-call traces suppressed"),
            case == "many",
            "stderr: {stderr}"
        );
        assert!(
            stderr.len() < 8_192,
            "trace output was {} bytes",
            stderr.len()
        );
    }
}
