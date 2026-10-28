use super::*;
use crate::core_ir::Typ;

fn create_simple_module() -> MirModule {
    let func = MirFunction {
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
        value_types: vec![],
        local_types: vec![],
        is_external: false,
    };

    MirModule {
        functions: vec![func],
        rodata: vec![],
        rodata_relocs: vec![],
    }
}

#[test]
fn test_emit_jit_simple() {
    let module = create_simple_module();
    let result = emit_jit(&module);
    assert!(result.is_ok(), "JIT emission failed for simple module");
    let (code, functions) = result.unwrap();
    assert!(!code.is_empty(), "Emitted JIT code should not be empty");
    assert_eq!(functions.len(), 1, "Should emit one function");
    assert_eq!(functions[0].0, "test_func", "Function name should match");
}
