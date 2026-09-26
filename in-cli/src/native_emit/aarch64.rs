//! AArch64 instruction encoding for the owned native subset backend.

pub const REG_SP: u8 = 31;
pub const REG_XZR: u8 = 31;
pub const REG_LR: u8 = 30;
pub const REG_FP: u8 = 29;

/// AArch64 condition codes for [`b_cond`], by their conventional names.
pub const COND_EQ: u8 = 0;
pub const COND_NE: u8 = 1;

pub fn movz64(rd: u8, imm16: u16, shift: u8) -> u32 {
    assert!(shift.is_multiple_of(16) && shift <= 48);
    let hw = (shift / 16) as u32;
    0xD280_0000 | (hw << 21) | ((imm16 as u32) << 5) | (rd as u32)
}

pub fn movk64(rd: u8, imm16: u16, shift: u8) -> u32 {
    assert!(shift.is_multiple_of(16) && shift <= 48);
    let hw = (shift / 16) as u32;
    0xF280_0000 | (hw << 21) | ((imm16 as u32) << 5) | (rd as u32)
}

/// `mov rd, rm` for a general-purpose register. `rm` of 31 is the stack pointer,
/// because the encoding cannot distinguish it from the zero register; use
/// [`mov_zero64`] to materialize zero.
pub fn mov_reg64(rd: u8, rm: u8) -> u32 {
    if rm == REG_SP {
        add_imm64(rd, REG_SP, 0)
    } else {
        // Canonical register move: `orr rd, xzr, rm`.
        0xAA00_03E0 | ((rm as u32) << 16) | (rd as u32)
    }
}

/// `mov rd, #0`, the zero-register form of a move.
pub fn mov_zero64(rd: u8) -> u32 {
    movz64(rd, 0, 0)
}

pub fn add_imm64(rd: u8, rn: u8, imm12: u16) -> u32 {
    0x9100_0000 | ((imm12 as u32) << 10) | ((rn as u32) << 5) | (rd as u32)
}

pub fn sub_imm64(rd: u8, rn: u8, imm12: u16) -> u32 {
    0xD100_0000 | ((imm12 as u32) << 10) | ((rn as u32) << 5) | (rd as u32)
}

pub fn add_reg64(rd: u8, rn: u8, rm: u8) -> u32 {
    0x8B00_0000 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn sub_reg64(rd: u8, rn: u8, rm: u8) -> u32 {
    0xCB00_0000 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn mul64(rd: u8, rn: u8, rm: u8) -> u32 {
    0x9B00_7C00 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn sdiv64(rd: u8, rn: u8, rm: u8) -> u32 {
    0x9AC0_0C00 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn msub64(rd: u8, rn: u8, rm: u8, ra: u8) -> u32 {
    0x9B00_8000 | ((rm as u32) << 16) | ((ra as u32) << 10) | ((rn as u32) << 5) | (rd as u32)
}

pub fn and_reg64(rd: u8, rn: u8, rm: u8) -> u32 {
    0x8A00_0000 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn orr_reg64(rd: u8, rn: u8, rm: u8) -> u32 {
    0xAA00_0000 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn eor_reg64(rd: u8, rn: u8, rm: u8) -> u32 {
    0xCA00_0000 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn lsl_reg64(rd: u8, rn: u8, rm: u8) -> u32 {
    0x9AC0_2000 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn lsr_reg64(rd: u8, rn: u8, rm: u8) -> u32 {
    0x9AC0_2400 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn bl(offset_bytes: i32) -> u32 {
    let imm26 = ((offset_bytes >> 2) as u32) & 0x03FF_FFFF;
    0x9400_0000 | imm26
}

pub fn nop() -> u32 {
    0xD503201F
}

pub fn b(offset_bytes: i32) -> u32 {
    let imm26 = ((offset_bytes >> 2) as u32) & 0x03FF_FFFF;
    0x1400_0000 | imm26
}

pub fn ret() -> u32 {
    0xD65F_03C0
}

pub fn cmp_reg64(rn: u8, rm: u8) -> u32 {
    0xEB00_001F | ((rm as u32) << 16) | ((rn as u32) << 5)
}

pub fn b_cond(cond: u8, offset_bytes: i32) -> u32 {
    let imm19 = ((offset_bytes >> 2) as u32) & 0x7_FFFF;
    0x5400_0000 | (imm19 << 5) | (cond as u32)
}

pub fn adr(rd: u8, offset_bytes: i32) -> u32 {
    let imm = offset_bytes as u32;
    let immlo = imm & 0x3;
    let immhi = (imm >> 2) & 0x7_FFFF;
    0x1000_0000 | (immlo << 29) | (immhi << 5) | (rd as u32)
}

/// Store pair, pre-index: `stp rt, rt2, [sp, #offset]!`. `offset` is negative
/// for the usual frame setup, and the 7-bit immediate is its two's complement.
pub fn stp_pre(rt: u8, rt2: u8, offset: i32) -> u32 {
    let imm7 = ((offset / 8) as u32) & 0x7F;
    0xA980_0000 | (imm7 << 15) | ((rt2 as u32) << 10) | (REG_SP as u32) << 5 | (rt as u32)
}

/// Load pair, post-index: `ldp rt, rt2, [sp], #offset`.
pub fn ldp_post(rt: u8, rt2: u8, offset: i32) -> u32 {
    let imm7 = ((offset / 8) as u32) & 0x7F;
    0xA8C0_0000 | (imm7 << 15) | ((rt2 as u32) << 10) | (REG_SP as u32) << 5 | (rt as u32)
}

pub fn str64(rt: u8, rn: u8, offset: u32) -> u32 {
    let imm12 = offset / 8;
    0xF900_0000 | (imm12 << 10) | ((rn as u32) << 5) | (rt as u32)
}

pub fn ldr64(rt: u8, rn: u8, offset: u32) -> u32 {
    let imm12 = offset / 8;
    0xF940_0000 | (imm12 << 10) | ((rn as u32) << 5) | (rt as u32)
}

pub fn ldr64_reg_offset(rt: u8, rn: u8, rm: u8) -> u32 {
    0xF860_7800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rt as u32)
}

pub fn str64_reg_offset(rt: u8, rn: u8, rm: u8) -> u32 {
    0xF820_7800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rt as u32)
}

pub fn svc(imm16: u16) -> u32 {
    0xD400_0001 | ((imm16 as u32) << 5)
}

pub fn brk(imm16: u16) -> u32 {
    0xD420_0000 | ((imm16 as u32) << 5)
}

pub fn strb(rt: u8, rn: u8, offset: u32) -> u32 {
    assert!(offset < 4096);
    0x3900_0000 | (offset << 10) | ((rn as u32) << 5) | (rt as u32)
}

pub fn ldrb(rt: u8, rn: u8, offset: u32) -> u32 {
    assert!(offset < 4096);
    0x3940_0000 | (offset << 10) | ((rn as u32) << 5) | (rt as u32)
}

pub fn cbnz_w(rt: u8, offset_bytes: i32) -> u32 {
    let imm19 = ((offset_bytes >> 2) as u32) & 0x7_FFFF;
    0x3500_0000 | (imm19 << 5) | (rt as u32)
}

pub fn cbz_w(rt: u8, offset_bytes: i32) -> u32 {
    cbnz_w(rt, offset_bytes) ^ (1 << 24)
}

pub fn fmov_from_gp(rd_v: u8, rn_x: u8) -> u32 {
    0x9E67_0000 | ((rn_x as u32) << 5) | (rd_v as u32)
}

pub fn fmov_to_gp(rd_x: u8, rn_v: u8) -> u32 {
    0x9E66_0000 | ((rn_v as u32) << 5) | (rd_x as u32)
}

pub fn fadd_s(rd: u8, rn: u8, rm: u8) -> u32 {
    0x1E20_2800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn fsub_s(rd: u8, rn: u8, rm: u8) -> u32 {
    0x1E20_3800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn fmul_s(rd: u8, rn: u8, rm: u8) -> u32 {
    0x1E20_0800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn fdiv_s(rd: u8, rn: u8, rm: u8) -> u32 {
    0x1E20_1800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

// The double-precision forms. `Float` values are `f64` and are moved with the
// 64-bit `fmov` encodings above, so arithmetic on them must be double precision:
// the single-precision forms read only the low half of the register and produced
// garbage for every float expression.
pub fn fadd_d(rd: u8, rn: u8, rm: u8) -> u32 {
    0x1E60_2800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn fsub_d(rd: u8, rn: u8, rm: u8) -> u32 {
    0x1E60_3800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn fmul_d(rd: u8, rn: u8, rm: u8) -> u32 {
    0x1E60_0800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn fdiv_d(rd: u8, rn: u8, rm: u8) -> u32 {
    0x1E60_1800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

/// `fcmp dn, dm` — sets the flags the comparison branch conditions read.
///
/// The float condition codes coincide with the integer ones (`LT`/`MI` and
/// `LE`/`LS` share an encoding), and unordered operands (NaN) fall out correctly:
/// only `!=` is true for them.
pub fn fcmp_d(rn: u8, rm: u8) -> u32 {
    0x1E60_2000 | ((rm as u32) << 16) | ((rn as u32) << 5)
}

pub fn udiv64(rd: u8, rn: u8, rm: u8) -> u32 {
    0x9AC0_0800 | ((rm as u32) << 16) | ((rn as u32) << 5) | (rd as u32)
}

pub fn load_i64(rd: u8, value: i64) -> Vec<u32> {
    let uv = value as u64;
    let mut insns = vec![movz64(rd, (uv & 0xFFFF) as u16, 0)];
    if uv > 0xFFFF {
        insns.push(movk64(rd, ((uv >> 16) & 0xFFFF) as u16, 16));
    }
    if uv > 0xFFFF_FFFF {
        insns.push(movk64(rd, ((uv >> 32) & 0xFFFF) as u16, 32));
    }
    if uv > 0xFFFF_FFFF_FFFF {
        insns.push(movk64(rd, ((uv >> 48) & 0xFFFF) as u16, 48));
    }
    insns
}

#[derive(Default)]
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

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn emit_u32(&mut self, insn: u32) {
        self.bytes.extend_from_slice(&insn.to_le_bytes());
    }

    pub fn emit_insn(&mut self, insn: u32) -> u32 {
        let off = self.len();
        self.emit_u32(insn);
        off
    }

    pub fn emit_insns(&mut self, insns: &[u32]) {
        for insn in insns {
            self.emit_u32(*insn);
        }
    }

    pub fn patch_u32(&mut self, offset: u32, insn: u32) {
        let start = offset as usize;
        self.bytes[start..start + 4].copy_from_slice(&insn.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every encoder's output, cross-checked against `as -arch arm64`. This audit
    /// found real bugs: `stp_pre` emitted a load with an inverted offset, the
    /// register-shift encoders used the wrong opcode field, and the float encoders
    /// were 32-bit forms — so `<<`, `>>`, and every float operation compiled to
    /// nonsense in the native backend.
    #[test]
    fn every_encoder_matches_the_assembler() {
        assert_eq!(movz64(3, 0x1234, 16), 0xD2A24683);
        assert_eq!(movk64(3, 0x1234, 16), 0xF2A24683);
        assert_eq!(mov_reg64(3, 7), 0xAA0703E3);
        assert_eq!(mov_zero64(3), 0xD2800003);
        assert_eq!(b(8), 0x14000002);
        assert_eq!(bl(8), 0x94000002);
        assert_eq!(add_imm64(3, 7, 0x123), 0x91048CE3);
        assert_eq!(sub_imm64(3, 7, 0x123), 0xD1048CE3);
        assert_eq!(add_reg64(3, 7, 9), 0x8B0900E3);
        assert_eq!(sub_reg64(3, 7, 9), 0xCB0900E3);
        assert_eq!(mul64(3, 7, 9), 0x9B097CE3);
        assert_eq!(sdiv64(3, 7, 9), 0x9AC90CE3);
        assert_eq!(msub64(3, 7, 9, 11), 0x9B09ACE3);
        assert_eq!(and_reg64(3, 7, 9), 0x8A0900E3);
        assert_eq!(orr_reg64(3, 7, 9), 0xAA0900E3);
        assert_eq!(eor_reg64(3, 7, 9), 0xCA0900E3);
        assert_eq!(lsl_reg64(3, 7, 9), 0x9AC920E3);
        assert_eq!(lsr_reg64(3, 7, 9), 0x9AC924E3);
        assert_eq!(ret(), 0xD65F03C0);
        assert_eq!(cmp_reg64(7, 9), 0xEB0900FF);
        assert_eq!(b_cond(0, 8), 0x54000040);
        assert_eq!(adr(3, 8), 0x10000043);
        assert_eq!(stp_pre(29, 30, -16), 0xA9BF7BFD);
        assert_eq!(ldp_post(29, 30, 16), 0xA8C17BFD);
        assert_eq!(str64(3, 7, 24), 0xF9000CE3);
        assert_eq!(ldr64(3, 7, 24), 0xF9400CE3);
        assert_eq!(ldr64_reg_offset(3, 7, 9), 0xF86978E3);
        assert_eq!(str64_reg_offset(3, 7, 9), 0xF82978E3);
        assert_eq!(svc(0x80), 0xD4001001);
        assert_eq!(brk(1), 0xD4200020);
        assert_eq!(strb(3, 7, 5), 0x390014E3);
        assert_eq!(ldrb(3, 7, 5), 0x394014E3);
        assert_eq!(cbnz_w(3, 8), 0x35000043);
        assert_eq!(cbz_w(3, 8), 0x34000043);
        assert_eq!(nop(), 0xD503201F);
        assert_eq!(fmov_from_gp(3, 7), 0x9E6700E3);
        assert_eq!(fmov_to_gp(3, 7), 0x9E6600E3);
        assert_eq!(fadd_s(3, 7, 9), 0x1E2928E3);
        assert_eq!(fsub_s(3, 7, 9), 0x1E2938E3);
        assert_eq!(fmul_s(3, 7, 9), 0x1E2908E3);
        assert_eq!(fdiv_s(3, 7, 9), 0x1E2918E3);
        assert_eq!(fadd_d(3, 7, 9), 0x1E6928E3);
        assert_eq!(fsub_d(3, 7, 9), 0x1E6938E3);
        assert_eq!(fmul_d(3, 7, 9), 0x1E6908E3);
        assert_eq!(fdiv_d(3, 7, 9), 0x1E6918E3);
        assert_eq!(fcmp_d(3, 9), 0x1E692060);
        assert_eq!(fcmp_d(0, 1), 0x1E612000);
        assert_eq!(udiv64(5, 0, 2), 0x9AC20805);
        assert_eq!(msub64(6, 5, 2, 0), 0x9B0280A6);
    }


    /// A pre-index store must be a store: bit 22 selects load over store.
    #[test]
    fn stp_pre_is_a_store_not_a_load() {
        assert_eq!(stp_pre(29, 30, -16) & (1 << 22), 0, "bit 22 must be 0");
        assert_ne!(ldp_post(29, 30, 16) & (1 << 22), 0, "bit 22 must be 1");
    }
}
