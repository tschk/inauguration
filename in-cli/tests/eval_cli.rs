#![cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
))]

use std::process::Command;

#[test]
fn eval_preserves_zero_unit_prints_and_missing_values() {
    let socket =
        std::env::temp_dir().join(format!("in-eval-test-{}-absent.sock", std::process::id()));
    assert!(!socket.exists());
    for (parser, source, status, stdout, error) in [
        ("rust", "0", 0, "0\n", ""),
        ("javascript", "1.5", 0, "1.5\n", ""),
        ("javascript", "1.5e2", 0, "150\n", ""),
        ("javascript", "1.5 + 2.5", 0, "4\n", ""),
        ("javascript", "-1.5", 0, "-1.5\n", ""),
        ("rust", "fn main(){}", 0, "", ""),
        ("javascript", "print(\"hi\")", 0, "hi", ""),
        ("javascript", "console.log(\"hi\")", 0, "hi", ""),
        ("nim", "1 + 2", 1, "", "eval expression produced no value"),
        ("javascript", "1 +", 1, "", ""),
        ("rust", "struct Empty{}", 1, "", "module has no functions"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_in"))
            .env("IN_DAEMON_SOCKET", &socket)
            .args(["eval", "--parser", parser, "--", source])
            .output()
            .expect("execute eval");
        assert_eq!(
            output.status.code(),
            Some(status),
            "{parser}: {source}: {output:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            stdout,
            "{parser}: {source}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(error),
            "{parser}: {source}: {output:?}"
        );
    }
}

fn eval_with_fake_daemon(
    args: &[&str],
    response: serde_json::Value,
) -> (std::process::Output, Vec<serde_json::Value>) {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    use std::time::{Duration, Instant};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let socket = std::env::temp_dir().join(format!(
        "in-fake-eval-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let listener = UnixListener::bind(&socket).expect("bind isolated eval socket");
    listener.set_nonblocking(true).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let server_done = done.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut requests = Vec::new();
        while !server_done.load(Ordering::Acquire) && Instant::now() < deadline {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept fake daemon request: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let answer = if request["command"] == "ping" {
                serde_json::json!({"success":true})
            } else {
                assert_eq!(request["command"], "eval");
                requests.push(request);
                response.clone()
            };
            writeln!(stream, "{answer}").unwrap();
        }
        requests
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_in"))
        .env("IN_DAEMON_SOCKET", &socket)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn isolated eval client");
    let deadline = Instant::now() + Duration::from_secs(15);
    let timed_out = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break true;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let output = child.wait_with_output().unwrap();
    done.store(true, Ordering::Release);
    let requests = server.join().unwrap();
    std::fs::remove_file(socket).unwrap();
    assert!(!timed_out, "eval client exceeded deadline: {output:?}");
    (output, requests)
}

#[test]
fn daemon_receives_resolved_parser_for_inferred_and_auto_sources() {
    for (source, expected_parser) in [
        ("fn main()->i64{42}", "rust"),
        ("function main(){return 1.5;}", "javascript"),
    ] {
        for automatic in [false, true] {
            let args = if automatic {
                vec!["eval", "--parser", "auto", source]
            } else {
                vec!["eval", source]
            };
            let (output, requests) = eval_with_fake_daemon(
                &args,
                serde_json::json!({"success":true,"result":42,"eval_result":{"Int":42}}),
            );
            assert!(output.status.success(), "{output:?}");
            assert_eq!(requests.len(), 1, "{source}: {requests:?}");
            assert_eq!(requests[0]["parser"], expected_parser);
            assert_eq!(requests[0]["code"], source);
        }
    }
}

#[test]
fn daemon_typed_and_legacy_results_render_without_replaying_requests() {
    for (response, expected) in [
        (
            serde_json::json!({"success":true,"result":0,"eval_result":{"Int":0}}),
            "0\n",
        ),
        (
            serde_json::json!({"success":true,"result":1,"eval_result":{"Bool":true}}),
            "true\n",
        ),
        (
            serde_json::json!({"success":true,"eval_result":{"Float":1.5}}),
            "1.5\n",
        ),
        (
            serde_json::json!({"success":true,"eval_result":{"Str":"héllo"}}),
            "héllo\n",
        ),
        (serde_json::json!({"success":true,"result":0}), "0\n"),
    ] {
        let (output, requests) = eval_with_fake_daemon(&["eval", "--parser", "in", "1"], response);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
        assert_eq!(requests.len(), 1, "{requests:?}");
    }
}

#[test]
fn daemon_missing_expression_value_is_an_error_without_statement_retry() {
    let (output, requests) = eval_with_fake_daemon(
        &["eval", "--parser", "in", "1 + 2"],
        serde_json::json!({"success":true}),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("eval expression produced no value"));
    assert_eq!(
        requests.len(),
        1,
        "statement fallback replayed expression: {requests:?}"
    );
}
