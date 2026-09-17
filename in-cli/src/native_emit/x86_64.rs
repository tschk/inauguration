//! x86_64 instruction encoder for the owned native subset.
//!
//! Produces byte sequences for a minimal x86_64 scalar-function subset.
//! Uses Intel SDM operand encoding (ModRM, SIB, REX, immediates).

// ── Register constants ──────────────────────────────────────────

pub const RAX: u8 = 0;
pub const RCX: u8 = 1;
pub const RDX: u8 = 2;
pub const RBX: u8 = 3;
pub const RSP: u8 = 4;
pub const RBP: u8 = 5;
pub const RSI: u8 = 6;
pub const RDI: u8 = 7;
pub const R8: u8 = 8;
pub const R9: u8 = 9;
pub const R10: u8 = 10;
pub const R11: u8 = 11;
pub const R12: u8 = 12;
pub const R13: u8 = 13;
pub const R14: u8 = 14;
pub const R15: u8 = 15;
pub const REG_SP: u8 = RSP;
pub const REG_FP: u8 = RBP;
pub const REG_XZR: u8 = 0; // alias for zero idiom (xor same)
pub const REG_RET: u8 = RAX;

thread_local! {
    pub static TL_IS_32BIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn is_32bit() -> bool {
    TL_IS_32BIT.with(|cell| cell.get())
}

pub fn set_32bit(enabled: bool) {
    TL_IS_32BIT.with(|cell| cell.set(enabled));
}

// ── CodeEmitter ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct CodeEmitter {
    pub bytes: Vec<u8>,
}

impl CodeEmitter {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn len(&self) -> u32 {
        self.bytes.len() as u32
    }

    pub fn emit_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn emit_u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn emit_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn emit_u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn emit_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub fn emit_insns(&mut self, insns: &[u8]) {
        self.bytes.extend_from_slice(insns);
    }

    /// Emit a 32-bit insn, return its offset.
    pub fn emit_insn(&mut self, insn: u32) -> u32 {
        let offset = self.len();
        self.emit_u32(insn);
        offset
    }

    /// Patch a previously emitted 32-bit value at `offset`.
    pub fn patch_u32(&mut self, offset: u32, value: u32) {
        let bytes = value.to_le_bytes();
        self.bytes[offset as usize..offset as usize + 4].copy_from_slice(&bytes);
    }

    /// Patch a byte at `offset`.
    pub fn patch_u8(&mut self, offset: u32, value: u8) {
        self.bytes[offset as usize] = value;
    }

    /// Emit a 32-bit or 64-bit encoding depending on the current mode.
    pub fn emit_width(&mut self, bytes_32: &[u8], bytes_64: &[u8]) {
        if is_32bit() {
            self.emit_insns(bytes_32);
        } else {
            self.emit_insns(bytes_64);
        }
    }
}

// ── REX prefix helpers ──────────────────────────────────────────
// In 32-bit protected mode REX prefixes are illegal and must be omitted.

fn push_rex_w(code: &mut Vec<u8>, rm: u8) {
    if !is_32bit() {
        code.push(0x48 | (if rm >= 8 { 1 } else { 0 }));
    }
}

fn push_rex_wr(code: &mut Vec<u8>, reg: u8, rm: u8) {
    if !is_32bit() {
        code.push(0x48 | (if reg >= 8 { 1 << 2 } else { 0 }) | (if rm >= 8 { 1 } else { 0 }));
    }
}

// ── ModRM byte ──────────────────────────────────────────────────

fn modrm(mod_: u8, reg: u8, rm: u8) -> u8 {
    (mod_ << 6) | ((reg & 7) << 3) | (rm & 7)
}

// ── Instructions ────────────────────────────────────────────────

/// ret (near return)
pub fn ret() -> Vec<u8> {
    vec![0xC3]
}

/// push r/m32 or r/m64
pub fn push_r(reg: u8) -> Vec<u8> {
    if reg < 8 {
        vec![0x50 + reg]
    } else if is_32bit() {
        // r8-r15 do not exist in protected mode; ignore rather than emit REX.
        Vec::new()
    } else {
        vec![0x41, 0x50 + (reg - 8)]
    }
}

/// pop r/m32 or r/m64
pub fn pop_r(reg: u8) -> Vec<u8> {
    if reg < 8 {
        vec![0x58 + reg]
    } else if is_32bit() {
        Vec::new()
    } else {
        vec![0x41, 0x58 + (reg - 8)]
    }
}

/// mov r32/r64, imm32/imm64  (REX.W + B8+rd io)
pub fn mov_ri64(reg: u8, imm: i64) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, reg);
    code.push(0xB8 + (reg & 7));
    if is_32bit() {
        code.extend_from_slice(&(imm as i32).to_le_bytes());
    } else {
        code.extend_from_slice(&imm.to_le_bytes());
    }
    code
}

/// mov r/m32/r/m64, r32/r64  (REX.W + 89 /r, reg=src, rm=dst)
pub fn mov_rr(dst: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, src, dst);
    code.push(0x89);
    code.push(modrm(3, src & 7, dst & 7));
    code
}

/// mov r/m32/r/m64, r32/r64  (REX.W + 89 /r) — alias for mov_rr
pub fn mov_mr(dst: u8, src: u8) -> Vec<u8> {
    mov_rr(dst, src)
}

/// mov r32/r64, [r/m32/r/m64 + disp]  (REX.W + 8B /r with ModRM)
/// Emit a SIB byte for [RSP + disp] addressing when base is RSP (4) or RBP (5).
/// Returns None if no SIB is needed.
fn sib_for_base(base: u8) -> Option<u8> {
    // RSP (4) and RBP (5) with Mod=01 or Mod=10 require a SIB byte.
    // For RSP: SIB = 0x24 (scale=0, index=4=no-index, base=4=RSP)
    if base == RSP {
        Some(0x24)
    } else if base == RBP {
        Some(0x25)
    }
    // [RBP + disp] uses SIB with index=4, base=5
    else {
        None
    }
}

/// mov [r/m32/r/m64 + disp], r32/r64  (REX.W + 89 /r with ModRM)
/// When base is RSP (4), a SIB byte must follow ModRM.
pub fn mov_m_r(base: u8, disp: i32, reg: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, base);
    code.push(0x89);
    // SIB needed only for RSP (rm=4 always requires SIB). RBP with Mod=01 works without SIB.
    let needs_sib = base == RSP;
    if disp == 0 && !needs_sib && base != RBP {
        code.push(modrm(0, reg & 7, base & 7));
        if needs_sib {
            code.push(sib_for_base(base).unwrap_or(0x24));
        }
    } else if disp as i8 as i32 == disp {
        code.push(modrm(1, reg & 7, base & 7));
        if needs_sib {
            code.push(sib_for_base(base).unwrap_or(0x24));
        }
        code.push(disp as u8);
    } else {
        code.push(modrm(2, reg & 7, base & 7));
        if needs_sib {
            code.push(sib_for_base(base).unwrap_or(0x24));
        }
        code.extend_from_slice(&disp.to_le_bytes());
    }
    code
}

/// mov r32/r64, [r/m32/r/m64 + disp]  (REX.W + 8B /r with ModRM)
/// When base is RSP (4), a SIB byte must follow ModRM.
pub fn mov_r_m(reg: u8, base: u8, disp: i32) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, base);
    code.push(0x8B);
    let needs_sib = base == RSP;
    if disp == 0 && !needs_sib && base != RBP {
        code.push(modrm(0, reg & 7, base & 7));
        if needs_sib {
            code.push(sib_for_base(base).unwrap_or(0x24));
        }
    } else if disp as i8 as i32 == disp {
        code.push(modrm(1, reg & 7, base & 7));
        if needs_sib {
            code.push(sib_for_base(base).unwrap_or(0x24));
        }
        code.push(disp as u8);
    } else {
        code.push(modrm(2, reg & 7, base & 7));
        if needs_sib {
            code.push(sib_for_base(base).unwrap_or(0x24));
        }
        code.extend_from_slice(&disp.to_le_bytes());
    }
    code
}

/// mov [rbp - (8 + disp)], r64  — RBP-relative spill (push/pop-safe)
pub fn str64(reg: u8, disp: u16) -> Vec<u8> {
    mov_m_r(RBP, -(disp as i32 + 8), reg)
}

/// mov r64, [rbp - (8 + disp)]  — RBP-relative load (push/pop-safe)
pub fn ldr64(reg: u8, disp: u16) -> Vec<u8> {
    mov_r_m(reg, RBP, -(disp as i32 + 8))
}

/// mov eax/rax, [abs] — load from absolute address (uses moffs form, eax/rax only)
pub fn mov_rax_from_abs(addr: u64) -> Vec<u8> {
    let mut code = Vec::new();
    if is_32bit() {
        code.push(0xA1); // MOV EAX, moffs32
        code.extend_from_slice(&(addr as u32).to_le_bytes());
    } else {
        code.push(0x48); // REX.W
        code.push(0xA1); // MOV RAX, moffs64
        code.extend_from_slice(&addr.to_le_bytes());
    }
    code
}

/// mov [abs], eax/rax — store to absolute address (uses moffs form, eax/rax only)
pub fn mov_abs_from_rax(addr: u64) -> Vec<u8> {
    let mut code = Vec::new();
    if is_32bit() {
        code.push(0xA3); // MOV moffs32, EAX
        code.extend_from_slice(&(addr as u32).to_le_bytes());
    } else {
        code.push(0x48); // REX.W
        code.push(0xA3); // MOV moffs64, RAX
        code.extend_from_slice(&addr.to_le_bytes());
    }
    code
}

/// mov r32/r64, [abs32] — load from absolute 32-bit address (any register)
pub fn mov_r_from_abs32(reg: u8, addr: u32) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, 0);
    code.push(0x8B);
    // ModRM: mod=00, reg=reg, r/m=100 (SIB)
    code.push(modrm(0, reg & 7, 4));
    // SIB: scale=0, index=4 (none), base=5 (disp32 = absolute)
    code.push(0x25);
    code.extend_from_slice(&addr.to_le_bytes());
    code
}

/// mov [abs32], r32/r64 — store to absolute 32-bit address (any register)
pub fn mov_abs32_from_r(reg: u8, addr: u32) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, 0);
    code.push(0x89);
    code.push(modrm(0, reg & 7, 4));
    code.push(0x25);
    code.extend_from_slice(&addr.to_le_bytes());
    code
}

/// sub r/m32/r/m64, imm8  (REX.W + 83 /5 ib)
pub fn sub_rmi8(dst: u8, imm: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, dst);
    code.push(0x83);
    code.push(modrm(3, 5, dst & 7));
    code.push(imm);
    code
}

/// sub r/m32/r/m64, imm32  (REX.W + 81 /5 id)
pub fn sub_rmi32(dst: u8, imm: i32) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, dst);
    code.push(0x81);
    code.push(modrm(3, 5, dst & 7));
    code.extend_from_slice(&imm.to_le_bytes());
    code
}

/// add r/m32/r/m64, imm8  (REX.W + 83 /0 ib)
pub fn add_rmi8(dst: u8, imm: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, dst);
    code.push(0x83);
    code.push(modrm(3, 0, dst & 7));
    code.push(imm);
    code
}

/// sub esp/rsp, imm8  — stack frame allocation
pub fn sub_rsp_i8(imm: u8) -> Vec<u8> {
    sub_rmi8(RSP, imm)
}

/// sub esp/rsp, imm32  — stack frame allocation (large frames)
pub fn sub_rsp_i32(imm: i32) -> Vec<u8> {
    sub_rmi32(RSP, imm)
}

/// lea r32/r64, [rsp + disp8]
pub fn lea_rsp_disp(reg: u8, disp: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, RSP);
    code.push(0x8D);
    code.push(modrm(1, reg & 7, RSP & 7));
    code.push(disp);
    code
}

/// add r/m32/r/m64, r32/r64  (REX.W + 01 /r, reg=src, rm=dst)
pub fn add_rr(dst: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, src, dst);
    code.push(0x01);
    code.push(modrm(3, src & 7, dst & 7));
    code
}

/// sub r/m32/r/m64, r32/r64  (REX.W + 29 /r, reg=src, rm=dst)
pub fn sub_rr(dst: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, src, dst);
    code.push(0x29);
    code.push(modrm(3, src & 7, dst & 7));
    code
}

/// imul r32/r64, r/m32/r/m64  (REX.W + 0F AF /r, reg=dst, rm=src)
pub fn imul_rr(dst: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, dst, src);
    code.push(0x0F);
    code.push(0xAF);
    code.push(modrm(3, dst & 7, src & 7));
    code
}

/// xor r32/r64, r/m32/r/m64 (REX.W + 33 /r, reg=dst, rm=src) — also used for zero idiom
pub fn xor_rr(dst: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, dst, src);
    code.push(0x33);
    code.push(modrm(3, dst & 7, src & 7));
    code
}

/// and r32/r64, r/m32/r/m64 (REX.W + 23 /r, reg=dst, rm=src)
pub fn and_rr(dst: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, dst, src);
    code.push(0x23);
    code.push(modrm(3, dst & 7, src & 7));
    code
}

/// or r32/r64, r/m32/r/m64 (REX.W + 0B /r, reg=dst, rm=src)
pub fn or_rr(dst: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, dst, src);
    code.push(0x0B);
    code.push(modrm(3, dst & 7, src & 7));
    code
}

/// cmp r/m32/r/m64, r32/r64  (REX.W + 39 /r, reg=src, rm=dst)
pub fn cmp_rr(a: u8, b: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, b, a);
    code.push(0x39);
    code.push(modrm(3, b & 7, a & 7));
    code
}

/// cmp r/m32/r/m64, imm8 (REX.W + 83 /7 ib)
pub fn cmp_rmi8(rm: u8, imm: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, rm);
    code.push(0x83);
    code.push(modrm(3, 7, rm & 7));
    code.push(imm);
    code
}

/// cmp r/m32/r/m64, imm32 (REX.W + 81 /7 id)
pub fn cmp_rmi32(rm: u8, imm: i32) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, rm);
    code.push(0x81);
    code.push(modrm(3, 7, rm & 7));
    code.extend_from_slice(&imm.to_le_bytes());
    code
}

/// jcc rel8  — conditional jump with short displacement
pub fn jcc(condition: u8, rel_offset: i8) -> Vec<u8> {
    vec![0x70 + condition, rel_offset as u8]
}

/// jcc rel32  — conditional jump with near displacement
pub fn jcc_near(condition: u8, rel_offset: i32) -> Vec<u8> {
    let mut code = vec![0x0F, 0x80 + condition];
    code.extend_from_slice(&rel_offset.to_le_bytes());
    code
}

/// je rel8
pub fn je(rel_offset: i8) -> Vec<u8> {
    jcc(0x04, rel_offset)
}

/// jne rel8
pub fn jne(rel_offset: i8) -> Vec<u8> {
    jcc(0x05, rel_offset)
}

/// jmp rel32 (near)
pub fn jmp_rel32(rel_offset: i32) -> Vec<u8> {
    let mut code = vec![0xE9];
    code.extend_from_slice(&rel_offset.to_le_bytes());
    code
}

/// jmp rel8 (short)
pub fn jmp_rel8(rel_offset: i8) -> Vec<u8> {
    vec![0xEB, rel_offset as u8]
}

/// call rel32
pub fn call_rel32(rel_offset: i32) -> Vec<u8> {
    let mut code = vec![0xE8];
    code.extend_from_slice(&rel_offset.to_le_bytes());
    code
}

/// test r/m32/r/m64, r32/r64  (REX.W + 85 /r, reg=src, rm=dst)
pub fn test_rr(a: u8, b: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, b, a);
    code.push(0x85);
    code.push(modrm(3, b & 7, a & 7));
    code
}

/// nop (multi-byte)
pub fn nop() -> Vec<u8> {
    vec![0x90]
}

// ── Width-agnostic helpers used by the lowerer ─────────────────────

/// mov [dst], src — store register to register-indirect address.
pub fn mov_ptr_reg(dst: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, src, dst);
    code.push(0x89);
    code.push(modrm(0, src & 7, dst & 7));
    code
}

/// mov reg, [base] — load from register-indirect address.
pub fn mov_reg_ptr(reg: u8, base: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, base);
    code.push(0x8B);
    code.push(modrm(0, reg & 7, base & 7));
    code
}

/// shl reg, imm8
pub fn shl_reg_imm(reg: u8, imm: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, reg);
    code.push(0xC1);
    code.push(modrm(3, 4, reg & 7));
    code.push(imm);
    code
}

/// shr reg, imm8
pub fn shr_reg_imm(reg: u8, imm: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, reg);
    code.push(0xC1);
    code.push(modrm(3, 5, reg & 7));
    code.push(imm);
    code
}

/// shl reg, cl
pub fn shl_reg_cl(reg: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, reg);
    code.push(0xD3);
    code.push(modrm(3, 4, reg & 7));
    code
}

/// shr reg, cl
pub fn shr_reg_cl(reg: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, reg);
    code.push(0xD3);
    code.push(modrm(3, 5, reg & 7));
    code
}

/// neg reg
pub fn neg_reg(reg: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, reg);
    code.push(0xF7);
    code.push(modrm(3, 3, reg & 7));
    code
}

/// not reg
pub fn not_reg(reg: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, reg);
    code.push(0xF7);
    code.push(modrm(3, 2, reg & 7));
    code
}

/// div reg  (unsigned divide: edx:eax / reg)
pub fn div_reg(reg: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_w(&mut code, reg);
    code.push(0xF7);
    code.push(modrm(3, 6, reg & 7));
    code
}

/// setcc reg (8-bit). condition is the second opcode byte (e.g. 0x94 sete, 0x95 setne).
pub fn setcc(condition: u8, reg: u8) -> Vec<u8> {
    let mut code = vec![0x0F, condition];
    code.push(modrm(3, 0, reg & 7));
    code
}

/// movzx reg, src8
pub fn movzx_r8(reg: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, src);
    code.push(0x0F);
    code.push(0xB6);
    code.push(modrm(3, reg & 7, src & 7));
    code
}

/// movzx reg, src16
pub fn movzx_r16(reg: u8, src: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, src);
    code.push(0x0F);
    code.push(0xB7);
    code.push(modrm(3, reg & 7, src & 7));
    code
}

/// movzx reg, byte [base]
pub fn movzx_m8(reg: u8, base: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, base);
    code.push(0x0F);
    code.push(0xB6);
    code.push(modrm(0, reg & 7, base & 7));
    code
}

/// movzx reg, word [base]
pub fn movzx_m16(reg: u8, base: u8) -> Vec<u8> {
    let mut code = Vec::new();
    push_rex_wr(&mut code, reg, base);
    code.push(0x0F);
    code.push(0xB7);
    code.push(modrm(0, reg & 7, base & 7));
    code
}

// ── Compound sequences ──────────────────────────────────────────

/// Zero a register using `xor reg, reg`
pub fn zero_reg(reg: u8) -> Vec<u8> {
    xor_rr(reg, reg)
}

/// Load a 32-bit/64-bit immediate into a register.
/// Uses `mov r64, imm64` in 64-bit mode and `mov r32, imm32` in 32-bit mode.
pub fn load_i64(reg: u8, value: i64) -> Vec<u8> {
    if value == 0 {
        zero_reg(reg)
    } else {
        mov_ri64(reg, value)
    }
}

/// Load a 32-bit immediate into a register.
pub fn load_i32(reg: u8, value: i32) -> Vec<u8> {
    if value == 0 {
        zero_reg(reg)
    } else {
        let mut code = Vec::new();
        if !is_32bit() && reg >= 8 {
            code.push(0x41); // REX.B for r8-r15
        } else if !is_32bit() {
            code.push(0x40); // REX prefix with no flags (operand-size 32)
        }
        code.push(0xB8 + (reg & 7));
        code.extend_from_slice(&value.to_le_bytes());
        code
    }
}

/// `lea rbp/ebp, [rsp/esp]` with a proper SIB (rm=RSP requires it).
pub fn lea_fp_from_sp() -> Vec<u8> {
    if is_32bit() {
        vec![0x8D, 0x2C, 0x24]
    } else {
        vec![0x48, 0x8D, 0x2C, 0x24]
    }
}

/// `lea rsp/esp, [rbp/ebp]` (mod=01 disp8=0; 64-bit `[rbp]` is not RIP-relative).
pub fn lea_sp_from_fp() -> Vec<u8> {
    if is_32bit() {
        vec![0x8D, 0x65, 0x00]
    } else {
        vec![0x48, 0x8D, 0x65, 0x00]
    }
}

/// Fast owned prologue: `push rbp; lea rbp, [rsp]` — not the classic `mov rbp, rsp`.
pub fn prologue() -> Vec<u8> {
    let mut code = push_r(REG_FP);
    code.extend_from_slice(&lea_fp_from_sp());
    code
}

/// Fast owned epilogue: `lea rsp, [rbp]; pop rbp; ret` — not classic `mov rsp, rbp`.
pub fn epilogue() -> Vec<u8> {
    let mut code = lea_sp_from_fp();
    code.extend_from_slice(&pop_r(REG_FP));
    code.extend_from_slice(&ret());
    code
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_ret() {
        assert_eq!(ret(), vec![0xC3]);
    }

    #[test]
    fn encodes_push_rbp() {
        assert_eq!(push_r(RBP), vec![0x55]);
    }

    #[test]
    fn encodes_pop_rbp() {
        assert_eq!(pop_r(RBP), vec![0x5D]);
    }

    #[test]
    fn encodes_mov_rbp_rsp() {
        assert_eq!(mov_rr(RBP, RSP), vec![0x48, 0x89, 0xE5]);
    }

    #[test]
    fn encodes_sub_rsp_imm8() {
        assert_eq!(sub_rsp_i8(0x20), vec![0x48, 0x83, 0xEC, 0x20]);
    }

    #[test]
    fn encodes_mov_rax_imm64() {
        assert_eq!(mov_ri64(RAX, 42), vec![0x48, 0xB8, 42, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn encodes_prologue() {
        let code = prologue();
        // push rbp; lea rbp, [rsp]  — not classic `mov rbp, rsp` (0x48 0x89 0xE5)
        assert_eq!(&code[..], &[0x55, 0x48, 0x8D, 0x2C, 0x24]);
    }

    #[test]
    fn encodes_epilogue() {
        let code = epilogue();
        assert_eq!(&code[code.len() - 1..], &[0xC3]);
    }

    #[test]
    fn encodes_call_rel32() {
        let code = call_rel32(42);
        assert_eq!(code[0], 0xE8);
        assert_eq!(&code[1..5], &[42, 0, 0, 0]);
    }

    #[test]
    fn encodes_add_rax_rbx() {
        assert_eq!(add_rr(RAX, RBX), vec![0x48, 0x01, 0xD8]);
    }

    #[test]
    fn encodes_sub_rax_rbx() {
        assert_eq!(sub_rr(RAX, RBX), vec![0x48, 0x29, 0xD8]);
    }

    #[test]
    fn encodes_xor_zero() {
        let code = xor_rr(RAX, RAX);
        assert_eq!(code, vec![0x48, 0x33, 0xC0]);
    }

    #[test]
    fn encodes_cmp_rr() {
        assert_eq!(cmp_rr(RAX, RBX), vec![0x48, 0x39, 0xD8]);
    }

    #[test]
    fn encodes_je_short() {
        assert_eq!(je(0x10), vec![0x74, 0x10]);
    }

    #[test]
    fn encodes_jne_short() {
        assert_eq!(jne(0xF0_u8 as i8), vec![0x75, 0xF0]);
    }

    #[test]
    fn encodes_jmp_rel32() {
        let code = jmp_rel32(0x1000);
        assert_eq!(code[0], 0xE9);
        assert_eq!(&code[1..5], &[0x00, 0x10, 0x00, 0x00]);
    }

    #[test]
    fn encodes_jmp_rel8() {
        assert_eq!(jmp_rel8(0x20), vec![0xEB, 0x20]);
    }

    #[test]
    fn encodes_str64_and_ldr64() {
        let store = str64(RAX, 8);
        assert!(store.len() >= 3);
        let load = ldr64(RAX, 8);
        assert!(load.len() >= 3);
    }

    #[test]
    fn load_i64_zero_uses_xor() {
        let code = load_i64(RAX, 0);
        assert_eq!(code, vec![0x48, 0x33, 0xC0]);
    }

    #[test]
    fn load_i64_nonzero_uses_mov() {
        let code = load_i64(RAX, 42);
        assert_eq!(code[0], 0x48);
        assert_eq!(code[1], 0xB8);
    }

    #[test]
    fn emitter_patch_u32() {
        let mut em = CodeEmitter::new();
        let site = em.emit_insn(0);
        em.patch_u32(site, 0xDEAD_BEEF);
        assert_eq!(
            &em.bytes[site as usize..site as usize + 4],
            &[0xEF, 0xBE, 0xAD, 0xDE]
        );
    }

    #[test]
    fn prologue_epilogue_round_trip() {
        let pro = prologue();
        let epi = epilogue();
        assert!(pro.len() > 0);
        assert!(epi.len() > 0);
        // prologue first bytes: push rbp (0x55); lea, not mov
        assert_eq!(pro[0], 0x55);
        assert_eq!(&pro[1..], &[0x48, 0x8D, 0x2C, 0x24]);
        assert_eq!(&epi[..4], &[0x48, 0x8D, 0x65, 0x00]);
        // epilogue last byte: ret (0xC3)
        assert_eq!(epi[epi.len() - 1], 0xC3);
    }
}
