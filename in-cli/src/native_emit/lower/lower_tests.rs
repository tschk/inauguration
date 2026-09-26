#![allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
use super::*;
use crate::boundary_emit;
use crate::core_ir::{
    CatchArm, CoreModuleIdentity, Decl, Expr, FloatVal, LoopKind, Stmt, Typ, UnifiedModule,
};
use crate::inrt;
use crate::inrt::INRT_BUILTINS;
use crate::native_emit::aarch64::{self, CodeEmitter};
use crate::native_emit::macho::ExportSymbol;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_executable(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "inauguration-native-emit-{}-{}-{name}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn answer_module() -> UnifiedModule {
    UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "answer".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            type_params: vec![],
        }],
    }
}

/// Run a native executable on macOS AArch64: ad-hoc codesign, direct exec (no shell).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn run_native_exe(path: &std::path::Path) -> std::process::Output {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sign = std::process::Command::new("codesign")
        .arg("-s")
        .arg("-")
        .arg("-f")
        .arg(path)
        .status()
        .expect("codesign spawn");
    assert!(sign.success(), "codesign failed for native executable");
    std::process::Command::new(path)
        .output()
        .expect("run executable")
}

/// Assert exit code matches, with fallback to otool disassembly check on signal 9.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn assert_exit_or_disasm(
    output: &std::process::Output,
    expected: i32,
    path: &std::path::Path,
    insns: &[&str],
) {
    use std::os::unix::process::ExitStatusExt;
    match output.status.code() {
        Some(code) => assert_eq!(code, expected, "exit {code} != expected {expected}"),
        None if output.status.signal() == Some(9) => {
            let dump = std::process::Command::new("otool")
                .arg("-tV")
                .arg(path)
                .output()
                .expect("otool");
            let text = String::from_utf8_lossy(&dump.stdout);
            for insn in insns {
                assert!(
                    text.contains(insn),
                    "expected '{insn}' in __text; otool:\n{text}"
                );
            }
        }
        other => panic!(
            "unexpected native exit {:?}; stdout={:?} stderr={:?}",
            other, output.stdout, output.stderr
        ),
    }
}

fn return_binary_module(op: &str, lhs: i64, rhs: i64) -> UnifiedModule {
    UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: op.into(),
                lhs: Box::new(Expr::IntLit(lhs)),
                rhs: Box::new(Expr::IntLit(rhs)),
            }))],
            type_params: vec![],
        }],
    }
}

fn code_contains_insn(code: &[u8], insn: u32) -> bool {
    code.windows(4)
        .step_by(4)
        .any(|bytes| bytes == insn.to_le_bytes())
}

fn assert_contains_divide_failure_path(code: &[u8], branch_distance: i32) {
    assert!(code_contains_insn(
        code,
        aarch64::cmp_reg64(1, aarch64::REG_XZR)
    ));
    assert!(code_contains_insn(
        code,
        aarch64::b_cond(0, branch_distance)
    ));
    assert!(code_contains_insn(code, aarch64::movz64(0, 1, 0)));
}

#[test]
fn lowers_answer_literal_module_to_bytes() {
    let module = answer_module();
    let lowered = lower_module(&module, "answer", NativeLinkage::Executable).expect("lower");
    assert!(lowered.code.len() > ENTRY_STUB_SIZE as usize);
    assert_eq!(&lowered.code[0..4], &inrt::build_entry_stub(32)[0..4]);
}

#[test]
fn lowers_borrowed_path_parameter() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![("path".into(), Typ::Named("Path".into()))],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
            type_params: vec![],
        }],
    };
    lower_module(&module, "main", NativeLinkage::Executable).expect("lower borrowed Path");
}

#[test]
fn compile_native_host_gate() {
    let module = answer_module();
    let path = temp_executable("gate");
    let result = compile_native_executable_for_host(&module, "answer", &path);
    if host_supports_native_subset() {
        result.expect("compile on host");
        assert!(path.exists());
        let _ = std::fs::remove_file(path);
    } else {
        assert_eq!(result.unwrap_err(), "native-host-unsupported");
    }
}

#[test]
fn dylib_lowering_tracks_export_symbols() {
    let module = answer_module();
    let lowered = lower_module(&module, "answer", NativeLinkage::Dylib).expect("lower");
    assert_eq!(lowered.entry_offset, None);
    assert_eq!(lowered.exports.len(), 1);
    assert_eq!(lowered.exports[0].name, "answer");
    assert_eq!(lowered.exports[0].offset, 32);
}

#[test]
fn dylib_compile_emits_abi_json() {
    let mut module = answer_module();
    module.identity = CoreModuleIdentity {
        package: Some("sample.pkg".to_string()),
        module: Some("sample.pkg.native".to_string()),
    };
    let path = temp_executable("dylib");
    let dylib_path = path.with_extension("dylib");
    let result = compile_native_artifact_for_host(
        &module,
        "App",
        "answer",
        NativeLinkage::Dylib,
        &dylib_path,
    );
    if host_supports_native_subset() {
        let abi_path = result.expect("compile dylib on host");
        let abi_path = abi_path.expect("abi path");
        assert!(dylib_path.exists());
        assert!(abi_path.exists());
        let manifest = std::fs::read_to_string(&abi_path).expect("read abi");
        assert!(manifest.contains("\"answer\""));
        assert!(manifest.contains("\"layout_hash\""));
        let parsed: serde_json::Value = serde_json::from_str(&manifest).expect("json");
        assert_eq!(parsed["package"], "sample.pkg");
        assert_eq!(parsed["module"], "sample.pkg.native");
        let _ = std::fs::remove_file(dylib_path);
        let _ = std::fs::remove_file(abi_path);
    } else {
        assert_eq!(result.unwrap_err(), "native-host-unsupported");
    }
}

#[test]
fn native_abi_manifest_carries_module_identity() {
    let mut module = answer_module();
    module.identity = CoreModuleIdentity {
        package: Some("sample.pkg".to_string()),
        module: Some("sample.pkg.native".to_string()),
    };
    let exports = vec![ExportSymbol {
        name: "answer".to_string(),
        offset: 0,
    }];
    let boundary = boundary_from_module(&module, "App", &exports);
    let manifest = boundary_emit::emit_abi_manifest_with_package(
        &boundary,
        module.identity.package.as_deref(),
    );
    let parsed: serde_json::Value = serde_json::from_str(&manifest).expect("json");

    assert_eq!(parsed["package"], "sample.pkg");
    assert_eq!(parsed["module"], "sample.pkg.native");
}

#[test]
fn lowers_expression_statement_for_side_effects() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![
            Decl::Function {
                name: "side".into(),
                params: vec![("value".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Ident("value".into())))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![
                    Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("side".into())),
                        args: vec![Expr::IntLit(1)],
                    }),
                    Stmt::Return(Some(Expr::IntLit(2))),
                ],
                type_params: vec![],
            },
        ],
    };

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_integer_division_to_aarch64_sdiv() {
    let module = return_binary_module("/", 18, 3);
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");

    assert!(code_contains_insn(&lowered.code, 0x9AC1_0C00));
}

#[test]
fn lowers_integer_modulo_to_aarch64_sdiv_msub() {
    let module = return_binary_module("%", 20, 6);
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");

    assert!(code_contains_insn(&lowered.code, 0x9AC1_0C02));
    assert!(code_contains_insn(&lowered.code, 0x9B01_8040));
}

#[test]
fn lowers_integer_division_by_zero_to_failure_return() {
    let module = return_binary_module("/", 18, 0);
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");

    assert_contains_divide_failure_path(&lowered.code, 12);
}

#[test]
fn lowers_integer_modulo_by_zero_to_failure_return() {
    let module = return_binary_module("%", 18, 0);
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");

    assert_contains_divide_failure_path(&lowered.code, 16);
}

#[test]
fn lowers_string_len_to_aarch64_ldr_offset_8() {
    let mut emitter = CodeEmitter::new();
    let structs = HashMap::new();
    let strings = HashMap::new();
    let mut pending_static_arrays = Vec::new();
    let mut pending_inrt_calls = Vec::new();
    let mut pending_strings = Vec::new();
    let mut ctx = LowerCtx::new(
        &[],
        &structs,
        &strings,
        &mut pending_static_arrays,
        &mut pending_inrt_calls,
        &mut pending_strings,
        "test",
    )
    .unwrap();
    let functions = HashMap::new();
    let mut pending_calls = Vec::new();
    let result = lower_stdlib::lower_stdlib_call(
        &mut emitter,
        &mut ctx,
        "String::len",
        &[Expr::IntLit(0)],
        0,
        &functions,
        &mut pending_calls,
        "test",
    );
    assert!(result.unwrap());
    assert!(code_contains_insn(&emitter.bytes, aarch64::ldr64(0, 0, 8)));
}

#[test]
fn lowers_string_contains_to_native_wrapper_call() {
    let mut emitter = CodeEmitter::new();
    let structs = HashMap::new();
    let strings = HashMap::new();
    let mut pending_static_arrays = Vec::new();
    let mut pending_inrt_calls = Vec::new();
    let mut pending_strings = Vec::new();
    let mut ctx = LowerCtx::new(
        &[],
        &structs,
        &strings,
        &mut pending_static_arrays,
        &mut pending_inrt_calls,
        &mut pending_strings,
        "test",
    )
    .unwrap();
    let functions = HashMap::new();
    let mut pending_calls = Vec::new();
    let result = lower_stdlib::lower_stdlib_call(
        &mut emitter,
        &mut ctx,
        "String::contains",
        &[Expr::IntLit(0), Expr::IntLit(0)],
        0,
        &functions,
        &mut pending_calls,
        "test",
    );
    assert!(result.unwrap());
    let words: Vec<u32> = emitter
        .bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    let has_bl_or_blr = words
        .iter()
        .any(|w| (*w >> 26) == 0b100101 || (*w == 0xD63F_01E0u32 | (15 << 5)));
    assert!(has_bl_or_blr, "expected a call to the native wrapper");
}

#[test]
fn lowers_bool_literals_as_scalar_values() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::BoolLit(true)))],
            type_params: vec![],
        }],
    };

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_unary_scalar_expressions() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![
            Decl::Function {
                name: "neg".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Unary {
                    op: "-".into(),
                    expr: Box::new(Expr::IntLit(7)),
                }))],
                type_params: vec![],
            },
            Decl::Function {
                name: "not".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Unary {
                    op: "!".into(),
                    expr: Box::new(Expr::IntLit(0)),
                }))],
                type_params: vec![],
            },
        ],
    };

    lower_module(&module, "neg", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_in_logical_binary_expressions() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let n: Int = 2;
  if n == 2 && true || false {
    return 7;
  }
  return 0;
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_in_struct_local_field_access() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
struct Point {
  Int x
  Int y
}

fn main() -> Int {
  let p: Point = Point { x: 2, y: 5 };
  return p.y;
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_struct_parameter_field_access() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
struct Point {
  Int x
  Int y
}

fn sum(p: Point) -> Int {
  return p.x + p.y;
}

fn main() -> Int {
  let p: Point = Point { x: 2, y: 5 };
  return sum(p);
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_struct_return_field_access() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
struct Point {
  Int x
  Int y
}

fn make-point() -> Point {
  return Point { x: 2, y: 5 };
}

fn main() -> Int {
  let p: Point = make-point();
  return p.y;
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_string_scalar_expressions() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![
            Decl::Function {
                name: "same".into(),
                params: vec![("value".into(), Typ::String)],
                ret: Typ::Int,
                body: vec![
                    Stmt::If {
                        cond: Expr::Binary {
                            op: "==".into(),
                            lhs: Box::new(Expr::Ident("value".into())),
                            rhs: Box::new(Expr::StringLit("ok".into())),
                        },
                        then_body: vec![Stmt::Return(Some(Expr::IntLit(7)))],
                        else_body: vec![],
                    },
                    Stmt::Return(Some(Expr::IntLit(1))),
                ],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("same".into())),
                    args: vec![Expr::StringLit("ok".into())],
                }))],
                type_params: vec![],
            },
        ],
    };

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_local_array_index_expressions() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  let i: Int = 1;
  return xs[i];
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_local_array_index_assignment() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  xs[1] = 9;
  return xs[1];
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_array_parameter_index_expressions() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn pick(xs: [Int], i: Int) -> Int {
  return xs[i];
}

fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  return pick(xs, 2);
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_array_return_index_expressions() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn identity(xs: [Int]) -> [Int] {
  return xs;
}

fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  let ys: [Int] = identity(xs);
  return ys[1];
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_local_array_len() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let values: [Int] = [2, 5, 8];
  return array-len(values);
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_split_array_assignment() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let values: [String] = str-split-lines("one\ntwo");
  return array-len(values);
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_split_array_len() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let values: [String] = str-split-lines("one\ntwo");
  return array-len(values);
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 2);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_split_array_string_index() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let values: [String] = str-split-lines("one\ntwo");
  return str-eq(values[1], "two");
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 1);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_split_array_string_lookup() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let values: [String] = str-split-lines("one\ntwo");
  return str-table-has("one|two", values[1]);
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 1);
}

#[test]
fn lowers_array_literal_return_as_owned_static_data() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn values() -> [Int] {
  return [2, 5, 8];
}

fn main() -> Int {
  let ys: [Int] = values();
  return ys[1];
}
"#,
    )
    .expect("parse");

    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let values: Vec<i64> = lowered
        .code
        .as_chunks::<8>()
        .0
        .iter()
        .map(|chunk| i64::from_le_bytes(*chunk))
        .collect();
    assert!(values.windows(3).any(|window| window == [2, 5, 8]));
}

#[test]
fn lowers_fixed_array_into_iter() {
    let module = crate::compiler::rust_front::parse_rust_source(
        "fn values() -> [i64; 2] { [2, 5] } fn main() { values().into_iter(); }",
    )
    .expect("parse rust");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_fixed_array_map_collect_to_aggregate_vec() {
    let mut module = crate::compiler::rust_front::parse_rust_source(
        "struct Item { value: String } fn values() -> [&'static str; 2] { [\"a\", \"b\"] } fn main() -> Vec<Item> { values().into_iter().map(|value| Item { value }).collect() }",
    )
    .expect("parse rust");
    crate::lower_core::desugar_module(&mut module);

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_bool_and_string_array_argument_return_paths() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn pick-bool(xs: [Bool], i: Int) -> Bool {
  return xs[i];
}

fn identity-strings(xs: [String]) -> [String] {
  return xs;
}

fn main() -> Int {
  let flags: [Bool] = [false, true];
  let words: [String] = ["no", "ok"];
  let returned: [String] = identity-strings(words);
  if pick-bool(flags, 1) && returned[1] == "ok" {
    return 7;
  }
  return 1;
}
"#,
    )
    .expect("parse");

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_local_reassignment() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![
                Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(1)),
                Stmt::Assign("x".into(), Expr::IntLit(2)),
                Stmt::Return(Some(Expr::Ident("x".into()))),
            ],
            type_params: vec![],
        }],
    };

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_runtime_if_conditions() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![("flag".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![
                Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(1)),
                Stmt::If {
                    cond: Expr::Ident("flag".into()),
                    then_body: vec![Stmt::Assign("x".into(), Expr::IntLit(2))],
                    else_body: vec![Stmt::Assign("x".into(), Expr::IntLit(3))],
                },
                Stmt::Return(Some(Expr::Ident("x".into()))),
            ],
            type_params: vec![],
        }],
    };

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_runtime_while_loop_conditions() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![
                Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Loop {
                    kind: crate::core_ir::LoopKind::While,
                    cond: Some(Expr::Binary {
                        op: "<".into(),
                        lhs: Box::new(Expr::Ident("x".into())),
                        rhs: Box::new(Expr::IntLit(3)),
                    }),
                    body: vec![Stmt::Assign(
                        "x".into(),
                        Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("x".into())),
                            rhs: Box::new(Expr::IntLit(1)),
                        },
                    )],
                },
                Stmt::Return(Some(Expr::Ident("x".into()))),
            ],
            type_params: vec![],
        }],
    };

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_vec_iterator_contract() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![
            Decl::Function {
                name: "values".into(),
                params: vec![],
                ret: Typ::Named("Vec".into()),
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("Vec::new".into())),
                    args: vec![],
                }))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let("count".into(), Some(Typ::Int), Expr::IntLit(0)),
                    Stmt::Loop {
                        kind: LoopKind::For {
                            binding: "value".into(),
                        },
                        cond: Some(Expr::Call {
                            callee: Box::new(Expr::Ident("values".into())),
                            args: vec![],
                        }),
                        body: vec![Stmt::Assign(
                            "count".into(),
                            Expr::Binary {
                                op: "+".into(),
                                lhs: Box::new(Expr::Ident("count".into())),
                                rhs: Box::new(Expr::IntLit(1)),
                            },
                        )],
                    },
                    Stmt::Return(Some(Expr::Ident("count".into()))),
                ],
                type_params: vec![],
            },
        ],
    };

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower Vec iterator");
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_vec_for() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![("values".into(), Typ::Named("Vec".into()))],
            ret: Typ::Int,
            body: vec![
                Stmt::Let("sum".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Loop {
                    kind: LoopKind::For {
                        binding: "value".into(),
                    },
                    cond: Some(Expr::Ident("values".into())),
                    body: vec![Stmt::Assign(
                        "sum".into(),
                        Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("sum".into())),
                            rhs: Box::new(Expr::Ident("value".into())),
                        },
                    )],
                },
                Stmt::Return(Some(Expr::Ident("sum".into()))),
            ],
            type_params: vec![],
        }],
    };
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let values = [3_u64, 5, 7];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    let result = unsafe {
        rt.invoke(
            "main",
            &[
                values.as_ptr() as i64,
                values.len() as i64,
                values.len() as i64,
            ],
        )
        .expect("invoke")
    };
    assert_eq!(result, 15);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_scalar_vec_literal() {
    let module = crate::compiler::rust_front::parse_rust_source(
        "fn main() -> i64 { let values: Vec<i64> = vec![2, 3, 5]; let mut sum = 0; for value in values { sum = sum + value; } return sum; }",
    )
    .expect("parse Vec literal");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 10);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_iter_once_vector() {
    let module = crate::compiler::rust_front::parse_rust_source(
        "fn main() -> i64 { let values: Vec<i64> = std::iter::once(5); let mut sum = 0; for value in values { sum = sum + value; } return sum; }",
    )
    .expect("parse iter once");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 5);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_passes_struct_vec_literal_argument() {
    let module = crate::compiler::rust_front::parse_rust_source(
        r#"
struct Pair { left: i64, right: i64 }
fn accept(values: Vec<Pair>) -> i64 { return 7; }
fn main() -> i64 { return accept(vec![Pair { left: 2, right: 3 }, Pair { left: 5, right: 7 }]); }
"#,
    )
    .expect("parse struct Vec literal");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 7);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_propagates_rust_result_error() {
    let success = crate::compiler::rust_front::parse_rust_source(
        r#"
fn leaf() -> Result<i64, i64> { return Ok(4); }
fn main() -> Result<i64, i64> {
    let value: i64 = leaf()?;
    return Ok(value + 1);
}
"#,
    )
    .expect("parse success result");
    let failure = crate::compiler::rust_front::parse_rust_source(
        r#"
fn leaf() -> Result<i64, i64> { return Err(4); }
fn main() -> Result<i64, i64> {
    let value: i64 = leaf()?;
    return Ok(value + 1);
}
"#,
    )
    .expect("parse failure result");
    for (module, expected) in [(success, 5), (failure, 0)] {
        let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
        let function_offsets = lowered
            .function_offsets
            .iter()
            .map(|(name, offset)| (name.clone(), *offset, 0))
            .collect::<Vec<_>>();
        let mut rt = crate::jit_runtime::JitRuntime::new();
        rt.load(&lowered.code, &function_offsets, &lowered.relocations)
            .expect("jit load");
        assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, expected);
    }
}

#[test]
fn lowers_numeric_match_with_default_arm() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![("tag".into(), Typ::Int)],
            ret: Typ::Int,
            body: vec![
                Stmt::Let("out".into(), Some(Typ::Int), Expr::IntLit(0)),
                Stmt::Match {
                    scrutinee: Expr::Ident("tag".into()),
                    arms: vec![
                        crate::core_ir::MatchArm {
                            pattern: "1".into(),
                            body: vec![Stmt::Assign("out".into(), Expr::IntLit(10))],
                        },
                        crate::core_ir::MatchArm {
                            pattern: "_".into(),
                            body: vec![Stmt::Assign("out".into(), Expr::IntLit(20))],
                        },
                    ],
                },
                Stmt::Return(Some(Expr::Ident("out".into()))),
            ],
            type_params: vec![],
        }],
    };

    lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn answer_executable_exits_with_return_value() {
    let module = answer_module();
    let path = temp_executable("answer-exe");
    compile_native_executable(&module, "answer", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 42, &path, &["mov\tx0, #0x2a"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn scalar_subset_executable_exits_with_return_value() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![
            Decl::Function {
                name: "side".into(),
                params: vec![("value".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Ident("value".into())))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![
                    Stmt::Expr(Expr::Call {
                        callee: Box::new(Expr::Ident("side".into())),
                        args: vec![Expr::IntLit(5)],
                    }),
                    Stmt::Let("gate".into(), Some(Typ::Int), Expr::IntLit(2)),
                    Stmt::Let("x".into(), Some(Typ::Int), Expr::IntLit(1)),
                    Stmt::If {
                        cond: Expr::Binary {
                            op: "||".into(),
                            lhs: Box::new(Expr::Binary {
                                op: "&&".into(),
                                lhs: Box::new(Expr::Binary {
                                    op: "==".into(),
                                    lhs: Box::new(Expr::Ident("gate".into())),
                                    rhs: Box::new(Expr::IntLit(2)),
                                }),
                                rhs: Box::new(Expr::BoolLit(true)),
                            }),
                            rhs: Box::new(Expr::BoolLit(false)),
                        },
                        then_body: vec![Stmt::Assign(
                            "x".into(),
                            Expr::Unary {
                                op: "-".into(),
                                expr: Box::new(Expr::IntLit(7)),
                            },
                        )],
                        else_body: vec![Stmt::Assign("x".into(), Expr::IntLit(3))],
                    },
                    Stmt::Return(Some(Expr::Binary {
                        op: "+".into(),
                        lhs: Box::new(Expr::Ident("x".into())),
                        rhs: Box::new(Expr::IntLit(8)),
                    })),
                ],
                type_params: vec![],
            },
        ],
    };
    let path = temp_executable("scalar-subset-exe");
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(
        &output,
        1,
        &path,
        &["b.eq", "neg\tx0, x0", "str\tx0, [sp]", "add\tx0, x0, x1"],
    );
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn struct_field_executable_exits_with_field_value() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
struct Point {
  Int x
  Int y
}

fn main() -> Int {
  let p: Point = Point { x: 2, y: 5 };
  return p.y;
}
"#,
    )
    .expect("parse");
    let path = temp_executable("struct-field-exe");
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 5, &path, &["str\tx0, [sp]", "ldr\tx0, [sp, #0x8]"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn struct_parameter_executable_exits_with_field_sum() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
struct Point {
  Int x
  Int y
}

fn sum(p: Point) -> Int {
  return p.x + p.y;
}

fn main() -> Int {
  let p: Point = Point { x: 2, y: 5 };
  return sum(p);
}
"#,
    )
    .expect("parse");
    let path = temp_executable("struct-param-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(
        &output,
        7,
        &path,
        &["str\tx0, [sp]", "str\tx1, [sp, #0x8]", "add\tx0, x0, x1"],
    );
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn struct_return_executable_exits_with_field_value() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
struct Point {
  Int x
  Int y
}

fn make-point() -> Point {
  return Point { x: 2, y: 5 };
}

fn main() -> Int {
  let p: Point = make-point();
  return p.y;
}
"#,
    )
    .expect("parse");
    let path = temp_executable("struct-return-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(
        &output,
        5,
        &path,
        &["mov\tx0, #0x2", "mov\tx1, #0x5", "str\tx1, [sp, #0x8]"],
    );
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn string_scalar_executable_exits_with_comparison_value() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![
            Decl::Function {
                name: "same".into(),
                params: vec![("value".into(), Typ::String)],
                ret: Typ::Int,
                body: vec![
                    Stmt::If {
                        cond: Expr::Binary {
                            op: "==".into(),
                            lhs: Box::new(Expr::Ident("value".into())),
                            rhs: Box::new(Expr::StringLit("ok".into())),
                        },
                        then_body: vec![Stmt::Return(Some(Expr::IntLit(7)))],
                        else_body: vec![],
                    },
                    Stmt::Return(Some(Expr::IntLit(1))),
                ],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("same".into())),
                    args: vec![Expr::StringLit("ok".into())],
                }))],
                type_params: vec![],
            },
        ],
    };
    let path = temp_executable("string-scalar-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 7, &path, &["cmp\tx0, x1", "mov\tx0, #0x7"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn local_array_index_executable_exits_with_indexed_value() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  let i: Int = 1;
  return xs[i];
}
"#,
    )
    .expect("parse");
    let path = temp_executable("array-index-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 5, &path, &["ldr\tx0, [sp, x1, lsl #3]"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn local_array_index_assignment_executable_exits_with_written_value() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  xs[1] = 9;
  return xs[1];
}
"#,
    )
    .expect("parse");
    let path = temp_executable("array-index-assign-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 9, &path, &["str\tx0, [sp, x4, lsl #3]"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn local_array_negative_index_assignment_executable_exits_with_failure() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  let i: Int = -1;
  xs[i] = 9;
  return xs[0];
}
"#,
    )
    .expect("parse");
    let path = temp_executable("array-negative-index-assign-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 1, &path, &["b.lt", "mov\tx0, #0x1"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn local_array_oob_index_assignment_executable_exits_with_failure() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  let i: Int = 3;
  xs[i] = 9;
  return xs[0];
}
"#,
    )
    .expect("parse");
    let path = temp_executable("array-oob-index-assign-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 1, &path, &["b.ge", "mov\tx0, #0x1"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn array_parameter_executable_exits_with_indexed_value() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn pick(xs: [Int], i: Int) -> Int {
  return xs[i];
}

fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  return pick(xs, 2);
}
"#,
    )
    .expect("parse");
    let path = temp_executable("array-param-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 8, &path, &["mov\tx1, #0x3", "ldr\tx0, [x"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn array_return_executable_exits_with_indexed_value() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn identity(xs: [Int]) -> [Int] {
  return xs;
}

fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  let ys: [Int] = identity(xs);
  return ys[1];
}
"#,
    )
    .expect("parse");
    let path = temp_executable("array-return-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 5, &path, &["str\tx0, [sp", "str\tx1, [sp"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn bool_string_array_executable_exits_with_comparison_value() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn pick-bool(xs: [Bool], i: Int) -> Bool {
  return xs[i];
}

fn identity-strings(xs: [String]) -> [String] {
  return xs;
}

fn main() -> Int {
  let flags: [Bool] = [false, true];
  let words: [String] = ["no", "ok"];
  let returned: [String] = identity-strings(words);
  if pick-bool(flags, 1) && returned[1] == "ok" {
    return 7;
  }
  return 1;
}
"#,
    )
    .expect("parse");
    let path = temp_executable("bool-string-array-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 7, &path, &["ldr\tx0, [", "cmp", "mov\tx0, #0x7"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn local_array_negative_index_executable_exits_with_failure() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  let i: Int = -1;
  return xs[i];
}
"#,
    )
    .expect("parse");
    let path = temp_executable("array-negative-index-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 1, &path, &["b.lt", "mov\tx0, #0x1"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn local_array_oob_index_executable_exits_with_failure() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  let xs: [Int] = [2, 5, 8];
  let i: Int = 3;
  return xs[i];
}
"#,
    )
    .expect("parse");
    let path = temp_executable("array-oob-index-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 1, &path, &["b.ge", "mov\tx0, #0x1"]);
    let _ = std::fs::remove_file(&path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn answer_native_artifact_and_const_eval_exit_42() {
    let module = answer_module();
    let path = temp_executable("answer-exe");
    compile_native_executable(&module, "answer", &path).expect("compile");
    assert!(path.exists());
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 42, &path, &["mov\tx0, #0x2a"]);
    let _ = std::fs::remove_file(path);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn rust_struct_local_copy_executable_preserves_nested_fields() {
    let module = crate::compiler::rust_front::parse_rust_source(
        r#"
struct Point { x: i64, y: i64 }
struct Segment { start: Point, end: Point }

fn main() -> i64 {
    let first = Point { x: 42, y: 1 };
    let second = Point { x: 2, y: 3 };
    let original = Segment { start: first, end: second };
    let copy = original;
    return copy.start.x;
}
"#,
    )
    .expect("parse Rust");
    let path = temp_executable("rust-struct-local-copy-exe");
    let _ = std::fs::remove_file(&path);
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_exit_or_disasm(&output, 42, &path, &["ldr\tx0, [sp", "str\tx0, [sp"]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn rejects_array_literal_return_type_mismatch() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Array(Box::new(Typ::Int)),
            body: vec![Stmt::Return(Some(Expr::ArrayLit(vec![Expr::StringLit(
                "bad".into(),
            )])))],
            type_params: vec![],
        }],
    };
    match lower_module(&module, "main", NativeLinkage::Executable) {
        Ok(lowered) => assert!(lowered.function_offsets.len() == 1, "stub created"),
        Err(err) => assert!(err.contains("array return type mismatch")),
    }
}

#[test]
fn rejects_nested_array_params_with_stable_diagnostic() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![(
                "xs".into(),
                Typ::Array(Box::new(Typ::Array(Box::new(Typ::Int)))),
            )],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
            type_params: vec![],
        }],
    };
    match lower_module(&module, "main", NativeLinkage::Executable) {
        Ok(lowered) => assert!(lowered.function_offsets.len() == 1, "stub created"),
        Err(err) => assert!(err.contains("native-array-nested-unsupported")),
    }
}

#[test]
fn rejects_aggregate_array_locals_with_stable_diagnostic() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![
            Decl::Struct {
                name: "Point".into(),
                fields: vec![("x".into(), Typ::Int)],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let(
                        "points".into(),
                        Some(Typ::Array(Box::new(Typ::Named("Point".into())))),
                        Expr::ArrayLit(vec![Expr::StructInit {
                            name: "Point".into(),
                            fields: vec![("x".into(), Expr::IntLit(1))],
                        }]),
                    ),
                    Stmt::Return(Some(Expr::IntLit(0))),
                ],
                type_params: vec![],
            },
        ],
    };
    match lower_module(&module, "main", NativeLinkage::Executable) {
        Ok(lowered) => assert!(lowered.function_offsets.len() == 1, "stub created"),
        Err(err) => assert!(err.contains("native-array-aggregate-unsupported")),
    }
}

fn code_contains_insns(code: &[u8], insns: &[u32]) -> bool {
    let words: Vec<u32> = code
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    for insn in insns {
        if !words.contains(insn) {
            return false;
        }
    }
    true
}

fn build_inrt_call_module(target: &str, args: Vec<Expr>, ret: Typ) -> UnifiedModule {
    UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret,
            body: vec![Stmt::Return(Some(Expr::Call {
                callee: Box::new(Expr::Ident(target.to_string())),
                args,
            }))],
            type_params: vec![],
        }],
    }
}

#[test]
fn inrt_call_emits_bl_placeholder() {
    let m = build_inrt_call_module(
        "__inrt_str_len",
        vec![Expr::StringLit("x".into())],
        Typ::Int,
    );
    let lowered = lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
    assert!(lowered.code.len() > ENTRY_STUB_SIZE as usize + 4);
    let words: Vec<u32> = lowered
        .code
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    let has_bl = words.iter().any(|w| (*w >> 26) == 0b100101);
    assert!(
        has_bl,
        "expected at least one bl instruction in lowered code"
    );
}

#[test]
fn lowers_array_load_on_aarch64() {
    let m = build_inrt_call_module(
        "__inrt_array_load",
        vec![Expr::IntLit(0x1000), Expr::IntLit(2)],
        Typ::Int,
    );
    let lowered = lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
    assert!(!lowered.code.is_empty());
}

#[test]
fn lowers_array_store_on_aarch64() {
    let m = build_inrt_call_module(
        "__inrt_array_store",
        vec![Expr::IntLit(0x2000), Expr::IntLit(1), Expr::IntLit(42)],
        Typ::Int,
    );
    lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_string_concat_on_aarch64() {
    let m = build_inrt_call_module(
        "__inrt_str_concat",
        vec![
            Expr::StringLit("hello".into()),
            Expr::StringLit("world".into()),
        ],
        Typ::String,
    );
    let lowered = lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
    assert!(lowered.code.len() > ENTRY_STUB_SIZE as usize);
}

#[test]
fn lowers_str_len_on_aarch64() {
    let m = build_inrt_call_module(
        "__inrt_str_len",
        vec![Expr::StringLit("test".into())],
        Typ::Int,
    );
    let lowered = lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
    assert!(lowered.code.len() > ENTRY_STUB_SIZE as usize);
}

#[test]
fn lowers_array_len_on_aarch64() {
    let m = build_inrt_call_module("__inrt_array_len", vec![Expr::IntLit(0x3000)], Typ::Int);
    lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_array_push_on_aarch64() {
    let m = build_inrt_call_module(
        "__inrt_array_push",
        vec![Expr::IntLit(0x4000), Expr::IntLit(7)],
        Typ::Int,
    );
    lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn lowers_str_substr_on_aarch64() {
    let m = build_inrt_call_module(
        "__inrt_str_substr",
        vec![
            Expr::StringLit("abcdef".into()),
            Expr::IntLit(2),
            Expr::IntLit(3),
        ],
        Typ::String,
    );
    lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn inrt_call_emits_runtime_blob_at_end() {
    let m = build_inrt_call_module(
        "__inrt_str_len",
        vec![Expr::StringLit("hello".into())],
        Typ::Int,
    );
    let lowered = lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
    let (blob, _) = inrt::build_runtime_blob();
    assert!(lowered.code.len() > blob.len() + ENTRY_STUB_SIZE as usize);
}

#[test]
fn rejects_inrt_call_with_wrong_arity() {
    let m = build_inrt_call_module(
        "__inrt_str_len",
        vec![Expr::IntLit(1), Expr::IntLit(2)],
        Typ::Int,
    );
    let err = lower_module(&m, "main", NativeLinkage::Executable)
        .expect_err("should fail on wrong arity");
    assert!(
        err.contains("inrt call arity mismatch"),
        "unexpected error: {err}"
    );
}

#[test]
fn lowers_inrt_call_with_ident_arg() {
    let m = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![("s".into(), Typ::String)],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::Call {
                callee: Box::new(Expr::Ident("__inrt_str_len".to_string())),
                args: vec![Expr::Ident("s".into())],
            }))],
            type_params: vec![],
        }],
    };
    lower_module(&m, "main", NativeLinkage::Executable).expect("lower");
}

#[test]
fn all_inrt_builtins_can_be_called() {
    for b in INRT_BUILTINS {
        let s = inrt::inrt_builtin_param_slots(b).unwrap_or(0);
        let a: Vec<Expr> = (0..s).map(|i| Expr::IntLit(i as i64)).collect();
        let m = build_inrt_call_module(b, a, Typ::Int);
        assert!(
            lower_module(&m, "main", NativeLinkage::Executable).is_ok(),
            "failed for {b}"
        );
    }
}

#[test]
fn lowers_throw_expression() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![
                Stmt::Throw(Expr::IntLit(42)),
                Stmt::Return(Some(Expr::IntLit(0))),
            ],
            type_params: vec![],
        }],
    };
    let lowered =
        lower_module(&module, "main", NativeLinkage::Executable).expect("throw should lower");
    assert!(code_contains_insns(
        &lowered.code,
        &[aarch64::load_i64(0, 42)[0]],
    ));
}

#[test]
fn lowers_try_catch_body_executes() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Try {
                body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
                catches: vec![],
            }],
            type_params: vec![],
        }],
    };
    let lowered =
        lower_module(&module, "main", NativeLinkage::Executable).expect("try should lower");
    assert!(code_contains_insns(
        &lowered.code,
        &[aarch64::load_i64(0, 1)[0]],
    ));
}

#[test]
fn lowers_try_catch_with_throw_emits_handler_code() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Try {
                body: vec![Stmt::Throw(Expr::IntLit(42))],
                catches: vec![CatchArm {
                    pattern: "e".into(),
                    body: vec![Stmt::Return(Some(Expr::IntLit(1)))],
                }],
            }],
            type_params: vec![],
        }],
    };
    let lowered = lower_module(&module, "main", NativeLinkage::Executable)
        .expect("try/catch with throw should lower");
    assert!(code_contains_insns(
        &lowered.code,
        &[aarch64::load_i64(0, 42)[0], aarch64::load_i64(0, 1)[0]],
    ));
}

fn return_float_binary_module(op: &str, lhs: f64, rhs: f64) -> UnifiedModule {
    UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Float,
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: op.into(),
                lhs: Box::new(Expr::FloatLit(FloatVal(lhs))),
                rhs: Box::new(Expr::FloatLit(FloatVal(rhs))),
            }))],
            type_params: vec![],
        }],
    }
}

#[test]
fn lowers_float_literal_as_bit_pattern() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Float,
            body: vec![Stmt::Return(Some(Expr::FloatLit(FloatVal(3.125))))],
            type_params: vec![],
        }],
    };
    let lowered =
        lower_module(&module, "main", NativeLinkage::Executable).expect("float should lower");
    assert!(lowered.code.len() > ENTRY_STUB_SIZE as usize);
}

#[test]
fn lowers_float_add_instruction() {
    let module = return_float_binary_module("+", 3.0, 4.0);
    let lowered =
        lower_module(&module, "main", NativeLinkage::Executable).expect("float add should lower");
    assert!(code_contains_insn(&lowered.code, aarch64::fadd_s(0, 0, 1)));
    assert!(code_contains_insn(
        &lowered.code,
        aarch64::fmov_from_gp(0, 0)
    ));
    assert!(code_contains_insn(&lowered.code, aarch64::fmov_to_gp(0, 0)));
}

#[test]
fn lowers_float_mul_instruction() {
    let module = return_float_binary_module("*", 2.0, 3.0);
    let lowered =
        lower_module(&module, "main", NativeLinkage::Executable).expect("float mul should lower");
    assert!(code_contains_insn(&lowered.code, aarch64::fmul_s(0, 0, 1)));
}

#[test]
fn lowers_float_sub_instruction() {
    let module = return_float_binary_module("-", 5.0, 2.0);
    let lowered =
        lower_module(&module, "main", NativeLinkage::Executable).expect("float sub should lower");
    assert!(code_contains_insn(&lowered.code, aarch64::fsub_s(0, 0, 1)));
}

#[test]
fn lowers_float_div_instruction() {
    let module = return_float_binary_module("/", 10.0, 2.0);
    let lowered =
        lower_module(&module, "main", NativeLinkage::Executable).expect("float div should lower");
    assert!(code_contains_insn(&lowered.code, aarch64::fdiv_s(0, 0, 1)));
}

#[test]
fn lowers_option_unwrap_or_none_to_default_inline() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::Call {
                callee: Box::new(Expr::Ident("Option::unwrap_or".into())),
                args: vec![Expr::IntLit(0), Expr::IntLit(42)],
            }))],
            type_params: vec![],
        }],
    };
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    assert!(
        lowered.external_refs.is_empty(),
        "Option::unwrap_or should be lowered inline"
    );
    assert!(code_contains_insn(&lowered.code, aarch64::cmp_reg64(1, 2)));
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_string_concat() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::String,
            body: vec![Stmt::Return(Some(Expr::Binary {
                op: "+".into(),
                lhs: Box::new(Expr::StringLit("hello".into())),
                rhs: Box::new(Expr::StringLit("world".into())),
            }))],
            type_params: vec![],
        }],
    };
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets: Vec<(String, u32, u32)> = vec![(
        "main".into(),
        ENTRY_STUB_SIZE as u32,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE as u32,
    )];
    crate::native_emit::native_link::bootstrap_jit_native();
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    let raw = unsafe { rt.invoke("main", &[]).expect("invoke") };
    assert_ne!(raw, 0, "string concat returned null pointer");
    let ptr = raw as *const u8;
    let result = unsafe {
        let len = *(ptr as *const u64) as usize;
        let data = ptr.add(8);
        let bytes = std::slice::from_raw_parts(data, len);
        String::from_utf8_lossy(bytes).to_string()
    };
    assert_eq!(result, "helloworld");
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_three_argument_string_slice() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  return str-eq(str-slice("abcdef", 2, 5), "cde");
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 1);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_string_concat_with_nested_call() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  return str-eq("value=" + to-string(7), "value=7");
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 1);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_internal_string_return_in_concat() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
import std.json;
fn quote(text: String) -> String {
  return json-stringify(text);
}

fn main() -> Int {
  return str-eq("prefix" + quote("kind"), "prefix\"kind\"");
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = lowered
        .function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset, 0))
        .collect::<Vec<_>>();
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 1);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_two_internal_string_returns_in_concat() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
import std.json;
fn quote(text: String) -> String {
  return json-stringify(text);
}

fn main() -> Int {
  return str-eq("{" + quote("a") + ":" + quote("b") + "}", "{\"a\":\"b\"}");
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = lowered
        .function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset, 0))
        .collect::<Vec<_>>();
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 1);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_preserves_nested_internal_call_arguments() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
import std.json;
fn quote(text: String) -> String {
  return json-stringify(text);
}
fn combine(left: String, right: String) -> String {
  return left + right;
}
fn main() -> Int {
  return str-eq(combine(quote("a"), quote("b")), "\"a\"\"b\"");
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = lowered
        .function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset, 0))
        .collect::<Vec<_>>();
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 1);
}

#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_string_concat_chain() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> Int {
  return str-eq("a" + "b" + "c" + "d", "abcd");
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    assert_eq!(unsafe { rt.invoke("main", &[]).expect("invoke") }, 1);
}

/// A function the lowerer cannot compile must be reported, and reaching it must
/// abort rather than return a fabricated value.
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn skipped_function_traps_instead_of_returning_zero() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn nothing() -> void { return; }

fn bad() -> Int {
  let v = nothing();
  return 1;
}

fn main() -> Int {
  return bad();
}
"#,
    )
    .expect("parse");

    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    assert_eq!(lowered.degradations.len(), 1, "{:?}", lowered.degradations);
    let degradation = &lowered.degradations[0];
    assert_eq!(degradation.code, DEGRADATION_SKIPPED_FUNCTION);
    assert_eq!(degradation.function, "bad");
    assert!(degradation.reachable, "main calls bad, so its trap is live");

    let path = temp_executable("skipped-trap");
    compile_native_executable(&module, "main", &path).expect("compile");
    let output = run_native_exe(&path);
    assert_eq!(
        output.status.code(),
        Some(i32::from(inrt::INRT_TRAP_EXIT_CODE)),
        "trapped program must not report a normal exit"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("IN3001"), "stderr was {stderr:?}");
    assert!(stderr.contains("bad"), "stderr was {stderr:?}");
    let _ = std::fs::remove_file(&path);
}

/// A trap nothing can reach is dead code, and is reported as such.
#[test]
fn unreachable_skipped_function_is_marked_not_reachable() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn nothing() -> void { return; }

fn unused() -> Int {
  let v = nothing();
  return 7;
}

fn main() -> Int {
  return 3;
}
"#,
    )
    .expect("parse");

    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    assert_eq!(lowered.degradations.len(), 1, "{:?}", lowered.degradations);
    assert_eq!(lowered.degradations[0].function, "unused");
    assert!(
        !lowered.degradations[0].reachable,
        "nothing calls unused, so its trap is dead code"
    );
}

/// throw/try/catch writes the flag and value through x27. The JIT runtime points
/// x27 at its error page, but a native executable has no runtime, so it used to
/// write through an unset register and die with a signal. The generated assembly
/// must declare a writable slot and the artifact must run the catch arm.
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_executable_runs_try_catch_without_crashing() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn might-fail(x: Int) -> Int {
  if (x < 0) {
    throw 7;
  }
  return 1;
}

fn main() -> Int {
  try {
    might-fail(-1);
  } catch (e) {
    return 42;
  }
  return 1;
}
"#,
    )
    .expect("parse");

    let path = temp_executable("try-catch");
    compile_native_executable(&module, "main", &path).expect("compile");

    let asm = std::fs::read_to_string(path.with_extension("s")).expect("assembly sidecar");
    assert!(
        asm.contains("_inrt_error_slot"),
        "assembly must declare the error slot:\n{asm}"
    );

    let output = run_native_exe(&path);
    assert_eq!(
        output.status.code(),
        Some(42),
        "the catch arm must run instead of the process dying: {output:?}"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("s"));
    let _ = std::fs::remove_file(path.with_extension("o"));
}

/// A Float return arrives in v0, so the entry stub must not exit with x0; doing
/// so made the process exit with whatever that register happened to hold.
#[test]
fn float_entry_is_not_an_exit_status() {
    assert_eq!(
        entry_return_kind(&crate::core_ir::Typ::Float),
        EntryReturn::VoidOrReference
    );
    assert_eq!(
        entry_return_kind(&crate::core_ir::Typ::Int),
        EntryReturn::IntLike
    );
    assert_eq!(
        entry_return_kind(&crate::core_ir::Typ::Bool),
        EntryReturn::IntLike
    );
}

/// The embedded string runtime has to actually concatenate. The JIT resolved the
/// Rust-side wrapper for every string operation, so the blob's copy loops were
/// never exercised and shipped broken: it copied eight bytes per index, used loop
/// bounds a syscall had clobbered, and wrote past the frame it saved into.
#[test]
#[cfg(target_arch = "aarch64")]
fn jit_executes_the_embedded_string_concat() {
    let module = crate::in_lang_parse::parse_in_source(
        r#"
fn main() -> String {
  return __inrt_str_concat("hello", " world");
}
"#,
    )
    .expect("parse");
    let lowered = lower_module(&module, "main", NativeLinkage::Executable).expect("lower");
    let function_offsets = vec![(
        "main".into(),
        ENTRY_STUB_SIZE,
        lowered.code.len() as u32 - ENTRY_STUB_SIZE,
    )];
    let mut rt = crate::jit_runtime::JitRuntime::new();
    rt.load(&lowered.code, &function_offsets, &lowered.relocations)
        .expect("jit load");
    let raw = unsafe { rt.invoke("main", &[]).expect("invoke") };
    assert_ne!(raw, 0, "concat returned a null string");

    // An instring is an 8-byte length header followed by the bytes.
    let ptr = raw as *const u8;
    let len = unsafe { *(ptr as *const u64) as usize };
    let bytes = unsafe { std::slice::from_raw_parts(ptr.add(8), len) };
    assert_eq!(String::from_utf8_lossy(bytes), "hello world");
}
