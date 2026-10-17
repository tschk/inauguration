//! Validated scalar machine MIR → x86_64 code.
use super::mir::*;
use crate::native_emit::x86_64 as x64;
use std::collections::HashMap;

pub fn emit_boot(module: &MirModule) -> Result<(Vec<u8>, Vec<(String, u32, u32)>), String> {
    module.verify_machine()?;
    if x64::is_32bit() {
        return Err("machine MIR emitter requires x86_64 ABI".into());
    }
    let mut code = Vec::new();
    let mut functions = Vec::new();
    let mut calls = Vec::new();
    for function in &module.functions {
        if function.is_external {
            continue;
        }
        let start = u32::try_from(code.len()).map_err(|_| "MIR code offset overflow")?;
        emit_function(function, &mut code, &mut calls)?;
        functions.push((
            function.name.clone(),
            start,
            u32::try_from(code.len()).map_err(|_| "MIR code offset overflow")? - start,
        ));
    }
    for (site, target) in calls {
        let offset = functions
            .iter()
            .find(|(name, _, _)| name == &target)
            .map(|(_, offset, _)| *offset as usize)
            .ok_or("MIR unresolved internal Call")?;
        patch(&mut code, site, offset, false)?;
    }
    if !module.rodata.is_empty() {
        while code.len() % 8 != 0 {
            code.push(0);
        }
    }
    let rodata_start = code.len();
    code.extend_from_slice(&module.rodata);
    for relocation in &module.rodata_relocs {
        let target = functions
            .iter()
            .find(|(name, _, _)| name == &relocation.symbol)
            .map(|(_, offset, _)| *offset)
            .ok_or_else(|| format!("MIR unresolved relocation `{}`", relocation.symbol))?;
        let bytes = match relocation.kind {
            RelocKind::Rel32 => {
                relative_displacement(u64::from(relocation.offset), u64::from(target))?
                    .to_le_bytes()
                    .to_vec()
            }
            RelocKind::Abs32 | RelocKind::Abs64 => {
                return Err("machine MIR absolute relocation requires a known load base".into());
            }
        };
        let start = relocation.offset as usize;
        if start < rodata_start {
            return Err(
                "machine MIR relocation field must be inside rodata, not verified instructions"
                    .into(),
            );
        }
        let end = start
            .checked_add(bytes.len())
            .ok_or("MIR relocation overflow")?;
        code.get_mut(start..end)
            .ok_or("MIR relocation outside emitted bytes")?
            .copy_from_slice(&bytes);
    }
    Ok((code, functions))
}
// Relocation.offset names the first displacement byte, not the opcode byte.
fn relative_displacement(field: u64, target: u64) -> Result<i32, String> {
    i32::try_from(i128::from(target) - (i128::from(field) + 4))
        .map_err(|_| "MIR relocation range overflow".into())
}
fn reg(register: u32) -> Result<u8, String> {
    match register {
        0 => Ok(0),
        1 => Ok(3),
        2 => Ok(1),
        3 => Ok(7),
        4 => Ok(6),
        5 => Ok(8),
        _ => Err(format!("machine MIR x86_64 vreg{register} not allocated")),
    }
}
fn condition(op: &MirOp) -> Option<u8> {
    Some(match op {
        MirOp::Je | MirOp::Jz => 4,
        MirOp::Jne | MirOp::Jnz => 5,
        MirOp::Jl => 12,
        MirOp::Jle => 14,
        MirOp::Jg => 15,
        MirOp::Jge => 13,
        _ => return None,
    })
}
fn emit_function(
    function: &MirFunction,
    code: &mut Vec<u8>,
    calls: &mut Vec<(usize, String)>,
) -> Result<(), String> {
    if function.param_types.len() > 6
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
            "machine MIR x86_64 requires scalar parameters/return and at most6 register arguments"
                .into(),
        );
    }
    let local_bytes = function
        .frame_size
        .checked_add(7)
        .ok_or("MIR frame overflow")?
        & !7;
    let outgoing = function
        .instructions
        .iter()
        .filter(|i| i.op == MirOp::Call)
        .map(|i| i.operands.len().saturating_sub(1))
        .max()
        .unwrap_or(0);
    let staged = u32::try_from(function.param_types.len().max(outgoing))
        .map_err(|_| "MIR argument count overflow")?
        .checked_mul(8)
        .ok_or("MIR frame overflow")?;
    let frame = local_bytes
        .checked_add(staged)
        .and_then(|n| n.checked_add(23))
        .ok_or("MIR frame overflow")?
        & !15;
    let frame = i32::try_from(frame.checked_sub(8).ok_or("MIR frame overflow")?)
        .map_err(|_| "MIR frame exceeds signed32 allocation")?;
    code.extend_from_slice(&x64::prologue());
    code.extend_from_slice(&x64::push_r(3)); // Preserve RBX (vreg1).
    if frame != 0 {
        code.extend_from_slice(&x64::sub_rsp_i32(frame));
    }
    // Save all incoming SysV arguments before shuffling into private vreg slots.
    for (index, physical) in [7, 6, 2, 1, 8, 9]
        .iter()
        .take(function.param_types.len())
        .enumerate()
    {
        code.extend_from_slice(&x64::mov_m_r(
            5,
            argument_offset(local_bytes, index)?,
            *physical,
        ));
    }
    for index in 0..function.param_types.len() {
        code.extend_from_slice(&x64::mov_r_m(
            reg(index as u32)?,
            5,
            argument_offset(local_bytes, index)?,
        ));
    }
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
                    return Err("MIR Call operands".into());
                };
                if arguments.len() > 6 {
                    return Err("MIR x86_64 stack call arguments unsupported".into());
                }
                for (index, argument) in arguments.iter().enumerate() {
                    let MirOperand::Reg(source) = argument else {
                        return Err("MIR Call register arguments required".into());
                    };
                    code.extend_from_slice(&x64::mov_m_r(
                        5,
                        argument_offset(local_bytes, index)?,
                        reg(*source)?,
                    ));
                }
                for (index, physical) in [7, 6, 2, 1, 8, 9].iter().take(arguments.len()).enumerate()
                {
                    code.extend_from_slice(&x64::mov_r_m(
                        *physical,
                        5,
                        argument_offset(local_bytes, index)?,
                    ));
                }
                calls.push((code.len(), target.clone()));
                code.extend_from_slice(&x64::call_rel32(0));
            }
            MirOp::Trap => code.extend_from_slice(&[0x0F, 0x0B]),
            MirOp::Ret => {
                if let [MirOperand::Reg(source)] = instruction.operands.as_slice() {
                    code.extend_from_slice(&x64::mov_rr(0, reg(*source)?));
                }
                returns.push(code.len());
                code.extend_from_slice(&x64::jmp_rel32(0));
            }
            MirOp::Jmp => {
                let [MirOperand::Label(name)] = instruction.operands.as_slice() else {
                    return Err("MIR branch operand".into());
                };
                branches.push((code.len(), name.as_str(), None));
                code.extend_from_slice(&x64::jmp_rel32(0));
            }
            op if condition(op).is_some() => {
                let [MirOperand::Label(name)] = instruction.operands.as_slice() else {
                    return Err("MIR branch operand".into());
                };
                let cond = condition(op).unwrap();
                branches.push((code.len(), name.as_str(), Some(cond)));
                code.extend_from_slice(&x64::jcc_near(cond, 0));
            }
            _ => emit_inst(instruction, code)?,
        }
    }
    let epilogue = code.len();
    code.extend_from_slice(&x64::mov_r_m(3, 5, -8));
    code.push(0xc9);
    code.extend_from_slice(&x64::ret());
    for site in returns {
        patch(code, site, epilogue, false)?;
    }
    for (site, name, condition) in branches {
        patch(
            code,
            site,
            *labels.get(name).ok_or("MIR unresolved branch")?,
            condition.is_some(),
        )?;
    }
    Ok(())
}
fn patch(code: &mut [u8], site: usize, target: usize, conditional: bool) -> Result<(), String> {
    let length = if conditional { 6 } else { 5 };
    let delta = i64::try_from(target).map_err(|_| "MIR target overflow")?
        - i64::try_from(site).map_err(|_| "MIR site overflow")?
        - length;
    let displacement = i32::try_from(delta).map_err(|_| "MIR x86 branch range exceeded")?;
    let start = site + if conditional { 2 } else { 1 };
    code[start..start + 4].copy_from_slice(&displacement.to_le_bytes());
    Ok(())
}
// LEA uses the same memory addressing form as MOV, with opcode8D and no read.
// Always use disp32; this keeps zero/negative/extended-base cases uniform.
fn lea_memory(destination: u8, base: u8, displacement: i32) -> Vec<u8> {
    let mut bytes = vec![
        0x48 | ((destination >> 3) << 2) | (base >> 3),
        0x8D,
        0x80 | ((destination & 7) << 3) | (base & 7),
    ];
    if base & 7 == 4 {
        bytes.push(0x24);
    }
    bytes.extend_from_slice(&displacement.to_le_bytes());
    bytes
}

fn frame_offset(slot: u32) -> Result<i32, String> {
    i32::try_from(-16i64 - 8 * i64::from(slot))
        .map_err(|_| "MIR FrameSlot displacement overflow".into())
}
fn argument_offset(local_bytes: u32, index: usize) -> Result<i32, String> {
    let index = i64::try_from(index).map_err(|_| "MIR argument offset overflow")?;
    i32::try_from(-16i64 - i64::from(local_bytes) - 8 * index)
        .map_err(|_| "MIR argument staging displacement overflow".into())
}
fn comparison_code(op: &str) -> Result<u8, String> {
    Ok(match op {
        "==" => 4,
        "!=" => 5,
        "<" => 12,
        "<=" => 14,
        ">" => 15,
        ">=" => 13,
        _ => return Err("MIR SetCond comparison unsupported".into()),
    })
}
fn emit_inst(instruction: &MirInst, code: &mut Vec<u8>) -> Result<(), String> {
    match (&instruction.op, instruction.operands.as_slice()) {
        (MirOp::Load, [MirOperand::Reg(d), MirOperand::FrameSlot(slot)]) => {
            code.extend_from_slice(&x64::mov_r_m(reg(*d)?, 5, frame_offset(*slot)?))
        }
        (MirOp::Store, [MirOperand::FrameSlot(slot), MirOperand::Reg(source)]) => {
            code.extend_from_slice(&x64::mov_m_r(5, frame_offset(*slot)?, reg(*source)?))
        }
        (MirOp::SetCond(op), [MirOperand::Reg(d)]) => {
            let destination = reg(*d)?;
            // MOV must preserve the comparison flags; load_i64(0) uses XOR.
            code.extend_from_slice(&x64::mov_ri64(destination, 0));
            // SETcc needs REX for R8b and for SIL/DIL instead of high legacy bytes.
            if destination >= 4 {
                code.push(0x40 | ((destination >> 3) & 1));
            }
            code.extend_from_slice(&[0x0F, 0x90 | comparison_code(op)?, 0xC0 | (destination & 7)]);
        }
        (op @ (MirOp::Neg | MirOp::Not), [MirOperand::Reg(d), MirOperand::Reg(source)]) => {
            let destination = reg(*d)?;
            code.extend_from_slice(&x64::mov_rr(destination, reg(*source)?));
            code.extend_from_slice(&if *op == MirOp::Neg {
                x64::neg_reg(destination)
            } else {
                x64::not_reg(destination)
            });
        }
        (MirOp::Mov, [MirOperand::Reg(d), MirOperand::Imm(value)]) => {
            code.extend_from_slice(&x64::mov_ri64(reg(*d)?, *value))
        }
        (MirOp::Mov, [MirOperand::Reg(d), MirOperand::Reg(s)]) => {
            code.extend_from_slice(&x64::mov_rr(reg(*d)?, reg(*s)?))
        }
        (MirOp::Mov, [MirOperand::Reg(d), MirOperand::Mem { base, offset }]) => {
            code.extend_from_slice(&x64::mov_r_m(reg(*d)?, reg(*base)?, *offset))
        }
        (MirOp::Lea, [MirOperand::Reg(d), MirOperand::Mem { base, offset }]) => {
            code.extend_from_slice(&lea_memory(reg(*d)?, reg(*base)?, *offset));
        }
        (MirOp::Load, [MirOperand::Reg(d), MirOperand::Reg(base)]) => {
            code.extend_from_slice(&x64::mov_r_m(reg(*d)?, reg(*base)?, 0))
        }
        (MirOp::Store, [MirOperand::Mem { base, offset }, MirOperand::Reg(s)]) => {
            code.extend_from_slice(&x64::mov_m_r(reg(*base)?, *offset, reg(*s)?))
        }
        (
            op @ (MirOp::Add | MirOp::Sub | MirOp::Mul | MirOp::And | MirOp::Or | MirOp::Xor),
            [MirOperand::Reg(d), MirOperand::Reg(a), b],
        ) => {
            match b {
                MirOperand::Reg(b) => code.extend_from_slice(&x64::mov_rr(11, reg(*b)?)),
                MirOperand::Imm(value) => code.extend_from_slice(&x64::mov_ri64(11, *value)),
                _ => return Err("MIR binary RHS operand".into()),
            }
            let dest = reg(*d)?;
            code.extend_from_slice(&x64::mov_rr(dest, reg(*a)?));
            code.extend_from_slice(&match op {
                MirOp::Add => x64::add_rr(dest, 11),
                MirOp::Sub => x64::sub_rr(dest, 11),
                MirOp::Mul => x64::imul_rr(dest, 11),
                MirOp::And => x64::and_rr(dest, 11),
                MirOp::Or => x64::or_rr(dest, 11),
                MirOp::Xor => x64::xor_rr(dest, 11),
                _ => unreachable!(),
            });
        }
        (
            op @ (MirOp::Shl | MirOp::Shr | MirOp::Lshr),
            [MirOperand::Reg(d), MirOperand::Reg(a), b],
        ) => {
            let dest = reg(*d)?;
            // RCX is an allocated register: preserve it around the implicit CL count.
            code.extend_from_slice(&x64::mov_rr(11, reg(*a)?));
            code.extend_from_slice(&x64::mov_rr(10, x64::RCX));
            match b {
                MirOperand::Reg(b) => code.extend_from_slice(&x64::mov_rr(x64::RCX, reg(*b)?)),
                MirOperand::Imm(value) => code.extend_from_slice(&x64::mov_ri64(x64::RCX, *value)),
                _ => return Err("MIR shift RHS operand".into()),
            }
            code.extend_from_slice(&match op {
                MirOp::Shl => x64::shl_reg_cl(11),
                MirOp::Shr => x64::sar_reg_cl(11),
                MirOp::Lshr => x64::shr_reg_cl(11),
                _ => unreachable!(),
            });
            code.extend_from_slice(&x64::mov_rr(x64::RCX, 10));
            code.extend_from_slice(&x64::mov_rr(dest, 11));
        }
        (MirOp::Cmp, [MirOperand::Reg(a), MirOperand::Reg(b)]) => {
            code.extend_from_slice(&x64::cmp_rr(reg(*a)?, reg(*b)?))
        }
        (MirOp::Cmp, [MirOperand::Reg(a), MirOperand::Imm(value)]) => {
            code.extend_from_slice(&x64::mov_ri64(11, *value));
            code.extend_from_slice(&x64::cmp_rr(reg(*a)?, 11));
        }
        (MirOp::Nop, []) => code.push(0x90),
        (MirOp::Comment(_), []) => {}
        _ => {
            return Err(format!(
                "machine MIR x86_64 unsupported opcode/operands: {instruction:?}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod relocation_tests {
    #[test]
    fn operand_relative_displacements_include_the_entire_four_byte_field() {
        assert_eq!(super::relative_displacement(0, 9), Ok(5));
        assert_eq!(super::relative_displacement(9, 0), Ok(-13));
        assert_eq!(super::relative_displacement(0, 4), Ok(0));
        assert!(super::relative_displacement(0, u64::MAX).is_err());
        assert!(super::relative_displacement(u64::MAX, 0).is_err());
    }
}

#[cfg(test)]
mod shift_tests {
    use super::*;

    #[test]
    fn shifts_save_and_restore_allocated_rcx_before_writing_destination() {
        if x64::is_32bit() {
            return;
        }
        for (op, shift) in [
            (MirOp::Shl, x64::shl_reg_cl(11)),
            (MirOp::Shr, x64::sar_reg_cl(11)),
            (MirOp::Lshr, x64::shr_reg_cl(11)),
        ] {
            for dest in [0, 2] {
                for right in [MirOperand::Reg(2), MirOperand::Imm(-1)] {
                    let mut code = Vec::new();
                    emit_inst(
                        &mir(
                            op.clone(),
                            vec![MirOperand::Reg(dest), MirOperand::Reg(2), right.clone()],
                        ),
                        &mut code,
                    )
                    .unwrap();
                    let mut expected = x64::mov_rr(11, x64::RCX);
                    expected.extend(x64::mov_rr(10, x64::RCX));
                    expected.extend(match right {
                        MirOperand::Reg(_) => x64::mov_rr(x64::RCX, x64::RCX),
                        MirOperand::Imm(value) => x64::mov_ri64(x64::RCX, value),
                        _ => unreachable!(),
                    });
                    expected.extend_from_slice(&shift);
                    expected.extend(x64::mov_rr(x64::RCX, 10));
                    expected.extend(x64::mov_rr(reg(dest).unwrap(), 11));
                    assert_eq!(code, expected);
                }
            }
        }
    }
}
