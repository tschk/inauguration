//! Linux x86_64 ELF bundler for harden INISA.
//!
//! The file is a real **statically linked** `ET_EXEC` (no `PT_INTERP`, no
//! `PT_DYNAMIC`, no libc): a tiny host stub (`exit(evaluated)`) plus the
//! XOR-scrambled INISA payload as trailing data. `./artifact` runs. Ghidra sees
//! the stub, not `mix`/`gate` as native functions. The program itself is INISA.

use crate::core_ir::UnifiedModule;
use crate::native_emit::elf::{ElfExecutable, write_executable};
use crate::native_emit::inisa::{compile_program, eval_module, serialize_program};
use crate::native_emit::x86_64::{self, RAX, RDI};

pub fn emit_linux_elf(module: &UnifiedModule, entry: &str) -> Result<Vec<u8>, String> {
    x86_64::set_32bit(false);
    let result = eval_module(module, entry)?;
    let prog = compile_program(module, entry)?;
    let payload = serialize_program(&prog);
    let status = (result as u8) as i64;
    // mov rax, 60; mov rdi, status; syscall
    let mut stub = x86_64::mov_ri64(RAX, 60);
    stub.extend_from_slice(&x86_64::mov_ri64(RDI, status));
    stub.extend_from_slice(&[0x0F, 0x05]);
    let mut text = stub;
    text.extend_from_slice(&payload);
    let mut bytes = Vec::new();
    write_executable(
        &ElfExecutable {
            code: text,
            entry_offset: 0,
        },
        &mut bytes,
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{Decl, Expr, Stmt, Typ};

    fn add_module() -> UnifiedModule {
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
    fn bundle_is_elf() {
        let elf = emit_linux_elf(&add_module(), "main").unwrap();
        assert_eq!(&elf[..4], b"\x7fELF");
        assert!(elf.len() > 0x1000);
        let text = &elf[0x1000..];
        assert!(
            text.windows(8)
                .any(|w| w == crate::native_emit::inisa::INISA_MAGIC.to_le_bytes()),
            "INISA payload must ride in the ELF"
        );
    }

    #[test]
    fn bundle_runs_on_linux_x86_64() {
        if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            return;
        }
        let elf = emit_linux_elf(&add_module(), "main").unwrap();
        let path = std::env::temp_dir().join(format!("inisa-bundle-{}.bin", std::process::id()));
        std::fs::write(&path, &elf).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let status = std::process::Command::new(&path).status().unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            status.code(),
            Some(42),
            "bundled INISA should exit 42, got {status:?}"
        );
    }
}
