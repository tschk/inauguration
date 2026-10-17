use super::*;

fn typed(source: &str) -> MirModule {
    let core = crate::in_lang_parse::parse_in_source(source).unwrap();
    crate::compiler::mir_lower::lower_to_mir(&core).unwrap()
}
const SOURCES: &[&str] = &[
    "fn main()->Int{return (20+22)*(3-2);}",
    "fn add(a:Int,b:Int)->Int{return a+b;}fn main()->Int{return add(20,22);}",
    "fn inc(value:Int)->Int{return value+1;}fn main()->Int{return inc(20)+inc(20);}",
    "fn nop()->Void{return;}fn main()->Int{let saved:Int=42;nop();return saved;}",
    "fn invert(value:Bool)->Bool{return !value;}fn main()->Int{if invert(false){return 42;}return 0;}",
    "fn sub(a:Int,b:Int)->Int{return a - b;}fn main()->Int{let a:Int=20;let b:Int=62;return sub(b,a);}",
    "fn sum(a:Int,b:Int,c:Int,d:Int,e:Int,f:Int)->Int{return a+b+c+d+e+f;}fn main()->Int{return sum(1,2,3,4,10,22);}",
    "fn triangular(n:Int)->Int{if n<=0{return 0;}return n+triangular(n - 1);}fn main()->Int{return triangular(8)+6;}",
    "fn main()->Int{let i:Int=0;let total:Int=0;while i<3{i+=1;let j:Int=0;while j<3{j+=1;if j==1{continue;}total+=7;break;}}return total*2;}",
    "fn main()->Int{return ~(-43);}",
    "fn main()->Int{let result:Int=0;if 20<22{result+=7;}if 20<=20{result+=7;}if 42>22{result+=7;}if 42>=42{result+=7;}if 42==42{result+=7;}if 42!=0{result+=7;}return result;}",
    "fn main()->Int{if (false&&{return 7;true}){return 0;}return 42;}",
    "fn main()->Int{let x:Int=20;if true{let x:Int=x+22;if x!=42{return 0;}}return x+22;}",
    "fn main()->Int{let value:Int=if true {20}else{0};return value+22;}",
    "fn main()->Int{if (false||{return 42;false}){return 0;}return 0;}",
];
#[test]
fn scalar_machine_pass_validates_both_target_graphs_and_spills_across_calls() {
    for source in SOURCES {
        let input = typed(source);
        for target in [ScalarTarget::Aarch64, ScalarTarget::X86_64] {
            let machine = lower_scalar_typed(&input, target)
                .unwrap_or_else(|error| panic!("{target:?} {source}: {error}"));
            assert!(machine.verify_machine().is_ok());
            for function in &machine.functions {
                assert!(function.value_types.is_empty());
                assert!(function.local_types.is_empty());
                assert!(
                    !function
                        .instructions
                        .iter()
                        .any(|inst| matches!(inst.op, MirOp::Typed(_)))
                );
            }
        }
    }
    let machine = lower_scalar_typed(
        &typed("fn inc(x:Int)->Int{return x+1;}fn main()->Int{return inc(20)+inc(20);}"),
        ScalarTarget::Aarch64,
    )
    .unwrap();
    let main = machine
        .functions
        .iter()
        .find(|function| function.name == "main")
        .unwrap();
    assert_eq!(
        main.instructions
            .iter()
            .filter(|inst| matches!(inst.op, MirOp::Call))
            .count(),
        2
    );
    assert!(
        main.instructions
            .iter()
            .any(|inst| matches!(inst.op, MirOp::Store)
                && matches!(inst.operands.first(), Some(MirOperand::FrameSlot(_))))
    );
}
#[test]
fn scalar_machine_pass_refuses_unimplemented_abi_and_checked_operations_without_mutating_input() {
    for source in [
        "fn main()->Int{return 84/2;}",
        "fn main()->Int{return 84%43;}",
        "extern c fn pending()->Int;fn main()->Int{return pending();}",
    ] {
        let input = typed(source);
        let before = format!("{input:?}");
        for target in [ScalarTarget::Aarch64, ScalarTarget::X86_64] {
            assert!(lower_scalar_typed(&input, target).is_err(), "{source}");
        }
        assert_eq!(before, format!("{input:?}"));
    }
    let input = typed("extern c fn unused(value:Int)->Int;fn main()->Int{return 42;}");
    let machine = lower_scalar_typed(&input, ScalarTarget::Aarch64).unwrap();
    assert!(machine.functions[0].is_external);
    assert!(machine.functions[0].instructions.is_empty());
    let mut malformed = typed("fn main()->Int{return 42;}");
    if let MirOp::Typed(TypedOp::ScalarInt { dst, .. }) =
        &mut malformed.functions[0].instructions[0].op
    {
        *dst = 999;
    }
    assert!(lower_scalar_typed(&malformed, ScalarTarget::Aarch64).is_err());
}
const EIGHT_ARGS: &str = "fn sum(a:Int,b:Int,c:Int,d:Int,e:Int,f:Int,g:Int,h:Int)->Int{return a+b+c+d+e+f+g+h;}fn main()->Int{return sum(1,2,3,4,5,6,7,14);}";
#[test]
fn scalar_machine_pass_preserves_argument_ceiling_and_literal_divergence() {
    let input = typed(EIGHT_ARGS);
    assert!(lower_scalar_typed(&input, ScalarTarget::Aarch64).is_ok());
    assert!(
        lower_scalar_typed(&input, ScalarTarget::X86_64)
            .unwrap_err()
            .contains("stack")
    );
    let input = typed("fn main()->Int{while true{continue;}}");
    for target in [ScalarTarget::Aarch64, ScalarTarget::X86_64] {
        assert!(lower_scalar_typed(&input, target).is_ok());
    } // not executed
}

#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "x86_64")
))]
fn scalar_typed_core_to_machine_executes_real_host_code() {
    #[cfg(target_arch = "aarch64")]
    let target = ScalarTarget::Aarch64;
    #[cfg(target_arch = "x86_64")]
    let target = ScalarTarget::X86_64;
    let sources = SOURCES
        .iter()
        .copied()
        .chain(if target == ScalarTarget::Aarch64 {
            Some(EIGHT_ARGS)
        } else {
            None
        });
    for source in sources {
        let machine = lower_scalar_typed(&typed(source), target).unwrap();
        #[cfg(target_arch = "aarch64")]
        let (code, exports) = crate::compiler::mir_emit::emit_jit(&machine).unwrap();
        #[cfg(target_arch = "x86_64")]
        let (code, exports) = crate::compiler::mir_emit_x86::emit_boot(&machine).unwrap();
        let mut runtime = crate::jit_runtime::JitRuntime::new();
        runtime.load(&code, &exports, &[]).unwrap();
        assert_eq!(unsafe { runtime.invoke("main", &[]) }, Some(42), "{source}");
    }
}

#[test]
fn scalar_machine_shifts_preserve_word_counts_and_emit_both_targets() {
    for op in ["<<", ">>", "lshr"] {
        for value in [1i64, -84, i64::MIN] {
            for count in [0i64, 1, 63, 64, 65, -1] {
                let source = format!(
                    "fn shift(value:Int,count:Int)->Int{{return value>>count;}}fn main()->Int{{return shift({value},{count});}}"
                );
                let mut core = crate::in_lang_parse::parse_in_source(&source).unwrap();
                crate::core_opt::map_decl_exprs_mut(&mut core.decls, &mut |expr| {
                    if let crate::core_ir::Expr::Binary { op: current, .. } = expr
                        && current == ">>"
                    {
                        *current = op.into();
                    }
                });
                let input = crate::compiler::mir_lower::lower_to_mir(&core).unwrap();
                let expected = match op {
                    "<<" => value.wrapping_shl(count as u32),
                    ">>" => value.wrapping_shr(count as u32),
                    _ => (value as u64).wrapping_shr(count as u32) as i64,
                };
                for target in [ScalarTarget::Aarch64, ScalarTarget::X86_64] {
                    let machine = lower_scalar_typed(&input, target).unwrap();
                    assert!(machine.verify_machine().is_ok());
                    let opcode = match op {
                        "<<" => MirOp::Shl,
                        ">>" => MirOp::Shr,
                        _ => MirOp::Lshr,
                    };
                    assert!(
                        machine
                            .functions
                            .iter()
                            .flat_map(|f| &f.instructions)
                            .any(|inst| inst.op == opcode)
                    );
                    let (code, exports) = match target {
                        ScalarTarget::Aarch64 => crate::compiler::mir_emit::emit_jit(&machine),
                        ScalarTarget::X86_64 => crate::compiler::mir_emit_x86::emit_boot(&machine),
                    }
                    .unwrap();
                    if (target == ScalarTarget::Aarch64 && cfg!(target_arch = "aarch64"))
                        || (target == ScalarTarget::X86_64 && cfg!(target_arch = "x86_64"))
                    {
                        #[cfg(any(
                            all(target_os = "macos", target_arch = "aarch64"),
                            all(target_os = "linux", target_arch = "x86_64"),
                            all(target_os = "macos", target_arch = "x86_64")
                        ))]
                        {
                            let mut runtime = crate::jit_runtime::JitRuntime::new();
                            runtime.load(&code, &exports, &[]).unwrap();
                            assert_eq!(
                                unsafe { runtime.invoke("main", &[]) },
                                Some(expected),
                                "{op} {value} {count} {target:?}"
                            );
                        }
                    }
                    // Other hosts still validate both target encodings without executing them.
                    let _ = (code, exports, expected);
                }
            }
        }
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux")))]
fn x86_machine_shifts_preserve_live_count_register_and_all_aliases() {
    for op in [MirOp::Shl, MirOp::Shr, MirOp::Lshr] {
        for dest in 0..3u32 {
            for left in 0..3u32 {
                for right in 0..3u32 {
                    let values = [-84i64, 1, 13];
                    let value = values[left as usize];
                    let count = values[right as usize] as u32;
                    let shifted = match op {
                        MirOp::Shl => value.wrapping_shl(count),
                        MirOp::Shr => value.wrapping_shr(count),
                        _ => (value as u64).wrapping_shr(count) as i64,
                    };
                    let expected = if dest == 2 {
                        shifted
                    } else {
                        shifted.wrapping_add(13)
                    };
                    let mut machine = lower_scalar_typed(
                        &typed("fn main()->Int{return 42;}"),
                        ScalarTarget::X86_64,
                    )
                    .unwrap();
                    let function = &mut machine.functions[0];
                    function.vreg_count = 6;
                    function.frame_size = 0;
                    function.instructions = values
                        .iter()
                        .enumerate()
                        .map(|(reg, value)| {
                            mir(
                                MirOp::Mov,
                                vec![MirOperand::Reg(reg as u32), MirOperand::Imm(*value)],
                            )
                        })
                        .collect();
                    function.instructions.push(mir(
                        op.clone(),
                        vec![
                            MirOperand::Reg(dest),
                            MirOperand::Reg(left),
                            MirOperand::Reg(right),
                        ],
                    ));
                    function.instructions.push(if dest == 2 {
                        mir(MirOp::Mov, vec![MirOperand::Reg(0), MirOperand::Reg(dest)])
                    } else {
                        mir(
                            MirOp::Add,
                            vec![
                                MirOperand::Reg(0),
                                MirOperand::Reg(dest),
                                MirOperand::Reg(2),
                            ],
                        )
                    });
                    function
                        .instructions
                        .push(mir(MirOp::Ret, vec![MirOperand::Reg(0)]));
                    let (code, exports) =
                        crate::compiler::mir_emit_x86::emit_boot(&machine).unwrap();
                    let mut runtime = crate::jit_runtime::JitRuntime::new();
                    runtime.load(&code, &exports, &[]).unwrap();
                    assert_eq!(
                        unsafe { runtime.invoke("main", &[]) },
                        Some(expected),
                        "{op:?} dest={dest} left={left} right={right}"
                    );
                }
            }
        }
    }
}
