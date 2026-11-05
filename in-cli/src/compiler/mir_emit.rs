//! Validated scalar machine MIR → AArch64 JIT code.
use super::mir::*;
use crate::jit_runtime::JitRuntime;
use crate::native_emit::aarch64;
use std::collections::HashMap;

pub fn emit_jit(module: &MirModule) -> Result<(Vec<u8>, Vec<(String, u32, u32)>), String> {
    module.verify_machine()?;
    if !module.rodata.is_empty() || !module.rodata_relocs.is_empty() {
        return Err("machine MIR AArch64 rodata/relocations are not supported".into());
    }
    let mut code = Vec::new();
    let mut functions = Vec::new();
    let mut calls = Vec::new();
    for function in &module.functions {
        if function.is_external {
            continue;
        }
        let start = u32::try_from(code.len()).map_err(|_| "MIR code exceeds32-bit offsets")?;
        emit_function(function, &mut code, &mut calls)?;
        functions.push((
            function.name.clone(),
            start,
            u32::try_from(code.len()).map_err(|_| "MIR code exceeds32-bit offsets")? - start,
        ));
    }
    for (site, target) in calls {
        let offset = functions
            .iter()
            .find(|(name, _, _)| name == &target)
            .map(|(_, offset, _)| *offset as usize)
            .ok_or("MIR unresolved internal call")?;
        patch(&mut code, site, offset, None)?;
        // Same signed26 displacement as B, with link bit set.
        let instruction =
            u32::from_le_bytes(code[site..site + 4].try_into().unwrap()) | 0x8000_0000;
        code[site..site + 4].copy_from_slice(&instruction.to_le_bytes());
    }
    Ok((code, functions))
}
fn word(code: &mut Vec<u8>, instruction: u32) {
    code.extend_from_slice(&instruction.to_le_bytes());
}
fn reg(r: u32) -> Result<u8, String> {
    if r <= 15 {
        Ok(r as u8)
    } else {
        Err(format!(
            "MIR AArch64 vreg{r} exceeds caller-saved scalar register set"
        ))
    }
}
fn condition(op: &MirOp) -> Option<u8> {
    Some(match op {
        MirOp::Je | MirOp::Jz => 0,
        MirOp::Jne | MirOp::Jnz => 1,
        MirOp::Jl => 11,
        MirOp::Jle => 13,
        MirOp::Jg => 12,
        MirOp::Jge => 10,
        _ => return None,
    })
}
fn emit_function(
    function: &MirFunction,
    code: &mut Vec<u8>,
    calls: &mut Vec<(usize, String)>,
) -> Result<(), String> {
    if function.param_types.len() > 8
        || function.vreg_count < function.param_types.len() as u32
        || function.param_types.iter().any(|t| {
            !matches!(
                scalar_type(t),
                Ok(crate::core_ir::Typ::Int | crate::core_ir::Typ::Bool)
            )
        })
        || function.return_type.as_ref().is_some_and(|t| {
            !matches!(
                scalar_type(t),
                Ok(crate::core_ir::Typ::Int
                    | crate::core_ir::Typ::Bool
                    | crate::core_ir::Typ::Void)
            )
        })
    {
        return Err(
            "MIR AArch64 requires scalar parameters/return and at most8 register arguments".into(),
        );
    }
    let local_bytes = u64::from(function.frame_size)
        .checked_add(7)
        .ok_or("MIR frame overflow")?
        & !7;
    let outgoing = function
        .instructions
        .iter()
        .filter(|i| i.op == MirOp::Call)
        .map(|i| i.operands.len().saturating_sub(1))
        .max()
        .unwrap_or(0) as u64;
    let frame = local_bytes
        .checked_add(outgoing.checked_mul(8).ok_or("MIR frame overflow")?)
        .and_then(|n| n.checked_add(15))
        .ok_or("MIR frame overflow")?
        & !15;
    word(code, aarch64::stp_pre(29, 30, -16));
    word(code, aarch64::add_imm64(29, 31, 0));
    allocate_frame(code, frame);
    let mut labels = HashMap::new();
    let mut branches = Vec::new();
    let mut returns = Vec::new();
    for instruction in &function.instructions {
        match &instruction.op {
            MirOp::Label(name) => {
                labels.insert(name.as_str(), code.len());
            }
            MirOp::Call => {
                let Some((MirOperand::Global(target), arguments)) =
                    instruction.operands.split_first()
                else {
                    return Err("MIR internal Call operands".into());
                };
                if arguments.len() > 8 {
                    return Err("MIR AArch64 stack call arguments unsupported".into());
                }
                for (index, argument) in arguments.iter().enumerate() {
                    let MirOperand::Reg(source) = argument else {
                        return Err("MIR Call register argument required".into());
                    };
                    frame_address(code, local_bytes + 8 * (index as u64 + 1), 16);
                    word(code, aarch64::str64(reg(*source)?, 16, 0));
                }
                for index in 0..arguments.len() {
                    frame_address(code, local_bytes + 8 * (index as u64 + 1), 16);
                    word(code, aarch64::ldr64(index as u8, 16, 0));
                }
                calls.push((code.len(), target.clone()));
                word(code, aarch64::bl(0));
            }
            MirOp::Trap => word(code, aarch64::brk(0)),
            MirOp::Ret => {
                if let [MirOperand::Reg(source)] = instruction.operands.as_slice() {
                    word(code, aarch64::mov_reg64(0, reg(*source)?));
                }
                returns.push(code.len());
                word(code, aarch64::b(0));
            }
            MirOp::Jmp => {
                let [MirOperand::Label(name)] = instruction.operands.as_slice() else {
                    return Err("MIR branch operand".into());
                };
                branches.push((code.len(), name.as_str(), None));
                word(code, aarch64::b(0));
            }
            op if condition(op).is_some() => {
                let [MirOperand::Label(name)] = instruction.operands.as_slice() else {
                    return Err("MIR branch operand".into());
                };
                let cond = condition(op).unwrap();
                branches.push((code.len(), name.as_str(), Some(cond)));
                word(code, aarch64::b_cond(cond, 0));
            }
            _ => emit_inst(instruction, code)?,
        }
    }
    let epilogue = code.len();
    word(code, aarch64::add_imm64(31, 29, 0));
    word(code, aarch64::ldp_post(29, 30, 16));
    word(code, aarch64::ret());
    for site in returns {
        patch(code, site, epilogue, None)?;
    }
    for (site, name, condition) in branches {
        patch(
            code,
            site,
            *labels.get(name).ok_or("MIR unresolved local label")?,
            condition,
        )?;
    }
    Ok(())
}
fn patch(code: &mut [u8], site: usize, target: usize, condition: Option<u8>) -> Result<(), String> {
    let delta = i64::try_from(target).map_err(|_| "MIR target overflow")?
        - i64::try_from(site).map_err(|_| "MIR site overflow")?;
    let limit = if condition.is_some() {
        1i64 << 20
    } else {
        1i64 << 27
    };
    if delta % 4 != 0 || !(-limit..limit).contains(&delta) {
        return Err("MIR AArch64 branch range exceeded".into());
    }
    let instruction = condition.map_or_else(
        || aarch64::b(delta as i32),
        |c| aarch64::b_cond(c, delta as i32),
    );
    code[site..site + 4].copy_from_slice(&instruction.to_le_bytes());
    Ok(())
}
fn allocate_frame(code: &mut Vec<u8>, mut bytes: u64) {
    while bytes >= 4096 {
        let units = (bytes >> 12).min(4095) as u16;
        word(code, aarch64::sub_imm64(31, 31, units) | (1 << 22));
        bytes -= u64::from(units) << 12;
    }
    if bytes != 0 {
        word(code, aarch64::sub_imm64(31, 31, bytes as u16));
    }
}
fn frame_address(code: &mut Vec<u8>, offset: u64, destination: u8) {
    for instruction in aarch64::load_i64(16, -(offset as i64)) {
        word(code, instruction);
    }
    word(code, aarch64::add_reg64(destination, 29, 16));
}
fn comparison_code(op: &str) -> Result<u8, String> {
    Ok(match op {
        "==" => 0,
        "!=" => 1,
        "<" => 11,
        "<=" => 13,
        ">" => 12,
        ">=" => 10,
        _ => return Err("MIR SetCond comparison unsupported".into()),
    })
}
fn emit_inst(instruction: &MirInst, code: &mut Vec<u8>) -> Result<(), String> {
    match (&instruction.op, instruction.operands.as_slice()) {
        (MirOp::Load, [MirOperand::Reg(d), MirOperand::FrameSlot(slot)]) => {
            frame_address(code, 8 * (u64::from(*slot) + 1), 16);
            word(code, aarch64::ldr64(reg(*d)?, 16, 0));
        }
        (MirOp::Store, [MirOperand::FrameSlot(slot), MirOperand::Reg(source)]) => {
            frame_address(code, 8 * (u64::from(*slot) + 1), 16);
            word(code, aarch64::str64(reg(*source)?, 16, 0));
        }
        (MirOp::SetCond(op), [MirOperand::Reg(d)]) => {
            let condition = comparison_code(op)?;
            word(
                code,
                0x9A9F_07E0 | (u32::from(condition ^ 1) << 12) | u32::from(reg(*d)?),
            );
        }
        (MirOp::Neg, [MirOperand::Reg(d), MirOperand::Reg(source)]) => {
            word(code, aarch64::sub_reg64(reg(*d)?, 31, reg(*source)?))
        }
        (MirOp::Not, [MirOperand::Reg(d), MirOperand::Reg(source)]) => {
            word(code, aarch64::mvn_reg64(reg(*d)?, reg(*source)?))
        }
        (MirOp::Mov, [MirOperand::Reg(d), MirOperand::Imm(value)]) => {
            for instruction in aarch64::load_i64(reg(*d)?, *value) {
                word(code, instruction);
            }
        }
        (MirOp::Mov, [MirOperand::Reg(d), MirOperand::Reg(s)]) => {
            word(code, aarch64::mov_reg64(reg(*d)?, reg(*s)?))
        }
        (MirOp::Add, [MirOperand::Reg(d), MirOperand::Reg(a), MirOperand::Reg(b)]) => {
            word(code, aarch64::add_reg64(reg(*d)?, reg(*a)?, reg(*b)?))
        }
        (MirOp::Sub, [MirOperand::Reg(d), MirOperand::Reg(a), MirOperand::Reg(b)]) => {
            word(code, aarch64::sub_reg64(reg(*d)?, reg(*a)?, reg(*b)?))
        }
        (
            op @ (MirOp::Add | MirOp::Sub),
            [
                MirOperand::Reg(d),
                MirOperand::Reg(a),
                MirOperand::Imm(value),
            ],
        ) if (0..=4095).contains(value) => word(
            code,
            if *op == MirOp::Add {
                aarch64::add_imm64(reg(*d)?, reg(*a)?, *value as u16)
            } else {
                aarch64::sub_imm64(reg(*d)?, reg(*a)?, *value as u16)
            },
        ),
        (
            op @ (MirOp::Mul
            | MirOp::And
            | MirOp::Or
            | MirOp::Xor
            | MirOp::Shl
            | MirOp::Shr
            | MirOp::Lshr),
            [MirOperand::Reg(d), MirOperand::Reg(a), MirOperand::Reg(b)],
        ) => {
            let (d, a, b) = (reg(*d)?, reg(*a)?, reg(*b)?);
            word(
                code,
                match op {
                    MirOp::Mul => aarch64::mul64(d, a, b),
                    MirOp::And => aarch64::and_reg64(d, a, b),
                    MirOp::Or => aarch64::orr_reg64(d, a, b),
                    MirOp::Xor => aarch64::eor_reg64(d, a, b),
                    MirOp::Shl => aarch64::lsl_reg64(d, a, b),
                    MirOp::Shr => aarch64::asr_reg64(d, a, b),
                    MirOp::Lshr => aarch64::lsr_reg64(d, a, b),
                    _ => unreachable!(),
                },
            );
        }
        (MirOp::Cmp, [MirOperand::Reg(a), MirOperand::Reg(b)]) => {
            word(code, aarch64::cmp_reg64(reg(*a)?, reg(*b)?))
        }
        (MirOp::Nop, []) => word(code, aarch64::nop()),
        (MirOp::Comment(_), []) => {}
        _ => {
            return Err(format!(
                "MIR AArch64 unsupported opcode/operands: {instruction:?}"
            ));
        }
    }
    Ok(())
}
pub fn load_jit(module: &MirModule, runtime: &mut JitRuntime) -> Result<(), String> {
    let (code, functions) = emit_jit(module)?;
    runtime.load(&code, &functions, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::Typ;

    #[test]
    fn test_emit_jit_returns_code() {
        let mut module = MirModule::new();
        let function = MirFunction {
            name: "test_func".to_string(),
            instructions: vec![MirInst {
                op: MirOp::Ret,
                operands: vec![],
                offset: 0,
            }],
            vreg_count: 0,
            frame_size: 0,
            return_type: Some(Typ::Void),
            param_types: vec![],
            var_map: vec![],
            value_types: vec![],
            local_types: vec![],
            is_external: false,
        };
        module.functions.push(function);

        let result = emit_jit(&module);
        assert!(result.is_ok(), "Expected Ok, got {:?}", result);
        let (code, _functions) = result.unwrap();
        assert!(!code.is_empty(), "Expected generated machine code");
    }
}
