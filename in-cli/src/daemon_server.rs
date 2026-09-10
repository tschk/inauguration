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
    crate::config::env_config()
        .daemon_socket
        .clone()
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timing_us: Option<u128>,
}

fn write_response(stream: &mut UnixStream, response: &DaemonResponse) -> std::io::Result<()> {
    let json = serde_json::to_string(response)?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn parser_from_cli(s: Option<&str>) -> ParserCli {
    match s {
        Some("auto") | None => ParserCli::Auto,
        Some("in") => ParserCli::In,
        Some("icore") => ParserCli::Icore,
        Some(other) => {
            eprintln!("[in daemon] unsupported parser '{other}', defaulting to auto");
            ParserCli::Auto
        }
    }
}

fn target_from_cli(s: Option<&str>) -> CompileTarget {
    match s {
        Some("native") => CompileTarget::Native,
        Some("jit") | Some(_) | None => CompileTarget::Jit,
    }
}

fn handle_eval(code: &str, parser: Option<&str>, _verbose: bool) -> DaemonResponse {
    let start = Instant::now();
    let dir = std::env::temp_dir().join(format!(
        "inaug-daemon-eval-{}-{}",
        std::process::id(),
        start.elapsed().as_nanos()
    ));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return DaemonResponse {
            success: false,
            result: None,
            report_json: None,
            output: None,
            error: Some(format!("create eval dir: {e}")),
            timing_us: None,
        };
    }
    let parser_id = parser_from_cli(parser);
    let ext = if parser_id == ParserCli::Icore {
        "icore"
    } else {
        "in"
    };
    let path = dir.join(format!("eval.{ext}"));
    if let Err(e) = std::fs::write(&path, code) {
        let _ = std::fs::remove_dir_all(&dir);
        return DaemonResponse {
            success: false,
            result: None,
            report_json: None,
            output: None,
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
    let report = compile_owned(&request);
    let _ = std::fs::remove_dir_all(&dir);
    let timing_us = start.elapsed().as_micros();
    if report.success {
        DaemonResponse {
            success: true,
            result: report.eval_result,
            report_json: None,
            output: None,
            error: None,
            timing_us: Some(timing_us),
        }
    } else {
        DaemonResponse {
            success: false,
            result: None,
            report_json: None,
            output: None,
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
        report_json,
        output: None,
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
                report_json: None,
                output: None,
                error: None,
                timing_us: None,
            },
            false,
        ),
        DaemonRequest::Stop => (
            DaemonResponse {
                success: true,
                result: None,
                report_json: None,
                output: None,
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
                report_json: None,
                output: None,
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
    fn test_parser_from_cli() {
        assert_eq!(parser_from_cli(None), ParserCli::Auto);
        assert_eq!(parser_from_cli(Some("auto")), ParserCli::Auto);
        assert_eq!(parser_from_cli(Some("in")), ParserCli::In);
        assert_eq!(parser_from_cli(Some("icore")), ParserCli::Icore);
        assert_eq!(parser_from_cli(Some("unsupported")), ParserCli::Auto);
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
