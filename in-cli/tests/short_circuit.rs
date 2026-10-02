#![cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]

//! Semantic regressions: isolate potentially faulting native code in subprocesses.
use inauguration::native_emit::inisa;
use inauguration::{
    core_ir::UnifiedModule, in_lang_parse::parse_in_source, jit_runtime::JitRuntime,
};
use std::process::{Command, Output, Stdio};

fn bounded_output(command: &mut Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child");
    let started = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if started.elapsed() > std::time::Duration::from_secs(10) {
            child.kill().expect("kill timed out child");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[cfg(target_arch = "aarch64")]
fn native(module: &UnifiedModule) -> i64 {
    use inauguration::native_emit::{NativeLinkage, lower};
    inauguration::native_emit::native_link::bootstrap_jit_native();
    let code = lower::lower_module(module, "main", NativeLinkage::Executable).unwrap();
    assert!(code.degradations.is_empty(), "{:?}", code.degradations);
    let functions = code
        .function_offsets
        .iter()
        .map(|(n, o)| (n.clone(), *o, code.code.len() as u32 - *o))
        .collect::<Vec<_>>();
    let mut rt = JitRuntime::new();
    rt.load(&code.code, &functions, &code.relocations).unwrap();
    unsafe { rt.invoke("main", &[]).unwrap() }
}

#[cfg(target_arch = "x86_64")]
fn native(module: &UnifiedModule) -> i64 {
    let code = inauguration::native_emit::x86_64_lower::lower_module(module, "main").unwrap();
    assert!(code.extern_calls.is_empty(), "unresolved extern calls");
    let functions = code
        .exports
        .iter()
        .map(|(n, o)| (n.clone(), *o, code.code.len() as u32 - *o))
        .collect::<Vec<_>>();
    let relocations = code
        .relocations
        .iter()
        .map(|o| (*o, code.codegen_base))
        .collect::<Vec<_>>();
    let mut rt = JitRuntime::new();
    rt.load_with_globals(
        &code.code,
        &functions,
        &relocations,
        &code.data,
        &code.global_relocs,
    )
    .unwrap();
    unsafe { rt.invoke("main", &[]).unwrap() }
}

fn cases() -> Vec<(&'static str, String, i64, bool)> {
    let mut cases = Vec::new();
    // The raw Core IR paths accept integer truth values and must normalize to 0/1.
    for (name, expression, expected) in [
        ("and_false", "0 && 7", 0),
        ("and_true", "2 && 7", 1),
        ("or_false", "0 || 0", 0),
        ("or_true", "2 || 0", 1),
        ("nested", "(0 || 8) && (9 || 0)", 1),
        ("high_bit", "4294967296 || 0", 1),
        ("rhs_zero", "2 && 0", 0),
        ("rhs_true", "0 || 9", 1),
    ] {
        cases.push((
            name,
            format!("fn main() -> Int {{ return {expression}; }}"),
            expected,
            true,
        ));
    }
    for (name, expression, expected) in [
        ("skip_and_effect", "false && touch(p, true)", 0),
        ("skip_or_effect", "true || touch(p, false)", 0),
        ("take_and_effect", "true && touch(p, true)", 1),
        ("take_or_effect", "false || touch(p, true)", 1),
        ("lhs_and_effect", "touch(p, true) && false", 1),
        ("lhs_or_effect", "touch(p, false) || true", 1),
        (
            "nested_skip_effect",
            "(false && touch(p, true)) || (true || touch(p, false))",
            0,
        ),
        (
            "nested_take_effect",
            "(true && touch(p, true)) && (false || touch(p, true))",
            2,
        ),
    ] {
        cases.push((name, format!("fn touch(p: Int, value: Bool) -> Bool {{\n putchar(126);\n return value;\n}}\nfn main() -> Int {{\n let p = 0;\n let result = {expression};\n return 42;\n}}"), expected, false));
    }
    for (name, expression) in [
        ("skip_and_fault", "false && (5 / zero == 1)"),
        ("skip_or_fault", "true || (5 / zero == 1)"),
    ] {
        cases.push((
            name,
            format!(
                "fn main() -> Int {{\n let zero = 0;\n if {expression} {{ return 7; }}\n return 42;\n}}"
            ),
            if name == "skip_or_fault" { 7 } else { 42 },
            true,
        ));
    }
    for (name, op, a, b, expected) in [
        ("order_take_and", "&&", 2, 9, 1),
        ("order_take_or", "||", 0, -7, 1),
        ("order_skip_and", "&&", 0, 9, 0),
        ("order_skip_or", "||", 4294967296i64, 0, 1),
        ("order_false_rhs", "&&", -7, 0, 0),
    ] {
        cases.push((name, format!("fn marked(code: Int, value: Int) -> Int {{\n putchar(code);\n return value;\n}}\nfn choose(a: Int, b: Int) -> Int {{\n return marked(64, a) {op} marked(37, b);\n}}\nfn main() -> Int {{\n return choose({a}, {b});\n}}"), expected, true));
    }
    for (name, expression, expected) in [
        ("runtime_nested", "a && (b || c)", 1),
        ("runtime_zero", "a && (c && b)", 0),
        ("runtime_rhs_call", "a && identity(identity(b))", 1),
        ("runtime_or_call", "c || identity(identity(b))", 1),
    ] {
        cases.push((name, format!("fn identity(value: Int) -> Int {{ return value; }}\nfn choose(a: Int, b: Int, c: Int) -> Int {{\n return {expression};\n}}\nfn main() -> Int {{ return choose(4294967296, -7, 0); }}"), expected, true));
    }
    cases
}

#[test]
fn short_circuit_child() {
    let Ok(selected) = std::env::var("IN_SHORT_CIRCUIT_CASE") else {
        return;
    };
    let (_, source, expected, supported) = cases()
        .into_iter()
        .find(|(name, ..)| *name == selected)
        .unwrap();
    let mut module = parse_in_source(&source).expect("parse fixture");
    if std::env::var_os("IN_SHORT_CIRCUIT_OPTIMIZED").is_some() {
        inauguration::core_opt::optimize_with_entry(&mut module.decls, Some("main"));
    }
    assert_eq!(
        native(&module),
        if supported { expected } else { 42 },
        "{selected}"
    );
}

#[test]
fn short_circuit_native_subprocesses() {
    let mut failures = Vec::new();
    for optimized in [false, true] {
        for (name, _, expected, supported) in cases() {
            // This direct x86 loader does not link external I/O imports.
            if cfg!(target_arch = "x86_64") && (!supported || name.starts_with("order_")) {
                println!("skip x86 host-I/O case {name}");
                continue;
            }
            let mut cmd = Command::new(std::env::current_exe().unwrap());
            cmd.args(["--exact", "short_circuit_child", "--nocapture"])
                .env("IN_SHORT_CIRCUIT_CASE", name)
                .env_remove("IN_SHORT_CIRCUIT_OPTIMIZED");
            if optimized {
                cmd.env("IN_SHORT_CIRCUIT_OPTIMIZED", "1");
            }
            let output = bounded_output(&mut cmd);
            println!("{name} optimized={optimized}: {}", output.status);
            let effects = String::from_utf8_lossy(&output.stdout).matches('~').count() as i64;
            let markers: String = String::from_utf8_lossy(&output.stdout)
                .chars()
                .filter(|c| matches!(c, '@' | '%'))
                .collect();
            let expected_markers = if name.starts_with("order_skip") {
                "@"
            } else if name.starts_with("order_") {
                "@%"
            } else {
                ""
            };
            if !output.status.success()
                || (!supported && effects != expected)
                || markers != expected_markers
            {
                failures.push(format!(
                    "{name} optimized={optimized}: {} effects={effects} expected={expected} markers={markers:?} expected_markers={expected_markers:?}\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn short_circuit_inisa_results() {
    let mut failures = Vec::new();
    for (name, source, expected, supported) in cases() {
        if supported {
            let actual = inisa::eval_module(&parse_in_source(&source).unwrap(), "main");
            if actual != Ok(expected) {
                failures.push(format!("{name}: {actual:?}, expected {expected}"));
            }
        }
    }
    // A skipped RHS must not enter a divergent function, even in the interpreter.
    for expression in ["false && spin()", "true || spin()"] {
        let source = format!(
            "fn spin() -> Bool {{\n while true {{ }}\n return true;\n}}\nfn main() -> Int {{\n let result = {expression};\n return 42;\n}}"
        );
        let actual = inisa::eval_module(&parse_in_source(&source).unwrap(), "main");
        if actual != Ok(42) {
            failures.push(format!("{expression}: {actual:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn short_circuit_c_backend_reference() {
    let dir = std::env::temp_dir().join(format!("in-short-circuit-c-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, source, expected, supported) in cases() {
        let mut module = parse_in_source(&source).unwrap();
        // Give C a standard int main wrapper while retaining the emitted entry signature.
        for decl in &mut module.decls {
            if let inauguration::core_ir::Decl::Function { name, .. } = decl {
                if name == "main" {
                    *name = "case_main".into();
                }
            }
        }
        let code = inauguration::native_emit::c_backend::emit_c_module(&module).unwrap();
        let source_path = dir.join(format!("{name}.c"));
        let executable = dir.join(name);
        std::fs::write(
            &source_path,
            format!("#include <stdio.h>\n{code}\nint main(void) {{ return (int)case_main(); }}\n"),
        )
        .unwrap();
        let built = Command::new("cc")
            .args(["-std=c99", "-O0"])
            .arg(&source_path)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("installed C compiler");
        assert!(
            built.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&built.stderr)
        );
        let output = bounded_output(&mut Command::new(&executable));
        assert_eq!(
            output.status.code(),
            Some(if supported { expected as i32 } else { 42 }),
            "{name}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !supported {
            assert_eq!(stdout.matches('~').count() as i64, expected, "{name}");
        }
        let markers: String = stdout.chars().filter(|c| matches!(c, '@' | '%')).collect();
        let expected_markers = if name.starts_with("order_skip") {
            "@"
        } else if name.starts_with("order_") {
            "@%"
        } else {
            ""
        };
        assert_eq!(markers, expected_markers, "{name}");
    }
}

#[test]
#[cfg(target_arch = "aarch64")]
fn short_circuit_required_faults() {
    // AArch64's checked division returns the failure sentinel 1 from the function.
    // These must retain the check even if the other operand decides the Boolean.
    let mut failures = Vec::new();
    for optimized in [false, true] {
        for expression in [
            "true && (5 / zero == 1)",
            "false || (5 / zero == 1)",
            "(5 / zero == 1) && false",
            "(5 / zero == 1) || true",
        ] {
            let source = format!(
                "fn main() -> Int {{\n let zero = 0;\n if {expression} {{ return 7; }}\n return 42;\n}}"
            );
            let mut module = parse_in_source(&source).unwrap();
            if optimized {
                inauguration::core_opt::optimize_with_entry(&mut module.decls, Some("main"));
            }
            let actual = native(&module);
            println!("required fault {expression} optimized={optimized}: {actual}");
            if actual != 1 {
                failures.push(format!("{expression} optimized={optimized}: {actual}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Optional actual x86 emission execution on an ARM Mac with Rosetta already installed.
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[ignore = "requires installed Rosetta and an x86_64-capable C compiler"]
fn short_circuit_x86_rosetta() {
    let dir = std::env::temp_dir().join(format!("in-short-circuit-x86-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let runner = dir.join("runner");
    let runner_source = dir.join("runner.c");
    std::fs::write(
        &runner_source,
        r#"
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <sys/mman.h>
int main(int argc, char **argv) {
    if (argc != 3) return 90;
    FILE *f = fopen(argv[1], "rb");
    if (!f) return 91;
    fseek(f, 0, SEEK_END); size_t size = (size_t)ftell(f); rewind(f);
    void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (p == MAP_FAILED || fread(p, 1, size, f) != size) return 92;
    fclose(f);
    if (mprotect(p, size, PROT_READ | PROT_EXEC)) return 93;
    int64_t (*entry)(void) = (int64_t (*)(void))((char *)p + strtoul(argv[2], NULL, 10));
    printf("%lld\n", (long long)entry());
    return 0;
}
"#,
    )
    .unwrap();
    let built = Command::new("cc")
        .args(["-arch", "x86_64", "-O0"])
        .arg(&runner_source)
        .arg("-o")
        .arg(&runner)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let mut failures = Vec::new();
    for (name, source, expected, supported) in cases() {
        if !supported || name.starts_with("order_") {
            continue;
        }
        let code = inauguration::native_emit::x86_64_lower::lower_module(
            &parse_in_source(&source).unwrap(),
            "main",
        )
        .unwrap();
        assert!(code.extern_calls.is_empty(), "unresolved extern calls");
        assert!(code.relocations.is_empty() && code.global_relocs.is_empty());
        let blob = dir.join(format!("{name}.bin"));
        std::fs::write(&blob, &code.code).unwrap();
        let output = bounded_output(
            Command::new(&runner)
                .arg(&blob)
                .arg(code.entry_offset.to_string()),
        );
        let actual = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<i64>();
        println!("x86 {name}: status={} result={actual:?}", output.status);
        if !output.status.success() || actual != Ok(expected) {
            failures.push(format!(
                "{name}: status={} actual={actual:?} expected={expected}",
                output.status
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
