//! Private ISA (`INISA`) for the harden emit profile.
//!
//! Harden artifacts bundle this XOR-scrambled stack ISA inside a Linux ELF
//! interpreter stub (`inisa_bundle`). The program is INISA; the stub is host
//! machine code so `./artifact` runs. Stock Ghidra has no INISA language.
//!
//! Layout after the 32-byte SCI manifest:
//! ```text
//! magic[8] = INISA_MAGIC
//! version u32, nfuncs u32, code_off u32, code_len u32,
//! str_off u32, str_len u32, entry_fn u32
//! func table: nfuncs × { name_off u32, name_len u32, pc u32, nlocals u16, nargs u16 }
//! strings, then XOR-scrambled code
//! ```

use crate::core_ir::{Decl, Expr, LoopKind, Stmt, UnifiedModule};
use crate::native_emit::sci::{SCI_INISA_MAGIC, SCI_MANIFEST_SIZE};

pub const INISA_MAGIC: u64 = 0x3141_5349_4e49_0001; // "\x01\0INISA1" le-ish unique
pub const INISA_VERSION: u32 = 1;

const OP_HALT: u8 = 0x00;
const OP_PUSHI: u8 = 0x01;
const OP_LOAD: u8 = 0x02;
const OP_STORE: u8 = 0x03;
const OP_ADD: u8 = 0x04;
const OP_SUB: u8 = 0x05;
const OP_MUL: u8 = 0x06;
const OP_DIV: u8 = 0x07;
const OP_MOD: u8 = 0x08;
const OP_XOR: u8 = 0x09;
const OP_AND: u8 = 0x0A;
const OP_OR: u8 = 0x0B;
const OP_SHL: u8 = 0x0C;
const OP_SHR: u8 = 0x0D;
const OP_NEG: u8 = 0x0E;
const OP_NOT: u8 = 0x0F;
const OP_EQ: u8 = 0x10;
const OP_NE: u8 = 0x11;
const OP_LT: u8 = 0x12;
const OP_LE: u8 = 0x13;
const OP_GT: u8 = 0x14;
const OP_GE: u8 = 0x15;
const OP_LAND: u8 = 0x16;
const OP_LOR: u8 = 0x17;
const OP_JMP: u8 = 0x18;
const OP_JZ: u8 = 0x19;
const OP_CALL: u8 = 0x1A;
const OP_RET: u8 = 0x1B;
const OP_POP: u8 = 0x1C;

/// SCI image wrapping an INISA program (harden distribution format).
pub fn emit_sci_inisa(module: &UnifiedModule, entry: &str) -> Result<Vec<u8>, String> {
    let prog = compile(module, entry)?;
    let payload = serialize(&prog);
    let image_size = SCI_MANIFEST_SIZE + payload.len();
    let mut image = Vec::with_capacity(image_size);
    image.extend_from_slice(&SCI_INISA_MAGIC.to_le_bytes());
    image.extend_from_slice(&0u64.to_le_bytes()); // required caps
    image.extend_from_slice(&(SCI_MANIFEST_SIZE as u64).to_le_bytes()); // entry = payload
    image.extend_from_slice(&(image_size as u64).to_le_bytes());
    image.extend_from_slice(&payload);
    Ok(image)
}

pub fn is_sci_inisa(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..8] == SCI_INISA_MAGIC.to_le_bytes()
}

/// Interpret an INISA program (from a module, not a blob) and return the entry result.
pub fn eval_module(module: &UnifiedModule, entry: &str) -> Result<i64, String> {
    let prog = compile(module, entry)?;
    interpret(&prog)
}

struct Func {
    name: String,
    nargs: u16,
    nlocals: u16,
    pc: u32,
}

pub(crate) struct Program {
    funcs: Vec<Func>,
    code: Vec<u8>,
    entry_fn: u32,
}

struct Compiler {
    funcs: Vec<Func>,
    code: Vec<u8>,
    fn_index: std::collections::HashMap<String, u32>,
}

pub(crate) fn compile_program(module: &UnifiedModule, entry: &str) -> Result<Program, String> {
    compile(module, entry)
}

pub(crate) fn serialize_program(prog: &Program) -> Vec<u8> {
    serialize(prog)
}

fn compile(module: &UnifiedModule, entry: &str) -> Result<Program, String> {
    let mut fn_index = std::collections::HashMap::new();
    let mut decls: Vec<&Decl> = Vec::new();
    for d in &module.decls {
        if let Decl::Function { name, body, .. } = d {
            if body.is_empty() {
                continue;
            }
            let idx = decls.len() as u32;
            fn_index.insert(name.clone(), idx);
            decls.push(d);
        }
    }
    if decls.is_empty() {
        return Err("inisa: module has no functions".into());
    }
    let entry_fn = *fn_index
        .get(entry)
        .ok_or_else(|| format!("inisa: missing entry `{entry}`"))?;

    let mut c = Compiler {
        funcs: Vec::new(),
        code: Vec::new(),
        fn_index,
    };
    for d in decls {
        compile_function(&mut c, d)?;
    }
    Ok(Program {
        funcs: c.funcs,
        code: c.code,
        entry_fn,
    })
}

fn compile_function(c: &mut Compiler, decl: &Decl) -> Result<(), String> {
    let Decl::Function {
        name, params, body, ..
    } = decl
    else {
        return Ok(());
    };
    let pc = c.code.len() as u32;
    let mut slots: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    for (i, (pname, _)) in params.iter().enumerate() {
        slots.insert(pname.clone(), i as u16);
    }
    let nargs = params.len() as u16;
    collect_locals(body, &mut slots, nargs);
    let nlocals = slots.len() as u16;
    compile_stmts(c, body, &slots, None)?;
    // Fall through: return 0
    emit_pushi(&mut c.code, 0);
    c.code.push(OP_RET);

    c.funcs.push(Func {
        name: name.clone(),
        nargs,
        nlocals,
        pc,
    });
    Ok(())
}

fn collect_locals(stmts: &[Stmt], slots: &mut std::collections::HashMap<String, u16>, start: u16) {
    let mut next = slots.len() as u16;
    if next < start {
        next = start;
    }
    fn walk(stmts: &[Stmt], slots: &mut std::collections::HashMap<String, u16>, next: &mut u16) {
        for s in stmts {
            match s {
                Stmt::Let(name, _, _) => {
                    if !slots.contains_key(name) {
                        slots.insert(name.clone(), *next);
                        *next += 1;
                    }
                }
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    walk(then_body, slots, next);
                    walk(else_body, slots, next);
                }
                Stmt::Loop { body, .. } => walk(body, slots, next),
                Stmt::Try { body, catches } => {
                    walk(body, slots, next);
                    for arm in catches {
                        walk(&arm.body, slots, next);
                    }
                }
                Stmt::Match { arms, .. } => {
                    for arm in arms {
                        walk(&arm.body, slots, next);
                    }
                }
                _ => {}
            }
        }
    }
    walk(stmts, slots, &mut next);
}

fn compile_stmts(
    c: &mut Compiler,
    stmts: &[Stmt],
    slots: &std::collections::HashMap<String, u16>,
    loop_end: Option<usize>,
) -> Result<(), String> {
    for s in stmts {
        compile_stmt(c, s, slots, loop_end)?;
    }
    Ok(())
}

fn compile_stmt(
    c: &mut Compiler,
    stmt: &Stmt,
    slots: &std::collections::HashMap<String, u16>,
    loop_end: Option<usize>,
) -> Result<(), String> {
    match stmt {
        Stmt::Let(name, _, e) | Stmt::Assign(name, e) => {
            compile_expr(c, e, slots)?;
            let slot = *slots
                .get(name)
                .ok_or_else(|| format!("inisa: unknown local `{name}`"))?;
            c.code.push(OP_STORE);
            c.code.extend_from_slice(&slot.to_le_bytes());
        }
        Stmt::Return(Some(e)) => {
            compile_expr(c, e, slots)?;
            c.code.push(OP_RET);
        }
        Stmt::Return(None) => {
            emit_pushi(&mut c.code, 0);
            c.code.push(OP_RET);
        }
        Stmt::Expr(e) => {
            compile_expr(c, e, slots)?;
            c.code.push(OP_POP);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
        } => {
            compile_expr(c, cond, slots)?;
            c.code.push(OP_JZ);
            let jz = c.code.len();
            c.code.extend_from_slice(&0i32.to_le_bytes());
            compile_stmts(c, then_body, slots, loop_end)?;
            c.code.push(OP_JMP);
            let jmp = c.code.len();
            c.code.extend_from_slice(&0i32.to_le_bytes());
            let else_pc = c.code.len() as i32;
            patch_i32(&mut c.code, jz, else_pc);
            compile_stmts(c, else_body, slots, loop_end)?;
            let end = c.code.len() as i32;
            patch_i32(&mut c.code, jmp, end);
        }
        Stmt::Loop { kind, cond, body } => {
            // Range/array fors are desugared to while; leftover For still loops.
            let head = c.code.len() as i32;
            let jz_at = if let Some(cond) = cond {
                compile_expr(c, cond, slots)?;
                c.code.push(OP_JZ);
                let at = c.code.len();
                c.code.extend_from_slice(&0i32.to_le_bytes());
                Some(at)
            } else {
                None
            };
            compile_stmts(c, body, slots, Some(0))?;
            c.code.push(OP_JMP);
            c.code.extend_from_slice(&head.to_le_bytes());
            let end = c.code.len() as i32;
            if let Some(at) = jz_at {
                patch_i32(&mut c.code, at, end);
            }
            let _ = loop_end;
        }
        Stmt::Break => {
            if let Some(end) = loop_end {
                c.code.push(OP_JMP);
                c.code.extend_from_slice(&(end as i32).to_le_bytes());
            }
        }
        Stmt::Continue => {
            if let Some(end) = loop_end {
                let _ = end;
            }
        }
        Stmt::Throw(e) => {
            compile_expr(c, e, slots)?;
            c.code.push(OP_POP);
        }
        Stmt::Try { body, catches } => {
            compile_stmts(c, body, slots, loop_end)?;
            for arm in catches {
                compile_stmts(c, &arm.body, slots, loop_end)?;
            }
        }
        Stmt::Match { scrutinee, arms } => {
            compile_expr(c, scrutinee, slots)?;
            c.code.push(OP_POP);
            for arm in arms {
                compile_stmts(c, &arm.body, slots, loop_end)?;
            }
        }
        Stmt::Propagate => {}
        Stmt::IndexAssign { base, index, value } => {
            compile_expr(c, base, slots)?;
            c.code.push(OP_POP);
            compile_expr(c, index, slots)?;
            c.code.push(OP_POP);
            compile_expr(c, value, slots)?;
            c.code.push(OP_POP);
        }
        Stmt::FieldAssign { base, value, .. } => {
            compile_expr(c, base, slots)?;
            c.code.push(OP_POP);
            compile_expr(c, value, slots)?;
            c.code.push(OP_POP);
        }
    }
    Ok(())
}

fn compile_expr(
    c: &mut Compiler,
    e: &Expr,
    slots: &std::collections::HashMap<String, u16>,
) -> Result<(), String> {
    match e {
        Expr::IntLit(v) => emit_pushi(&mut c.code, *v),
        Expr::BoolLit(b) => emit_pushi(&mut c.code, i64::from(*b)),
        Expr::Ident(name) => {
            if let Some(&slot) = slots.get(name) {
                c.code.push(OP_LOAD);
                c.code.extend_from_slice(&slot.to_le_bytes());
            } else if let Some(&idx) = c.fn_index.get(name) {
                emit_pushi(&mut c.code, idx as i64);
            } else {
                // Rust-front leftovers (iterator adapters, unresolved paths).
                emit_pushi(&mut c.code, 0);
            }
        }
        Expr::Unary { op, expr } => {
            compile_expr(c, expr, slots)?;
            match op.as_str() {
                "-" | "neg" => c.code.push(OP_NEG),
                "!" | "not" => c.code.push(OP_NOT),
                "~" => {
                    emit_pushi(&mut c.code, -1);
                    c.code.push(OP_XOR);
                }
                _ => return Err(format!("inisa: unary `{op}`")),
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            compile_expr(c, lhs, slots)?;
            compile_expr(c, rhs, slots)?;
            let opc = match op.as_str() {
                "+" | "add" | "+=" => OP_ADD,
                "-" | "sub" | "-=" => OP_SUB,
                "*" | "mul" | "*=" => OP_MUL,
                "/" | "div" | "/=" => OP_DIV,
                "%" | "mod" => OP_MOD,
                "^" => OP_XOR,
                "&" => OP_AND,
                "|" => OP_OR,
                "<<" => OP_SHL,
                ">>" => OP_SHR,
                "==" | "=" => OP_EQ,
                "!=" => OP_NE,
                "<" => OP_LT,
                "<=" => OP_LE,
                ">" => OP_GT,
                ">=" => OP_GE,
                "&&" => OP_LAND,
                "||" => OP_LOR,
                _ => return Err(format!("inisa: binary `{op}`")),
            };
            c.code.push(opc);
        }
        Expr::Call { callee, args } => {
            let Expr::Ident(fname) = callee.as_ref() else {
                compile_expr(c, callee, slots)?;
                c.code.push(OP_POP);
                for a in args {
                    compile_expr(c, a, slots)?;
                    c.code.push(OP_POP);
                }
                emit_pushi(&mut c.code, 0);
                return Ok(());
            };
            match c.fn_index.get(fname).copied() {
                Some(idx) => {
                    for a in args {
                        compile_expr(c, a, slots)?;
                    }
                    c.code.push(OP_CALL);
                    c.code.extend_from_slice(&idx.to_le_bytes());
                    c.code.push(args.len() as u8);
                }
                None => {
                    for a in args {
                        compile_expr(c, a, slots)?;
                        c.code.push(OP_POP);
                    }
                    emit_pushi(&mut c.code, 0);
                }
            }
        }
        Expr::FloatLit(v) => emit_pushi(&mut c.code, v.0 as i64),
        Expr::StringLit(_) => emit_pushi(&mut c.code, 0),
        Expr::ArrayLit(items) => {
            emit_pushi(&mut c.code, items.len() as i64);
        }
        Expr::Index { base, index } => {
            if let (Expr::ArrayLit(items), Expr::IntLit(i)) = (base.as_ref(), index.as_ref()) {
                if *i >= 0 {
                    let idx = *i as usize;
                    if idx < items.len() {
                        return compile_expr(c, &items[idx], slots);
                    }
                }
            }
            compile_expr(c, base, slots)?;
            c.code.push(OP_POP);
            compile_expr(c, index, slots)?;
        }
        Expr::Field { base, .. } => compile_expr(c, base, slots)?,
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                compile_expr(c, e, slots)?;
                c.code.push(OP_POP);
            }
            emit_pushi(&mut c.code, 0);
        }
        Expr::Closure { body, .. } => {
            compile_stmts(c, body, slots, None)?;
            emit_pushi(&mut c.code, 0);
        }
    }
    Ok(())
}

fn emit_pushi(code: &mut Vec<u8>, v: i64) {
    code.push(OP_PUSHI);
    code.extend_from_slice(&v.to_le_bytes());
}

fn patch_i32(code: &mut [u8], at: usize, abs_pc: i32) {
    code[at..at + 4].copy_from_slice(&abs_pc.to_le_bytes());
}

fn scramble(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key_at(i))
        .collect()
}

fn key_at(i: usize) -> u8 {
    0xA5u8
        .wrapping_add(i as u8)
        .wrapping_mul(0x1D)
        .wrapping_add(0x3C)
}

fn serialize(prog: &Program) -> Vec<u8> {
    let nfuncs = prog.funcs.len() as u32;
    let header_len = 8 + 4 * 7;
    let func_tab_len = nfuncs as usize * 16;
    let str_off = header_len + func_tab_len;
    let mut strings = Vec::new();
    let mut func_recs = Vec::new();
    for f in &prog.funcs {
        let off = strings.len() as u32;
        strings.extend_from_slice(f.name.as_bytes());
        strings.push(0);
        func_recs.push((off, f.name.len() as u32, f.pc, f.nlocals, f.nargs));
    }
    let code_off = str_off + strings.len();
    let scrambled = scramble(&prog.code);

    let mut out = Vec::new();
    out.extend_from_slice(&INISA_MAGIC.to_le_bytes());
    out.extend_from_slice(&INISA_VERSION.to_le_bytes());
    out.extend_from_slice(&nfuncs.to_le_bytes());
    out.extend_from_slice(&(code_off as u32).to_le_bytes());
    out.extend_from_slice(&(scrambled.len() as u32).to_le_bytes());
    out.extend_from_slice(&(str_off as u32).to_le_bytes());
    out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    out.extend_from_slice(&prog.entry_fn.to_le_bytes());
    for (off, len, pc, nlocals, nargs) in func_recs {
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&pc.to_le_bytes());
        out.extend_from_slice(&nlocals.to_le_bytes());
        out.extend_from_slice(&nargs.to_le_bytes());
    }
    out.extend_from_slice(&strings);
    out.extend_from_slice(&scrambled);
    debug_assert_eq!(out.len(), code_off + scrambled.len());
    out
}

struct Frame {
    locals: Vec<i64>,
    ret_pc: usize,
}

fn interpret(prog: &Program) -> Result<i64, String> {
    let mut stack: Vec<i64> = Vec::new();
    let mut frames: Vec<Frame> = Vec::new();
    let entry = &prog.funcs[prog.entry_fn as usize];
    frames.push(Frame {
        locals: vec![0; entry.nlocals as usize],
        ret_pc: usize::MAX,
    });
    let mut pc = entry.pc as usize;
    let code = &prog.code;
    let mut steps = 0u32;
    loop {
        steps += 1;
        if steps > 2_000_000 {
            return Err("inisa: step limit".into());
        }
        if pc >= code.len() {
            return Err("inisa: pc oob".into());
        }
        let op = code[pc];
        pc += 1;
        match op {
            OP_HALT => return Ok(stack.pop().unwrap_or(0)),
            OP_PUSHI => {
                let v = i64::from_le_bytes(code[pc..pc + 8].try_into().unwrap());
                pc += 8;
                stack.push(v);
            }
            OP_LOAD => {
                let slot = u16::from_le_bytes(code[pc..pc + 2].try_into().unwrap()) as usize;
                pc += 2;
                let v = *frames
                    .last()
                    .unwrap()
                    .locals
                    .get(slot)
                    .ok_or("inisa: load oob")?;
                stack.push(v);
            }
            OP_STORE => {
                let slot = u16::from_le_bytes(code[pc..pc + 2].try_into().unwrap()) as usize;
                pc += 2;
                let v = stack.pop().ok_or("inisa: store empty")?;
                let loc = frames
                    .last_mut()
                    .unwrap()
                    .locals
                    .get_mut(slot)
                    .ok_or("inisa: store oob")?;
                *loc = v;
            }
            OP_ADD => bin(&mut stack, |a, b| a.wrapping_add(b))?,
            OP_SUB => bin(&mut stack, |a, b| a.wrapping_sub(b))?,
            OP_MUL => bin(&mut stack, |a, b| a.wrapping_mul(b))?,
            OP_DIV => bin(&mut stack, |a, b| if b == 0 { 0 } else { a / b })?,
            OP_MOD => bin(&mut stack, |a, b| if b == 0 { 0 } else { a % b })?,
            OP_XOR => bin(&mut stack, |a, b| a ^ b)?,
            OP_AND => bin(&mut stack, |a, b| a & b)?,
            OP_OR => bin(&mut stack, |a, b| a | b)?,
            OP_SHL => bin(&mut stack, |a, b| a.wrapping_shl((b as u32) & 63))?,
            OP_SHR => bin(&mut stack, |a, b| ((a as u64) >> ((b as u32) & 63)) as i64)?,
            OP_NEG => {
                let a = stack.pop().ok_or("inisa: neg")?;
                stack.push(a.wrapping_neg());
            }
            OP_NOT => {
                let a = stack.pop().ok_or("inisa: not")?;
                stack.push(if a == 0 { 1 } else { 0 });
            }
            OP_EQ => bin(&mut stack, |a, b| i64::from(a == b))?,
            OP_NE => bin(&mut stack, |a, b| i64::from(a != b))?,
            OP_LT => bin(&mut stack, |a, b| i64::from(a < b))?,
            OP_LE => bin(&mut stack, |a, b| i64::from(a <= b))?,
            OP_GT => bin(&mut stack, |a, b| i64::from(a > b))?,
            OP_GE => bin(&mut stack, |a, b| i64::from(a >= b))?,
            OP_LAND => bin(&mut stack, |a, b| i64::from(a != 0 && b != 0))?,
            OP_LOR => bin(&mut stack, |a, b| i64::from(a != 0 || b != 0))?,
            OP_JMP => {
                pc = i32::from_le_bytes(code[pc..pc + 4].try_into().unwrap()) as usize;
            }
            OP_JZ => {
                let target = i32::from_le_bytes(code[pc..pc + 4].try_into().unwrap()) as usize;
                pc += 4;
                let v = stack.pop().ok_or("inisa: jz")?;
                if v == 0 {
                    pc = target;
                }
            }
            OP_CALL => {
                let idx = u32::from_le_bytes(code[pc..pc + 4].try_into().unwrap()) as usize;
                pc += 4;
                let argc = code[pc] as usize;
                pc += 1;
                let callee = prog.funcs.get(idx).ok_or("inisa: bad call")?;
                let mut locals = vec![0i64; callee.nlocals.max(callee.nargs) as usize];
                for i in (0..argc).rev() {
                    let v = stack.pop().ok_or("inisa: call arg")?;
                    if i < locals.len() {
                        locals[i] = v;
                    }
                }
                frames.push(Frame { locals, ret_pc: pc });
                pc = callee.pc as usize;
            }
            OP_RET => {
                let v = stack.pop().unwrap_or(0);
                let frame = frames.pop().ok_or("inisa: ret empty")?;
                if frames.is_empty() {
                    return Ok(v);
                }
                stack.push(v);
                pc = frame.ret_pc;
            }
            OP_POP => {
                stack.pop();
            }
            other => return Err(format!("inisa: bad opcode {other:#x}")),
        }
    }
}

fn bin(stack: &mut Vec<i64>, f: impl FnOnce(i64, i64) -> i64) -> Result<(), String> {
    let b = stack.pop().ok_or("inisa: bin rhs")?;
    let a = stack.pop().ok_or("inisa: bin lhs")?;
    stack.push(f(a, b));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{Decl, Expr, Stmt, Typ};

    fn module_add() -> UnifiedModule {
        UnifiedModule::new(vec![
            Decl::Function {
                name: "add".into(),
                params: vec![("a".into(), Typ::Int), ("b".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Binary {
                    op: "+".into(),
                    lhs: Box::new(Expr::Ident("a".into())),
                    rhs: Box::new(Expr::Ident("b".into())),
                }))],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("add".into())),
                    args: vec![Expr::IntLit(40), Expr::IntLit(2)],
                }))],
                type_params: vec![],
            },
        ])
    }

    #[test]
    fn eval_add_entry() {
        assert_eq!(eval_module(&module_add(), "main").unwrap(), 42);
    }

    #[test]
    fn sci_inisa_is_not_elf_and_holds_names() {
        let img = emit_sci_inisa(&module_add(), "main").unwrap();
        assert!(is_sci_inisa(&img));
        assert_ne!(&img[..4], b"\x7fELF");
        let s = String::from_utf8_lossy(&img);
        assert!(s.contains("main"));
        assert!(s.contains("add"));
    }

    #[test]
    fn scrambled_code_is_not_raw_opcodes() {
        let prog = compile(&module_add(), "main").unwrap();
        let ser = serialize(&prog);
        assert!(
            !ser.windows(prog.code.len())
                .any(|w| w == prog.code.as_slice()),
            "plaintext ISA must not appear in the SCI payload"
        );
    }

    #[test]
    fn harden_ir_sample_still_evals() {
        let mut decls = vec![
            Decl::Function {
                name: "mix".into(),
                params: vec![("a".into(), Typ::Int), ("b".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let(
                        "s".into(),
                        Some(Typ::Int),
                        Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("a".into())),
                            rhs: Box::new(Expr::Ident("b".into())),
                        },
                    ),
                    Stmt::Let(
                        "t".into(),
                        Some(Typ::Int),
                        Expr::Binary {
                            op: "*".into(),
                            lhs: Box::new(Expr::Ident("s".into())),
                            rhs: Box::new(Expr::IntLit(3)),
                        },
                    ),
                    Stmt::Return(Some(Expr::Binary {
                        op: "-".into(),
                        lhs: Box::new(Expr::Ident("t".into())),
                        rhs: Box::new(Expr::Ident("a".into())),
                    })),
                ],
                type_params: vec![],
            },
            Decl::Function {
                name: "gate".into(),
                params: vec![("n".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![Stmt::If {
                    cond: Expr::Binary {
                        op: ">".into(),
                        lhs: Box::new(Expr::Ident("n".into())),
                        rhs: Box::new(Expr::IntLit(0)),
                    },
                    then_body: vec![Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident("mix".into())),
                        args: vec![Expr::Ident("n".into()), Expr::IntLit(2)],
                    }))],
                    else_body: vec![Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident("mix".into())),
                        args: vec![Expr::IntLit(1), Expr::Ident("n".into())],
                    }))],
                }],
                type_params: vec![],
            },
            Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![Stmt::Return(Some(Expr::Call {
                    callee: Box::new(Expr::Ident("gate".into())),
                    args: vec![Expr::IntLit(7)],
                }))],
                type_params: vec![],
            },
        ];
        crate::core_opt::optimize_with_profile(
            &mut decls,
            Some("main"),
            crate::emit_profile::EmitProfile::Harden,
        );
        let module = UnifiedModule::new(decls);
        assert_eq!(eval_module(&module, "main").unwrap(), 20);
    }
}
