//! Shared machine-MIR CFG, stack-slot, and definite use/def validation.
use super::*;
use std::collections::{HashMap, HashSet, VecDeque};

pub(super) fn validate(module: &MirModule, runtime: bool, errors: &mut Vec<String>) {
    let functions: HashMap<_, _> = module
        .functions
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();
    for function in &module.functions {
        if function.is_external {
            if !function.instructions.is_empty() {
                errors.push(format!(
                    "external MIR function `{}` has instructions",
                    function.name
                ));
            }
            continue;
        }
        if function
            .instructions
            .iter()
            .any(|i| matches!(i.op, MirOp::Typed(_)))
        {
            if runtime {
                errors.push(format!(
                    "MIR function `{}` still contains staged Typed instructions",
                    function.name
                ));
            }
            continue;
        }
        if let Err(error) = validate_function(function, &functions) {
            errors.push(format!("MIR function `{}`: {error}", function.name));
        }
    }
}
fn branch_target(instruction: &MirInst) -> Result<&str, String> {
    match instruction.operands.as_slice() {
        [MirOperand::Label(name)] => Ok(name),
        _ => Err("branch requires exactly one local Label operand".into()),
    }
}
fn conditional(op: &MirOp) -> bool {
    matches!(
        op,
        MirOp::Jz
            | MirOp::Jnz
            | MirOp::Je
            | MirOp::Jne
            | MirOp::Jl
            | MirOp::Jle
            | MirOp::Jg
            | MirOp::Jge
    )
}
fn void(function: &MirFunction) -> bool {
    function
        .return_type
        .as_ref()
        .is_none_or(|t| scalar_type(t) == Ok(crate::core_ir::Typ::Void))
}
fn scalar(typ: &crate::core_ir::Typ) -> bool {
    matches!(
        scalar_type(typ),
        Ok(crate::core_ir::Typ::Int | crate::core_ir::Typ::Bool)
    )
}
fn call_target<'a>(
    instruction: &MirInst,
    functions: &HashMap<&str, &'a MirFunction>,
) -> Result<&'a MirFunction, String> {
    let Some((MirOperand::Global(name), arguments)) = instruction.operands.split_first() else {
        return Err("Call requires Global target followed by register arguments".into());
    };
    let target = *functions
        .get(name.as_str())
        .ok_or_else(|| format!("unknown internal Call target `{name}`"))?;
    if target.is_external
        || arguments.len() != target.param_types.len()
        || arguments.iter().any(|a| !matches!(a, MirOperand::Reg(_)))
        || target.param_types.iter().any(|t| !scalar(t))
        || (!void(target) && !target.return_type.as_ref().is_some_and(scalar))
    {
        return Err("Call requires an exact known internal scalar signature".into());
    }
    Ok(target)
}
#[derive(Clone, PartialEq, Eq)]
struct State {
    registers: HashSet<VReg>,
    slots: HashSet<u32>,
    flags: bool,
}
fn validate_function(
    function: &MirFunction,
    functions: &HashMap<&str, &MirFunction>,
) -> Result<(), String> {
    let code = &function.instructions;
    let mut labels = HashMap::new();
    for (index, instruction) in code.iter().enumerate() {
        match &instruction.op {
            MirOp::Label(name)
                if name.is_empty()
                    || !instruction.operands.is_empty()
                    || labels.insert(name.as_str(), index).is_some() =>
            {
                return Err("empty, duplicate, or malformed local label".into());
            }
            MirOp::Ret if !matches!(instruction.operands.as_slice(), [] | [MirOperand::Reg(_)]) => {
                return Err("Ret requires no operands or exactly one register".into());
            }
            MirOp::Trap if !instruction.operands.is_empty() => {
                return Err("Trap requires no operands".into());
            }
            MirOp::SetCond(op)
                if !matches!(op.as_str(), "==" | "!=" | "<" | "<=" | ">" | ">=")
                    || !matches!(instruction.operands.as_slice(), [MirOperand::Reg(_)]) =>
            {
                return Err(
                    "SetCond requires canonical comparison and one destination register".into(),
                );
            }
            MirOp::Call => {
                call_target(instruction, functions)?;
            }
            _ => {}
        }
        for operand in &instruction.operands {
            let register = match operand {
                MirOperand::Reg(r) => Some(*r),
                MirOperand::Mem { base, .. } => Some(*base),
                _ => None,
            };
            if register.is_some_and(|r| r >= function.vreg_count) {
                return Err("register exceeds declared vreg_count".into());
            }
            if let MirOperand::FrameSlot(slot) = operand {
                if slot
                    .checked_add(1)
                    .and_then(|n| n.checked_mul(8))
                    .is_none_or(|end| end > function.frame_size)
                {
                    return Err("FrameSlot exceeds declared local frame".into());
                }
            }
        }
    }
    let mut successors = vec![vec![]; code.len()];
    for (index, instruction) in code.iter().enumerate() {
        if instruction.op == MirOp::Jmp || conditional(&instruction.op) {
            let name = branch_target(instruction)?;
            successors[index].push(
                *labels
                    .get(name)
                    .ok_or_else(|| format!("unknown local branch label `{name}`"))?,
            );
            if conditional(&instruction.op) {
                successors[index].push(index + 1);
            }
        } else if !matches!(instruction.op, MirOp::Ret | MirOp::Trap) {
            successors[index].push(index + 1);
        }
    }
    let mut incoming = vec![None; code.len() + 1];
    incoming[0] = Some(State {
        registers: (0..function.param_types.len() as u32).collect(),
        slots: HashSet::new(),
        flags: false,
    });
    let mut queue = VecDeque::from([0]);
    while let Some(index) = queue.pop_front() {
        if index == code.len() {
            continue;
        }
        let mut state = incoming[index].clone().unwrap();
        let instruction = &code[index];
        if instruction.op == MirOp::Call {
            state.registers.clear();
            state.flags = false;
            if !void(call_target(instruction, functions)?) {
                state.registers.insert(0);
            }
        } else {
            if let Some(destination) = destination(instruction) {
                state.registers.insert(destination);
            }
            if let (MirOp::Store, [MirOperand::FrameSlot(slot), _]) =
                (&instruction.op, instruction.operands.as_slice())
            {
                state.slots.insert(*slot);
            }
            state.flags = match instruction.op {
                MirOp::Cmp => true,
                MirOp::Label(_)
                | MirOp::Comment(_)
                | MirOp::Nop
                | MirOp::Mov
                | MirOp::Load
                | MirOp::Store
                | MirOp::Lea
                | MirOp::SetCond(_)
                | MirOp::Ret
                | MirOp::Trap
                | MirOp::Jmp
                | MirOp::Jz
                | MirOp::Jnz
                | MirOp::Je
                | MirOp::Jne
                | MirOp::Jl
                | MirOp::Jle
                | MirOp::Jg
                | MirOp::Jge => state.flags,
                _ => false,
            };
        }
        for &next in &successors[index] {
            let merged = match &incoming[next] {
                None => state.clone(),
                Some(previous) => State {
                    registers: previous
                        .registers
                        .intersection(&state.registers)
                        .copied()
                        .collect(),
                    slots: previous.slots.intersection(&state.slots).copied().collect(),
                    flags: previous.flags && state.flags,
                },
            };
            if incoming[next].as_ref() != Some(&merged) {
                incoming[next] = Some(merged);
                queue.push_back(next);
            }
        }
    }
    if incoming[code.len()].is_some() && !void(function) {
        return Err("non-void function can reach its end without returning".into());
    }
    for (index, instruction) in code.iter().enumerate() {
        let Some(state) = &incoming[index] else {
            continue;
        };
        if (conditional(&instruction.op) || matches!(instruction.op, MirOp::SetCond(_)))
            && !state.flags
        {
            return Err("conditional operation lacks a definitely available Cmp result".into());
        }
        for register in reads(instruction, void(function)) {
            if !state.registers.contains(&register) {
                return Err(format!(
                    "register {register} is read before a definite write"
                ));
            }
        }
        if let (MirOp::Load, [_, MirOperand::FrameSlot(slot)]) =
            (&instruction.op, instruction.operands.as_slice())
        {
            if !state.slots.contains(slot) {
                return Err(format!("FrameSlot {slot} is read before a definite store"));
            }
        }
    }
    Ok(())
}
fn destination(instruction: &MirInst) -> Option<VReg> {
    if matches!(
        instruction.op,
        MirOp::Mov
            | MirOp::Load
            | MirOp::Lea
            | MirOp::SetCond(_)
            | MirOp::Add
            | MirOp::Sub
            | MirOp::Mul
            | MirOp::Div
            | MirOp::And
            | MirOp::Or
            | MirOp::Xor
            | MirOp::Shl
            | MirOp::Shr
            | MirOp::Not
            | MirOp::Neg
            | MirOp::FAdd
            | MirOp::FSub
            | MirOp::FMul
            | MirOp::FDiv
            | MirOp::FMov
            | MirOp::FCvt
    ) {
        if let Some(MirOperand::Reg(r)) = instruction.operands.first() {
            return Some(*r);
        }
    }
    None
}
fn reads(instruction: &MirInst, is_void: bool) -> Vec<VReg> {
    if instruction.op == MirOp::Ret && instruction.operands.is_empty() {
        return if is_void { vec![] } else { vec![0] };
    }
    let skip = usize::from(destination(instruction).is_some());
    instruction
        .operands
        .iter()
        .skip(skip)
        .filter_map(|o| match o {
            MirOperand::Reg(r) => Some(*r),
            MirOperand::Mem { base, .. } => Some(*base),
            _ => None,
        })
        .collect()
}
