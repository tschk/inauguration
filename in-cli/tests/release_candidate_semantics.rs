//! Port release-candidate evaluator regressions to the current compiler API.
use inauguration::{core_opt, in_lang_parse::parse_in_source, native_emit::inisa};

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_rust_assignments_preserve_values_order_and_integer_kinds() {
    use inauguration::emit_profile::EmitProfile;
    for source in [
        "fn main()->i64{let mut x:i64=20;x+={x=40;2};x}",
        "const VALUE:i64={let mut x:i64=20;x+=22;x};fn main()->i64{VALUE}",
        "fn main()->i64{let mut a:i64=20;let mut b:i64=22;(a,b)=(b,a);if a==22&&b==20{42}else{7}}",
        "struct Row{bits:u64}fn main()->i64{let mut row=Row{bits:0x8000000000000000u64};row.bits>>=1;if row.bits==0x4000000000000000u64&&row.bits>0{42}else{7}}",
        "fn main()->i64{let mut bits:[u64;1]=[0x8000000000000000u64];bits[0]/=2;if bits[0]==0x4000000000000000u64{42}else{7}}",
        "fn main()->i64{let mut bits=[0x8000000000000000u64];bits[0]/=1+1;if bits[0]==0x4000000000000000u64{42}else{7}}",
        "fn main()->i64{let mut bits=[0x8000000000000000u64];bits[0]/={2};if bits[0]==0x4000000000000000u64{42}else{7}}",
        "fn main()->i64{let mut bits=[0x8000000000000000u64];bits[0]/=if true{2}else{3};if bits[0]==0x4000000000000000u64{42}else{7}}",
        "fn main()->i64{let mut bits=[0x8000000000000000u64];let divisor=2;bits[0]/=divisor;if bits[0]==0x4000000000000000u64{42}else{7}}",
        "fn main()->i64{let mut bits=[0x8000000000000000u64];bits[0]/={let divisor=2;divisor};if bits[0]==0x4000000000000000u64{42}else{7}}",
        "fn main()->i64{let mut bits=[0x8000000000000000u64];bits[0]/=match true{true=>2,false=>3};if bits[0]==0x4000000000000000u64{42}else{7}}",
        "struct Row{bits:u64}static mut ROW:Row=Row{bits:0x8000000000000000u64};fn change(){unsafe{ROW.bits>>=\"x\".len();}}fn main()->i64{change();unsafe{if ROW.bits==0x4000000000000000u64{42}else{7}}}",
        "fn main()->i64{let value=if true{{return 42;}}else{0};value}",
        "fn main()->i64{let value=match 0{0=>return 42,_=>0};value}",
    ] {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let parsed = inauguration::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module = inauguration::typecheck::normalize_module(
                inauguration::parser_registry::ParserId::Rust,
                &parsed,
            );
            inauguration::lower_core::desugar_module(&mut module);
            core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            let runtime = native_runtime(&module);
            assert_eq!(
                unsafe { runtime.invoke("main", &[]) },
                Some(42),
                "{source} {profile:?}"
            );
        }
    }
}

fn assert_result(source: &str, expected: i64) {
    let raw = parse_in_source(source).expect("parse release-candidate regression");
    assert_eq!(inisa::eval_module(&raw, "main").unwrap(), expected);
    let mut optimized = raw;
    core_opt::optimize_with_entry(&mut optimized.decls, Some("main"));
    assert_eq!(inisa::eval_module(&optimized, "main").unwrap(), expected);
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_runtime(
    module: &inauguration::core_ir::UnifiedModule,
) -> inauguration::jit_runtime::JitRuntime {
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
    runtime
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
        assert_eq!(
            unsafe { native_runtime(&module).invoke("main", &[]) },
            Some(42),
            "{source}"
        );
        core_opt::optimize_with_entry(&mut module.decls, Some("main"));
        assert_eq!(
            unsafe { native_runtime(&module).invoke("main", &[]) },
            Some(42),
            "optimized: {source}"
        );
    }
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_record_arrays_preserve_values_aliases_and_store_order() {
    use inauguration::emit_profile::EmitProfile;
    // Exercise all optimizer profiles with the native lowerer's default emission profile.
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        for (name, source, expected) in [
            (
                "local-fields",
                "struct Row{Int x}\nfn main()->Int{let rows:[Row]=[Row{x:20},Row{x:22}];return rows[0].x+rows[1].x;}\n",
                42,
            ),
            (
                "independent-repeated-values",
                "struct Row{Int x}\nfn main()->Int{let same:Row=Row{x:20};let rows:[Row]=[same,same];let copy:Row=rows[0];rows[0].x=22;return copy.x+rows[1].x+rows[0].x - 20;}\n",
                42,
            ),
            (
                "alias-and-element-replacement",
                "struct Row{Int x;Int y}\nfn main()->Int{let rows:[Row]=[Row{x:20,y:1},Row{x:1,y:2}];let alias=rows;alias[0]=Row{x:21,y:2};alias[0]=alias[0];let copy:Row=rows[0];rows[0].x=19;return copy.x+copy.y+rows[0].x;}\n",
                42,
            ),
            (
                "returned-array-lifetime",
                "struct Row{Int x}\nfn make()->[Row]{let rows:[Row]=[Row{x:20},Row{x:22}];return rows;}\nfn main()->Int{let rows=make();let trash:[Int]=[99,99,99];return rows[0].x+rows[1].x;}\n",
                42,
            ),
            (
                "parameter-mutation",
                "struct Row{Int x}\nfn alter(rows:[Row])->Void{rows[0].x=22;}\nfn main()->Int{let rows:[Row]=[Row{x:20},Row{x:20}];alter(rows);return rows[0].x+rows[1].x;}\n",
                42,
            ),
            (
                "indexed-self-receiver",
                "struct Row{Int x}\nfn change(self:Row)->Void{self.x=self.x+2;}\nfn main()->Int{let rows:[Row]=[Row{x:20},Row{x:20}];change(rows[0]);return rows[0].x+rows[1].x;}\n",
                42,
            ),
            (
                "by-value-argument",
                "struct Row{Int x}\nfn byvalue(value:Row)->Int{value.x=99;return 22;}\nfn main()->Int{let rows:[Row]=[Row{x:20}];return byvalue(rows[0])+rows[0].x;}\n",
                42,
            ),
            (
                "mixed-field-types",
                "struct Row{Float amount;String text;Bool flag}\nfn main()->Int{let rows:[Row]=[Row{amount:1.5,text:\"λ\",flag:false}];rows[0].text=\"雪\";rows[0].flag=true;rows[0].amount=2.5;if rows[0].amount==2.5 && rows[0].text==\"雪\" && rows[0].flag{return 42;}return 0;}\n",
                42,
            ),
            (
                "global-element-value-copy",
                "struct Row{Int x}\nvar rows:[Row]=[Row{x:20},Row{x:22}]\nfn main()->Int{let copy:Row=rows[0];rows[0]=rows[1];rows[1].x=20;return copy.x+rows[0].x;}\n",
                42,
            ),
            (
                "base-index-rhs-order",
                "struct Row{Int x;Int y}\nvar rows:[Row]=[Row{x:20,y:1}]\nvar trace:Int=0\nfn get()->[Row]{trace=trace*10+1;return rows;}\nfn at()->Int{trace=trace*10+2;return 0;}\nfn next()->Row{trace=trace*10+3;return Row{x:40,y:2};}\nfn main()->Int{get()[at()]=next();if trace!=123{return 1;}return rows[0].x+rows[0].y;}\n",
                42,
            ),
            (
                "throwing-element-replacement",
                "struct Row{Int x;Int y}\nfn fail()->Row{throw 7;return Row{x:99,y:99};}\nfn main()->Int{let rows:[Row]=[Row{x:20,y:22}];try{rows[0]=fail();}catch e{}return rows[0].x+rows[0].y;}\n",
                42,
            ),
            (
                "index-bounds",
                "struct Row{Int x}\nfn read(rows:[Row],index:Int)->Int{return rows[index].x;}\nfn main()->Int{let rows:[Row]=[Row{x:42}];if read(rows,-1)!=1 || read(rows,1)!=1{return 2;}return read(rows,0);}\n",
                42,
            ),
            (
                "empty-array",
                "struct Row{Int x}\nfn read(rows:[Row],index:Int)->Int{return rows[index].x;}\nfn main()->Int{let rows:[Row]=[];if read(rows,0)==1{return 42;}return 0;}\n",
                42,
            ),
            (
                "array-rebinding",
                "struct Row{Int x}\nfn main()->Int{let rows:[Row]=[Row{x:20}];let earlier=rows;rows=[Row{x:22}];rows[0].x=23;if earlier[0].x!=20{return 1;}return earlier[0].x+rows[0].x - 1;}\n",
                42,
            ),
            (
                "replacement-field-block",
                "struct Row{Int x;Int y}\nvar trace:Int=0\nfn main()->Int{let rows:[Row]=[Row{x:1,y:2}];rows[0]=Row{x:{trace=trace*10+1;20},y:{trace=trace*10+2;22}};if trace!=12{return 1;}return rows[0].x+rows[0].y;}\n",
                42,
            ),
            (
                "replacement-field-throw",
                "struct Row{Int x;Int y}\nfn main()->Int{let rows:[Row]=[Row{x:20,y:22}];try{rows[0]=Row{x:{7},y:{throw 9;99}};}catch e{}return rows[0].x+rows[0].y;}\n",
                42,
            ),
            (
                "global-string-relocations",
                "struct Row{String text;Bool flag;Float amount;Int x}\nvar rows:[Row]=[Row{text:\"λ\0x\",flag:true,amount:1.5,x:20},Row{text:\"雪\",flag:false,amount:2.5,x:22}]\nfn main()->Int{if rows[0].text==\"λ\0y\"{return 2;}if rows[0].text!=\"λ\0x\" || rows[1].text!=\"雪\" || !rows[0].flag || rows[1].amount!=2.5{return 1;}return rows[0].x+rows[1].x;}\n",
                42,
            ),
            (
                "field-oob-rhs-effect",
                "struct Row{Int x;Int y}\nvar effects:Int=0;\nfn effect()->Int{effects=effects+1;return 42;}\nfn write()->Int{let rows:[Row]=[Row{x:20,y:22}];rows[9].x=effect();return 7;}\nfn main()->Int{let failure=write();if failure==1&&effects==1{return 42;}return 7;}\n",
                42,
            ),
            (
                "whole-oob-rhs-effect",
                "struct Row{Int x;Int y}\nvar effects:Int=0;\nfn effect()->Int{effects=effects+1;return 20;}\nfn write()->Int{let rows:[Row]=[Row{x:1,y:2}];rows[9]=Row{x:effect(),y:22};return 7;}\nfn main()->Int{let failure=write();if failure==1&&effects==1{return 42;}return 7;}\n",
                42,
            ),
            (
                "field-oob-throwing-rhs",
                "struct Row{Int x;Int y}\nvar effects:Int=0;\nfn effect()->Int{effects=effects+1;throw 42;}\nfn write()->Int{let rows:[Row]=[Row{x:1,y:2}];rows[9].x=effect();return 7;}\nfn main()->Int{try{write();}catch(_){if effects==1{return 42;}}return 7;}\n",
                42,
            ),
            (
                "whole-oob-throwing-rhs",
                "struct Row{Int x;Int y}\nvar effects:Int=0;\nfn effect()->Int{effects=effects+1;throw 42;}\nfn write()->Int{let rows:[Row]=[Row{x:1,y:2}];rows[9]=Row{x:effect(),y:22};return 7;}\nfn main()->Int{try{write();}catch(_){if effects==1{return 42;}}return 7;}\n",
                42,
            ),
        ] {
            let mut module = parse_in_source(source).expect(name);
            inauguration::lower_core::desugar_module(&mut module);
            core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            let runtime = native_runtime(&module);
            assert_eq!(
                unsafe { runtime.invoke("main", &[]) },
                Some(expected),
                "{name}: {profile:?}"
            );
        }
    }
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_record_array_globals_survive_repeated_entry() {
    use inauguration::emit_profile::EmitProfile;
    // Exercise all optimizer profiles with the native lowerer's default emission profile.
    let source = "struct Row{Int value;String label}\nvar rows:[Row]=[Row{value:20,label:\"λ\"},Row{value:22,label:\"雪\"}]\nvar calls:Int=0\nfn main()->Int{\n if rows[0].label!=\"λ\" || rows[1].label!=\"雪\"{return 1;}\n rows[0].value=20;rows[1].value=22;\n calls=calls+1;\n rows[1].value=rows[0].value+rows[1].value;\n return rows[1].value;\n}\n\nfn read-calls()->Int{return calls;}\n";
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        let mut module = parse_in_source(source).unwrap();
        inauguration::lower_core::desugar_module(&mut module);
        core_opt::optimize_with_linkage(&mut module.decls, Some("main"), profile, true);
        let runtime = native_runtime(&module);
        assert_eq!(
            unsafe { runtime.invoke("main", &[]) },
            Some(42),
            "first: {profile:?}"
        );
        assert_eq!(
            unsafe { runtime.invoke("main", &[]) },
            Some(42),
            "second: {profile:?}"
        );
        assert_eq!(
            unsafe { runtime.invoke("read-calls", &[]) },
            Some(2),
            "calls: {profile:?}"
        );
    }
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_rust_flat_record_array_patterns_select_matching_arm() {
    for (pattern, yes, no) in [("20", 42, 0), ("19", 0, 42)] {
        let source = format!(
            "struct Point {{x:i64,y:i64}} fn main()->i64 {{ let values=[Point{{x:20,y:22}}]; match values {{[Point{{x:{pattern},y:22}}]=>{yes},_=>{no}}} }}"
        );
        let parsed = inauguration::compiler::rust_front::parse_rust_source(&source).unwrap();
        let mut module = inauguration::typecheck::normalize_module(
            inauguration::parser_registry::ParserId::Rust,
            &parsed,
        );
        inauguration::lower_core::desugar_module(&mut module);
        let runtime = native_runtime(&module);
        assert_eq!(
            unsafe { runtime.invoke("main", &[]) },
            Some(42),
            "{pattern}"
        );
    }
}
#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_record_array_nested_replacements_preserve_outer_staging() {
    let fields = (0..16)
        .map(|index| format!("Int f{index}"))
        .collect::<Vec<_>>()
        .join(";");
    let row = |first: i64, second: &str| {
        let values = (0..16)
            .map(|index| {
                let value = match index {
                    0 => first.to_string(),
                    1 => second.to_owned(),
                    15 => "22".to_owned(),
                    _ => "0".to_owned(),
                };
                format!("f{index}:{value}")
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("Row{{{values}}}")
    };
    let initial = row(0, "0");
    let mut replacement = row(7, "0");
    for first in [5, 3, 1, 20] {
        replacement = row(first, &format!("{{rows[0]={replacement};0}}"));
    }
    let source = format!(
        "struct Row{{{fields}}}\nfn main()->Int{{let rows:[Row]=[{initial}];rows[0]={replacement};return rows[0].f0+rows[0].f15;}}"
    );
    // Five live 16-word staging regions exceed the old 64-word scratch allowance.
    let module = parse_in_source(&source).unwrap();
    let runtime = native_runtime(&module);
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(42));
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_rust_flat_record_globals_use_initialized_payloads() {
    let source = "struct Bag { value: i64 } static VALUES: [Bag;2]=[Bag { value: 20 }, Bag { value: 22 }]; fn main() -> i64 { return VALUES[0].value + VALUES[1].value; }";
    let parsed = inauguration::compiler::rust_front::parse_rust_source(source).unwrap();
    let mut module = inauguration::typecheck::normalize_module(
        inauguration::parser_registry::ParserId::Rust,
        &parsed,
    );
    inauguration::lower_core::desugar_module(&mut module);
    let runtime = native_runtime(&module);
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(42));
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_field_compounds_preserve_place_read_and_rhs_order() {
    use inauguration::emit_profile::EmitProfile;
    let mut cases = vec![
        (
            "changing-index-once".to_string(),
            "struct Row{Int x}\nvar calls:Int=0;\nfn at()->Int{let result=calls;calls=calls+1;return result;}\nfn main()->Int{let rows:[Row]=[Row{x:20},Row{x:100}];rows[at()].x+=2;if calls==1&&rows[0].x==22&&rows[1].x==100{return 42;}return 7;}".to_string(),
        ),
        (
            "base-index-read-before-rhs".to_string(),
            "struct Row{Int x}\nvar trace:Int=0;\nvar rows:[Row]=[Row{x:20}];\nfn backing()->[Row]{trace=trace*10+1;return rows;}\nfn at()->Int{trace=trace*10+2;return 0;}\nfn rhs()->Int{trace=trace*10+3;rows[0].x=100;return 2;}\nfn main()->Int{backing()[at()].x+=rhs();if trace==123&&rows[0].x==22{return 42;}return 7;}".to_string(),
        ),
        (
            "rhs-rebind-keeps-original-backing".to_string(),
            "struct Row{Int x}\nfn main()->Int{let rows:[Row]=[Row{x:20}];let old=rows;rows[0].x+={rows=[Row{x:100}];2};return old[0].x+rows[0].x - 80;}".to_string(),
        ),
        (
            "throwing-rhs-no-final-store".to_string(),
            "struct Row{Int x}\nvar effects:Int=0;\nfn fail()->Int{effects=effects+1;throw 99;}\nfn main()->Int{let rows:[Row]=[Row{x:20}];try{rows[0].x+=fail();}catch(_){if effects==1&&rows[0].x==20{return 42;}}return 7;}".to_string(),
        ),
        (
            "plain-local-field-read-before-rhs".to_string(),
            "struct Row{Int x}\nfn main()->Int{let row=Row{x:20};row.x+={row.x=100;2};return row.x+20;}".to_string(),
        ),
        (
            "plain-global-field-read-before-rhs".to_string(),
            "struct Row{Int x}\nvar row:Row=Row{x:20};\nfn rhs()->Int{row.x=100;return 2;}\nfn main()->Int{row.x+=rhs();return row.x+20;}".to_string(),
        ),
        (
            "out-of-bounds-read-skips-rhs".to_string(),
            "struct Row{Int x}\nvar effects:Int=0;\nfn effect()->Int{effects=effects+1;return 2;}\nfn write()->Int{let rows:[Row]=[Row{x:20}];rows[9].x+=effect();return 7;}\nfn main()->Int{let failed=write();if failed==1&&effects==0{return 42;}return 7;}".to_string(),
        ),
        (
            "float-field-add".to_string(),
            "struct Row{Float x}\nfn main()->Int{let rows:[Row]=[Row{x:20.5}];rows[0].x+=21.5;if rows[0].x==42.0{return 42;}return 7;}".to_string(),
        ),
    ];
    for (op, initial, rhs, adjustment) in [
        ("+", 20, 2, 20),
        ("-", 44, 2, 0),
        ("*", 21, 2, 0),
        ("/", 84, 2, 0),
        ("%", 85, 43, 0),
        ("&", 63, 42, 0),
        ("|", 40, 2, 0),
        ("^", 40, 2, 0),
        ("<<", 21, 1, 0),
        (">>", 84, 1, 0),
    ] {
        cases.push((
            format!("operator-{op}"),
            format!("struct Row{{Int x}}\nvar calls:Int=0;\nfn rhs()->Int{{calls=calls+1;return {rhs};}}\nfn main()->Int{{let rows:[Row]=[Row{{x:{initial}}}];rows[0].x{op}=rhs();if calls!=1{{return 7;}}return rows[0].x+{adjustment};}}"),
        ));
    }
    for (name, source) in cases {
        let raw = parse_in_source(&source).expect(&name);
        let runtime = native_runtime(&raw);
        assert_eq!(
            unsafe { runtime.invoke("main", &[]) },
            Some(42),
            "raw: {name}"
        );
        let mut desugared = raw;
        inauguration::lower_core::desugar_module(&mut desugared);
        let runtime = native_runtime(&desugared);
        assert_eq!(
            unsafe { runtime.invoke("main", &[]) },
            Some(42),
            "desugared: {name}"
        );
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut module = desugared.clone();
            core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            let runtime = native_runtime(&module);
            assert_eq!(
                unsafe { runtime.invoke("main", &[]) },
                Some(42),
                "{name}: {profile:?}"
            );
        }
    }
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_rust_field_compound_keeps_value_first_contract() {
    use inauguration::emit_profile::EmitProfile;
    // Rust's existing private value-first operation reads the field after RHS.
    // The .in read-before-RHS cases above deliberately have a different contract.
    let parsed = inauguration::compiler::rust_front::parse_rust_source(
        "struct Row{x:i64} fn main()->i64{let mut row=Row{x:20};row.x+={row.x=40;2};row.x}",
    )
    .unwrap();
    let mut module = inauguration::typecheck::normalize_module(
        inauguration::parser_registry::ParserId::Rust,
        &parsed,
    );
    inauguration::lower_core::desugar_module(&mut module);
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        let mut optimized = module.clone();
        core_opt::optimize_with_profile(&mut optimized.decls, Some("main"), profile);
        let runtime = native_runtime(&optimized);
        assert_eq!(
            unsafe { runtime.invoke("main", &[]) },
            Some(42),
            "{profile:?}"
        );
    }
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_declared_field_modify_name_and_compound_syntax_are_distinct() {
    use inauguration::emit_profile::EmitProfile;
    let source = "struct Row{Int x}\nvar calls:Int=0;\nfn field-modify-place-first(row:Row,field:String,value:Int,op:String)->Void{calls=calls+1;}\nfn main()->Int{let row=Row{x:20};field-modify-place-first(row,\"x\",2,\"+\");row.x+=1;return row.x+calls+20;}";
    let canonical = inauguration::in_canonical::canonicalize_in_source(source).unwrap();
    for source in [source, canonical.as_str()] {
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            let mut module = parse_in_source(source).unwrap();
            inauguration::lower_core::desugar_module(&mut module);
            core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            let runtime = native_runtime(&module);
            assert_eq!(
                unsafe { runtime.invoke("main", &[]) },
                Some(42),
                "{profile:?}"
            );
        }
    }
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_pure_field_updates_do_not_exhaust_frame_with_capture_slots() {
    use inauguration::emit_profile::EmitProfile;
    let source = format!(
        "struct Row{{Int x}}\nvar rows:[Row]=[Row{{x:0}}]\nfn main()->Int{{rows[0].x=0;{}if rows[0].x==300{{return 42;}}return 7;}}",
        "rows[0].x+=1;".repeat(300)
    );
    let raw = parse_in_source(&source).unwrap();
    let runtime = native_runtime(&raw);
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(42), "raw");
    for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
        let mut module = raw.clone();
        inauguration::lower_core::desugar_module(&mut module);
        core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
        let runtime = native_runtime(&module);
        assert_eq!(
            unsafe { runtime.invoke("main", &[]) },
            Some(42),
            "{profile:?}"
        );
    }
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_wide_harden_dispatch_preserves_effect_order() {
    use inauguration::emit_profile::EmitProfile;
    let calls = (0..300)
        .map(|index| format!("step({index});"))
        .collect::<String>();
    let source = format!(
        "var next:Int=0;var bad:Int=0;fn step(value:Int)->Void{{if value!=next{{bad=1;}}next=next+1;}}fn main()->Int{{{calls}if next==300 && bad==0{{return 42;}}return 7;}}"
    );
    let mut module = parse_in_source(&source).unwrap();
    inauguration::lower_core::desugar_module(&mut module);
    core_opt::optimize_with_profile(&mut module.decls, Some("main"), EmitProfile::Harden);
    let runtime = native_runtime(&module);
    assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(42));
}

#[test]
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn native_legacy_operation_names_respect_functions_and_local_closures() {
    use inauguration::emit_profile::EmitProfile;
    let generated = "let generated:[Int]=[20];generated[0]+=2;if generated[0]!=22{return 7;}generated[0]=40;generated[0]+=2;if generated[0]!=42{return 8;}let row=GeneratedRow{value:40};row.value+=2;if row.value!=42{return 9;}";
    for (name, params, setup, args, ret, body, result) in [
        (
            "index-assign-value-first",
            "items:[Int],index:Int,value:Int",
            "let items:[Int]=[1];",
            "items,0,2",
            "Void",
            "called=42;",
            "called",
        ),
        (
            "index-modify-value-first",
            "items:[Int],index:Int,value:Int,op:String",
            "let items:[Int]=[1];",
            "items,0,2,\"+\"",
            "Void",
            "called=42;",
            "called",
        ),
        (
            "index-modify-place-first",
            "items:[Int],index:Int,value:Int,op:String",
            "let items:[Int]=[1];",
            "items,0,2,\"+\"",
            "Void",
            "called=42;",
            "called",
        ),
        (
            "field-modify-value-first",
            "row:GeneratedRow,field:String,value:Int,op:String",
            "let target=GeneratedRow{value:1};",
            "target,\"value\",2,\"+\"",
            "Void",
            "called=42;",
            "called",
        ),
        (
            "modify-value-first",
            "target:Int,value:Int,op:String",
            "let target=1;",
            "target,2,\"+\"",
            "Void",
            "called=42;",
            "called",
        ),
        (
            "array-repeat",
            "value:Int,count:Int",
            "",
            "7,2",
            "[Int]",
            "return [42];",
            "items[0]",
        ),
    ] {
        for local in [false, true] {
            let declaration = format!("fn {name}({params})->{ret}{{{body}}}");
            let binding = format!("let {name}=fn({params})->{ret}{{{body}}};");
            let call = if ret == "Void" {
                format!("{name}({args});")
            } else {
                format!("let items={name}({args});")
            };
            let source = format!(
                "struct GeneratedRow{{Int value}}var called:Int=0;{}fn main()->Int{{{}{setup}{call}{generated}return {result};}}",
                if local { "" } else { &declaration },
                if local { &binding } else { "" }
            );
            let canonical = inauguration::in_canonical::canonicalize_in_source(&source).unwrap();
            for source in [source.as_str(), canonical.as_str()] {
                for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
                    let mut module = parse_in_source(source).unwrap();
                    inauguration::lower_core::desugar_module(&mut module);
                    core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                    let runtime = native_runtime(&module);
                    assert_eq!(
                        unsafe { runtime.invoke("main", &[]) },
                        Some(42),
                        "{name} local={local} {profile:?}"
                    );
                }
            }
        }
    }
}
