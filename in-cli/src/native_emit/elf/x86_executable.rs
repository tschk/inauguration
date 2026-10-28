//! Static Linux process startup, runtime imports and separate RX/RW segments.
use super::*;
use crate::core_ir::{Decl, Typ, UnifiedModule};
use crate::native_emit::NativeSection;
use crate::native_emit::x86_64_lower::{lower_linux_module, patch_absolute_reference};

pub(crate) fn lower_x86_executable(module: &UnifiedModule, entry: &str) -> Result<Vec<u8>, String> {
    let returns_status = module
        .decls
        .iter()
        .find_map(|decl| match decl {
            Decl::Function {
                name, params, ret, ..
            } if name == entry && params.is_empty() => match ret.canonical() {
                Typ::Int | Typ::Bool => Some(true),
                Typ::Void => Some(false),
                _ => None,
            },
            _ => None,
        })
        .ok_or("native ELF requires a no-argument Int, Bool or Void entry")?;
    struct WidthGuard(bool);
    impl Drop for WidthGuard {
        fn drop(&mut self) {
            crate::native_emit::x86_64::set_32bit(self.0);
        }
    }
    let _width = WidthGuard(crate::native_emit::x86_64::is_32bit());
    crate::native_emit::x86_64::set_32bit(false);
    let mut lowered = lower_linux_module(module, entry, TEXT_VADDR)?;
    if !lowered.exports.iter().any(|(name, _)| name == entry) {
        return Err(format!("native ELF entry `{entry}` has no body"));
    }
    let mut imports = std::collections::HashMap::new();
    for symbol in &lowered.externs {
        let code = match symbol.as_str() {
            "malloc" => linux_malloc(),
            // SysV memcpy returns its original destination and clears DF.
            "memcpy" => vec![0x48, 0x89, 0xf8, 0x48, 0x89, 0xd1, 0xfc, 0xf3, 0xa4, 0xc3],
            "abort" => vec![
                0xbf, 134, 0, 0, 0, 0xb8, 231, 0, 0, 0, 0x0f, 0x05, 0x0f, 0x0b,
            ],
            _ => return Err(format!("native ELF unresolved runtime import `{symbol}`")),
        };
        imports.insert(symbol, lowered.code.len());
        lowered.code.extend_from_slice(&code);
    }
    for (symbol, site) in &lowered.extern_calls {
        let target = imports
            .get(symbol)
            .ok_or("native ELF missing runtime import")?;
        let displacement = i32::try_from(*target as i64 - i64::from(*site) - 5)
            .map_err(|_| "native ELF runtime call exceeds rel32")?;
        let start = *site as usize + 1;
        lowered
            .code
            .get_mut(start..start + 4)
            .ok_or("native ELF import patch outside code")?
            .copy_from_slice(&displacement.to_le_bytes());
    }

    // The ELF entry is not a function entry: Linux supplies an aligned stack,
    // not a return address. CALL establishes the SysV ABI for the source entry.
    // R15 is callee-saved and points to [error flag, value, type] in writable data.
    let error_offset = lowered.data.len().next_multiple_of(8);
    lowered.data.resize(error_offset + 24, 0);
    let start = lowered.code.len();
    lowered.code.extend_from_slice(&[0x49, 0xbf]); // movabs r15,error_slot
    let error_site = lowered.code.len();
    lowered.code.extend_from_slice(&0u64.to_le_bytes());
    let displacement =
        i32::try_from(i64::from(lowered.entry_offset) - lowered.code.len() as i64 - 5)
            .map_err(|_| "native ELF entry call exceeds rel32")?;
    lowered.code.push(0xe8);
    lowered.code.extend_from_slice(&displacement.to_le_bytes());
    if returns_status {
        lowered.code.extend_from_slice(&[0x48, 0x89, 0xc7]); // mov rdi,rax
    } else {
        lowered.code.extend_from_slice(&[0x31, 0xff]); // xor edi,edi
    }
    lowered.code.extend_from_slice(&[
        0x49, 0x83, 0x3f, 0, // cmp qword [r15],0
        0x74, 5, // je exit; uncaught errors exit nonzero
        0xbf, 1, 0, 0, 0, 0xb8, 60, 0, 0, 0, 0x0f, 0x05, 0x0f, 0x0b,
    ]);
    let data_offset = (PAGE_SIZE + lowered.code.len() as u64).next_multiple_of(PAGE_SIZE);
    let data_base = TEXT_VADDR + data_offset - PAGE_SIZE;
    for reference in &lowered.absolute_refs {
        let base = match reference.target_section {
            NativeSection::Code => TEXT_VADDR,
            NativeSection::Data => data_base,
        };
        patch_absolute_reference(&mut lowered.code, *reference, base, false)?;
    }
    lowered.code[error_site..error_site + 8]
        .copy_from_slice(&(data_base + error_offset as u64).to_le_bytes());
    crate::native_emit::global_data::apply_data_relocations(
        &mut lowered.data,
        &lowered.data_relocations,
        data_base,
        u64::MAX,
    )?;

    let mut out = Vec::new();
    out.extend_from_slice(&ELF_MAGIC);
    out.extend_from_slice(&[ELFCLASS64, ELFDATA2LSB, EV_CURRENT]);
    out.extend_from_slice(&[0; 9]);
    out.extend_from_slice(&ET_EXEC.to_le_bytes());
    out.extend_from_slice(&EM_X86_64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(TEXT_VADDR + start as u64).to_le_bytes());
    out.extend_from_slice(&EHDR_SIZE.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for value in [EHDR_SIZE as u16, PHDR_SIZE as u16, 3, 0, 0, 0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    segment(
        &mut out,
        PT_LOAD,
        PF_R | PF_X,
        PAGE_SIZE,
        TEXT_VADDR,
        lowered.code.len() as u64,
    );
    segment(
        &mut out,
        PT_LOAD,
        PF_R | 2,
        data_offset,
        data_base,
        lowered.data.len() as u64,
    );
    // PT_GNU_STACK: writable, explicitly non-executable.
    segment(&mut out, 0x6474_e551, PF_R | 2, 0, 0, 0);
    out.resize(PAGE_SIZE as usize, 0);
    out.extend_from_slice(&lowered.code);
    out.resize(
        usize::try_from(data_offset).map_err(|_| "native ELF file offset overflow")?,
        0,
    );
    out.extend_from_slice(&lowered.data);
    Ok(out)
}

fn segment(out: &mut Vec<u8>, kind: u32, flags: u32, offset: u64, address: u64, size: u64) {
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    for value in [offset, address, address, size, size, PAGE_SIZE] {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

fn linux_malloc() -> Vec<u8> {
    // Anonymous mappings implement the existing process-lifetime allocation ABI.
    // mmap(NULL,size,RW,PRIVATE|ANONYMOUS,-1,0), translating Linux errors to NULL.
    use crate::native_emit::x86_64;
    let mut code = vec![0x48, 0x89, 0xfe]; // mov rsi,rdi
    for (register, value) in [(7, 0), (2, 3), (10, 0x22), (8, -1), (9, 0), (0, 9)] {
        code.extend_from_slice(&x86_64::load_i64(register, value));
    }
    code.extend_from_slice(&[
        0x0f, 0x05, 0x48, 0x3d, 0x01, 0xf0, 0xff, 0xff, 0x72, 2, 0x31, 0xc0, 0xc3,
    ]);
    code
}
