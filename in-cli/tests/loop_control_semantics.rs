//! Loop control must target the innermost loop in both the private ISA and native JIT.

use inauguration::{
    core_ir::{Decl, Expr, LoopKind, Stmt, Typ, UnifiedModule},
    core_opt,
    in_lang_parse::parse_in_source,
    native_emit::inisa,
};
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
use inauguration::{
    jit_runtime::JitRuntime,
    native_emit::{NativeLinkage, lower},
};

fn assert_loop_result(source: &str, expected: i64) {
    let raw = parse_in_source(source).expect("parse loop fixture");
    assert_eq!(inisa::eval_module(&raw, "main").unwrap(), expected);

    let mut optimized = raw.clone();
    core_opt::optimize_with_entry(&mut optimized.decls, Some("main"));
    assert_eq!(inisa::eval_module(&optimized, "main").unwrap(), expected);

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        let lowered = lower::lower_module(&optimized, "main", NativeLinkage::Executable)
            .expect("lower loop fixture");
        assert!(
            lowered.degradations.is_empty(),
            "{:?}",
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
            .expect("load loop fixture");
        assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(expected));
    }
}

#[test]
fn conformance_continue_skips_even_values() {
    assert_loop_result(
        include_str!("../../conformance/control-flow/continue-compound.in"),
        5,
    );
    assert_loop_result(
        include_str!("../../conformance/control-flow/z-continue-only.in"),
        5,
    );
}

#[test]
fn nested_break_and_continue_target_the_right_loop() {
    assert_loop_result(
        r#"
fn main() -> Int {
  let total = 0;
  let outer = 0;
  while outer < 3 {
    outer += 1;
    if outer == 2 { continue; }
    let inner = 0;
    while inner < 5 {
      inner += 1;
      if inner == 2 { continue; }
      if inner == 4 { break; }
      total += outer * 10 + inner;
    }
  }
  return total;
}
"#,
        88,
    );
}

#[test]
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
fn loop_control_cannot_skip_try_error_flag_restoration() {
    let module = parse_in_source(
        r#"
fn main() -> Int {
  let count = 0;
  while count < 3 {
    count += 1;
    try { continue; } catch (e) { return 9; }
  }
  return count;
}
"#,
    )
    .expect("parse try in loop");
    let error = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect_err("jumping across try cleanup must not compile");
    assert!(
        error.contains("loop control exiting a try block is unsupported"),
        "{error}"
    );
}

#[test]
fn closure_loop_control_cannot_escape_to_the_enclosing_loop() {
    let module = UnifiedModule {
        identity: Default::default(),
        decls: vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![
                Stmt::Loop {
                    kind: LoopKind::While,
                    cond: Some(Expr::IntLit(0)),
                    body: vec![Stmt::Expr(Expr::Closure {
                        params: vec![],
                        ret: Typ::Void,
                        body: vec![Stmt::Continue],
                        captures: vec![],
                    })],
                },
                Stmt::Return(Some(Expr::IntLit(0))),
            ],
            type_params: vec![],
        }],
    };
    let error =
        inisa::eval_module(&module, "main").expect_err("closure cannot continue outer loop");
    assert!(error.contains("continue outside a loop"), "{error}");
}
