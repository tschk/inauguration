//! Semantic float regressions built directly in Core IR to isolate native lowering.
#![cfg(all(unix, target_arch = "aarch64"))]

use inauguration::{
    core_ir::{Decl, Expr, FloatVal, Stmt, Typ, UnifiedModule},
    jit_runtime::JitRuntime,
    native_emit::{NativeLinkage, lower},
};

fn ident(name: &str) -> Expr {
    Expr::Ident(name.into())
}

fn binary(op: &str, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op: op.into(),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Call {
        callee: Box::new(ident(name)),
        args,
    }
}

fn module(expression: Expr, values: [f64; 4], ret: Typ) -> UnifiedModule {
    UnifiedModule::new(vec![
        Decl::Function {
            name: "identity".into(),
            params: vec![("value".into(), Typ::Float)],
            ret: Typ::Float,
            body: vec![Stmt::Return(Some(ident("value")))],
            type_params: vec![],
        },
        Decl::Function {
            name: "combine".into(),
            params: ["a", "b", "c", "d"]
                .into_iter()
                .map(|name| (name.into(), Typ::Float))
                .collect(),
            ret: ret.clone(),
            body: vec![Stmt::Return(Some(expression))],
            type_params: vec![],
        },
        Decl::Function {
            name: "main".into(),
            params: vec![],
            ret,
            body: vec![Stmt::Return(Some(call(
                "combine",
                values
                    .into_iter()
                    .map(|value| Expr::FloatLit(FloatVal(value)))
                    .collect(),
            )))],
            type_params: vec![],
        },
    ])
}

fn native_bits(module: &UnifiedModule) -> u64 {
    let code = lower::lower_module(module, "main", NativeLinkage::Executable).unwrap();
    assert!(code.degradations.is_empty(), "{:?}", code.degradations);
    assert!(code.external_refs.is_empty(), "unexpected host call");
    let functions = code
        .function_offsets
        .iter()
        .map(|(name, offset)| (name.clone(), *offset, code.code.len() as u32 - *offset))
        .collect::<Vec<_>>();
    let mut runtime = JitRuntime::new();
    runtime
        .load(&code.code, &functions, &code.relocations)
        .unwrap();
    // This backend's scalar Float ABI returns the f64 bits in X0, as documented
    // by JitRuntime::invoke_float; Boolean comparisons return ordinary 0/1.
    unsafe { runtime.invoke("main", &[]).unwrap() as u64 }
}

fn check(module: UnifiedModule, expected_bits: u64) {
    check_cases(vec![(module, expected_bits)]);
}

fn check_cases(cases: Vec<(UnifiedModule, u64)>) {
    let mut failures = Vec::new();
    for (index, (module, expected_bits)) in cases.into_iter().enumerate() {
        for optimized in [false, true] {
            let mut candidate = module.clone();
            if optimized {
                inauguration::core_opt::optimize_with_entry(&mut candidate.decls, Some("main"));
            }
            let actual = native_bits(&candidate);
            println!(
                "float case={index} optimized={optimized} actual_bits={actual:#018x} expected_bits={expected_bits:#018x} pass={}",
                actual == expected_bits
            );
            if actual != expected_bits {
                failures.push(format!(
                    "case={index} optimized={optimized}: actual={actual:#018x}, expected={expected_bits:#018x}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn right_nested_sum() -> Expr {
    binary(
        "+",
        ident("a"),
        binary("+", ident("b"), binary("+", ident("c"), ident("d"))),
    )
}

#[test]
fn float_nested_sum_preserves_each_left_operand() {
    check(
        module(right_nested_sum(), [1.0, 2.0, 3.0, 4.0], Typ::Float),
        10.0f64.to_bits(),
    );
}

#[test]
fn float_nested_fractional_sum_preserves_full_bit_patterns() {
    // All operands and the independent result are exactly representable in f64.
    check(
        module(right_nested_sum(), [1.5, 2.25, 3.125, 4.0], Typ::Float),
        10.875f64.to_bits(),
    );
}

#[test]
fn float_nested_subtraction_preserves_operand_order() {
    let expr = binary(
        "-",
        ident("a"),
        binary("-", ident("b"), binary("-", ident("c"), ident("d"))),
    );
    check(
        module(expr, [1.0, 2.0, 3.0, 4.0], Typ::Float),
        (-2.0f64).to_bits(),
    );
}

#[test]
fn float_nested_division_uses_the_complete_denominator() {
    let expr = binary(
        "/",
        ident("a"),
        binary("+", ident("b"), binary("+", ident("c"), ident("d"))),
    );
    check(
        module(expr, [72.0, 2.0, 3.0, 4.0], Typ::Float),
        8.0f64.to_bits(),
    );
}

#[test]
fn float_rhs_call_preserves_the_preceding_value() {
    let expr = binary("+", ident("a"), call("identity", vec![ident("b")]));
    check(
        module(expr, [1.0, 2.0, 0.0, 0.0], Typ::Float),
        3.0f64.to_bits(),
    );
}

#[test]
fn float_comparison_preserves_composite_operands_and_boolean_abi() {
    let expr = binary(
        "<",
        ident("a"),
        binary("+", ident("b"), binary("+", ident("c"), ident("d"))),
    );
    // 7 < (1 + (2 + 3)) is false; losing b produces 7 < 8 instead.
    let nested_comparison = module(expr, [7.0, 1.0, 2.0, 3.0], Typ::Bool);
    let expr = binary("==", ident("a"), call("identity", vec![ident("b")]));
    check_cases(vec![
        (nested_comparison, 0),
        (module(expr, [1.0, 2.0, 0.0, 0.0], Typ::Bool), 0),
    ]);
}

#[test]
fn float_composite_nesting_honors_the_existing_spill_pool_bound() {
    let nested = |operators| {
        let mut expression = ident("a");
        for _ in 0..operators {
            expression = binary("+", ident("a"), expression);
        }
        module(expression, [1.0; 4], Typ::Float)
    };
    // The innermost RHS is a leaf, so 65 operators require 64 live spills.
    let result = native_bits(&nested(65));
    // Make the deeply nested function itself the entry, so a lowering rejection
    // cannot be hidden behind the fallback body of a non-entry function.
    let error = lower::lower_module(&nested(66), "combine", NativeLinkage::Executable).err();
    println!(
        "float boundary65={} boundary66_error={error:?}",
        f64::from_bits(result)
    );
    assert_eq!(result, 66.0f64.to_bits());
    let error = error.expect("excessive nesting must be rejected");
    assert!(
        error.contains("binary expression nesting is too deep"),
        "{error}"
    );
}
