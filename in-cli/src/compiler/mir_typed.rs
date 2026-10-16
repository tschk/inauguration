//! Validation for explicit scalar typed MIR. No physical-register assumptions.
use super::{MirFunction, MirModule, MirOp, TypedOp, VReg};
use crate::core_ir::Typ;
use std::collections::{HashMap, HashSet, VecDeque};

pub(crate) fn scalar_type(typ: &Typ) -> Result<Typ, String> {
    // Int here is the already-normalized signed 64-bit word. Raw frontend
    // width/signedness spellings must not silently become word arithmetic.
    match typ {
        Typ::Int | Typ::Bool | Typ::Void => Ok(typ.clone()),
        Typ::Named(name) => match name.trim() {
            "Int" | "i64" => Ok(Typ::Int),
            "Bool" | "bool" => Ok(Typ::Bool),
            "Void" | "void" | "Unit" | "unit" | "()" => Ok(Typ::Void),
            _ if name.starts_with('#') => {
                Err("typed-mir: normalize private type metadata before lowering".into())
            }
            _ => Err(format!(
                "typed-mir: unsupported or unnormalized scalar type {typ:?}"
            )),
        },
        _ => Err(format!("typed-mir: unsupported scalar type {typ:?}")),
    }
}

pub(crate) fn scalar_binary_type(op: &str, typ: &Typ) -> Result<Typ, String> {
    match op {
        "+" | "add" | "-" | "sub" | "*" | "mul" | "/" | "div" | "%" | "rem" | "udiv" | "urem"
        | "&" | "bitand" | "|" | "bitor" | "^" | "xor" | "bitxor" | "<<" | "shl" | ">>" | "shr"
        | "lshr"
            if *typ == Typ::Int =>
        {
            Ok(Typ::Int)
        }
        "<" | "<=" | ">" | ">=" if *typ == Typ::Int => Ok(Typ::Bool),
        "==" | "eq" | "!=" | "neq" if matches!(typ, Typ::Int | Typ::Bool) => Ok(Typ::Bool),
        _ => Err(format!("typed-mir: unsupported binary {op} on {typ:?}")),
    }
}
pub(crate) fn scalar_unary_result(op: &str, input: &Typ) -> Result<Typ, String> {
    match op {
        "-" | "neg" | "~" | "bitnot" if *input == Typ::Int => Ok(Typ::Int),
        "!" | "not" if matches!(input, Typ::Int | Typ::Bool) => Ok(Typ::Bool),
        _ => Err(format!("typed-mir: unsupported unary {op} on {input:?}")),
    }
}
fn canonical_void(typ: Option<&Typ>) -> bool {
    typ.is_none_or(|typ| typ.canonical() == Typ::Void)
}
fn scalar(typ: &Typ) -> bool {
    matches!(typ, Typ::Int | Typ::Bool)
}

pub(super) fn is_typed(function: &MirFunction) -> bool {
    function.is_external
        || !function.value_types.is_empty()
        || !function.local_types.is_empty()
        || function
            .instructions
            .iter()
            .any(|inst| matches!(inst.op, MirOp::Typed(_)))
}
pub(super) fn verify(module: &MirModule, function: &MirFunction) -> Result<(), String> {
    let fail = |message: &str| format!("typed-mir: {}: {message}", function.name);
    if function.vreg_count as usize != function.value_types.len() {
        return Err(fail("value type/count mismatch"));
    }
    if function
        .value_types
        .iter()
        .chain(&function.local_types)
        .any(|typ| !scalar(typ))
    {
        return Err(fail("unsupported value/local type"));
    }
    if function.param_types.iter().any(|typ| !scalar(typ))
        || (!canonical_void(function.return_type.as_ref())
            && !function.return_type.as_ref().is_some_and(scalar))
    {
        return Err(fail("unsupported signature type"));
    }
    let parameters = function.param_types.len();
    if !function.value_types.starts_with(&function.param_types)
        || !function.local_types.starts_with(&function.param_types)
    {
        return Err(fail("parameters must own first values and local slots"));
    }
    if function.is_external {
        if !function.instructions.is_empty()
            || function.value_types.len() != parameters
            || function.local_types.len() != parameters
        {
            return Err(fail("external declaration carries a definition"));
        }
        return Ok(());
    }
    let n = function.instructions.len();
    if n == 0 {
        return Err(fail("definition has no instructions"));
    }
    let mut labels = HashMap::new();
    for (index, inst) in function.instructions.iter().enumerate() {
        if let MirOp::Label(name) = &inst.op {
            if !inst.operands.is_empty() || labels.insert(name.as_str(), index).is_some() {
                return Err(fail("duplicate/malformed local label"));
            }
        }
    }
    let reg_type = |reg: VReg| {
        function
            .value_types
            .get(reg as usize)
            .ok_or_else(|| fail("unknown value register"))
    };
    let local_type = |local: u32| {
        function
            .local_types
            .get(local as usize)
            .ok_or_else(|| fail("unknown local slot"))
    };
    let target = |name: &str| {
        labels
            .get(name)
            .copied()
            .ok_or_else(|| fail("missing local branch target"))
    };
    let mut uses = vec![vec![]; n];
    let mut defs = vec![vec![]; n];
    let mut reads = vec![vec![]; n];
    let mut writes = vec![vec![]; n];
    let mut edges = vec![vec![]; n];
    let mut definitions: HashSet<VReg> = (0..parameters as u32).collect();
    let constants: HashMap<VReg, bool> = function
        .instructions
        .iter()
        .filter_map(|inst| match &inst.op {
            MirOp::Typed(TypedOp::ScalarBool { dst, value }) => Some((*dst, *value)),
            _ => None,
        })
        .collect();
    for (index, inst) in function.instructions.iter().enumerate() {
        let mut next = true;
        match &inst.op {
            MirOp::Label(_) => {}
            MirOp::Typed(op) if inst.operands.is_empty() => match op {
                TypedOp::ScalarInt { dst, .. } => {
                    if *reg_type(*dst)? != Typ::Int {
                        return Err(fail("integer result type"));
                    }
                    defs[index].push(*dst);
                }
                TypedOp::ScalarBool { dst, .. } => {
                    if *reg_type(*dst)? != Typ::Bool {
                        return Err(fail("boolean result type"));
                    }
                    defs[index].push(*dst);
                }
                TypedOp::LocalLoad { dst, local } => {
                    if reg_type(*dst)? != local_type(*local)? {
                        return Err(fail("load type mismatch"));
                    }
                    defs[index].push(*dst);
                    reads[index].push(*local);
                }
                TypedOp::LocalStore { local, src } => {
                    if local_type(*local)? != reg_type(*src)? {
                        return Err(fail("store type mismatch"));
                    }
                    uses[index].push(*src);
                    writes[index].push(*local);
                }
                TypedOp::Binary { dst, op, lhs, rhs } => {
                    if reg_type(*lhs)? != reg_type(*rhs)?
                        || *reg_type(*dst)? != scalar_binary_type(op, reg_type(*lhs)?)?
                    {
                        return Err(fail("binary type mismatch"));
                    }
                    uses[index].extend([*lhs, *rhs]);
                    defs[index].push(*dst);
                }
                TypedOp::Unary { dst, op, src } => {
                    let typ = scalar_unary_result(op, reg_type(*src)?)?;
                    if *reg_type(*dst)? != typ {
                        return Err(fail("unary type mismatch"));
                    }
                    uses[index].push(*src);
                    defs[index].push(*dst);
                }
                TypedOp::DirectCall { dst, target, args } => {
                    let mut matches = module
                        .functions
                        .iter()
                        .filter(|callee| callee.name == *target);
                    let callee = matches.next().ok_or_else(|| fail("unknown direct call"))?;
                    if matches.next().is_some() || args.len() != callee.param_types.len() {
                        return Err(fail("ambiguous call or arity mismatch"));
                    }
                    for (reg, typ) in args.iter().zip(&callee.param_types) {
                        if reg_type(*reg)? != typ {
                            return Err(fail("call argument type mismatch"));
                        }
                    }
                    match dst {
                        None if canonical_void(callee.return_type.as_ref()) => {}
                        Some(reg)
                            if !canonical_void(callee.return_type.as_ref())
                                && Some(reg_type(*reg)?) == callee.return_type.as_ref() =>
                        {
                            defs[index].push(*reg)
                        }
                        _ => return Err(fail("call result type mismatch")),
                    }
                    uses[index].extend(args);
                }
                TypedOp::Trap => {
                    next = false;
                }
                TypedOp::ReturnValue(value) => {
                    match value {
                        None if canonical_void(function.return_type.as_ref()) => {}
                        Some(reg)
                            if !canonical_void(function.return_type.as_ref())
                                && Some(reg_type(*reg)?) == function.return_type.as_ref() =>
                        {
                            uses[index].push(*reg)
                        }
                        _ => return Err(fail("return type mismatch")),
                    }
                    next = false;
                }
                TypedOp::Jump(label) => {
                    edges[index].push(target(label)?);
                    next = false;
                }
                TypedOp::BranchIf {
                    cond,
                    then_label,
                    else_label,
                } => {
                    if *reg_type(*cond)? != Typ::Bool {
                        return Err(fail("branch requires Bool"));
                    }
                    uses[index].push(*cond);
                    let yes = target(then_label)?;
                    let no = target(else_label)?;
                    match constants.get(cond) {
                        Some(true) => edges[index].push(yes),
                        Some(false) => edges[index].push(no),
                        None => edges[index].extend([yes, no]),
                    }
                    next = false;
                }
                _ => return Err(fail("ambiguous legacy typed operation")),
            },
            _ => return Err(fail("typed definition contains non-typed instruction")),
        }
        for reg in &defs[index] {
            reg_type(*reg)?;
            if !definitions.insert(*reg) {
                return Err(fail("value register defined more than once"));
            }
        }
        if next {
            edges[index].push(index + 1);
        }
    }
    // Must-defined state intersects at joins and reaches a fixed point before
    // checking uses. Entry parameter values are supplied by the scalar call ABI.
    type State = (HashSet<VReg>, HashSet<u32>);
    let mut incoming: Vec<Option<State>> = vec![None; n + 1];
    incoming[0] = Some(((0..parameters as u32).collect(), HashSet::new()));
    let mut queue = VecDeque::from([0]);
    while let Some(index) = queue.pop_front() {
        if index == n {
            continue;
        }
        let Some(mut state) = incoming[index].clone() else {
            continue;
        };
        state.0.extend(&defs[index]);
        state.1.extend(&writes[index]);
        for &edge in &edges[index] {
            let merged = match &incoming[edge] {
                None => state.clone(),
                Some(previous) => (
                    previous.0.intersection(&state.0).copied().collect(),
                    previous.1.intersection(&state.1).copied().collect(),
                ),
            };
            if incoming[edge].as_ref() != Some(&merged) {
                incoming[edge] = Some(merged);
                queue.push_back(edge);
            }
        }
    }
    for index in 0..n {
        if let Some((values, locals)) = &incoming[index] {
            if uses[index].iter().any(|reg| !values.contains(reg)) {
                return Err(fail("value used before definition on a reachable path"));
            }
            if reads[index].iter().any(|local| !locals.contains(local)) {
                return Err(fail("local read before initialization on a reachable path"));
            }
        }
    }
    if incoming[n].is_some() {
        return Err(fail("definition can fall through without a return"));
    }
    Ok(())
}
