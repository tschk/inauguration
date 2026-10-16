//! Persistent compiler daemon.
//!
//! Keeps the compiler process alive so that repeated `in eval` / `in compile`
//! invocations skip the ~30 ms binary startup cost. The daemon listens on a
//! Unix domain socket and accepts JSON requests.

use crate::owned_compile::{CompileTarget, OwnedCompileRequest, compile_owned};
use crate::parser_registry::ParserCli;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const DEFAULT_SOCKET_NAME: &str = "inauguration-daemon.sock";

pub fn default_socket_path() -> PathBuf {
    resolve_socket_path(crate::config::env_config().daemon_socket.as_ref())
}

pub(crate) fn resolve_socket_path(daemon_socket: Option<&PathBuf>) -> PathBuf {
    daemon_socket
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join(DEFAULT_SOCKET_NAME))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonRequest {
    Eval {
        code: String,
        #[serde(default)]
        parser: Option<String>,
        #[serde(default)]
        verbose: bool,
    },
    Compile {
        path: PathBuf,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        entry: Option<String>,
        #[serde(default)]
        out: Option<PathBuf>,
        #[serde(default)]
        module_id: Option<String>,
        #[serde(default)]
        target_triple: Option<String>,
        #[serde(default)]
        linkage: Option<String>,
        #[serde(default)]
        jobs: Option<usize>,
    },
    Ping,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<i64>,
    #[serde(
        default,
        with = "daemon_eval_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub eval_result: Option<crate::owned_compile::EvalValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_started: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Lossless fallback when hosted output is not valid UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing_us: Option<u128>,
}

// Keep the existing EvalValue JSON for ordinary values; JSON cannot represent
// nonfinite f64 values, so only those use an exact bit-preserving payload.
mod daemon_eval_value {
    use crate::owned_compile::EvalValue;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    enum FloatBits {
        FloatBits(u64),
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Bits(FloatBits),
        Value(EvalValue),
    }
    pub(super) fn serialize<S: Serializer>(
        value: &Option<EvalValue>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(EvalValue::Float(value)) if !value.is_finite() => {
                FloatBits::FloatBits(value.to_bits()).serialize(serializer)
            }
            _ => value.serialize(serializer),
        }
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<EvalValue>, D::Error> {
        Ok(
            Option::<Wire>::deserialize(deserializer)?.map(|value| match value {
                Wire::Bits(FloatBits::FloatBits(bits)) => EvalValue::Float(f64::from_bits(bits)),
                Wire::Value(value) => value,
            }),
        )
    }
}

fn create_eval_dir() -> std::io::Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    for _ in 0..128 {
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "inaug-daemon-eval-{}-{sequence}",
            std::process::id()
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not reserve unique eval directory",
    ))
}

fn write_response(stream: &mut UnixStream, response: &DaemonResponse) -> std::io::Result<()> {
    let json = serde_json::to_string(response)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn target_from_cli(s: Option<&str>) -> CompileTarget {
    match s {
        Some("native") => CompileTarget::Native,
        Some("jit") | Some(_) | None => CompileTarget::Jit,
    }
}

fn handle_eval(code: &str, parser: Option<&str>, _verbose: bool) -> DaemonResponse {
    let start = Instant::now();
    let dir = match create_eval_dir() {
        Ok(dir) => dir,
        Err(error) => {
            return DaemonResponse {
                success: false,
                result: None,
                eval_result: None,
                execution_started: Some(false),
                report_json: None,
                output: None,
                output_bytes: None,
                error: Some(format!("create eval dir: {error}")),
                timing_us: None,
            };
        }
    };
    let source_parser = match parser {
        None | Some("auto") => crate::parser_registry::ParserId::In,
        Some(name) => match crate::parser_registry::parser_id_from_cli_token(name) {
            Some(parser) => parser,
            None => {
                let _ = std::fs::remove_dir_all(&dir);
                return DaemonResponse {
                    success: false,
                    result: None,
                    eval_result: None,
                    execution_started: Some(false),
                    report_json: None,
                    output: None,
                    output_bytes: None,
                    error: Some(format!("unsupported eval parser `{name}`")),
                    timing_us: None,
                };
            }
        },
    };
    let parser_id = match source_parser {
        crate::parser_registry::ParserId::In => ParserCli::In,
        crate::parser_registry::ParserId::Icore => ParserCli::Icore,
        _ => ParserCli::Auto,
    };
    let ext = source_parser.default_extension();
    let path = dir.join(format!("eval.{ext}"));
    if let Err(e) = std::fs::write(&path, code) {
        let _ = std::fs::remove_dir_all(&dir);
        return DaemonResponse {
            success: false,
            result: None,
            eval_result: None,
            execution_started: Some(false),
            report_json: None,
            output: None,
            output_bytes: None,
            error: Some(format!("write eval source: {e}")),
            timing_us: None,
        };
    }
    let request = OwnedCompileRequest {
        path: path.clone(),
        module_id: "App".to_string(),
        parser: parser_id,
        target: CompileTarget::Jit,
        entry: Some("main".to_string()),
        out: None,
        linkage: crate::native_emit::NativeLinkage::Executable,
        target_triple: None,
        jobs: 1,
        debug: false,
        profile: crate::emit_profile::EmitProfile::Default,
        emit: None,
        base: None,
    };
    let (report, output) = crate::native_stdlib::capture_print_output(|| compile_owned(&request));
    let (output, output_bytes) = if output.is_empty() {
        (None, None)
    } else {
        match String::from_utf8(output) {
            Ok(text) => (Some(text), None),
            Err(error) => (None, Some(error.into_bytes())),
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    let timing_us = start.elapsed().as_micros();
    if report.success {
        DaemonResponse {
            success: true,
            result: report.eval_result.as_ref().and_then(|value| value.as_int()),
            eval_result: report.eval_result.clone(),
            execution_started: Some(report.execution_started),
            report_json: None,
            output,
            output_bytes,
            error: None,
            timing_us: Some(timing_us),
        }
    } else {
        DaemonResponse {
            success: false,
            result: None,
            eval_result: None,
            execution_started: Some(report.execution_started),
            report_json: None,
            output,
            output_bytes,
            error: Some(report.error.unwrap_or_else(|| "eval failed".to_string())),
            timing_us: Some(timing_us),
        }
    }
}

fn handle_compile(
    path: &Path,
    target: CompileTarget,
    entry: Option<String>,
    out: Option<PathBuf>,
    module_id: Option<String>,
    target_triple: Option<String>,
    linkage: crate::native_emit::NativeLinkage,
    jobs: usize,
) -> DaemonResponse {
    let start = Instant::now();
    let request = OwnedCompileRequest {
        path: path.to_path_buf(),
        module_id: module_id.unwrap_or_else(|| "App".to_string()),
        parser: ParserCli::Auto,
        target,
        entry,
        out,
        linkage,
        target_triple,
        jobs: jobs.max(1),
        debug: false,
        profile: crate::emit_profile::EmitProfile::Default,
        emit: None,
        base: None,
    };
    let report = compile_owned(&request);
    let timing_us = start.elapsed().as_micros();
    let report_json = crate::owned_compile::report_to_json(&report).ok();
    DaemonResponse {
        success: report.success,
        result: None,
        eval_result: None,
        execution_started: Some(report.execution_started),
        report_json,
        output: None,
        output_bytes: None,
        error: report.error,
        timing_us: Some(timing_us),
    }
}

fn handle_request(request: DaemonRequest) -> (DaemonResponse, bool) {
    match request {
        DaemonRequest::Ping => (
            DaemonResponse {
                success: true,
                result: None,
                eval_result: None,
                execution_started: Some(false),
                report_json: None,
                output: None,
                output_bytes: None,
                error: None,
                timing_us: None,
            },
            false,
        ),
        DaemonRequest::Stop => (
            DaemonResponse {
                success: true,
                result: None,
                eval_result: None,
                execution_started: Some(false),
                report_json: None,
                output: None,
                output_bytes: None,
                error: None,
                timing_us: None,
            },
            true,
        ),
        DaemonRequest::Eval {
            code,
            parser,
            verbose,
        } => (handle_eval(&code, parser.as_deref(), verbose), false),
        DaemonRequest::Compile {
            path,
            target,
            entry,
            out,
            module_id,
            target_triple,
            linkage,
            jobs,
        } => {
            let target = target_from_cli(target.as_deref());
            let linkage = match linkage.as_deref() {
                Some("staticlib") | Some("static") | Some("static-lib") => {
                    crate::native_emit::NativeLinkage::StaticLib
                }
                Some("dylib") | Some("dynamic") => crate::native_emit::NativeLinkage::Dylib,
                _ => crate::native_emit::NativeLinkage::Executable,
            };
            (
                handle_compile(
                    &path,
                    target,
                    entry,
                    out,
                    module_id,
                    target_triple,
                    linkage,
                    jobs.unwrap_or(1),
                ),
                false,
            )
        }
    }
}

fn handle_client(mut stream: UnixStream) -> std::io::Result<bool> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let line = line.trim();
    if line.is_empty() {
        return Ok(false);
    }
    let (response, stop) = match serde_json::from_str::<DaemonRequest>(line) {
        Ok(request) => handle_request(request),
        Err(e) => (
            DaemonResponse {
                success: false,
                result: None,
                eval_result: None,
                execution_started: Some(false),
                report_json: None,
                output: None,
                output_bytes: None,
                error: Some(format!("invalid request: {e}")),
                timing_us: None,
            },
            false,
        ),
    };
    write_response(&mut stream, &response)?;
    Ok(stop)
}

pub fn run_compiler_daemon(socket_path: &Path) -> std::io::Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    eprintln!("[in daemon] listening on {}", socket_path.display());
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => match handle_client(stream) {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => eprintln!("[in daemon] client error: {e}"),
            },
            Err(e) => {
                eprintln!("[in daemon] accept error: {e}");
            }
        }
    }
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(daemon_pid_path(socket_path));
    Ok(())
}

pub fn daemon_pid_path(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("pid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn test_default_socket_path() {
        let path = default_socket_path();
        assert!(path.ends_with(DEFAULT_SOCKET_NAME));
    }

    #[test]
    fn test_resolve_socket_path_default() {
        let path = resolve_socket_path(None);
        assert_eq!(path, std::env::temp_dir().join(DEFAULT_SOCKET_NAME));
    }

    #[test]
    fn test_resolve_socket_path_custom() {
        let custom_path = PathBuf::from("/tmp/custom.sock");
        let path = resolve_socket_path(Some(&custom_path));
        assert_eq!(path, custom_path);
    }

    #[test]
    fn test_target_from_cli() {
        assert_eq!(target_from_cli(None), CompileTarget::Jit);
        assert_eq!(target_from_cli(Some("jit")), CompileTarget::Jit);
        assert_eq!(target_from_cli(Some("native")), CompileTarget::Native);
        assert_eq!(target_from_cli(Some("unsupported")), CompileTarget::Jit);
    }

    #[test]
    fn responds_to_ping() -> std::io::Result<()> {
        let (mut client, server) = UnixStream::pair()?;
        client.write_all(b"{\"command\":\"ping\"}\n")?;
        handle_client(server)?;
        let mut response = String::new();
        client.read_to_string(&mut response)?;
        let response: DaemonResponse = serde_json::from_str(response.trim())?;
        assert!(response.success);
        assert!(response.error.is_none());
        Ok(())
    }

    #[test]
    fn reports_invalid_requests() -> std::io::Result<()> {
        let (mut client, server) = UnixStream::pair()?;
        client.write_all(b"invalid\n")?;
        handle_client(server)?;
        let mut response = String::new();
        client.read_to_string(&mut response)?;
        let response: DaemonResponse = serde_json::from_str(response.trim())?;
        assert!(!response.success);
        assert!(response.error.unwrap().contains("invalid request"));
        Ok(())
    }

    #[test]
    fn derives_pid_path() {
        assert_eq!(
            daemon_pid_path(Path::new("/tmp/in.sock")),
            Path::new("/tmp/in.pid")
        );
    }

    fn connect_with_retry(socket_path: &Path) -> std::io::Result<UnixStream> {
        let start = std::time::Instant::now();
        loop {
            if let Ok(stream) = UnixStream::connect(socket_path) {
                return Ok(stream);
            }
            if start.elapsed() > std::time::Duration::from_secs(2) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Failed to connect to daemon socket",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn test_run_compiler_daemon_setup_and_stop() -> std::io::Result<()> {
        let dir =
            std::env::temp_dir().join(format!("inaug-daemon-test-setup-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let socket_path = dir.join("test.sock");

        let path_clone = socket_path.clone();

        let t = std::thread::spawn(move || run_compiler_daemon(&path_clone));

        let mut client = connect_with_retry(&socket_path).expect("Could not connect");
        client.write_all(b"{\"command\":\"stop\"}\n")?;

        let mut response = String::new();
        client.read_to_string(&mut response)?;
        let response: DaemonResponse = serde_json::from_str(response.trim())?;
        assert!(response.success);

        t.join()
            .expect("daemon thread panicked")
            .expect("daemon error");

        assert!(!socket_path.exists());
        assert!(!daemon_pid_path(&socket_path).exists());
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn test_run_compiler_daemon_concurrent_connections() -> std::io::Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "inaug-daemon-test-concurrent-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)?;
        let socket_path = dir.join("test.sock");

        let path_clone = socket_path.clone();

        let t = std::thread::spawn(move || run_compiler_daemon(&path_clone));

        // Ensure daemon is ready
        let _ =
            connect_with_retry(&socket_path).expect("Could not connect to verify daemon readiness");

        let mut handles = vec![];
        for _ in 0..10 {
            let p = socket_path.clone();
            handles.push(std::thread::spawn(move || {
                let mut client = connect_with_retry(&p).expect("Client connect failed");
                client
                    .write_all(b"{\"command\":\"ping\"}\n")
                    .expect("Write failed");
                let mut response = String::new();
                client.read_to_string(&mut response).expect("Read failed");
                let response: DaemonResponse = serde_json::from_str(response.trim()).unwrap();
                assert!(response.success);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let mut client = connect_with_retry(&socket_path).expect("Could not connect to stop");
        client.write_all(b"{\"command\":\"stop\"}\n")?;

        t.join()
            .expect("daemon thread panicked")
            .expect("daemon error");

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}

#[cfg(test)]
mod typed_eval_response_tests {
    use super::*;
    use crate::owned_compile::EvalValue;

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    ))]
    fn typed_eval_values_keep_legacy_integer_field_and_unit_absence() {
        for (source, expected, legacy) in [
            (
                "fn main()->Int{return 0;}",
                Some(EvalValue::Int(0)),
                Some(0),
            ),
            (
                "fn main()->Bool{return true;}",
                Some(EvalValue::Bool(true)),
                Some(1),
            ),
            (
                "fn main()->Float{return 1.5;}",
                Some(EvalValue::Float(1.5)),
                None,
            ),
            (
                "fn main()->String{return \"hi\";}",
                Some(EvalValue::Str("hi".into())),
                None,
            ),
            ("fn main()->Void{}", None, None),
        ] {
            let response = handle_eval(source, Some("in"), false);
            assert!(response.success, "{source}: {:?}", response.error);
            assert_eq!(response.eval_result, expected, "{source}");
            assert_eq!(response.result, legacy);
            let wire = serde_json::to_string(&response).unwrap();
            let decoded: DaemonResponse = serde_json::from_str(&wire).unwrap();
            assert_eq!(decoded.eval_result, expected);
        }
    }
    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    ))]
    fn foreign_eval_routes_actual_rust_and_javascript_sources() {
        for (parser, source, expected) in [
            ("rust", "fn main()->i64{42}", Some(EvalValue::Int(42))),
            ("rust", "fn main()->f64{1.5}", Some(EvalValue::Float(1.5))),
            ("rust", "fn main()->bool{true}", Some(EvalValue::Bool(true))),
            (
                "rust",
                "fn main()->&'static str{\"hi\"}",
                Some(EvalValue::Str("hi".into())),
            ),
            ("rust", "fn main(){}", None),
            (
                "javascript",
                "function main(){return 1.5;}",
                Some(EvalValue::Float(1.5)),
            ),
            (
                "javascript",
                "function main(){return true;}",
                Some(EvalValue::Bool(true)),
            ),
            (
                "javascript",
                "function main(){return \"hi\";}",
                Some(EvalValue::Str("hi".into())),
            ),
            ("javascript", "function main(){}", None),
        ] {
            let response = handle_eval(source, Some(parser), false);
            assert!(response.success, "{parser} {source}: {:?}", response.error);
            assert_eq!(response.eval_result, expected, "{parser}: {source}");
        }
        let unknown = handle_eval("unused", Some("not-a-parser"), false);
        assert!(!unknown.success);
        assert!(unknown.error.unwrap().contains("unsupported eval parser"));
    }
    #[test]
    fn legacy_wire_missing_typed_field_still_deserializes() {
        let response: DaemonResponse =
            serde_json::from_str(r#"{"success":true,"result":0}"#).unwrap();
        assert_eq!(response.result, Some(0));
        assert_eq!(response.eval_result, None);
        assert_eq!(response.execution_started, None);
    }
    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    ))]
    fn errors_and_missing_results_do_not_invent_zero() {
        let missing = handle_eval(
            "struct Pair{x:i64}fn main()->Pair{Pair{x:42}}",
            Some("rust"),
            false,
        );
        assert!(missing.success);
        assert_eq!(missing.execution_started, Some(false));
        let tail = handle_eval("proc main(): int =\n  1 + 2", Some("nim"), false);
        assert!(tail.success, "{:?}", tail.error);
        assert_eq!(tail.eval_result, Some(EvalValue::Int(3)));
        assert_eq!(tail.execution_started, Some(true));
        assert_eq!(missing.eval_result, None);
        assert_eq!(missing.result, None);
        let error = handle_eval("fn main()->Int{return unknown();}", Some("in"), false);
        assert!(!error.success);
        assert_eq!(error.eval_result, None);
        assert_eq!(error.result, None);
    }
    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    ))]
    fn one_eval_request_executes_effectful_entry_once() {
        let response = handle_eval(
            "var count:Int=0;\nfn main()->Int{count+=1;if count==1{return 42;}return 7;}",
            Some("in"),
            false,
        );
        assert!(response.success, "{:?}", response.error);
        assert_eq!(response.eval_result, Some(EvalValue::Int(42)));
    }
    #[test]
    fn typed_float_wire_preserves_finite_zero_and_nonfinite_bits() {
        for bits in [
            0.0f64.to_bits(),
            (-0.0f64).to_bits(),
            1.5f64.to_bits(),
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            0x7ff8_0000_0000_0042,
            0xfff8_0000_0000_0012,
        ] {
            let value = f64::from_bits(bits);
            let response = DaemonResponse {
                success: true,
                result: None,
                eval_result: Some(EvalValue::Float(value)),
                execution_started: Some(false),
                report_json: None,
                output: None,
                output_bytes: None,
                error: None,
                timing_us: None,
            };
            let json = serde_json::to_string(&response).unwrap();
            assert_eq!(json.contains("FloatBits"), !value.is_finite());
            let decoded: DaemonResponse = serde_json::from_str(&json).unwrap();
            let Some(EvalValue::Float(actual)) = decoded.eval_result else {
                panic!("float response");
            };
            assert_eq!(actual.to_bits(), bits);
        }
    }
    #[test]
    fn eval_directories_are_distinct_live_reservations() {
        let first = create_eval_dir().unwrap();
        let second = create_eval_dir().unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        std::fs::remove_dir(first).unwrap();
        std::fs::remove_dir(second).unwrap();
    }
}

#[cfg(all(
    test,
    any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    )
))]
mod eval_output_tests {
    use super::*;
    #[test]
    fn daemon_execution_state_distinguishes_legacy_absence_from_false() {
        for (json, expected) in [
            (r#"{"success":true}"#, None),
            (r#"{"success":true,"execution_started":false}"#, Some(false)),
            (r#"{"success":true,"execution_started":true}"#, Some(true)),
        ] {
            let response: DaemonResponse = serde_json::from_str(json).unwrap();
            assert_eq!(response.execution_started, expected);
        }
    }

    #[test]
    fn hosted_print_output_is_carried_with_result_and_before_error() {
        let response = handle_eval(
            "fn main()->Int{print(\"hé\0\");print-int(-42);return 42;}",
            Some("in"),
            false,
        );
        assert!(response.success, "{:?}", response.error);
        assert_eq!(response.output.as_deref(), Some("hé\0-42"));
        assert_eq!(response.result, Some(42));
        assert_eq!(response.execution_started, Some(true));
        let response = handle_eval(
            "fn main()->Int{print(\"before\");throw 7;}",
            Some("in"),
            false,
        );
        assert!(!response.success);
        assert_eq!(response.execution_started, Some(true));
        assert_eq!(response.output.as_deref(), Some("before"));
        let response = handle_eval("fn main()->Int{throw 7;}", Some("in"), false);
        assert!(!response.success);
        assert_eq!(response.execution_started, Some(true));
        assert_eq!(response.output, None);
        let response = handle_eval("fn main()->Int{return unknown();}", Some("in"), false);
        assert!(!response.success);
        assert_eq!(response.execution_started, Some(false));
        let response = handle_eval(
            "struct Pair{x:i64}fn main()->Pair{Pair{x:42}}",
            Some("rust"),
            false,
        );
        assert!(response.success, "{:?}", response.error);
        assert_eq!(response.execution_started, Some(false));
        assert_eq!(response.eval_result, None);
    }
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn hosted_reserved_runtime_print_uses_the_same_sink() {
        let response = handle_eval(
            "fn main()->Int{__inrt_print(\"raw\");__inrt_print_int(42);return 42;}",
            Some("in"),
            false,
        );
        assert!(response.success, "{:?}", response.error);
        assert_eq!(response.output.as_deref(), Some("raw42"));
    }
    #[test]
    fn native_artifact_creation_does_not_start_entry_execution() {
        let directory = create_eval_dir().unwrap();
        let path = directory.join("native.in");
        std::fs::write(&path, "fn main()->Int{return 42;}").unwrap();
        let report = compile_owned(&OwnedCompileRequest {
            path,
            module_id: "OutputState".into(),
            parser: ParserCli::In,
            target: CompileTarget::Native,
            entry: Some("main".into()),
            out: Some(directory.join("native.o")),
            linkage: crate::native_emit::NativeLinkage::StaticLib,
            target_triple: Some("x86_64-unknown-none".into()),
            jobs: 1,
            debug: false,
            profile: crate::emit_profile::EmitProfile::Default,
            emit: None,
            base: None,
        });
        assert!(report.success, "{:?}", report.error);
        assert!(!report.execution_started);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn hosted_reserved_print_preserves_byte_slice_before_runtime_error() {
        for tail in ["return 42;", "throw 7;"] {
            let source = format!("fn main()->Int{{__inrt_print(str-slice(\"λ\",1,2));{tail}}}");
            let response = handle_eval(&source, Some("in"), false);
            assert_eq!(
                response.success,
                tail == "return 42;",
                "{:?}",
                response.error
            );
            assert_eq!(response.execution_started, Some(true));
            assert_eq!(response.output, None);
            assert_eq!(response.output_bytes, Some(vec![0xbb]));
            let wire = serde_json::to_string(&response).unwrap();
            let decoded: DaemonResponse = serde_json::from_str(&wire).unwrap();
            assert_eq!(decoded.output_bytes, Some(vec![0xbb]));
        }
    }
    #[test]
    fn compile_response_execution_state_matches_nested_report() {
        let directory = create_eval_dir().unwrap();
        let path = directory.join("compile.in");
        std::fs::write(&path, "fn main()->Int{return 42;}").unwrap();
        let response = handle_compile(
            &path,
            CompileTarget::Jit,
            Some("main".into()),
            None,
            None,
            None,
            crate::native_emit::NativeLinkage::Executable,
            1,
        );
        assert!(response.success, "{:?}", response.error);
        assert_eq!(response.execution_started, Some(true));
        let report: serde_json::Value =
            serde_json::from_str(response.report_json.as_ref().unwrap()).unwrap();
        assert_eq!(
            report["execution_started"].as_bool(),
            response.execution_started
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
