use crate::compiler::mir::{MirFunction, MirInst, MirModule, MirOp, MirOperand};
use crate::core_ir::Typ;

#[test]
fn test_emit_jit_empty_module() {
    let module = MirModule {
        functions: vec![],
        rodata: vec![],
        rodata_relocs: vec![],
    };
    let result = super::emit_jit(&module).unwrap();
    assert_eq!(result.0.len(), 0);
    assert_eq!(result.1.len(), 0);
}

#[test]
fn test_emit_jit_simple_function() {
    let module = MirModule {
        functions: vec![MirFunction {
            name: "test".to_string(),
            instructions: vec![
                MirInst {
                    op: MirOp::Mov,
                    operands: vec![MirOperand::Reg(0), MirOperand::Imm(42)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Ret,
                    operands: vec![MirOperand::Reg(0)],
                    offset: 0,
                },
            ],
            vreg_count: 1,
            frame_size: 0,
            return_type: Some(Typ::Int),
            param_types: vec![],
            var_map: vec![],
            // Empty value_types so it won't be considered a typed function
            value_types: vec![],
            local_types: vec![],
            is_external: false,
        }],
        rodata: vec![],
        rodata_relocs: vec![],
    };
    let result = super::emit_jit(&module).unwrap();
    assert!(!result.0.is_empty());
    assert_eq!(result.1.len(), 1);
    assert_eq!(result.1[0].0, "test");
}

#[test]
fn test_emit_jit_function_with_args_and_call() {
    let module = MirModule {
        functions: vec![
            MirFunction {
                name: "callee".to_string(),
                instructions: vec![MirInst {
                    op: MirOp::Ret,
                    operands: vec![MirOperand::Reg(0)],
                    offset: 0,
                }],
                vreg_count: 1,
                frame_size: 0,
                return_type: Some(Typ::Int),
                param_types: vec![Typ::Int],
                var_map: vec![],
                value_types: vec![],
                local_types: vec![],
                is_external: false,
            },
            MirFunction {
                name: "caller".to_string(),
                instructions: vec![
                    MirInst {
                        op: MirOp::Mov,
                        operands: vec![MirOperand::Reg(0), MirOperand::Imm(10)],
                        offset: 0,
                    },
                    MirInst {
                        op: MirOp::Call,
                        operands: vec![
                            MirOperand::Global("callee".to_string()),
                            MirOperand::Reg(0),
                        ],
                        offset: 0,
                    },
                    MirInst {
                        op: MirOp::Ret,
                        operands: vec![MirOperand::Reg(0)],
                        offset: 0,
                    },
                ],
                vreg_count: 1,
                frame_size: 0,
                return_type: Some(Typ::Int),
                param_types: vec![],
                var_map: vec![],
                value_types: vec![],
                local_types: vec![],
                is_external: false,
            },
        ],
        rodata: vec![],
        rodata_relocs: vec![],
    };
    let result = super::emit_jit(&module).unwrap();
    assert!(!result.0.is_empty());
    assert_eq!(result.1.len(), 2);
    assert_eq!(result.1[0].0, "callee");
    assert_eq!(result.1[1].0, "caller");
}

#[test]
fn test_emit_jit_function_with_branches() {
    let module = MirModule {
        functions: vec![MirFunction {
            name: "branching".to_string(),
            instructions: vec![
                MirInst {
                    op: MirOp::Cmp,
                    operands: vec![MirOperand::Reg(0), MirOperand::Reg(1)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Je,
                    operands: vec![MirOperand::Label("equal".to_string())],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Mov,
                    operands: vec![MirOperand::Reg(0), MirOperand::Imm(0)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Ret,
                    operands: vec![MirOperand::Reg(0)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Label("equal".to_string()),
                    operands: vec![],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Mov,
                    operands: vec![MirOperand::Reg(0), MirOperand::Imm(1)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Ret,
                    operands: vec![MirOperand::Reg(0)],
                    offset: 0,
                },
            ],
            vreg_count: 2,
            frame_size: 0,
            return_type: Some(Typ::Int),
            param_types: vec![Typ::Int, Typ::Int],
            var_map: vec![],
            value_types: vec![],
            local_types: vec![],
            is_external: false,
        }],
        rodata: vec![],
        rodata_relocs: vec![],
    };
    let result = super::emit_jit(&module).unwrap();
    assert!(!result.0.is_empty());
    assert_eq!(result.1.len(), 1);
    assert_eq!(result.1[0].0, "branching");
}

#[test]
fn test_emit_jit_function_with_memory_ops() {
    let module = MirModule {
        functions: vec![MirFunction {
            name: "mem_ops".to_string(),
            instructions: vec![
                MirInst {
                    op: MirOp::Store,
                    operands: vec![MirOperand::FrameSlot(0), MirOperand::Reg(0)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Load,
                    operands: vec![MirOperand::Reg(1), MirOperand::FrameSlot(0)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Ret,
                    operands: vec![MirOperand::Reg(1)],
                    offset: 0,
                },
            ],
            vreg_count: 2,
            frame_size: 8,
            return_type: Some(Typ::Int),
            param_types: vec![Typ::Int],
            var_map: vec![],
            value_types: vec![],
            local_types: vec![],
            is_external: false,
        }],
        rodata: vec![],
        rodata_relocs: vec![],
    };
    let result = super::emit_jit(&module).unwrap();
    assert!(!result.0.is_empty());
    assert_eq!(result.1.len(), 1);
    assert_eq!(result.1[0].0, "mem_ops");
}

#[test]
fn test_emit_jit_function_with_arithmetic() {
    let module = MirModule {
        functions: vec![MirFunction {
            name: "arithmetic".to_string(),
            instructions: vec![
                MirInst {
                    op: MirOp::Add,
                    operands: vec![MirOperand::Reg(2), MirOperand::Reg(0), MirOperand::Reg(1)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Sub,
                    operands: vec![MirOperand::Reg(3), MirOperand::Reg(2), MirOperand::Imm(5)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Mul,
                    operands: vec![MirOperand::Reg(4), MirOperand::Reg(3), MirOperand::Reg(0)],
                    offset: 0,
                },
                MirInst {
                    op: MirOp::Ret,
                    operands: vec![MirOperand::Reg(4)],
                    offset: 0,
                },
            ],
            vreg_count: 5,
            frame_size: 0,
            return_type: Some(Typ::Int),
            param_types: vec![Typ::Int, Typ::Int],
            var_map: vec![],
            value_types: vec![],
            local_types: vec![],
            is_external: false,
        }],
        rodata: vec![],
        rodata_relocs: vec![],
    };
    let result = super::emit_jit(&module).unwrap();
    assert!(!result.0.is_empty());
    assert_eq!(result.1.len(), 1);
    assert_eq!(result.1[0].0, "arithmetic");
}

#[test]
fn test_emit_jit_function_with_unsupported_rodata() {
    let module = MirModule {
        functions: vec![],
        rodata: vec![1, 2, 3],
        rodata_relocs: vec![],
    };
    let result = super::emit_jit(&module);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "machine MIR AArch64 rodata/relocations are not supported"
    );
}

#[test]
fn test_emit_jit_function_with_unsupported_params() {
    let module = MirModule {
        functions: vec![MirFunction {
            name: "many_params".to_string(),
            instructions: vec![MirInst {
                op: MirOp::Ret,
                operands: vec![MirOperand::Reg(0)],
                offset: 0,
            }],
            vreg_count: 9,
            frame_size: 0,
            return_type: Some(Typ::Int),
            param_types: vec![
                Typ::Int,
                Typ::Int,
                Typ::Int,
                Typ::Int,
                Typ::Int,
                Typ::Int,
                Typ::Int,
                Typ::Int,
                Typ::Int,
            ],
            var_map: vec![],
            value_types: vec![],
            local_types: vec![],
            is_external: false,
        }],
        rodata: vec![],
        rodata_relocs: vec![],
    };
    let result = super::emit_jit(&module);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "MIR AArch64 requires scalar parameters/return and at most8 register arguments"
    );
}
