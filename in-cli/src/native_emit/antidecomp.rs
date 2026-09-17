//! Novel codegen shapes for the `harden` emit profile.
//!
//! These are intentional anti-patterns relative to conventional compilers so
//! Ghidra / Hex-Rays prologue, constant, and call-site heuristics fire less
//! cleanly. They preserve SysV ABI and observable semantics.

use crate::emit_profile::EmitProfile;
use crate::native_emit::x86_64::{self, R10, R11, RAX, RBX, REG_FP};
use std::cell::RefCell;

thread_local! {
    static TL_EMIT_PROFILE: RefCell<EmitProfile> = const { RefCell::new(EmitProfile::Default) };
    static TL_SHAPE_TICK: RefCell<u64> = const { RefCell::new(0) };
}

fn next_tick() -> u64 {
    TL_SHAPE_TICK.with(|t| {
        let v = *t.borrow();
        *t.borrow_mut() = v.wrapping_add(1);
        v
    })
}

pub fn set_profile(profile: EmitProfile) {
    TL_EMIT_PROFILE.with(|p| *p.borrow_mut() = profile);
}

pub fn clear_profile() {
    set_profile(EmitProfile::Default);
    TL_SHAPE_TICK.with(|t| *t.borrow_mut() = 0);
}

pub fn current_profile() -> EmitProfile {
    TL_EMIT_PROFILE.with(|p| *p.borrow())
}

pub fn harden_active() -> bool {
    current_profile() == EmitProfile::Harden
}

/// Never-taken `ud2` behind an always-true `je` (ZF from `xor r11,r11`).
/// Ghidra often treats `ud2` as a hard stop; hiding it behind a predicate
/// still poisons linear disassembly when the branch is inverted by a heuristic.
fn opaque_ud2_skip() -> Vec<u8> {
    let mut code = x86_64::xor_rr(R11, R11);
    code.extend_from_slice(&x86_64::test_rr(R11, R11));
    code.extend_from_slice(&x86_64::je(2)); // skip 2-byte ud2
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    code
}

/// Unusual prologue variants (all start with `push rbx` + lea frame).
/// Caller must pair with [`harden_epilogue`].
pub fn harden_prologue() -> Vec<u8> {
    match next_tick() % 3 {
        0 => {
            let mut code = x86_64::push_r(RBX);
            code.extend_from_slice(&x86_64::prologue());
            code.extend_from_slice(&opaque_ud2_skip());
            code
        }
        1 => {
            let mut code = x86_64::push_r(RBX);
            code.extend_from_slice(&x86_64::xor_rr(R10, R10));
            code.extend_from_slice(&x86_64::prologue());
            code.extend_from_slice(&opaque_ud2_skip());
            code.extend_from_slice(&x86_64::nop());
            code
        }
        _ => {
            let mut code = x86_64::push_r(RBX);
            code.extend_from_slice(&x86_64::sub_rsp_i8(0));
            code.extend_from_slice(&x86_64::prologue());
            code.extend_from_slice(&x86_64::lea_rsp_disp(R11, 0));
            code.extend_from_slice(&opaque_ud2_skip());
            code
        }
    }
}

pub fn harden_epilogue() -> Vec<u8> {
    let mut code = x86_64::lea_sp_from_fp();
    code.extend_from_slice(&x86_64::pop_r(REG_FP));
    code.extend_from_slice(&x86_64::pop_r(RBX));
    code.extend_from_slice(&ret_via_jmp());
    code
}

/// `jmp +0; ret` is a common anti-linear-sweep decoy; we use `lea rax, [rip+ret]; jmp rax` shape
/// via a short `jmp` over a dummy then `ret`.
fn ret_via_jmp() -> Vec<u8> {
    // jmp +2; ud2; ret  — linear sweep hits ud2; actual flow jumps to ret.
    let mut code = x86_64::jmp_rel8(2);
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    code.extend_from_slice(&x86_64::ret());
    code
}

/// Materialize `imm` into RAX via rotating MBA identities using R10 as scratch.
pub fn weird_materialize_rax(imm: i64) -> Vec<u8> {
    match (imm as u64).wrapping_mul(0x9E3779B97F4A7C15) & 3 {
        0 => {
            let mask: i64 = 0x5A5A_5A5A_5A5A_5A5A;
            let mut code = x86_64::mov_ri64(RAX, imm ^ mask);
            code.extend_from_slice(&x86_64::mov_ri64(R10, mask));
            code.extend_from_slice(&x86_64::xor_rr(RAX, R10));
            code
        }
        1 => {
            let m: i64 = 0x1111_1111_1111_1111;
            let mut code = x86_64::mov_ri64(RAX, imm.wrapping_add(m));
            code.extend_from_slice(&x86_64::mov_ri64(R10, m));
            code.extend_from_slice(&x86_64::sub_rr(RAX, R10));
            code
        }
        2 => {
            let m: i64 = 0x0F0F_0F0F_0F0F_0F0F;
            let mut code = x86_64::mov_ri64(RAX, imm.wrapping_sub(m));
            code.extend_from_slice(&x86_64::mov_ri64(R10, m));
            code.extend_from_slice(&x86_64::add_rr(RAX, R10));
            code
        }
        _ => {
            let a: i64 = 0xA5A5_A5A5_A5A5_A5A5u64 as i64;
            let b: i64 = 0x5A5A_5A5A_5A5A_5A5A;
            let mut code = x86_64::mov_ri64(RAX, imm ^ a ^ b);
            code.extend_from_slice(&x86_64::mov_ri64(R10, a));
            code.extend_from_slice(&x86_64::xor_rr(RAX, R10));
            code.extend_from_slice(&x86_64::mov_ri64(R10, b));
            code.extend_from_slice(&x86_64::xor_rr(RAX, R10));
            code
        }
    }
}

/// Semantic no-ops preferred before calls under harden (rotating shapes).
pub fn junk_pad() -> Vec<u8> {
    match next_tick() % 4 {
        0 => {
            let mut code = opaque_ud2_skip();
            code.extend_from_slice(&x86_64::xor_rr(R10, R10));
            code.push(0x90);
            code
        }
        1 => {
            let mut code = x86_64::xor_rr(R10, R10);
            code.extend_from_slice(&x86_64::lea_rsp_disp(R11, 0));
            code.extend_from_slice(&opaque_ud2_skip());
            code
        }
        2 => {
            let mut code = x86_64::nop();
            code.extend_from_slice(&opaque_ud2_skip());
            code.extend_from_slice(&x86_64::nop());
            code
        }
        _ => {
            let mut code = x86_64::xor_rr(R11, R11);
            code.extend_from_slice(&x86_64::add_rr(R11, R11));
            code.extend_from_slice(&opaque_ud2_skip());
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harden_prologue_starts_with_push_rbx() {
        clear_profile();
        for _ in 0..6 {
            let p = harden_prologue();
            assert_eq!(p[0], 0x53); // push rbx
            assert!(
                p.windows(2).any(|w| w == [0x0F, 0x0B]),
                "harden prologue should hide a ud2 decoy"
            );
        }
    }

    #[test]
    fn harden_epilogue_jumps_over_ud2_to_ret() {
        let e = harden_epilogue();
        assert_eq!(*e.last().unwrap(), 0xC3);
        assert!(e.windows(2).any(|w| w == [0xEB, 0x02]));
        assert!(e.windows(2).any(|w| w == [0x0F, 0x0B]));
    }

    #[test]
    fn weird_materialize_variants_nonempty() {
        clear_profile();
        for imm in [0i64, 1, -1, 7, 0x1234_5678_9ABC] {
            let code = weird_materialize_rax(imm);
            assert!(code.len() >= 10, "imm={imm} len={}", code.len());
        }
    }

    #[test]
    fn junk_pad_variants_nonempty() {
        clear_profile();
        for _ in 0..8 {
            assert!(!junk_pad().is_empty());
        }
    }

    #[test]
    fn profile_tls_roundtrip() {
        set_profile(EmitProfile::Harden);
        assert!(harden_active());
        clear_profile();
        assert!(!harden_active());
    }
}
