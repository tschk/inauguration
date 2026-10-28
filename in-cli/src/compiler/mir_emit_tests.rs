use crate::compiler::mir::{MirFunction, MirInst, MirModule, MirOp};
use crate::compiler::mir_emit::emit_jit;
use crate::core_ir::Typ;

#[test]
fn test_emit_jit() {
    let func = MirFunction {
        name: "test".to_string(),
        instructions: vec![MirInst {
            op: MirOp::Ret,
            operands: vec![],
            offset: 0,
        }],
        vreg_count: 0,
        frame_size: 0,
        return_type: Some(Typ::Void),
        param_types: vec![],
        is_external: false,
        var_map: vec![],
        value_types: vec![],
        local_types: vec![],
    };

    let module = MirModule {
        functions: vec![func],
        rodata: vec![],
        rodata_relocs: vec![],
    };

    let result = emit_jit(&module);
    assert!(result.is_ok());
    let (code, functions) = result.unwrap();
    assert!(!code.is_empty());
    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].0, "test");
}
