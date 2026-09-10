//! Core IR → Thumb-2 lowering for freestanding Cortex-M scalar subset.
//!
//! Owned subset:
//! - Int/Bool locals and params (AAPCS r0-r3)
//! - return
//! - if/else
//! - while loops
//! - arithmetic: + - * & | ^ unary-
//! - compares: == != < <= > >=
//! - direct function calls (same module)
//! - MMIO memory ops: load8/16/32/64, store8/16/32/64 (volatile via plain ldr/str)
//!
//! No heap, no strings, no floats, no interrupts in this pass.

pub(crate) mod ctx;
pub(crate) mod expr;
pub(crate) mod stmt;

use crate::core_ir::{Decl, Typ, UnifiedModule};
use crate::native_emit::thumb::{self, CodeEmitter};
use crate::native_emit::thumb_lower::ctx::RelocKind;
use std::collections::HashMap;

pub const THUMB_TRIPLE: &str = "thumbv8m.main-none-eabi";

#[derive(Debug)]
pub struct ThumbCompileResult {
    pub code: Vec<u8>,
    pub entry_offset: u32,
    pub exports: Vec<(String, u32)>,
    pub externs: Vec<String>,
    /// Call sites that need linker relocation: (BL offset, symbol name).
    pub relocations: Vec<(u32, String)>,
    /// Function-address movw/movt pairs: (movw offset, movt offset, symbol).
    pub addr_relocs: Vec<(u32, u32, String)>,
}

pub fn lower_module(module: &UnifiedModule, entry: &str) -> Result<ThumbCompileResult, String> {
    let functions = collect_functions(module)?;
    let structs = collect_structs(module);
    if !functions.contains_key(entry) {
        return Err(format!("thumb-lower: entry `{entry}` not found"));
    }

    let mut emitter = CodeEmitter::new();
    let mut exports = Vec::new();
    let mut all_pending: Vec<ctx::PendingCall> = Vec::new();
    let mut offsets: HashMap<String, u32> = HashMap::new();

    // Stable emission order: entry first, then others alphabetically for determinism
    let mut names: Vec<String> = functions.keys().cloned().collect();
    names.sort();
    if let Some(pos) = names.iter().position(|n| n == entry) {
        let e = names.remove(pos);
        names.insert(0, e);
    }

    for name in &names {
        let func = functions.get(name).expect("name in map");
        // Extern declarations have no body and are not emitted here; they resolve
        // at link time via relocations.
        if func.body.is_empty() {
            continue;
        }
        let start = emitter.len();
        offsets.insert(name.clone(), start);
        exports.push((name.clone(), start));
        stmt::lower_function(&mut emitter, func, &functions, &structs, &mut all_pending)?;
        // ensure 2-byte alignment (always true for Thumb)
    }

    // Patch internal BL sites; collect extern calls and address refs for
    // linker relocation.
    let mut externs = Vec::new();
    let mut relocations = Vec::new();
    let mut addr_relocs = Vec::new();
    for call in &all_pending {
        match call.kind {
            RelocKind::MovwAbs => {
                // Function address loaded via movw/movt; linker patches both
                // halves. The symbol is added to externs if it's an extern fn.
                if call.is_extern && !externs.contains(&call.target) {
                    externs.push(call.target.clone());
                }
                addr_relocs.push((call.site, call.site2, call.target.clone()));
                continue;
            }
            RelocKind::Call => {}
        }
        if call.is_extern {
            if !externs.contains(&call.target) {
                externs.push(call.target.clone());
            }
            relocations.push((call.site, call.target.clone()));
            continue;
        }
        let Some(&target_off) = offsets.get(&call.target) else {
            return Err(format!("thumb-lower: unresolved call `{}`", call.target));
        };
        // BL is 4 bytes; PC for offset calc is address of next insn after BL = site + 4.
        let site = call.site as i32;
        let next = site + 4;
        let rel_bytes = target_off as i32 - next;
        if rel_bytes % 2 != 0 {
            return Err("thumb-lower: unaligned bl target".into());
        }
        let rel_half = rel_bytes / 2;
        let enc = thumb::bl_rel(rel_half)?;
        let hi = (enc >> 16) as u16;
        let lo = enc as u16;
        emitter.patch_u16(call.site, hi);
        emitter.patch_u16(call.site + 2, lo);
    }

    let entry_offset = *offsets.get(entry).ok_or_else(|| {
        format!("thumb-lower: entry `{entry}` offset not found (possibly an empty extern function)")
    })?;
    Ok(ThumbCompileResult {
        code: emitter.bytes,
        entry_offset,
        exports,
        externs,
        relocations,
        addr_relocs,
    })
}

fn collect_functions(module: &UnifiedModule) -> Result<HashMap<String, ctx::FunctionInfo>, String> {
    let mut out = HashMap::new();
    for decl in &module.decls {
        if let Decl::Function {
            name,
            params,
            ret,
            body,
            ..
        } = decl
        {
            out.insert(
                name.clone(),
                ctx::FunctionInfo {
                    name: name.clone(),
                    params: params.clone(),
                    ret: ret.canonical(),
                    body: body.clone(),
                },
            );
        }
    }
    if out.is_empty() {
        return Err("thumb-lower: module has no functions".into());
    }
    Ok(out)
}

fn collect_structs(module: &UnifiedModule) -> HashMap<String, Vec<(String, Typ)>> {
    let mut out = HashMap::new();
    for decl in &module.decls {
        if let Decl::Struct { name, fields, .. } = decl {
            out.insert(name.clone(), fields.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> UnifiedModule {
        crate::in_lang_parse::parse_in_source(src).expect("parse")
    }

    #[test]
    fn lower_simple_return() {
        let module = parse(
            r#"
fn answer() -> Int { return 42 }
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "answer").expect("lower");
        assert!(!result.code.is_empty());
        // movs r0, #42 = 0x202A, little-endian 2A 20
        assert!(result.code.windows(2).any(|w| w == [0x2A, 0x20]));
        // pop {r4-r7, pc} ends with 0xBDF0
        assert!(result.code.windows(2).any(|w| w == [0xF0, 0xBD]));
    }

    #[test]
    fn lower_more_than_four_params() {
        let module = parse(
            r#"
fn sum7(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int, g: Int) -> Int {
  return a + b + c + d + e + f + g
}
fn main() -> Int {
  return sum7(1, 2, 3, 4, 5, 6, 7)
}
"#,
        );
        let result = lower_module(&module, "main").expect("lower >4 params");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_add_params() {
        let module = parse(
            r#"
fn add(a: Int, b: Int) -> Int { return a + b }
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "add").expect("lower");
        assert!(!result.code.is_empty());
        // adds r0, r1, r2 encoding 0x1888? adds rd,rn,rm: 0001100 rm rn rd
        // We just require successful lower and some code size.
        assert!(result.code.len() > 8);
    }

    #[test]
    fn lower_call() {
        let module = parse(
            r#"
fn helper() -> Int { return 7 }
fn entry() -> Int { return helper() }
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "entry").expect("lower");
        // BL high halfword starts with 0xF0..
        assert!(result.code.windows(2).any(|w| w[1] == 0xF0 || w[0] == 0xF0));
        assert!(result.exports.iter().any(|(n, _)| n == "helper"));
        assert!(result.exports.iter().any(|(n, _)| n == "entry"));
    }

    #[test]
    fn lower_function_address_and_invoke() {
        let module = parse(
            r#"
fn helper() -> Int { return 7 }
fn entry() -> Int {
  let addr = helper
  let r = invoke1(addr, 0)
  return r
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "entry").expect("lower fn addr + invoke");
        // movw/movt pair for the function address.
        let pair = result
            .addr_relocs
            .iter()
            .find(|(_, _, n)| n == "helper")
            .expect("helper addr reloc");
        assert!(pair.0 < pair.1);
        // invoke1 → blx register (high byte 0x47) somewhere in the code.
        assert!(
            result
                .code
                .windows(2)
                .any(|w| w[1] == 0x47 && (w[0] & 0x80) != 0)
        );
        // Return value flows through r0.
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_extern_fn_address() {
        let module = parse(
            r#"
extern asm fn rtos_pendsv() -> void
fn entry() -> Int {
  let addr = rtos_pendsv
  return addr
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "entry").expect("lower extern fn addr");
        assert!(result.externs.contains(&"rtos_pendsv".to_string()));
        assert!(
            result
                .addr_relocs
                .iter()
                .any(|(_, _, n)| n == "rtos_pendsv")
        );
    }

    #[test]
    fn lower_if_else() {
        let module = parse(
            r#"
fn max(a: Int, b: Int) -> Int {
  if a > b {
    return a
  } else {
    return b
  }
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "max").expect("lower");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_while() {
        let module = parse(
            r#"
fn sum_to(n: Int) -> Int {
  let i = 0
  let acc = 0
  while i < n {
    acc = acc + i
    i = i + 1
  }
  return acc
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "sum_to").expect("lower");
        assert!(!result.code.is_empty());
        // Back-edge unconditional B must be a backward branch: either the
        // 16-bit form (0xExxx, imm11 > 0x400 for backward) or the 32-bit B.W
        // (low halfword 0x9xxx in memory).
        let has_backward_b = result.code.windows(2).any(|w| {
            let insn = u16::from_le_bytes([w[0], w[1]]);
            ((insn & 0xF800) == 0xE000 && (insn & 0x400) != 0) || ((w[1] & 0xF0) == 0x90)
        });
        assert!(has_backward_b, "while back-edge missing signed imm11 b");
    }

    #[test]
    fn lower_nested_while_if_return() {
        let module = parse(
            r#"
fn uart_put(ch: Int) -> void {
  let state = 1075838980
  let data = 1075838976
  let spins = 0
  while spins < 1000 {
    let s = load32(state)
    if (s & 1) == 0 {
      store32(data, ch)
      return
    }
    spins = spins + 1
  }
  store32(data, ch)
  return
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "uart_put").expect("lower");
        assert!(result.code.windows(2).any(|w| w == [0x20, 0x60]));
    }

    #[test]
    fn lower_short_circuit_and_or() {
        let module = parse(
            r#"
fn both(a: Int, b: Int) -> Int {
  if a > 0 && b > 0 {
    return 1
  }
  return 0
}
fn either(a: Int, b: Int) -> Int {
  if a > 0 || b > 0 {
    return 1
  }
  return 0
}
fn main() -> void { return }
"#,
        );
        let and_fn = lower_module(&module, "both").expect("lower &&");
        assert!(!and_fn.code.is_empty());
        let or_fn = lower_module(&module, "either").expect("lower ||");
        assert!(!or_fn.code.is_empty());
    }

    #[test]
    fn lower_break() {
        let module = parse(
            r#"
fn sum_until(max: Int) -> Int {
  let i = 0
  let acc = 0
  while i < max {
    if i == 5 {
      break
    }
    acc = acc + i
    i = i + 1
  }
  return acc
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "sum_until").expect("lower break");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_array_init_index_and_assign() {
        let module = parse(
            r#"
fn sum() -> Int {
  let a: [Int] = [10, 20, 30]
  a[1] = a[0] + a[1]
  return a[1]
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "sum").expect("lower array");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_array_with_call_item_and_nested_index() {
        let module = parse(
            r#"
extern zig fn helper(x: Int) -> Int
fn sum() -> Int {
  let a: [Int] = [helper(1), helper(2), helper(3)]
  let b: [Int] = [1, 2]
  return a[b[0]]
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "sum").expect("lower array with nested index");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_struct_init_and_field() {
        let module = parse(
            r#"
struct Point {
  Int x
  Int y
}
fn sum() -> Int {
  let p: Point = Point { x: 3, y: 4 }
  p.x = p.x + 1
  return p.x + p.y
}
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "sum").expect("lower struct");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn lower_extern_call() {
        let module = parse(
            r#"
extern zig fn helper(x: Int) -> Int
fn main() -> Int {
  return helper(7)
}
"#,
        );
        let result = lower_module(&module, "main").expect("lower extern");
        assert!(!result.code.is_empty());
        assert!(result.relocations.iter().any(|(_, s)| s == "helper"));
    }

    #[test]
    fn lower_extern_any_language_tag() {
        let module = parse(
            r#"
extern c fn c_helper() -> Int
extern rust fn rust_helper() -> Int
extern go fn go_helper() -> Int
extern v fn v_helper() -> Int
fn main() -> Int {
  return c_helper() + rust_helper() + go_helper() + v_helper()
}
"#,
        );
        let result = lower_module(&module, "main").expect("lower extern tags");
        assert_eq!(result.relocations.len(), 4);
    }

    #[test]
    fn rejects_string() {
        let module = parse(
            r#"
fn f() -> Int {
  let s = "hi"
  return 0
}
fn main() -> void { return }
"#,
        );
        let err = lower_module(&module, "f").expect_err("string");
        assert!(err.contains("string") || err.contains("unsupported"));
    }

    #[test]
    fn lower_load32_store32() {
        let module = parse(
            r#"
fn peek(addr: Int) -> Int { return load32(addr) }
fn poke(addr: Int, val: Int) -> void { store32(addr, val); return }
fn main() -> void { return }
"#,
        );
        let load = lower_module(&module, "peek").expect("load32");
        assert!(load.code.windows(2).any(|w| w == [0x08, 0x68]));
        let store = lower_module(&module, "poke").expect("store32");
        // str rt,[rn,#0] with rn=r4: 0x6020 (rt=0, rn=4)
        assert!(
            store.code.windows(2).any(|w| w == [0x20, 0x60]),
            "store32 should str via r4 base: {:?}",
            store.code
        );
    }

    #[test]
    fn lower_load8_store8() {
        let module = parse(
            r#"
fn peek8(addr: Int) -> Int { return load8(addr) }
fn poke8(addr: Int, val: Int) -> void { store8(addr, val); return }
fn main() -> void { return }
"#,
        );
        let load = lower_module(&module, "peek8").expect("load8");
        assert!(load.code.windows(2).any(|w| w == [0x08, 0x78]));
        let store = lower_module(&module, "poke8").expect("store8");
        assert!(store.code.windows(2).any(|w| w == [0x20, 0x70]));
    }

    #[test]
    fn lower_load16_store16() {
        let module = parse(
            r#"
fn peek16(addr: Int) -> Int { return load16(addr) }
fn poke16(addr: Int, val: Int) -> void { store16(addr, val); return }
fn main() -> void { return }
"#,
        );
        let load = lower_module(&module, "peek16").expect("load16");
        assert!(load.code.windows(2).any(|w| w == [0x08, 0x88]));
        let store = lower_module(&module, "poke16").expect("store16");
        assert!(store.code.windows(2).any(|w| w == [0x20, 0x80]));
    }

    #[test]
    fn mmio_arg_count_errors() {
        let module = parse(
            r#"
fn bad() -> Int { return load32() }
fn main() -> void { return }
"#,
        );
        let err = lower_module(&module, "bad").expect_err("arity");
        assert!(err.contains("load32"));
    }

    #[test]
    fn rejects_empty() {
        let module = UnifiedModule::new(Vec::new());
        assert!(lower_module(&module, "main").is_err());
    }

    #[test]
    fn entry_offset_valid() {
        let module = parse(
            r#"
fn answer() -> Int { return 1 }
fn main() -> void { return }
"#,
        );
        let result = lower_module(&module, "answer").expect("lower");
        assert!(result.entry_offset < result.code.len() as u32);
    }
}
