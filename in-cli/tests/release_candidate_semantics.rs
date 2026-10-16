//! Port release-candidate evaluator regressions to the current compiler API.
use inauguration::{core_opt, in_lang_parse::parse_in_source, native_emit::inisa};

fn assert_result(source: &str, expected: i64) {
    let raw = parse_in_source(source).expect("parse release-candidate regression");
    assert_eq!(inisa::eval_module(&raw, "main").unwrap(), expected);
    let mut optimized = raw;
    core_opt::optimize_with_entry(&mut optimized.decls, Some("main"));
    assert_eq!(inisa::eval_module(&optimized, "main").unwrap(), expected);
}

#[test]
fn logical_operators_skip_rhs_that_exceeds_the_vm_step_budget() {
    for source in [
        "fn fail() -> Bool { while true {} return false; }\nfn main() -> Int { if false && fail() { return 0; } return 42; }",
        "fn fail() -> Bool { while true {} return false; }\nfn main() -> Int { if true || fail() { return 42; } return 0; }",
        "fn yes() -> Bool { return true; }\nfn main() -> Int { if yes() && true { return 42; } return 0; }",
    ] {
        assert_result(source, 42);
    }
}

#[test]
fn nested_loop_jumps_target_the_innermost_loop() {
    assert_result(
        "fn main() -> Int { var i: Int = 0; var total: Int = 0; while i < 3 { i += 1; var j: Int = 0; while j < 4 { j += 1; if j == 2 { continue; } if j == 4 { break; } total += 7; } } return total; }",
        42,
    );
}

#[test]
fn logical_simplification_retains_a_required_lhs_call() {
    use inauguration::core_ir::{Decl, Expr, Stmt, Typ};
    for op in ["&&", "||"] {
        let expression = Expr::Binary {
            op: op.into(),
            lhs: Box::new(Expr::Call {
                callee: Box::new(Expr::Ident("effect".into())),
                args: vec![],
            }),
            rhs: Box::new(Expr::BoolLit(op == "||")),
        };
        let mut decls = vec![Decl::Function {
            name: "main".into(),
            params: vec![],
            ret: Typ::Bool,
            type_params: vec![],
            body: vec![Stmt::Return(Some(expression.clone()))],
        }];
        core_opt::optimize_with_entry(&mut decls, Some("main"));
        let Decl::Function { body, .. } = &decls[0] else {
            panic!("missing main")
        };
        assert_eq!(body, &vec![Stmt::Return(Some(expression))]);
    }
}

#[test]
fn logical_integer_truth_is_normalized_to_zero_or_one() {
    use inauguration::core_ir::{Decl, Expr, Stmt};
    for (op, lhs, rhs, expected) in [
        ("&&", 2, 7, 1),
        ("&&", 0, 7, 0),
        ("||", 2, 7, 1),
        ("||", 0, 7, 1),
        ("||", 0, 0, 0),
    ] {
        let mut module = parse_in_source("fn main() -> Int { return 0; }").unwrap();
        let Decl::Function { body, .. } = &mut module.decls[0] else {
            panic!("missing main")
        };
        *body = vec![Stmt::Return(Some(Expr::Binary {
            op: op.into(),
            lhs: Box::new(Expr::IntLit(lhs)),
            rhs: Box::new(Expr::IntLit(rhs)),
        }))];
        assert_eq!(inisa::eval_module(&module, "main").unwrap(), expected);
        core_opt::optimize_with_entry(&mut module.decls, Some("main"));
        assert_eq!(inisa::eval_module(&module, "main").unwrap(), expected);
    }
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn rust_integer_widths_survive_normalization_and_optimization() {
    use inauguration::{
        compiler::rust_front::parse_rust_source, parser_registry::ParserId, typecheck,
    };
    fn run(module: &inauguration::core_ir::UnifiedModule) -> i64 {
        #[cfg(target_arch = "aarch64")]
        let (code, exports, relocations, data, global_relocs, data_relocations) = {
            let lowered = inauguration::native_emit::lower::lower_module(
                module,
                "main",
                inauguration::native_emit::NativeLinkage::Executable,
            )
            .unwrap();
            assert!(
                lowered.degradations.is_empty(),
                "{:?}",
                lowered.degradations
            );
            (
                lowered.code,
                lowered
                    .exports
                    .into_iter()
                    .map(|export| (export.name, export.offset, 0))
                    .collect::<Vec<_>>(),
                lowered.relocations,
                lowered.data,
                lowered.global_relocs,
                lowered.data_relocations,
            )
        };
        #[cfg(target_arch = "x86_64")]
        let (code, exports, relocations, data, global_relocs, data_relocations) = {
            let lowered =
                inauguration::native_emit::x86_64_lower::lower_module(module, "main").unwrap();
            (
                lowered.code,
                lowered
                    .exports
                    .into_iter()
                    .map(|(name, offset)| (name, offset, 0))
                    .collect::<Vec<_>>(),
                lowered
                    .relocations
                    .into_iter()
                    .map(|offset| (offset, lowered.codegen_base))
                    .collect::<Vec<_>>(),
                lowered.data,
                lowered.global_relocs,
                lowered.data_relocations,
            )
        };
        let mut runtime = inauguration::jit_runtime::JitRuntime::new();
        runtime
            .load_with_data_relocations(
                &code,
                &exports,
                &relocations,
                &data,
                &global_relocs,
                &data_relocations,
            )
            .unwrap();
        unsafe { runtime.invoke("main", &[]).unwrap() }
    }
    for source in [
        "fn main()->i32 { let x:u8=128; if (x << 1) == 0 { return 42; } return 7; }",
        "fn main()->i32 { let x:u16=32768; if (x << 1) == 0 { return 42; } return 7; }",
        "fn main()->i32 { let x:u32=2147483648; if (x << 1) == 0 { return 42; } return 7; }",
        "fn main()->i32 { let x:u8=0; if !x == 255 { return 42; } return 7; }",
        "fn main()->i32 { let x:u64=18446744073709551615u64; if (x >> 63) == 1 && x / 2 == 9223372036854775807u64 && x % 2 == 1 && x > 1 { return 42; } return 7; }",
        "fn main()->i64 { let x=42; return x; }",
        "fn main()->i64{255u8.wrapping_add(43) as i64}",
        "fn main()->i64{0u16.wrapping_sub(65494) as i64}",
        "fn main()->i64{4294967295u32.wrapping_mul(4294967254) as i64}",
        "fn main()->i64{0xffffffffffffffffu64.wrapping_add(43) as i64}",
        "fn main()->i64{if 127i8.wrapping_add(1)==-128i8{42}else{7}}",
        "fn main()->i64{if (-128i8).wrapping_sub(1)==127i8{42}else{7}}",
        "static mut TRACE:i64=0;fn left()->u8{unsafe{TRACE=TRACE*10+1;}255}fn right()->u8{unsafe{TRACE=TRACE*10+2;}43}fn main()->i64{let result=left().wrapping_add(right());if result==42 && unsafe{TRACE}==12{42}else{7}}",
        "static mut TRACE:i64=0;fn next()->u8{unsafe{TRACE=1;}255}fn main()->i64{if false && next().wrapping_add(43)==42{return 7;}if unsafe{TRACE}==0{42}else{7}}",
        "fn main()->i64{let value=const{let x:u8=255;x.wrapping_add(43)};value as i64}",
        "fn main()->i64{let value=const{let x:i64=9223372036854775807;x.wrapping_add(1)};if value == -9223372036854775808i64{42}else{7}}",
        "static VALUE:u8={let mut x:u8=254;let mut i:u8=0;while i<44{x=x.wrapping_add(1);i=i+1;}x};fn main()->i64{VALUE as i64}",
        "struct Counter{value:u8}impl Counter{fn wrapping_add(&self,extra:u8)->u8{self.value+extra}}fn main()->i64{let value=Counter{value:20};value.wrapping_add(22) as i64}",
        "fn byte()->u8{254} fn mask(x:bool,y:bool)->bool{!((x as u8)|(y as u8))==byte()} fn main()->i64{if mask(true,false){42}else{7}}",
        "fn main()->i32 { let mut values=Vec::new(); values.push(213u8); if !values[0] == 42 { return 42; } return 7; }",
        "fn next()->u8 { return 128; } fn main()->i32 { if (next() << 1) == 0 { return 42; } return 7; }",
        "struct Value { bits:u8 } fn main()->i32 { let x=Value { bits:128 }; if (x.bits << 1) == 0 { return 42; } return 7; }",
    ] {
        let raw = parse_rust_source(source).unwrap();
        let mut module = typecheck::normalize_module(ParserId::Rust, &raw);
        inauguration::lower_core::desugar_module(&mut module);
        assert_eq!(run(&module), 42, "{source}");
        core_opt::optimize_with_entry(&mut module.decls, Some("main"));
        assert_eq!(run(&module), 42, "optimized: {source}");
    }
}
