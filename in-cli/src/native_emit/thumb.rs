//! Thumb-2 instruction encoder for Cortex-M freestanding scalar subset.
//!
//! AAPCS: r0-r3 args/temps, r0 return, r4-r7 callee-saved, r13=sp, r14=lr, r15=pc.
//! Emits pure Thumb (T32) encodings suitable for thumbv8m.main-none-eabi.

pub const R0: u8 = 0;
pub const R1: u8 = 1;
pub const R2: u8 = 2;
pub const R3: u8 = 3;
pub const R4: u8 = 4;
pub const R5: u8 = 5;
pub const R6: u8 = 6;
pub const R7: u8 = 7;
pub const R8: u8 = 8;
pub const R9: u8 = 9;
pub const R10: u8 = 10;
pub const R11: u8 = 11;
pub const R12: u8 = 12;
pub const SP: u8 = 13;
pub const LR: u8 = 14;
pub const PC: u8 = 15;

pub const REG_RET: u8 = R0;
pub const REG_FP: u8 = R7;
pub const REG_SP: u8 = SP;

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

    pub fn emit_u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn emit_u32_thumb(&mut self, value: u32) {
        // T32 is stored as two little-endian halfwords, high half first in memory
        // for the 32-bit encoding stream (ARM ARM encoding order).
        let hi = (value >> 16) as u16;
        let lo = value as u16;
        self.emit_u16(hi);
        self.emit_u16(lo);
    }

    pub fn emit_bytes(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub fn patch_u16(&mut self, offset: u32, value: u16) {
        let b = value.to_le_bytes();
        let i = offset as usize;
        self.bytes[i] = b[0];
        self.bytes[i + 1] = b[1];
    }

    pub fn patch_u32(&mut self, offset: u32, value: u32) {
        // Match emit_u32_thumb: T32 stores the high halfword first in memory.
        let hi = (value >> 16) as u16;
        let lo = value as u16;
        self.patch_u16(offset, hi);
        self.patch_u16(offset + 2, lo);
    }

    pub fn patch_i8_at(&mut self, offset: u32, value: i8) {
        self.bytes[offset as usize] = value as u8;
    }
}

/// `movs rd, #imm8` (T1)
pub fn movs_imm8(rd: u8, imm8: u8) -> u16 {
    debug_assert!(rd < 8);
    0x2000 | ((rd as u16) << 8) | u16::from(imm8)
}

/// `movs rd, rm` (T2, register)
pub fn movs_reg(rd: u8, rm: u8) -> u16 {
    debug_assert!(rd < 8 && rm < 8);
    ((rm as u16) << 3) | rd as u16
}

/// `mov rd, sp` (T1 MOV register, low destination)
pub fn mov_sp(rd: u8) -> u16 {
    debug_assert!(rd < 8);
    // 01000110 D=0 Rm=1101 rd[2:0]
    0x4668 | (rd as u16)
}

/// `mov rd, rm` high/low via T1 MOV (register) encoding for low regs uses
/// `adds rd, rm, #0` when both low, otherwise specials. For low regs use movs.
pub fn mov_low(rd: u8, rm: u8) -> u16 {
    // encodes as `adds rd, rm, #0` which is a common Thumb move for low regs
    debug_assert!(rd < 8 && rm < 8);
    0x1C00 | ((rm as u16) << 3) | rd as u16
}

/// `movs rd, #0` via eors rd, rd
pub fn zero_reg(rd: u8) -> u16 {
    debug_assert!(rd < 8);
    0x4040 | ((rd as u16) << 3) | rd as u16
}

/// T2 `movw rd, #imm16` (32-bit)
pub fn movw(rd: u8, imm16: u16) -> u32 {
    let i = ((imm16 >> 11) & 1) as u32;
    let imm4 = ((imm16 >> 12) & 0xF) as u32;
    let imm3 = ((imm16 >> 8) & 0x7) as u32;
    let imm8 = (imm16 & 0xFF) as u32;
    // 11110 i 100100 imm4 | 0 imm3 rd imm8
    let hi = 0xF240 | (i << 10) | imm4;
    let lo = (imm3 << 12) | ((rd as u32) << 8) | imm8;
    (hi << 16) | lo
}

/// T2 `movt rd, #imm16` (32-bit)
pub fn movt(rd: u8, imm16: u16) -> u32 {
    let i = ((imm16 >> 11) & 1) as u32;
    let imm4 = ((imm16 >> 12) & 0xF) as u32;
    let imm3 = ((imm16 >> 8) & 0x7) as u32;
    let imm8 = (imm16 & 0xFF) as u32;
    let hi = 0xF2C0 | (i << 10) | imm4;
    let lo = (imm3 << 12) | ((rd as u32) << 8) | imm8;
    (hi << 16) | lo
}

/// `blx rn` — indirect branch/call to the address in `rn` (r0-r7).
pub fn blx_reg(rn: u8) -> u16 {
    (0x4780u16) | ((rn as u16) << 3)
}

/// `lsls rd, rn, rm` (T2, 32-bit) — logical shift left by register.
/// Encoding: high = 1111 1010 0001 Rn, low = 1111 Rd Rm.
pub fn lsls_reg(rd: u8, rn: u8, rm: u8) -> u32 {
    let high = 0xFA10u32 | (rn as u32);
    let low = 0xF000u32 | ((rd as u32) << 8) | (rm as u32);
    (high << 16) | low
}

/// `lsrs rd, rn, rm` (T2, 32-bit) — logical shift right by register.
pub fn lsrs_reg(rd: u8, rn: u8, rm: u8) -> u32 {
    let high = 0xFA30u32 | (rn as u32);
    let low = 0xF000u32 | ((rd as u32) << 8) | (rm as u32);
    (high << 16) | low
}

/// `asrs rd, rn, rm` (T2, 32-bit) — arithmetic shift right by register.
pub fn asrs_reg(rd: u8, rn: u8, rm: u8) -> u32 {
    let high = 0xFA50u32 | (rn as u32);
    let low = 0xF000u32 | ((rd as u32) << 8) | (rm as u32);
    (high << 16) | low
}

/// `sdiv rd, rn, rm` (T1, 32-bit) — signed divide (rd = rn / rm).
/// Matches clang's encoding for `sdiv r0, r1, r2` = fb91 f0f2.
pub fn sdiv(rd: u8, rn: u8, rm: u8) -> u32 {
    let high = 0xFB90u32 | (rn as u32);
    let low = 0xF0F0u32 | ((rd as u32) << 8) | (rm as u32);
    (high << 16) | low
}

/// `udiv rd, rn, rm` (T1, 32-bit) — unsigned divide (rd = rn / rm).
pub fn udiv(rd: u8, rn: u8, rm: u8) -> u32 {
    let high = 0xFBB0u32 | (rn as u32);
    let low = 0xF0F0u32 | ((rd as u32) << 8) | (rm as u32);
    (high << 16) | low
}

/// 32-bit conditional branch `b<cond>.w label` (T3), offset in halfwords.
/// imm32 = SignExtend(S:J2:J1:imm6:imm11:'0', 25); field bits are direct.
pub fn b_cond_wide(cond: u8, imm_half: i32) -> u32 {
    let bits = (((imm_half as i64) * 2) & 0x1FFFFFF) as u32;
    let s = (bits >> 24) & 1;
    let j2 = (bits >> 23) & 1;
    let j1 = (bits >> 22) & 1;
    let imm6 = (bits >> 12) & 0x3F;
    let imm11 = (bits >> 1) & 0x7FF;
    let high = 0xF000 | (s << 10) | ((cond as u32) << 6) | imm6;
    let low = 0x8000 | (j1 << 13) | (j2 << 11) | imm11;
    (high << 16) | low
}

/// 32-bit unconditional branch `b.w label` (T4), offset in halfwords.
/// imm32 = SignExtend(S:I1:I2:imm10:imm11:'0', 25); I1=NOT(J1 EOR S),
/// I2=NOT(J2 EOR S).
pub fn b_wide(imm_half: i32) -> u32 {
    let bits = (((imm_half as i64) * 2) & 0x1FFFFFF) as u32;
    let s = (bits >> 24) & 1;
    let i2 = (bits >> 23) & 1;
    let i1 = (bits >> 22) & 1;
    let imm10 = (bits >> 12) & 0x3FF;
    let imm11 = (bits >> 1) & 0x7FF;
    let j1 = (!(i1 ^ s)) & 1;
    let j2 = (!(i2 ^ s)) & 1;
    let high = 0xF000 | (s << 10) | imm10;
    let low = 0x9000 | (j1 << 13) | (j2 << 11) | imm11;
    (high << 16) | low
}

/// Load signed 32-bit immediate into `rd` (r0-r12) via movw/movt or movs.
pub fn load_i32(emitter: &mut CodeEmitter, rd: u8, value: i32) {
    if rd < 8 && (0..=255).contains(&value) {
        emitter.emit_u16(movs_imm8(rd, value as u8));
        return;
    }
    if rd < 8 && value == 0 {
        emitter.emit_u16(zero_reg(rd));
        return;
    }
    let bits = value as u32;
    let lo = bits as u16;
    let hi = (bits >> 16) as u16;
    emitter.emit_u32_thumb(movw(rd, lo));
    if hi != 0 {
        emitter.emit_u32_thumb(movt(rd, hi));
    }
}

/// `adds rd, rn, rm` (T1, all low regs)
pub fn adds_reg(rd: u8, rn: u8, rm: u8) -> u16 {
    debug_assert!(rd < 8 && rn < 8 && rm < 8);
    0x1800 | ((rm as u16) << 6) | ((rn as u16) << 3) | rd as u16
}

/// `subs rd, rn, rm`
pub fn subs_reg(rd: u8, rn: u8, rm: u8) -> u16 {
    debug_assert!(rd < 8 && rn < 8 && rm < 8);
    0x1A00 | ((rm as u16) << 6) | ((rn as u16) << 3) | rd as u16
}

/// `muls rd, rm` (rd = rd * rm), T1
pub fn muls(rd: u8, rm: u8) -> u16 {
    debug_assert!(rd < 8 && rm < 8);
    0x4340 | ((rm as u16) << 3) | rd as u16
}

/// `ands rd, rm`
pub fn ands(rd: u8, rm: u8) -> u16 {
    debug_assert!(rd < 8 && rm < 8);
    0x4000 | ((rm as u16) << 3) | rd as u16
}

/// `orrs rd, rm`
pub fn orrs(rd: u8, rm: u8) -> u16 {
    debug_assert!(rd < 8 && rm < 8);
    0x4300 | ((rm as u16) << 3) | rd as u16
}

/// `eors rd, rm`
pub fn eors(rd: u8, rm: u8) -> u16 {
    debug_assert!(rd < 8 && rm < 8);
    0x4040 | ((rm as u16) << 3) | rd as u16
}

/// `mvns rd, rm` (bitwise complement, low registers).
pub fn mvns(rd: u8, rm: u8) -> u16 {
    debug_assert!(rd < 8 && rm < 8);
    0x43C0 | ((rm as u16) << 3) | rd as u16
}

/// `rsbs rd, rn, #0` (negate)
pub fn rsbs0(rd: u8, rn: u8) -> u16 {
    debug_assert!(rd < 8 && rn < 8);
    0x4240 | ((rn as u16) << 3) | rd as u16
}

/// `cmp rn, rm`
pub fn cmp_reg(rn: u8, rm: u8) -> u16 {
    debug_assert!(rn < 8 && rm < 8);
    0x4280 | ((rm as u16) << 3) | rn as u16
}

/// `cmp rn, #imm8`
pub fn cmp_imm8(rn: u8, imm8: u8) -> u16 {
    debug_assert!(rn < 8);
    0x2800 | ((rn as u16) << 8) | u16::from(imm8)
}

/// `adds rd, #imm8`
pub fn adds_imm8(rd: u8, imm8: u8) -> u16 {
    debug_assert!(rd < 8);
    0x3000 | ((rd as u16) << 8) | u16::from(imm8)
}

/// `subs rd, #imm8`
pub fn subs_imm8(rd: u8, imm8: u8) -> u16 {
    debug_assert!(rd < 8);
    0x3800 | ((rd as u16) << 8) | u16::from(imm8)
}

/// `push {regs}` low regs bitmask bits0-7 = r0-r7, bit8 = lr when using T1 with M bit
pub fn push(mask_low: u8, push_lr: bool) -> u16 {
    let m = if push_lr { 1u16 << 8 } else { 0 };
    0xB400 | m | u16::from(mask_low)
}

/// `pop {regs}`; bit8 = pc when pop_pc
pub fn pop(mask_low: u8, pop_pc: bool) -> u16 {
    let p = if pop_pc { 1u16 << 8 } else { 0 };
    0xBC00 | p | u16::from(mask_low)
}

/// `sub sp, #imm` where imm is multiple of 4, imm/4 encoded in 7 bits
pub fn sub_sp_imm(bytes: u32) -> Result<u16, String> {
    if !bytes.is_multiple_of(4) || bytes / 4 > 0x7F {
        return Err(format!("thumb: sub sp imm out of range ({bytes})"));
    }
    Ok(0xB080 | ((bytes / 4) as u16))
}

/// `add sp, #imm`
pub fn add_sp_imm(bytes: u32) -> Result<u16, String> {
    if !bytes.is_multiple_of(4) || bytes / 4 > 0x7F {
        return Err(format!("thumb: add sp imm out of range ({bytes})"));
    }
    Ok(0xB000 | ((bytes / 4) as u16))
}

/// `str rt, [rn, #imm]` imm multiple of 4, 0..124
pub fn str_imm(rt: u8, rn: u8, imm: u32) -> Result<u16, String> {
    if rt >= 8 || rn >= 8 || !imm.is_multiple_of(4) || imm / 4 > 0x1F {
        return Err(format!("thumb: str_imm invalid rt={rt} rn={rn} imm={imm}"));
    }
    Ok(0x6000 | (((imm / 4) as u16) << 6) | ((rn as u16) << 3) | rt as u16)
}

/// `ldr rt, [rn, #imm]`
pub fn ldr_imm(rt: u8, rn: u8, imm: u32) -> Result<u16, String> {
    if rt >= 8 || rn >= 8 || !imm.is_multiple_of(4) || imm / 4 > 0x1F {
        return Err(format!("thumb: ldr_imm invalid rt={rt} rn={rn} imm={imm}"));
    }
    Ok(0x6800 | (((imm / 4) as u16) << 6) | ((rn as u16) << 3) | rt as u16)
}

/// `ldrb rt, [rn, #imm]` imm 0..31
pub fn ldrb_imm(rt: u8, rn: u8, imm: u32) -> Result<u16, String> {
    if rt >= 8 || rn >= 8 || imm > 0x1F {
        return Err(format!("thumb: ldrb_imm invalid rt={rt} rn={rn} imm={imm}"));
    }
    Ok(0x7800 | ((imm as u16) << 6) | ((rn as u16) << 3) | rt as u16)
}

/// `strb rt, [rn, #imm]` imm 0..31
pub fn strb_imm(rt: u8, rn: u8, imm: u32) -> Result<u16, String> {
    if rt >= 8 || rn >= 8 || imm > 0x1F {
        return Err(format!("thumb: strb_imm invalid rt={rt} rn={rn} imm={imm}"));
    }
    Ok(0x7000 | ((imm as u16) << 6) | ((rn as u16) << 3) | rt as u16)
}

/// `ldrh rt, [rn, #imm]` imm even, 0..62
pub fn ldrh_imm(rt: u8, rn: u8, imm: u32) -> Result<u16, String> {
    if rt >= 8 || rn >= 8 || !imm.is_multiple_of(2) || imm / 2 > 0x1F {
        return Err(format!("thumb: ldrh_imm invalid rt={rt} rn={rn} imm={imm}"));
    }
    Ok(0x8800 | (((imm / 2) as u16) << 6) | ((rn as u16) << 3) | rt as u16)
}

/// `strh rt, [rn, #imm]` imm even, 0..62
pub fn strh_imm(rt: u8, rn: u8, imm: u32) -> Result<u16, String> {
    if rt >= 8 || rn >= 8 || !imm.is_multiple_of(2) || imm / 2 > 0x1F {
        return Err(format!("thumb: strh_imm invalid rt={rt} rn={rn} imm={imm}"));
    }
    Ok(0x8000 | (((imm / 2) as u16) << 6) | ((rn as u16) << 3) | rt as u16)
}

/// `str rt, [sp, #imm]` imm multiple of 4, 0..1020
pub fn str_sp(rt: u8, imm: u32) -> Result<u16, String> {
    if rt >= 8 || !imm.is_multiple_of(4) || imm / 4 > 0xFF {
        return Err(format!("thumb: str_sp invalid rt={rt} imm={imm}"));
    }
    Ok(0x9000 | ((rt as u16) << 8) | ((imm / 4) as u16))
}

/// `ldr rt, [sp, #imm]`
pub fn ldr_sp(rt: u8, imm: u32) -> Result<u16, String> {
    if rt >= 8 || !imm.is_multiple_of(4) || imm / 4 > 0xFF {
        return Err(format!("thumb: ldr_sp invalid rt={rt} imm={imm}"));
    }
    Ok(0x9800 | ((rt as u16) << 8) | ((imm / 4) as u16))
}

/// Unconditional `b` T2: 11100 imm11 (signed halfword offset from next insn).
/// Range ±1024 halfwords (±2048 bytes). Do not treat as imm8 — high bits of
/// imm11 must be set for negative offsets.
pub fn b_rel11(rel_halfwords: i32) -> Result<u16, String> {
    if !(-1024..=1023).contains(&rel_halfwords) {
        return Err(format!("thumb: b range ({rel_halfwords})"));
    }
    Ok(0xE000 | ((rel_halfwords as u32) & 0x7FF) as u16)
}

/// `b.n` backward-compat wrapper for tiny signed offsets (still T2 imm11).
pub fn b_rel8(rel_halfwords: i8) -> u16 {
    b_rel11(i32::from(rel_halfwords)).expect("i8 always in b imm11 range")
}

/// Conditional branch T1: cond in 0..13, rel8 signed halfwords from next insn.
/// cond: 0 EQ, 1 NE, 2 CS/HS, 3 CC/LO, 4 MI, 5 PL, 6 VS, 7 VC,
///       8 HI, 9 LS, 10 GE, 11 LT, 12 GT, 13 LE
pub fn b_cond_rel8(cond: u8, rel_halfwords: i8) -> u16 {
    debug_assert!(cond <= 13);
    0xD000 | ((cond as u16) << 8) | ((rel_halfwords as u8) as u16)
}

pub const COND_EQ: u8 = 0;
pub const COND_NE: u8 = 1;
pub const COND_GE: u8 = 10;
pub const COND_LT: u8 = 11;
pub const COND_GT: u8 = 12;
pub const COND_LE: u8 = 13;

/// `bl` T1 32-bit, rel_halfwords signed from next insn (after 4-byte bl)
pub fn bl_rel(rel_halfwords: i32) -> Result<u32, String> {
    // S:I1:I2:imm10:imm11 encoding
    if !(-0x800000..=0x7FFFFF).contains(&rel_halfwords) {
        return Err(format!("thumb: bl range ({rel_halfwords})"));
    }
    let imm = rel_halfwords as u32;
    let s = (imm >> 23) & 1;
    let i1 = (imm >> 22) & 1;
    let i2 = (imm >> 21) & 1;
    let imm10 = (imm >> 11) & 0x3FF;
    let imm11 = imm & 0x7FF;
    let j1 = ((!i1) ^ s) & 1;
    let j2 = ((!i2) ^ s) & 1;
    let hi = 0xF000 | (s << 10) | imm10;
    let lo = 0xD000 | (j1 << 13) | (j2 << 11) | imm11;
    Ok((hi << 16) | lo)
}

/// `bx lr`
pub fn bx_lr() -> u16 {
    0x4770
}

/// Standard freestanding prologue: push {r4-r7, lr}; mov r7, sp
pub fn emit_prologue(emitter: &mut CodeEmitter) {
    // push {r4, r5, r6, r7, lr}
    emitter.emit_u16(push(0b1111_0000, true)); // r4-r7 + lr → mask r4-r7 = 0xF0
    // mov r7, sp — T1 MOV (register): 01000110 D Rm rd[2:0]
    // rd=7 => D=0, rd[2:0]=111; rm=sp=13=1101 => 01000110 0 1101 111 = 0x466F
    // (0x46BF is mov pc, r7 and must not be used.)
    emitter.emit_u16(0x466F);
}

/// Epilogue restoring r4-r7 and returning via pop {r4-r7, pc}
pub fn emit_epilogue(emitter: &mut CodeEmitter, frame_bytes: u32) -> Result<(), String> {
    if frame_bytes > 0 {
        emitter.emit_u16(add_sp_imm(frame_bytes)?);
    }
    // pop {r4, r5, r6, r7, pc}
    emitter.emit_u16(pop(0b1111_0000, true));
    Ok(())
}

/// Allocate frame after prologue: sub sp, #frame
pub fn emit_frame(emitter: &mut CodeEmitter, frame_bytes: u32) -> Result<(), String> {
    if frame_bytes == 0 {
        return Ok(());
    }
    emitter.emit_u16(sub_sp_imm(frame_bytes)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_movs_and_bx_lr() {
        assert_eq!(movs_imm8(0, 42), 0x202A);
        assert_eq!(bx_lr(), 0x4770);
    }

    #[test]
    fn load_small_imm_uses_movs() {
        let mut e = CodeEmitter::new();
        load_i32(&mut e, R0, 42);
        assert_eq!(e.bytes, [0x2A, 0x20]); // little-endian 0x202A
    }

    #[test]
    fn load_large_imm_uses_movw_movt() {
        let mut e = CodeEmitter::new();
        load_i32(&mut e, R0, 0x1234_5678u32 as i32);
        assert_eq!(e.bytes.len(), 8);
    }

    #[test]
    fn bl_zero_offset() {
        let enc = bl_rel(0).unwrap();
        // high halfword first in stream via emit_u32_thumb
        assert_eq!((enc >> 16) & 0xF800, 0xF000);
        assert_eq!(enc & 0xD000, 0xD000);
    }

    #[test]
    fn encodes_bitwise_complement_with_distinct_low_registers() {
        assert_eq!(mvns(R0, R0), 0x43c0);
        assert_eq!(mvns(R1, R4), 0x43e1);
        assert_eq!(mvns(R7, R7), 0x43ff);
        let mut emitter = CodeEmitter::new();
        emitter.emit_u16(mvns(R1, R4));
        assert_eq!(emitter.bytes, [0xe1, 0x43]);
    }

    #[test]
    fn encodes_mmio_load_store() {
        assert_eq!(ldr_imm(0, 1, 0).unwrap(), 0x6808);
        assert_eq!(str_imm(0, 1, 0).unwrap(), 0x6008);
        assert_eq!(ldrb_imm(0, 1, 0).unwrap(), 0x7808);
        assert_eq!(strb_imm(0, 1, 0).unwrap(), 0x7008);
        assert_eq!(ldrh_imm(0, 1, 0).unwrap(), 0x8808);
        assert_eq!(strh_imm(0, 1, 0).unwrap(), 0x8008);
    }

    #[test]
    fn prologue_is_mov_r7_sp_not_mov_pc() {
        let mut e = CodeEmitter::new();
        emit_prologue(&mut e);
        // push {r4-r7,lr} = 0xB5F0, mov r7,sp = 0x466F
        assert_eq!(e.bytes, [0xF0, 0xB5, 0x6F, 0x46]);
    }

    #[test]
    fn unconditional_b_encodes_negative_imm11() {
        // -52 halfwords must set high bits of imm11, not look like +204.
        let enc = b_rel11(-52).unwrap();
        assert_eq!(enc, 0xE000 | (0x7CC));
        assert_eq!(b_rel11(0).unwrap(), 0xE000);
        assert!(b_rel11(-1025).is_err());
    }
}
