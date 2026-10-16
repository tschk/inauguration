use super::*;
use crate::core_ir::Typ;
fn instruction(op: MirOp, operands: Vec<MirOperand>) -> MirInst {
    mir(op, operands)
}
fn function(code: Vec<MirInst>, params: usize) -> MirModule {
    MirModule {
        functions: vec![MirFunction {
            name: "main".into(),
            instructions: code,
            vreg_count: 6,
            frame_size: 64,
            return_type: Some(Typ::Int),
            param_types: vec![Typ::Int; params],
            var_map: vec![],
            value_types: vec![],
            local_types: vec![],
            is_external: false,
        }],
        rodata: vec![],
        rodata_relocs: vec![],
    }
}
fn set(reg: u32, value: i64) -> MirInst {
    instruction(
        MirOp::Mov,
        vec![MirOperand::Reg(reg), MirOperand::Imm(value)],
    )
}
fn ret() -> MirInst {
    instruction(MirOp::Ret, vec![])
}
fn target(op: MirOp, name: &str) -> MirInst {
    instruction(op, vec![MirOperand::Label(name.into())])
}
fn label(name: &str) -> MirInst {
    instruction(MirOp::Label(name.into()), vec![])
}
fn compare(a: u32, b: u32) -> MirInst {
    instruction(MirOp::Cmp, vec![MirOperand::Reg(a), MirOperand::Reg(b)])
}
#[test]
fn cfg_rejects_fallthrough_bad_targets_and_uninitialized_returns() {
    for code in [
        vec![set(0, 42)],
        vec![ret()],
        vec![target(MirOp::Jmp, "absent")],
        vec![label("x"), label("x"), set(0, 42), ret()],
        vec![
            set(0, 1),
            target(MirOp::Je, "end"),
            set(0, 42),
            ret(),
            label("end"),
        ],
        vec![
            set(0, 1),
            set(1, 2),
            compare(0, 1),
            target(MirOp::Je, "end"),
            ret(),
            label("end"),
        ],
    ] {
        assert!(function(code, 0).verify(None).is_err());
    }
    let divergence = function(vec![label("again"), target(MirOp::Jmp, "again")], 0);
    assert!(divergence.verify(None).is_ok());
    let mut void = function(vec![], 0);
    void.functions[0].return_type = Some(Typ::Void);
    assert!(void.verify(None).is_ok());
}
#[test]
fn cfg_requires_register_and_comparison_definitions_on_all_incoming_paths() {
    let code = vec![
        set(0, 1),
        set(1, 2),
        compare(0, 1),
        target(MirOp::Je, "skip"),
        set(2, 42),
        label("skip"),
        instruction(MirOp::Ret, vec![MirOperand::Reg(2)]),
    ];
    assert!(function(code, 0).verify(None).is_err());
    let code = vec![
        set(0, 1),
        set(1, 2),
        compare(0, 1),
        instruction(
            MirOp::Add,
            vec![MirOperand::Reg(0), MirOperand::Reg(0), MirOperand::Reg(1)],
        ),
        target(MirOp::Je, "end"),
        ret(),
        label("end"),
        ret(),
    ];
    assert!(function(code, 0).verify(None).is_err());
}
#[test]
fn emit_boundaries_refuse_staged_and_malformed_instructions() {
    for code in [
        vec![instruction(MirOp::Typed(TypedOp::Nop), vec![])],
        vec![instruction(MirOp::Mov, vec![]), set(0, 42), ret()],
        vec![
            set(0, 42),
            instruction(MirOp::Ret, vec![MirOperand::Imm(42)]),
        ],
    ] {
        let module = function(code, 0);
        assert!(crate::compiler::mir_emit::emit_jit(&module).is_err());
        assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_err());
    }
}
fn finite_programs() -> Vec<(MirModule, Vec<i64>, i64)> {
    let early = function(vec![set(0, 42), ret(), set(0, 7), ret()], 0);
    let forward = function(
        vec![
            set(0, 7),
            target(MirOp::Jmp, "selected"),
            set(0, 0),
            label("selected"),
            set(0, 42),
            ret(),
        ],
        0,
    );
    let backward = function(
        vec![
            set(0, 0),
            set(1, 6),
            label("loop"),
            instruction(
                MirOp::Add,
                vec![MirOperand::Reg(0), MirOperand::Reg(0), MirOperand::Imm(7)],
            ),
            instruction(
                MirOp::Sub,
                vec![MirOperand::Reg(1), MirOperand::Reg(1), MirOperand::Imm(1)],
            ),
            set(2, 0),
            compare(1, 2),
            target(MirOp::Jg, "loop"),
            ret(),
        ],
        0,
    );
    let signed = function(
        vec![
            set(0, 7),
            set(1, -1),
            set(2, 0),
            compare(1, 2),
            target(MirOp::Jl, "negative"),
            ret(),
            label("negative"),
            set(0, 42),
            ret(),
        ],
        0,
    );
    let swap = function(
        vec![
            instruction(
                MirOp::Sub,
                vec![MirOperand::Reg(0), MirOperand::Reg(1), MirOperand::Reg(0)],
            ),
            ret(),
        ],
        2,
    );
    let mut cases = vec![
        (early, vec![], 42),
        (forward, vec![], 42),
        (backward, vec![], 42),
        (signed, vec![], 42),
        (swap, vec![7, 49], 42),
    ];
    let args = vec![101, 202, 303, 404, 505, 606];
    for index in 0..6 {
        cases.push((
            function(
                vec![instruction(MirOp::Ret, vec![MirOperand::Reg(index)])],
                6,
            ),
            args.clone(),
            args[index as usize],
        ));
    }
    cases
}
#[test]
fn finite_cfg_programs_emit_on_both_architectures() {
    for (module, _, _) in finite_programs() {
        assert!(crate::compiler::mir_emit::emit_jit(&module).is_ok());
        assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_ok());
    }
}
#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(unix, target_arch = "x86_64")
))]
fn native_early_returns_branches_frames_and_parameter_shuffles_execute() {
    for (module, args, expected) in finite_programs() {
        #[cfg(target_arch = "aarch64")]
        let result = crate::compiler::mir_emit::emit_jit(&module);
        #[cfg(target_arch = "x86_64")]
        let result = crate::compiler::mir_emit_x86::emit_boot(&module);
        let (code, exports) = result.unwrap();
        let mut runtime = crate::jit_runtime::JitRuntime::new();
        runtime.load(&code, &exports, &[]).unwrap();
        assert_eq!(
            unsafe { runtime.invoke("main", &args) },
            Some(expected),
            "{module:?}"
        );
    }
}

#[test]
fn external_signatures_and_scalar_abi_limits_are_explicit() {
    let mut external = function(vec![], 0);
    external.functions[0].is_external = true;
    external.functions[0].vreg_count = 0;
    assert!(external.verify(None).is_ok());
    assert!(
        crate::compiler::mir_emit::emit_jit(&external)
            .unwrap()
            .0
            .is_empty()
    );
    external.functions[0].instructions.push(ret());
    assert!(external.verify(None).is_err());
    let mut bad = function(vec![ret()], 1);
    bad.functions[0].param_types[0] = Typ::Float;
    assert!(crate::compiler::mir_emit::emit_jit(&bad).is_err());
    assert!(crate::compiler::mir_emit_x86::emit_boot(&bad).is_err());
    let mut many = function(vec![ret()], 9);
    many.functions[0].vreg_count = 9;
    assert!(crate::compiler::mir_emit::emit_jit(&many).is_err());
    assert!(crate::compiler::mir_emit_x86::emit_boot(&many).is_err());
}
#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_arm_seventh_and_eighth_scalar_parameters_execute() {
    for index in [6, 7] {
        let mut module = function(
            vec![instruction(MirOp::Ret, vec![MirOperand::Reg(index)])],
            8,
        );
        module.functions[0].vreg_count = 8;
        let (code, exports) = crate::compiler::mir_emit::emit_jit(&module).unwrap();
        let mut runtime = crate::jit_runtime::JitRuntime::new();
        runtime.load(&code, &exports, &[]).unwrap();
        let args = [1, 2, 3, 4, 5, 6, 707, 808];
        assert_eq!(
            unsafe { runtime.invoke("main", &args) },
            Some(args[index as usize])
        );
        assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_err());
    }
}

#[test]
fn rodata_relative_fields_are_checked_and_cannot_overwrite_verified_code() {
    let mut module = function(vec![set(0, 42), ret()], 0);
    module.rodata = vec![0; 4];
    let (original, exports) = crate::compiler::mir_emit_x86::emit_boot(&module).unwrap();
    let field = (original.len() - 4) as u32;
    module.rodata_relocs.push(MirRelocation {
        symbol: "main".into(),
        offset: field,
        kind: RelocKind::Rel32,
    });
    let (relocated, _) = crate::compiler::mir_emit_x86::emit_boot(&module).unwrap();
    assert_eq!(&relocated[..field as usize], &original[..field as usize]);
    let displacement = i32::from_le_bytes(relocated[field as usize..].try_into().unwrap());
    assert_eq!(
        i64::from(displacement),
        i64::from(exports[0].1) - i64::from(field) - 4
    );
    assert!(crate::compiler::mir_emit::emit_jit(&module).is_err());
    module.rodata_relocs[0].offset = 0;
    assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_err());
    module.rodata_relocs[0].offset = field + 1;
    assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_err());
    module.rodata_relocs[0].offset = field;
    for kind in [RelocKind::Abs32, RelocKind::Abs64] {
        module.rodata_relocs[0].kind = kind;
        assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_err());
    }
}

// Runtime pointer inputs must be created by the executing host, never serialized.
fn x86_memory_programs() -> Vec<MirModule> {
    vec![
        function(
            vec![
                set(1, 22),
                instruction(
                    MirOp::Store,
                    vec![MirOperand::Mem { base: 0, offset: 8 }, MirOperand::Reg(1)],
                ),
                instruction(MirOp::Load, vec![MirOperand::Reg(2), MirOperand::Reg(0)]),
                instruction(
                    MirOp::Mov,
                    vec![MirOperand::Reg(3), MirOperand::Mem { base: 0, offset: 8 }],
                ),
                instruction(
                    MirOp::Add,
                    vec![MirOperand::Reg(0), MirOperand::Reg(2), MirOperand::Reg(3)],
                ),
                ret(),
            ],
            1,
        ),
        function(
            vec![
                instruction(
                    MirOp::Lea,
                    vec![MirOperand::Reg(1), MirOperand::Mem { base: 0, offset: 8 }],
                ),
                set(2, 22),
                instruction(
                    MirOp::Store,
                    vec![MirOperand::Mem { base: 1, offset: 0 }, MirOperand::Reg(2)],
                ),
                instruction(MirOp::Load, vec![MirOperand::Reg(3), MirOperand::Reg(1)]),
                instruction(
                    MirOp::Lea,
                    vec![
                        MirOperand::Reg(4),
                        MirOperand::Mem {
                            base: 1,
                            offset: -8,
                        },
                    ],
                ),
                instruction(
                    MirOp::Mov,
                    vec![MirOperand::Reg(4), MirOperand::Mem { base: 4, offset: 0 }],
                ),
                instruction(
                    MirOp::Add,
                    vec![MirOperand::Reg(0), MirOperand::Reg(3), MirOperand::Reg(4)],
                ),
                ret(),
            ],
            1,
        ),
    ]
}
#[test]
fn x86_memory_programs_emit_real_addressing_operations() {
    for module in x86_memory_programs() {
        assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_ok());
    }
}
#[test]
#[cfg(all(unix, target_arch = "x86_64"))]
fn native_x86_load_store_and_lea_preserve_values_and_pointer_arguments() {
    for module in x86_memory_programs() {
        let mut cells = [20i64, 0];
        let args = [cells.as_mut_ptr() as i64];
        let (code, exports) = crate::compiler::mir_emit_x86::emit_boot(&module).unwrap();
        let mut runtime = crate::jit_runtime::JitRuntime::new();
        runtime.load(&code, &exports, &[]).unwrap();
        assert_eq!(unsafe { runtime.invoke("main", &args) }, Some(42));
        assert_eq!(cells, [20, 22]);
    }
}

fn machine_function(name: &str, params: usize, frame: u32, code: Vec<MirInst>) -> MirFunction {
    let mut result = function(code, params).functions.remove(0);
    result.name = name.into();
    result.frame_size = frame;
    result
}
fn internal_call(name: &str, args: &[u32]) -> MirInst {
    instruction(
        MirOp::Call,
        std::iter::once(MirOperand::Global(name.into()))
            .chain(args.iter().map(|r| MirOperand::Reg(*r)))
            .collect(),
    )
}
fn slot_store(slot: u32, source: u32) -> MirInst {
    instruction(
        MirOp::Store,
        vec![MirOperand::FrameSlot(slot), MirOperand::Reg(source)],
    )
}
fn slot_load(destination: u32, slot: u32) -> MirInst {
    instruction(
        MirOp::Load,
        vec![MirOperand::Reg(destination), MirOperand::FrameSlot(slot)],
    )
}
fn module_functions(functions: Vec<MirFunction>) -> MirModule {
    MirModule {
        functions,
        rodata: vec![],
        rodata_relocs: vec![],
    }
}
pub(crate) fn pipeline_programs() -> Vec<(MirModule, Vec<i64>, i64)> {
    let increment = machine_function(
        "increment",
        1,
        0,
        vec![
            instruction(
                MirOp::Add,
                vec![MirOperand::Reg(0), MirOperand::Reg(0), MirOperand::Imm(1)],
            ),
            ret(),
        ],
    );
    let large = machine_function(
        "main",
        0,
        8192,
        vec![
            set(0, 20),
            slot_store(0, 0),
            internal_call("increment", &[0]),
            slot_store(1023, 0),
            internal_call("increment", &[0]),
            slot_load(1, 0),
            slot_load(2, 1023),
            set(3, 21),
            compare(2, 3),
            target(MirOp::Jne, "bad"),
            instruction(
                MirOp::Add,
                vec![MirOperand::Reg(0), MirOperand::Reg(0), MirOperand::Reg(1)],
            ),
            ret(),
            label("bad"),
            set(0, 7),
            ret(),
        ],
    );
    let shuffle = machine_function(
        "main",
        6,
        16,
        vec![internal_call("sum_ends", &[5, 4, 3, 2, 1, 0]), ret()],
    );
    let ends = machine_function(
        "sum_ends",
        6,
        0,
        vec![
            instruction(
                MirOp::Add,
                vec![MirOperand::Reg(0), MirOperand::Reg(0), MirOperand::Reg(5)],
            ),
            ret(),
        ],
    );
    let mut sideeffect = machine_function("touch", 0, 0, vec![set(0, 999), ret()]);
    sideeffect.return_type = Some(Typ::Void);
    let void_caller = machine_function(
        "main",
        0,
        8,
        vec![
            set(0, 42),
            slot_store(0, 0),
            internal_call("touch", &[]),
            slot_load(0, 0),
            ret(),
        ],
    );
    let factorial = machine_function(
        "factorial",
        1,
        8,
        vec![
            set(1, 1),
            compare(0, 1),
            target(MirOp::Jle, "base"),
            slot_store(0, 0),
            instruction(
                MirOp::Sub,
                vec![MirOperand::Reg(0), MirOperand::Reg(0), MirOperand::Imm(1)],
            ),
            internal_call("factorial", &[0]),
            slot_load(1, 0),
            instruction(
                MirOp::Mul,
                vec![MirOperand::Reg(0), MirOperand::Reg(0), MirOperand::Reg(1)],
            ),
            ret(),
            label("base"),
            set(0, 1),
            ret(),
        ],
    );
    let recursive = machine_function(
        "main",
        0,
        0,
        vec![set(0, 6), internal_call("factorial", &[0]), ret()],
    );
    let mut cases = vec![
        (module_functions(vec![large, increment]), vec![], 42),
        (
            module_functions(vec![shuffle, ends]),
            vec![101, 202, 303, 404, 505, 606],
            707,
        ),
        (module_functions(vec![void_caller, sideeffect]), vec![], 42),
        (module_functions(vec![recursive, factorial]), vec![], 720),
    ];
    for (op, args, expected) in [
        ("==", vec![20, 20], 1),
        ("==", vec![20, 22], 0),
        ("!=", vec![20, 20], 0),
        ("!=", vec![20, 22], 1),
        ("<", vec![-1, 0], 1),
        ("<", vec![0, -1], 0),
        ("<=", vec![20, 20], 1),
        ("<=", vec![22, 20], 0),
        (">", vec![-1, 0], 0),
        (">", vec![0, -1], 1),
        (">=", vec![20, 20], 1),
        (">=", vec![20, 22], 0),
    ] {
        for destination in [1, 3, 4, 5] {
            let code = vec![
                compare(0, 1),
                instruction(
                    MirOp::SetCond(op.into()),
                    vec![MirOperand::Reg(destination)],
                ),
                instruction(MirOp::Ret, vec![MirOperand::Reg(destination)]),
            ];
            cases.push((function(code, 2), args.clone(), expected));
        }
    }
    cases.push((
        function(
            vec![
                set(1, -43),
                instruction(MirOp::Not, vec![MirOperand::Reg(0), MirOperand::Reg(1)]),
                ret(),
            ],
            0,
        ),
        vec![],
        42,
    ));
    cases.push((
        function(
            vec![
                set(1, -42),
                instruction(MirOp::Neg, vec![MirOperand::Reg(0), MirOperand::Reg(1)]),
                ret(),
            ],
            0,
        ),
        vec![],
        42,
    ));
    cases.push((
        function(
            vec![
                set(0, 7),
                set(1, 2),
                set(2, 1),
                compare(1, 2),
                set(5, 0),
                target(MirOp::Jg, "selected"),
                ret(),
                label("selected"),
                set(0, 42),
                ret(),
            ],
            0,
        ),
        vec![],
        42,
    ));
    cases
}
#[test]
fn pipeline_machine_programs_emit_on_both_targets() {
    for (module, _, _) in pipeline_programs() {
        assert!(
            crate::compiler::mir_emit::emit_jit(&module).is_ok(),
            "{module:?}"
        );
        assert!(
            crate::compiler::mir_emit_x86::emit_boot(&module).is_ok(),
            "{module:?}"
        );
    }
}
#[test]
#[cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(unix, target_arch = "x86_64")
))]
fn native_internal_calls_spills_recursion_comparisons_and_large_frames_execute() {
    for (module, args, expected) in pipeline_programs() {
        #[cfg(target_arch = "aarch64")]
        let emitted = crate::compiler::mir_emit::emit_jit(&module);
        #[cfg(target_arch = "x86_64")]
        let emitted = crate::compiler::mir_emit_x86::emit_boot(&module);
        let (code, exports) = emitted.unwrap();
        let mut runtime = crate::jit_runtime::JitRuntime::new();
        runtime.load(&code, &exports, &[]).unwrap();
        assert_eq!(
            unsafe { runtime.invoke("main", &args) },
            Some(expected),
            "{module:?}"
        );
    }
}
#[test]
fn slots_and_call_clobbers_require_real_spills_and_reloads() {
    let helper = machine_function("helper", 0, 0, vec![set(0, 42), ret()]);
    for code in [
        vec![slot_load(0, 0), ret()],
        vec![
            set(0, 1),
            set(1, 2),
            compare(0, 1),
            target(MirOp::Je, "skip"),
            slot_store(0, 0),
            label("skip"),
            slot_load(0, 0),
            ret(),
        ],
        vec![
            set(1, 42),
            internal_call("helper", &[]),
            instruction(MirOp::Ret, vec![MirOperand::Reg(1)]),
        ],
        vec![
            set(0, 1),
            set(1, 2),
            compare(0, 1),
            internal_call("helper", &[]),
            instruction(MirOp::SetCond("==".into()), vec![MirOperand::Reg(0)]),
            ret(),
        ],
        vec![internal_call("missing", &[]), ret()],
        vec![internal_call("helper", &[0]), ret()],
        vec![set(0, 42), slot_store(1, 0), ret()],
    ] {
        let module = module_functions(vec![machine_function("main", 0, 8, code), helper.clone()]);
        assert!(module.verify(None).is_err(), "{module:?}");
    }
    let trap = function(vec![instruction(MirOp::Trap, vec![])], 0);
    assert!(trap.verify(None).is_ok());
    assert!(crate::compiler::mir_emit::emit_jit(&trap).is_ok());
    assert!(crate::compiler::mir_emit_x86::emit_boot(&trap).is_ok());
    let mut external = helper;
    external.is_external = true;
    external.instructions.clear();
    external.vreg_count = 0;
    assert!(
        module_functions(vec![
            machine_function("main", 0, 0, vec![internal_call("helper", &[]), ret()]),
            external
        ])
        .verify(None)
        .is_err()
    );
}

#[test]
fn raw_emitters_share_scalar64_signature_policy() {
    for name in [
        "i8",
        "i16",
        "i32",
        "u8",
        "u16",
        "u32",
        "u64",
        "int",
        "isize",
        "usize",
        "#integer-u8",
        "#rust-integer-i64",
        "#rust-integer-u64",
    ] {
        for parameter in [true, false] {
            let mut module = function(
                vec![
                    set(0, 42),
                    instruction(MirOp::Ret, vec![MirOperand::Reg(0)]),
                ],
                usize::from(parameter),
            );
            let typ = Typ::Named(name.into());
            if parameter {
                module.functions[0].param_types[0] = typ;
            } else {
                module.functions[0].return_type = Some(typ);
            }
            assert!(
                crate::compiler::mir_emit::emit_jit(&module).is_err(),
                "A64 {name} parameter={parameter}"
            );
            assert!(
                crate::compiler::mir_emit_x86::emit_boot(&module).is_err(),
                "x86 {name} parameter={parameter}"
            );
        }
    }
    for typ in [
        Typ::Int,
        Typ::Bool,
        Typ::Named("Int".into()),
        Typ::Named("i64".into()),
        Typ::Named("Bool".into()),
        Typ::Named("bool".into()),
    ] {
        let mut module = function(vec![instruction(MirOp::Ret, vec![MirOperand::Reg(0)])], 1);
        module.functions[0].param_types[0] = typ.clone();
        module.functions[0].return_type = Some(typ);
        assert!(crate::compiler::mir_emit::emit_jit(&module).is_ok());
        assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_ok());
    }
    for typ in [
        None,
        Some(Typ::Void),
        Some(Typ::Named("Void".into())),
        Some(Typ::Named("void".into())),
        Some(Typ::Named("Unit".into())),
        Some(Typ::Named("unit".into())),
        Some(Typ::Named("()".into())),
    ] {
        let mut module = function(vec![ret()], 0);
        module.functions[0].return_type = typ;
        assert!(crate::compiler::mir_emit::emit_jit(&module).is_ok());
        assert!(crate::compiler::mir_emit_x86::emit_boot(&module).is_ok());
    }
}
