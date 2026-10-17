//! Conservative scalar typed-MIR → machine-MIR stack lowering.
//!
//! Each SSA value and local owns an eight-byte frame slot. This deliberately
//! favors correctness across calls over register allocation; optimize only
//! after profiling demonstrates that the extra loads/stores matter.
use super::mir::{MirFunction, MirInst, MirModule, MirOp, MirOperand, TypedOp, VReg, mir};
use std::collections::HashMap;

/// The proven scalar, 64-bit register-call ABIs for this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarTarget {
    Aarch64,
    X86_64,
}
impl ScalarTarget {
    fn argument_limit(self) -> usize {
        match self {
            Self::Aarch64 => 8,
            Self::X86_64 => 6,
        }
    }
}

/// Lower only validated typed scalar definitions. No partial module escapes on
/// refusal. The hosted JIT retains native_emit for modules this pass refuses.
pub fn lower_scalar_typed(module: &MirModule, target: ScalarTarget) -> Result<MirModule, String> {
    module.verify(None).map_err(|errors| errors.join("; "))?;
    if !module.rodata.is_empty() || !module.rodata_relocs.is_empty() {
        return Err("scalar-mir: data pools/relocations are not represented by this pass".into());
    }
    let mut result = MirModule::new();
    for function in &module.functions {
        if function.is_external {
            // Keep the declaration's typed signature metadata. No instructions
            // are emitted, and actual external calls are rejected below.
            result.functions.push(function.clone());
            continue;
        }
        let prefix = || format!("scalar-mir: {}", function.name);
        if function.param_types.len() > target.argument_limit() {
            return Err(format!(
                "{}: stack parameters are not implemented",
                prefix()
            ));
        }
        if function
            .instructions
            .iter()
            .any(|inst| !matches!(inst.op, MirOp::Typed(_) | MirOp::Label(_)))
        {
            return Err(format!("{}: expected a typed definition", prefix()));
        }
        let value_count = u32::try_from(function.value_types.len())
            .map_err(|_| format!("{}: value slots overflow", prefix()))?;
        let local_count = u32::try_from(function.local_types.len())
            .map_err(|_| format!("{}: local slots overflow", prefix()))?;
        let slots = value_count
            .checked_add(local_count)
            .ok_or_else(|| format!("{}: frame slots overflow", prefix()))?;
        let bytes = slots
            .checked_mul(8)
            .and_then(|bytes| bytes.checked_add(15))
            .map(|bytes| bytes & !15)
            .ok_or_else(|| format!("{}: frame size overflow", prefix()))?;
        let mut output = MirFunction {
            name: function.name.clone(),
            instructions: vec![],
            vreg_count: target.argument_limit() as u32,
            frame_size: bytes,
            return_type: function.return_type.clone(),
            param_types: function.param_types.clone(),
            var_map: vec![],
            value_types: vec![],
            local_types: vec![],
            is_external: false,
        };
        // Spill every incoming parameter before reusing any ABI register.
        for reg in 0..function.param_types.len() as u32 {
            store(&mut output.instructions, reg, reg);
        }
        let booleans: HashMap<VReg, bool> = function
            .instructions
            .iter()
            .filter_map(|inst| match &inst.op {
                MirOp::Typed(TypedOp::ScalarBool { dst, value }) => Some((*dst, *value)),
                _ => None,
            })
            .collect();
        for instruction in &function.instructions {
            let code = &mut output.instructions;
            match &instruction.op {
                MirOp::Label(name) => code.push(mir(MirOp::Label(name.clone()), vec![])),
                MirOp::Typed(op) => match op {
                    TypedOp::ScalarInt { dst, value } => {
                        immediate(code, 0, *value);
                        store(code, *dst, 0);
                    }
                    TypedOp::ScalarBool { dst, value } => {
                        immediate(code, 0, i64::from(*value));
                        store(code, *dst, 0);
                    }
                    TypedOp::LocalLoad { dst, local } => {
                        load(code, 0, value_count + *local);
                        store(code, *dst, 0);
                    }
                    TypedOp::LocalStore { local, src } => {
                        load(code, 0, *src);
                        store(code, value_count + *local, 0);
                    }
                    TypedOp::Binary { dst, op, lhs, rhs } => {
                        let operation = match op.as_str() {
                            "+" | "add" => Some(MirOp::Add),
                            "-" | "sub" => Some(MirOp::Sub),
                            "*" | "mul" => Some(MirOp::Mul),
                            "&" | "bitand" => Some(MirOp::And),
                            "|" | "bitor" => Some(MirOp::Or),
                            "^" | "xor" | "bitxor" => Some(MirOp::Xor),
                            "<<" | "shl" => Some(MirOp::Shl),
                            ">>" | "shr" => Some(MirOp::Shr),
                            "lshr" => Some(MirOp::Lshr),
                            "==" | "eq" | "!=" | "neq" | "<" | "<=" | ">" | ">=" => None,
                            _ => {
                                return Err(format!(
                                    "{}: checked division/remainder or unknown binary `{op}` is not implemented",
                                    prefix()
                                ));
                            }
                        };
                        load(code, 0, *lhs);
                        load(code, 1, *rhs);
                        if let Some(operation) = operation {
                            code.push(mir(
                                operation,
                                vec![MirOperand::Reg(0), MirOperand::Reg(0), MirOperand::Reg(1)],
                            ));
                        } else {
                            code.push(mir(
                                MirOp::Cmp,
                                vec![MirOperand::Reg(0), MirOperand::Reg(1)],
                            ));
                            let condition = match op.as_str() {
                                "eq" => "==",
                                "neq" => "!=",
                                op => op,
                            };
                            code.push(mir(
                                MirOp::SetCond(condition.into()),
                                vec![MirOperand::Reg(0)],
                            ));
                        }
                        store(code, *dst, 0);
                    }
                    TypedOp::Unary { dst, op, src } => {
                        load(code, 0, *src);
                        match op.as_str() {
                            "-" | "neg" => code.push(mir(
                                MirOp::Neg,
                                vec![MirOperand::Reg(0), MirOperand::Reg(0)],
                            )),
                            "~" | "bitnot" => code.push(mir(
                                MirOp::Not,
                                vec![MirOperand::Reg(0), MirOperand::Reg(0)],
                            )),
                            "!" | "not" => {
                                immediate(code, 1, 0);
                                code.push(mir(
                                    MirOp::Cmp,
                                    vec![MirOperand::Reg(0), MirOperand::Reg(1)],
                                ));
                                code.push(mir(
                                    MirOp::SetCond("==".into()),
                                    vec![MirOperand::Reg(0)],
                                ));
                            }
                            _ => return Err(format!("{}: unsupported unary `{op}`", prefix())),
                        }
                        store(code, *dst, 0);
                    }
                    TypedOp::DirectCall {
                        dst,
                        target: callee,
                        args,
                    } => {
                        let callee_definition = module
                            .functions
                            .iter()
                            .find(|function| function.name == *callee)
                            .ok_or_else(|| {
                                format!("{}: unknown call target `{callee}`", prefix())
                            })?;
                        if callee_definition.is_external {
                            return Err(format!(
                                "{}: external scalar call `{callee}` is not implemented",
                                prefix()
                            ));
                        }
                        if args.len() > target.argument_limit() {
                            return Err(format!(
                                "{}: stack call arguments are not implemented",
                                prefix()
                            ));
                        }
                        for (index, arg) in args.iter().enumerate() {
                            load(code, index as u32, *arg);
                        }
                        let mut operands = vec![MirOperand::Global(callee.clone())];
                        operands.extend((0..args.len() as u32).map(MirOperand::Reg));
                        code.push(mir(MirOp::Call, operands));
                        if let Some(dst) = dst {
                            store(code, *dst, 0);
                        }
                    }
                    TypedOp::Trap => code.push(mir(MirOp::Trap, vec![])),
                    TypedOp::ReturnValue(value) => {
                        let operands = if let Some(value) = value {
                            load(code, 0, *value);
                            vec![MirOperand::Reg(0)]
                        } else {
                            vec![]
                        };
                        code.push(mir(MirOp::Ret, operands));
                    }
                    TypedOp::Jump(label) => {
                        code.push(mir(MirOp::Jmp, vec![MirOperand::Label(label.clone())]))
                    }
                    TypedOp::BranchIf {
                        cond,
                        then_label,
                        else_label,
                    } => {
                        if let Some(value) = booleans.get(cond) {
                            code.push(mir(
                                MirOp::Jmp,
                                vec![MirOperand::Label(if *value {
                                    then_label.clone()
                                } else {
                                    else_label.clone()
                                })],
                            ));
                        } else {
                            load(code, 0, *cond);
                            immediate(code, 1, 0);
                            code.push(mir(
                                MirOp::Cmp,
                                vec![MirOperand::Reg(0), MirOperand::Reg(1)],
                            ));
                            code.push(mir(MirOp::Jne, vec![MirOperand::Label(then_label.clone())]));
                            code.push(mir(MirOp::Jmp, vec![MirOperand::Label(else_label.clone())]));
                        }
                    }
                    _ => {
                        return Err(format!(
                            "{}: legacy or unsupported typed operation",
                            prefix()
                        ));
                    }
                },
                _ => return Err(format!("{}: mixed typed/machine definition", prefix())),
            }
        }
        result.functions.push(output);
    }
    result.verify_machine()?;
    Ok(result)
}
fn load(code: &mut Vec<MirInst>, reg: VReg, slot: u32) {
    code.push(mir(
        MirOp::Load,
        vec![MirOperand::Reg(reg), MirOperand::FrameSlot(slot)],
    ));
}
fn store(code: &mut Vec<MirInst>, slot: u32, reg: VReg) {
    code.push(mir(
        MirOp::Store,
        vec![MirOperand::FrameSlot(slot), MirOperand::Reg(reg)],
    ));
}
fn immediate(code: &mut Vec<MirInst>, reg: VReg, value: i64) {
    code.push(mir(
        MirOp::Mov,
        vec![MirOperand::Reg(reg), MirOperand::Imm(value)],
    ));
}

#[cfg(test)]
#[path = "mir_scalar_lower_tests.rs"]
mod tests;
