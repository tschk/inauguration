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
        ("nim", "1 + 2", 0, "3\n", ""),
        ("nim", "echo \"hi\"", 0, "hi\n", ""),
        ("nim", "echo 42", 0, "42\n", ""),
        ("nim", "echo\"hi\"", 0, "hi\n", ""),
        ("nim", "1 + 2 * 3", 0, "7\n", ""),
        ("nim", "1 / 2", 0, "0.5\n", ""),
        ("nim", "1 + 1 / 2", 0, "1.5\n", ""),
        ("nim", "\"a\" & \"b\"", 0, "ab\n", ""),
        ("nim", "true", 0, "true\n", ""),
        ("nim", "1 < 2", 0, "true\n", ""),
        ("nim", "\"hi\"", 0, "hi\n", ""),
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
            stream.set_nonblocking(false).unwrap();
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
        serde_json::json!({"success":true,"execution_started":true}),
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

#[test]
fn daemon_replays_source_output_before_value_and_error_without_retry() {
    for (response, success, stdout) in [
        (
            serde_json::json!({"success":true,"output":"hé\0","eval_result":{"Int":42}}),
            true,
            "hé\x0042\n",
        ),
        (
            serde_json::json!({"success":false,"output":"before","execution_started":true,"error":"program failed"}),
            false,
            "before",
        ),
    ] {
        let (output, requests) =
            eval_with_fake_daemon(&["eval", "--parser", "in", "1+2"], response);
        assert_eq!(output.status.success(), success, "{output:?}");
        assert_eq!(output.stdout, stdout.as_bytes());
        assert_eq!(
            requests.len(),
            1,
            "printed request was replayed: {requests:?}"
        );
        if !success {
            assert!(String::from_utf8_lossy(&output.stderr).contains("program failed"));
        }
    }
}

#[test]
fn daemon_runtime_error_without_output_is_not_replayed() {
    let (output, requests) = eval_with_fake_daemon(
        &["eval", "--parser", "in", "1+2"],
        serde_json::json!({"success":false,"execution_started":true,"error":"runtime failed"}),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        requests.len(),
        1,
        "runtime error was replayed: {requests:?}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("runtime failed"));
}

#[test]
fn daemon_preexecution_error_retains_statement_fallback() {
    let (output, requests) = eval_with_fake_daemon(
        &["eval", "--parser", "in", "1+2"],
        serde_json::json!({"success":false,"execution_started":false,"error":"compile failed"}),
    );
    assert!(!output.status.success());
    assert_eq!(
        requests.len(),
        2,
        "compile refusal lost fallback: {requests:?}"
    );
}

#[test]
fn local_runtime_error_does_not_reexecute_effectful_expression() {
    let directory = std::env::temp_dir().join(format!("in-eval-once-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let trace = directory.join("trace");
    std::fs::write(&trace, "").unwrap();
    let path = serde_json::to_string(trace.to_str().unwrap()).unwrap();
    let source = format!("{{write-file({path},read-file({path})+\"x\");throw 7;0}}");
    let output = Command::new(env!("CARGO_BIN_EXE_in"))
        .env("IN_DAEMON_SOCKET", directory.join("absent.sock"))
        .args(["eval", "--parser", "in", &source])
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(&trace).unwrap(),
        "x",
        "executed expression retried: {output:?}"
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn daemon_replays_lossless_bytes_once_before_value_or_error() {
    for success in [true, false] {
        let response = if success {
            serde_json::json!({"success":true,"execution_started":true,"output_bytes":[187],"output":"must not duplicate","eval_result":{"Int":42}})
        } else {
            serde_json::json!({"success":false,"execution_started":true,"output_bytes":[187],"error":"runtime failed"})
        };
        let (output, requests) =
            eval_with_fake_daemon(&["eval", "--parser", "in", "1+2"], response);
        assert_eq!(output.status.success(), success);
        let mut expected = vec![0xbb];
        if success {
            expected.extend_from_slice(b"42\n");
        }
        assert_eq!(output.stdout, expected);
        assert_eq!(requests.len(), 1, "raw output was replayed: {requests:?}");
    }
}

#[test]
fn eval_rejects_compile_only_aggregate_but_preserves_void_execution() {
    let socket = std::env::temp_dir().join(format!(
        "in-aggregate-eval-{}-absent.sock",
        std::process::id()
    ));
    assert!(!socket.exists());
    let aggregate = "struct Pair{x:i64}fn main()->Pair{println!(\"effect\");Pair{x:42}}";
    let output = Command::new(env!("CARGO_BIN_EXE_in"))
        .env("IN_DAEMON_SOCKET", &socket)
        .args(["eval", "--parser", "rust", aggregate])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("compiled but was not executed"));
    let output = Command::new(env!("CARGO_BIN_EXE_in"))
        .env("IN_DAEMON_SOCKET", &socket)
        .args([
            "eval",
            "--parser",
            "rust",
            "fn main(){println!(\"effect\");}",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"effect");
}

#[test]
fn daemon_eval_requires_proven_execution_and_never_replays_legacy_unknown() {
    for (response, success, stdout, error) in [
        (
            serde_json::json!({"success":true,"execution_started":true}),
            true,
            "",
            "",
        ),
        (
            serde_json::json!({"success":true,"execution_started":false}),
            false,
            "",
            "compiled but was not executed",
        ),
        (
            serde_json::json!({"success":true}),
            false,
            "",
            "execution status unavailable",
        ),
        (serde_json::json!({"success":true,"result":0}), true, "", ""),
        (
            serde_json::json!({"success":true,"output":"before"}),
            false,
            "before",
            "execution status unavailable",
        ),
        (
            serde_json::json!({"success":false,"error":"legacy runtime failed"}),
            false,
            "",
            "legacy runtime failed",
        ),
    ] {
        let (output, requests) = eval_with_fake_daemon(
            &["eval", "--parser", "in", "fn main()->Void{return;}"],
            response,
        );
        assert_eq!(output.status.success(), success, "{output:?}");
        assert_eq!(output.stdout, stdout.as_bytes());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(error),
            "{output:?}"
        );
        assert_eq!(requests.len(), 1, "{requests:?}");
    }
    let (output, requests) = eval_with_fake_daemon(
        &["eval", "--parser", "in", "1+2"],
        serde_json::json!({"success":false,"error":"legacy unknown failed"}),
    );
    assert!(!output.status.success());
    assert_eq!(
        requests.len(),
        1,
        "unknown execution state was replayed: {requests:?}"
    );
}

#[test]
fn c_projection_promotes_c_family_shifts_without_coercing_in_operands() {
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "in-c-projection-fronts-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    for (index, (extension, source)) in [
        ("c", "int shift(_Bool value,int count){return value<<count;}int main(){return shift(1,1)+40;}"),
        ("c", "int shift(int value,_Bool count){return value<<count;}int main(){return shift(21,1);}"),
        ("cpp", "int shift(bool value,int count){return value<<count;}int main(){return shift(true,1)+40;}"),
        ("cpp", "int shift(int value,bool count){return value<<count;}int main(){return shift(21,true);}"),
        ("cpp", "int shift(bool value,bool count){return value<<count;}int main(){return shift(true,true)+40;}"),
        ("m", "int shift(_Bool value,int count){return value<<count;}int main(){return shift(1,1)+40;}"),
        ("mm", "int shift(bool value,int count){return value<<count;}int main(){return shift(true,1)+40;}"),
        ("rs", "fn shift(value:u32,count:u32)->u32{value<<count}fn main()->i64{shift(21,1) as i64}"),
    ].into_iter().enumerate() {
        let directory = root.join(index.to_string()); std::fs::create_dir(&directory).unwrap();
        let input = directory.join(format!("input.{extension}"));
        let output = directory.join("output.c"); let object = directory.join("output.o");
        let wrapper = directory.join("wrapper.c"); let executable = directory.join("program");
        std::fs::write(&input, source).unwrap();
        let projected = Command::new(env!("CARGO_BIN_EXE_in")).args(["compile","--path"]).arg(&input).args(["--emit","c","--out"]).arg(&output).output().unwrap();
        assert!(projected.status.success(), "{extension} {source}: {}", String::from_utf8_lossy(&projected.stderr));
        let compiled = Command::new("clang").args(["-std=gnu11","-O2","-Dmain=inauguration_main","-c"]).arg(&output).arg("-o").arg(&object).output().unwrap();
        assert!(compiled.status.success(), "{extension}: {}", String::from_utf8_lossy(&compiled.stderr));
        std::fs::write(&wrapper, "#include <stdint.h>\nint64_t inauguration_main(void);int main(void){return inauguration_main()==42?42:7;}\n").unwrap();
        let linked = Command::new("clang").arg(&object).arg(&wrapper).arg("-o").arg(&executable).output().unwrap();
        assert!(linked.status.success(), "{}", String::from_utf8_lossy(&linked.stderr));
        let executed = Command::new(&executable).output().unwrap();
        assert_eq!(executed.status.code(), Some(42), "{extension}: {source}");
        assert!(executed.stdout.is_empty() && executed.stderr.is_empty());
    }
    let invalid = root.join("invalid.in");
    let output = root.join("invalid.c");
    std::fs::write(&invalid,"fn shift(value:Bool,count:Int)->Int{return value<<count;}fn main()->Int{return shift(true,1);}").unwrap();
    let rejected = Command::new(env!("CARGO_BIN_EXE_in"))
        .args(["compile", "--path"])
        .arg(&invalid)
        .args(["--emit", "c", "--out"])
        .arg(&output)
        .output()
        .unwrap();
    assert!(!rejected.status.success() && !output.exists());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("word shift requires resolved Int operands")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn c_projection_rejects_nested_invalid_shift_operands_before_writing_source() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("in-c-nested-shifts-{}-{stamp}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let input = root.join("invalid.in");
    let output = root.join("invalid.c");
    for op in ["<<", ">>"] {
        for invalid in ["(\"hello\"+1)", "(true+20)"] {
            for side in [0usize, 1] {
                let expression = if side == 0 {
                    format!("{invalid}{op}1")
                } else {
                    format!("84{op}{invalid}")
                };
                std::fs::write(&input, format!("fn main()->Int{{return {expression};}}\n"))
                    .unwrap();
                let result = Command::new(env!("CARGO_BIN_EXE_in"))
                    .args(["compile", "--path"])
                    .arg(&input)
                    .args(["--emit", "c", "--out"])
                    .arg(&output)
                    .output()
                    .unwrap();
                assert!(
                    !result.status.success() && !output.exists(),
                    "{expression}: {result:?}"
                );
                assert!(
                    String::from_utf8_lossy(&result.stderr)
                        .contains("word shift requires resolved Int operands"),
                    "{expression}: {result:?}"
                );
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn c_projection_rejects_invalid_unsigned_operands_before_writing_source() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "in-c-unsigned-proof-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let input = root.join("invalid.icore");
    let output = root.join("invalid.c");
    for op in ["udiv", "urem"] {
        for bad in [
            serde_json::json!({"kind":"float","value":2.5}),
            serde_json::json!(true),
            serde_json::json!("hello"),
            serde_json::json!({"kind":"binary","op":"+","lhs":"hello","rhs":1}),
            serde_json::json!({"kind":"binary","op":"+","lhs":true,"rhs":1}),
        ] {
            for side in [0usize, 1] {
                let mut args = [serde_json::json!(84), serde_json::json!(2)];
                args[side] = bad.clone();
                let module = serde_json::json!({"icoreVersion":2,"decls":[{"kind":"function","name":"main","params":[],"return":"Int","body":[{"kind":"return","value":{"kind":"binary","op":op,"lhs":args[0],"rhs":args[1]}}]}]});
                std::fs::write(&input, serde_json::to_string(&module).unwrap()).unwrap();
                let result = Command::new(env!("CARGO_BIN_EXE_in"))
                    .args(["compile", "--path"])
                    .arg(&input)
                    .args(["--emit", "c", "--out"])
                    .arg(&output)
                    .output()
                    .unwrap();
                assert!(
                    !result.status.success() && !output.exists(),
                    "{op} {side} {bad}: {result:?}"
                );
                assert!(
                    String::from_utf8_lossy(&result.stderr)
                        .contains("unsigned division requires resolved Int operands"),
                    "{result:?}"
                );
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn c_projection_rejects_invalid_complement_operands_before_writing_source() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "in-c-complement-proof-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&root).unwrap();
    let input = root.join("invalid.in");
    let output = root.join("invalid.c");
    for operand in [
        "true",
        "false",
        "1.5",
        "\"hello\"",
        "unknown",
        "(true+1)",
        "(\"hello\"+1)",
    ] {
        std::fs::write(&input, format!("fn main()->Int{{return ~{operand};}}\n")).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_in"))
            .args(["compile", "--path"])
            .arg(&input)
            .args(["--emit", "c", "--out"])
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            !result.status.success() && !output.exists(),
            "{operand}: {result:?}"
        );
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("bitwise complement requires a resolved Int operand"),
            "{result:?}"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
