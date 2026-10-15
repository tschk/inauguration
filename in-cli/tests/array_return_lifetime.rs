//! A local array cannot be returned as a pointer into a callee's expired stack.
#![cfg(all(unix, target_arch = "aarch64"))]

use inauguration::{
    core_ir::{Decl, Expr, Stmt},
    core_opt,
    in_lang_parse::parse_in_source,
    jit_runtime::JitRuntime,
    native_emit::{NativeLinkage, lower},
};

const FORWARDED_LOCAL: &str = r#"
fn forward(xs: [Int], flag: Int) -> [Int] {
  if flag > 0 { return xs; }
  return xs;
}
fn make() -> [Int] {
  let local: [Int] = [3, 7];
  return forward(local, 1);
}
fn main() -> Int { let returned: [Int] = make(); return returned[1]; }
"#;

const ALIASED_FORWARDED_LOCAL: &str = r#"
fn forward(xs: [Int], flag: Int) -> [Int] {
  if flag > 0 { return xs; }
  return xs;
}
fn make() -> [Int] {
  let local: [Int] = [3, 7];
  let alias: [Int] = forward(local, 1);
  return alias;
}
fn main() -> Int { let returned: [Int] = make(); return returned[1]; }
"#;

const REFORWARDED_ALIAS: &str = r#"
fn forward(xs: [Int], flag: Int) -> [Int] {
  if flag > 0 { return xs; }
  return xs;
}
fn make() -> [Int] {
  let local: [Int] = [3, 7];
  let alias: [Int] = forward(local, 1);
  return forward(alias, 1);
}
fn main() -> Int { let returned: [Int] = make(); return returned[1]; }
"#;

const REASSIGNED_PARAMETER: &str = r#"
fn forward(xs: [Int], flag: Int) -> [Int] {
  if flag > 0 { return xs; }
  return xs;
}
fn overwrite(xs: [Int]) -> [Int] {
  let local: [Int] = [3, 7];
  xs = forward(local, 1);
  return xs;
}
fn main() -> Int {
  let original: [Int] = [9, 11];
  let returned: [Int] = overwrite(original);
  return returned[1];
}
"#;

const BORROWED_PARAMETER: &str = r#"
fn forward(xs: [Int], flag: Int) -> [Int] {
  if flag > 0 { return xs; }
  return xs;
}
fn bridge(xs: [Int]) -> [Int] { return forward(xs, 1); }
fn main() -> Int {
  let local: [Int] = [3, 7];
  let borrowed: [Int] = bridge(local);
  return borrowed[1] - 7;
}
"#;

/// Executable variant of the preserved `nexnet_array_copy_update.in` probe:
/// `update` keeps the copy-on-write shape but returns the updated scalar, so
/// `main` can verify the caller's `xs` stays unchanged at runtime. The fixture
/// itself stays degraded (IN3001) because it returns its local array.
const COPY_ISOLATION: &str = r#"
fn update(xs: [Int], at: Int, value: Int) -> Int {
  let next: [Int] = [xs[0], xs[1], xs[2]];
  next[at] = value;
  return next[at];
}
fn main() -> Int {
  let xs: [Int] = [1, 2, 3];
  let updated: Int = update(xs, 1, 7);
  if xs[1] != 2 { return 1; }
  return updated - 7;
}
"#;

#[test]
fn returning_a_local_array_fails_lowering_before_jit_execution() {
    // Exact source from Nexnet's preserved array_copy_update.in probe.
    let source = include_str!("nexnet_array_copy_update.in");
    let module = parse_in_source(source).expect("parse array copy/update fixture");
    let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect("lowering emits a diagnosed trap for unsupported returns");
    let degradation = lowered
        .degradations
        .iter()
        .find(|item| item.function == "update")
        .expect("local-array return must be diagnosed");
    assert_eq!(degradation.code, "IN3001");
    assert!(degradation.reachable, "main calls update");
    assert!(
        degradation
            .reason
            .contains("would outlive its stack storage"),
        "{}",
        degradation.reason
    );
}

#[test]
fn returning_a_literal_array_remains_supported() {
    // Separate passing Nexnet control: literal data has a stable lifetime.
    let source = include_str!("nexnet_array_return_literal.in");
    let module = parse_in_source(source).expect("parse literal-array return fixture");
    let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect("lower literal-array return fixture");
    assert!(
        lowered.degradations.is_empty(),
        "{:?}",
        lowered.degradations
    );
    // main must also be able to read the array returned by make() at runtime.
    let functions = lowered
        .function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset, lowered.code.len() as u32 - *offset))
        .collect::<Vec<_>>();
    let mut runtime = JitRuntime::new();
    runtime
        .load(&lowered.code, &functions, &lowered.relocations)
        .expect("load literal-array return fixture");
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(0));
}

#[test]
fn returning_a_call_that_may_forward_a_local_array_is_rejected() {
    for optimized in [false, true] {
        let mut module = parse_in_source(FORWARDED_LOCAL).expect("parse forwarded local array");
        if optimized {
            core_opt::optimize_with_entry(&mut module.decls, Some("main"));
        }
        let make_body = module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function { name, body, .. } if name == "make" => Some(body),
                _ => None,
            })
            .expect("make remains reachable");
        assert!(
            make_body.iter().any(|stmt| matches!(
                stmt,
                Stmt::Return(Some(Expr::Call { callee, .. }))
                    if matches!(callee.as_ref(), Expr::Ident(name) if name == "forward")
            )),
            "forward call must remain present to test the non-inlined return path"
        );
        let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
            .expect("lowering emits a diagnosed trap for the escaping call");
        let degradation = lowered
            .degradations
            .iter()
            .find(|item| item.function == "make")
            .expect("forwarded local array must be diagnosed");
        assert_eq!(degradation.code, "IN3001");
        assert!(degradation.reachable, "main calls make");
        assert!(
            degradation
                .reason
                .contains("stack-local array argument `local`"),
            "{}",
            degradation.reason
        );
    }
}

#[test]
fn returning_an_alias_of_a_forwarded_local_array_is_rejected() {
    for optimized in [false, true] {
        let mut module =
            parse_in_source(ALIASED_FORWARDED_LOCAL).expect("parse alias of forwarded local array");
        if optimized {
            core_opt::optimize_with_entry(&mut module.decls, Some("main"));
        }
        let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
            .expect("lowering emits a diagnosed trap for the escaping alias");
        let degradation = lowered
            .degradations
            .iter()
            .find(|item| item.function == "make")
            .expect("alias of forwarded local array must be diagnosed");
        assert_eq!(degradation.code, "IN3001");
        assert!(degradation.reachable, "main calls make");
        assert!(
            degradation.reason.contains("unproven storage lifetime"),
            "{}",
            degradation.reason
        );
    }
}

#[test]
fn reforwarded_alias_and_reassigned_parameter_do_not_escape() {
    for (source, degraded_function) in [
        (REFORWARDED_ALIAS, "make"),
        (REASSIGNED_PARAMETER, "overwrite"),
    ] {
        for optimized in [false, true] {
            let mut module = parse_in_source(source).expect("parse array escape fixture");
            if optimized {
                core_opt::optimize_with_entry(&mut module.decls, Some("main"));
            }
            let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
                .expect("lowering emits a diagnosed trap for the array escape");
            let degradation = lowered
                .degradations
                .iter()
                .find(|item| item.function == degraded_function)
                .expect("array escape must be diagnosed");
            assert_eq!(degradation.code, "IN3001");
            assert!(degradation.reachable);
            assert!(
                degradation.reason.contains("unproven storage lifetime"),
                "{}",
                degradation.reason
            );
        }
    }
}

#[test]
fn forwarded_borrowed_parameter_can_be_consumed_in_the_callers_frame() {
    let module = parse_in_source(BORROWED_PARAMETER).expect("parse borrowed array path");
    let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect("lower borrowed array path");
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
        .expect("load borrowed array path");
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(0));
}

#[test]
fn array_copy_update_isolates_the_callers_array_when_executed() {
    let module = parse_in_source(COPY_ISOLATION).expect("parse copy-isolation fixture");
    let lowered = lower::lower_module(&module, "main", NativeLinkage::Executable)
        .expect("lower copy-isolation fixture");
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
        .expect("load copy-isolation fixture");
    // Returns 0 only when xs[1] is still 2 and the updated element is 7.
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(0));
}
