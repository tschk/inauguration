//! Conditional writes must be observed by a following return on every backend.
#![cfg(all(unix, target_arch = "aarch64"))]

use inauguration::{
    core_opt,
    in_lang_parse::parse_in_source,
    jit_runtime::JitRuntime,
    native_emit::{NativeLinkage, inisa, lower},
};

const SOURCE: &str = r#"
fn choose(value: Int) -> Int {
  let count: Int = 0;
  if value <= 255 { count = 1; }
  return count;
}
fn main() -> Int { return choose(42) * 10 + choose(256); }
"#;

const EXACT_FAILURE_SOURCE: &str = include_str!("nexnet_if_assign.in");

fn assert_all_backends(source: &str, expected: i64) {
    let raw = parse_in_source(source).expect("parse conditional assignment");
    assert_eq!(
        inisa::eval_module(&raw, "main").expect("raw INISA"),
        expected
    );

    let mut optimized = raw.clone();
    core_opt::optimize_with_entry(&mut optimized.decls, Some("main"));
    assert_eq!(
        inisa::eval_module(&optimized, "main").expect("optimized INISA"),
        expected
    );

    let lowered = lower::lower_module(&optimized, "main", NativeLinkage::Executable)
        .expect("lower conditional assignment");
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
        .expect("load conditional assignment");
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(expected));
}

#[test]
fn exact_observed_conditional_assignment_survives_all_backends() {
    assert_all_backends(EXACT_FAILURE_SOURCE, 1);
}

#[test]
fn conditional_assignment_in_function_calls_preserves_both_paths() {
    assert_all_backends(SOURCE, 10);
}
