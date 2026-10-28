use super::*;
use crate::compiler::mir::{
    MirFunction, MirInst, MirModule, MirOp, MirOperand, MirRelocation, RelocKind,
};
use crate::core_ir::Typ;

#[test]
fn test_emit_jit_basic() {
    let module = MirModule {
        functions: vec![MirFunction {
            name: "test_func".to_string(),
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
            value_types: vec![], // Bypass typed verification
            local_types: vec![],
            is_external: false,
        }],
        rodata: vec![],
        rodata_relocs: vec![],
    };

    let result = emit_jit(&module);
    assert!(result.is_ok());
    let (code, functions) = result.unwrap();
    assert!(!code.is_empty());
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].0, "test_func");
}

#[test]
fn test_emit_jit_rodata_error() {
    let module = MirModule {
        functions: vec![],
        rodata: vec![0, 1, 2, 3],
        rodata_relocs: vec![],
    };

    let result = emit_jit(&module);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "machine MIR AArch64 rodata/relocations are not supported"
    );
}

#[test]
fn test_emit_jit_reloc_error() {
    let module = MirModule {
        functions: vec![],
        rodata: vec![],
        rodata_relocs: vec![MirRelocation {
            symbol: "test".to_string(),
            offset: 0,
            kind: RelocKind::Abs64,
        }],
    };

    let result = emit_jit(&module);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "machine MIR AArch64 rodata/relocations are not supported"
    );
}
