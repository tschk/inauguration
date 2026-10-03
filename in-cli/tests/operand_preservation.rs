//! AArch64 RHS scratch registers must not destroy an enclosing binary LHS.
#![cfg(all(unix, target_arch = "aarch64"))]
use inauguration::{
    core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule},
    in_lang_parse::parse_in_source,
    jit_runtime::JitRuntime,
    native_emit::{NativeLinkage, lower},
};
use std::{
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

fn bounded(command: &mut Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(10) {
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn native(module: &UnifiedModule) -> i64 {
    inauguration::native_emit::native_link::bootstrap_jit_native();
    let code = lower::lower_module(module, "main", NativeLinkage::Executable).unwrap();
    assert!(code.degradations.is_empty(), "{:?}", code.degradations);
    let functions = code
        .function_offsets
        .iter()
        .map(|(n, o)| (n.clone(), *o, code.code.len() as u32 - *o))
        .collect::<Vec<_>>();
    let mut runtime = JitRuntime::new();
    runtime
        .load(&code.code, &functions, &code.relocations)
        .unwrap();
    unsafe { runtime.invoke("main", &[]).unwrap() }
}

struct Case {
    name: &'static str,
    source: String,
    expected: i64,
    markers: &'static str,
    c_reference: bool,
    inisa_reference: bool,
}

fn cases() -> Vec<Case> {
    let original = include_str!("../../conformance/control-flow/compound-ops.in");
    let mut cases = vec![
        Case {
            name: "original_compound",
            source: original.into(),
            expected: 25,
            markers: "",
            c_reference: false,
            inisa_reference: false,
        },
        // The C emitter needs an explicit array type. Preserve the original fixture.
        Case {
            name: "typed_compound",
            source: original.replace("let arr =", "let arr: [Int] ="),
            expected: 25,
            markers: "",
            c_reference: true,
            inisa_reference: false,
        },
    ];
    for (name, expression, expected) in [
        ("nested_add", "a + (b + (c + d))", 10),
        ("nested_multiply", "a * (b + (c * d))", 14),
        ("nested_subtract", "a - (b - (c - d))", -2),
        ("nested_bitwise", "a ^ (b | (c << d))", 51),
        ("nested_unary", "a + (b + (-(c + d)))", -4),
        ("nested_call", "identity(a) + (b + (identity(c) + d))", 10),
    ] {
        cases.push(Case { name, source: format!("fn identity(v: Int) -> Int {{ return v; }}\nfn combine(a: Int, b: Int, c: Int, d: Int) -> Int {{\n return {expression};\n}}\nfn main() -> Int {{ return combine(1, 2, 3, 4); }}"), expected, markers: "", c_reference: true, inisa_reference: true });
    }
    cases.push(Case {
        name: "nested_index",
        source:
            "fn main() -> Int {\n let arr: [Int] = [3, 2, 21];\n return 1 + (arr[0] + arr[2]);\n}"
                .into(),
        expected: 25,
        markers: "",
        c_reference: true,
        inisa_reference: false,
    });
    cases.push(Case { name: "index_call_order", source: "fn index() -> Int {\n putchar(35);\n return 0;\n}\nfn mark(code: Int, value: Int) -> Int {\n putchar(code);\n return value;\n}\nfn main() -> Int {\n let arr: [Int] = [3];\n return mark(64, 10) + (mark(37, 20) + arr[index()]);\n}".into(), expected: 33, markers: "@%#", c_reference: true, inisa_reference: false });
    for (name, index) in [("rhs_bounds_failure", 2), ("rhs_negative_index", -1)] {
        cases.push(Case {
            name,
            source: format!(
                "fn main() -> Int {{\n let arr: [Int] = [3];\n return 10 + (20 + arr[{index}]);\n}}"
            ),
            expected: 1,
            markers: "",
            c_reference: false,
            inisa_reference: false,
        });
    }
    for (name, index, expected) in [
        ("helper_rhs_valid_index", 0, 33),
        ("helper_rhs_bounds_failure", 2, 1),
        ("helper_rhs_negative_index", -1, 1),
    ] {
        cases.push(Case {
            name,
            source: format!(
                "fn combine(a: Int, b: Int, index: Int) -> Int {{\n let arr: [Int] = [3];\n return a + (b + arr[index]);\n}}\nfn main() -> Int {{ return combine(10, 20, {index}); }}"
            ),
            expected,
            markers: "",
            c_reference: false,
            inisa_reference: false,
        });
    }
    cases
}

#[test]
fn operand_child() {
    let Ok(name) = std::env::var("IN_OPERAND_CASE") else {
        return;
    };
    let case = cases().into_iter().find(|c| c.name == name).unwrap();
    let mut module = parse_in_source(&case.source).unwrap();
    if std::env::var_os("IN_OPERAND_OPTIMIZE").is_some() {
        inauguration::core_opt::optimize_with_entry(&mut module.decls, Some("main"));
    }
    assert_eq!(native(&module), case.expected, "{name}");
}

#[test]
fn operand_native_subprocesses() {
    let mut failures = Vec::new();
    for optimized in [false, true] {
        for case in cases() {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "operand_child", "--nocapture"])
                .env("IN_OPERAND_CASE", case.name)
                .env_remove("IN_OPERAND_OPTIMIZE");
            if optimized {
                command.env("IN_OPERAND_OPTIMIZE", "1");
            }
            let output = bounded(&mut command);
            let markers: String = String::from_utf8_lossy(&output.stdout)
                .chars()
                .filter(|c| matches!(c, '@' | '%' | '#'))
                .collect();
            let passed = output.status.success() && markers == case.markers;
            println!(
                "native {} optimized={optimized} pass={passed} markers={markers:?}",
                case.name
            );
            if !passed {
                failures.push(format!(
                    "{} optimized={optimized}: status={} markers={markers:?} expected={:?}\n{}",
                    case.name,
                    output.status,
                    case.markers,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn operand_supported_references() {
    let dir = std::env::temp_dir().join(format!("in-operands-c-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for case in cases() {
        let mut module = parse_in_source(&case.source).unwrap();
        if case.inisa_reference {
            let result = inauguration::native_emit::inisa::eval_module(&module, "main").unwrap();
            assert_eq!(result, case.expected, "INISA {}", case.name);
            println!("INISA {} result={result}", case.name);
        }
        if !case.c_reference {
            continue;
        }
        for decl in &mut module.decls {
            if let Decl::Function { name, .. } = decl
                && name == "main"
            {
                *name = "case_main".into();
            }
        }
        let code = inauguration::native_emit::c_backend::emit_c_module(&module).unwrap();
        let source = dir.join(format!("{}.c", case.name));
        let executable = dir.join(case.name);
        // Read the full signed result instead of truncating through process exit status.
        std::fs::write(&source, format!("#include <stdio.h>\n{code}\nint main(void) {{ long long value = case_main(); printf(\"RESULT=%lld\\n\", value); return 0; }}\n")).unwrap();
        let built = bounded(
            Command::new("cc")
                .args(["-std=c99", "-O0"])
                .arg(&source)
                .arg("-o")
                .arg(&executable),
        );
        assert!(
            built.status.success(),
            "{}: {}",
            case.name,
            String::from_utf8_lossy(&built.stderr)
        );
        let output = bounded(&mut Command::new(&executable));
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{}: {}", case.name, output.status);
        assert!(
            stdout.contains(&format!("RESULT={}\n", case.expected)),
            "{}: {stdout}",
            case.name
        );
        // C does not specify evaluation order of arithmetic operands; only native
        // lowering's established left-to-right order is asserted by the marker test.
        println!("C {} result={}", case.name, case.expected);
    }
}

fn right_nested_module(operators: usize) -> UnifiedModule {
    let mut expression = Expr::Ident("value".into());
    for _ in 0..operators {
        expression = Expr::Binary {
            op: "+".into(),
            lhs: Box::new(Expr::Ident("value".into())),
            rhs: Box::new(expression),
        };
    }
    UnifiedModule::new(vec![Decl::Function {
        name: "main".into(),
        params: vec![],
        ret: Typ::Int,
        body: vec![
            Stmt::Let("value".into(), Some(Typ::Int), Expr::IntLit(1)),
            Stmt::Return(Some(expression)),
        ],
        type_params: vec![],
    }])
}

#[test]
fn operand_existing_temporary_pool_boundary() {
    // 65 operators need 64 simultaneously live spills: the innermost RHS is a leaf.
    assert_eq!(native(&right_nested_module(65)), 66);
    // Composite non-call RHS expressions now reach the existing 64-slot bound.
    // Fail with its diagnostic rather than silently emitting incorrect code.
    let error = lower::lower_module(&right_nested_module(66), "main", NativeLinkage::Executable)
        .expect_err("excessive nesting in the entry must be rejected");
    assert!(
        error.contains("entry function `main` unsupported"),
        "{error}"
    );
    assert!(
        error.contains("binary expression nesting is too deep"),
        "{error}"
    );
}
