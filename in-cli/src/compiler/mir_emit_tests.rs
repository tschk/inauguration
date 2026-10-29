#[cfg(test)]
mod tests {
    use crate::compiler::mir::{MirFunction, MirInst, MirModule, MirOp, MirOperand};
    use crate::compiler::mir_emit::emit_jit;
    use crate::core_ir::Typ;

    fn mir(op: MirOp, operands: Vec<MirOperand>) -> MirInst {
        MirInst {
            op,
            operands,
            offset: 0,
        }
    }

    #[test]
    fn test_emit_jit_empty_module() {
        let module = MirModule {
            functions: vec![],
            rodata: vec![],
            rodata_relocs: vec![],
        };
        let result = emit_jit(&module);
        assert!(result.is_ok());
        let (code, functions) = result.unwrap();
        assert_eq!(code.len(), 0);
        assert_eq!(functions.len(), 0);
    }

    #[test]
    fn test_emit_jit_simple_function() {
        let module = MirModule {
            functions: vec![MirFunction {
                name: "test".to_string(),
                instructions: vec![
                    mir(MirOp::Mov, vec![MirOperand::Reg(0), MirOperand::Imm(42)]),
                    mir(MirOp::Ret, vec![MirOperand::Reg(0)]),
                ],
                vreg_count: 1,
                frame_size: 0,
                return_type: Some(Typ::Int),
                param_types: vec![],
                var_map: vec![],
                value_types: vec![],
                local_types: vec![],
                is_external: false,
            }],
            rodata: vec![],
            rodata_relocs: vec![],
        };
        let result = emit_jit(&module);
        match result {
            Ok((code, functions)) => {
                assert!(!code.is_empty());
                assert_eq!(functions.len(), 1);
                assert_eq!(functions[0].0, "test");
            }
            Err(e) => {
                panic!("emit_jit failed: {}", e);
            }
        }
    }
}
