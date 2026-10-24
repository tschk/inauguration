//! Owned, static Linux executable layout. No process-local JIT addresses or linker.
use super::*;
use crate::core_ir::{Decl, Typ, UnifiedModule};
use crate::native_emit::NativeSection;
use crate::native_emit::x86_64_lower::{lower_module_with_bases, patch_absolute_reference};

pub(crate) fn lower_x86_executable(module: &UnifiedModule, entry: &str) -> Result<Vec<u8>, String> {
    let valid_entry = module.decls.iter().any(|decl| {
        matches!(decl,
        Decl::Function { name, params, ret, .. } if name == entry && params.is_empty()
            && matches!(ret.canonical(), Typ::Int | Typ::Bool | Typ::Void))
    });
    if !valid_entry {
        return Err("native ELF executable requires a no-argument Int, Bool or Void entry".into());
    }
    struct WidthGuard(bool);
    impl Drop for WidthGuard {
        fn drop(&mut self) {
            crate::native_emit::x86_64::set_32bit(self.0);
        }
    }
    let _width = WidthGuard(crate::native_emit::x86_64::is_32bit());
    crate::native_emit::x86_64::set_32bit(false);
    let mut lowered = lower_module_with_bases(module, entry, TEXT_VADDR, 0)?;
    // Small owned Linux ABI routines support array values without a dynamic
    // loader. Each allocation is an anonymous mapping reclaimed at process exit.
    let mut runtime = std::collections::HashMap::new();
    for symbol in &lowered.externs {
        let code = match symbol.as_str() {
            "malloc" => linux_malloc(),
            "memcpy" => vec![0x48, 0x89, 0xf8, 0x48, 0x89, 0xd1, 0xfc, 0xf3, 0xa4, 0xc3],
            "abort" => vec![
                0xbf, 134, 0, 0, 0, 0xb8, 231, 0, 0, 0, 0x0f, 0x05, 0x0f, 0x0b,
            ],
            _ => return Err(format!("native ELF unresolved runtime import `{symbol}`")),
        };
        runtime.insert(symbol.clone(), lowered.code.len());
        lowered.code.extend_from_slice(&code);
    }
    for (symbol, site) in &lowered.extern_calls {
        let target = runtime
            .get(symbol)
            .ok_or("native ELF missing runtime relocation")?;
        let displacement = i32::try_from(*target as i64 - i64::from(*site) - 5)
            .map_err(|_| "native ELF runtime call exceeds rel32 range")?;
        let start = *site as usize + 1;
        lowered
            .code
            .get_mut(start..start + 4)
            .ok_or("native ELF runtime call patch outside code")?
            .copy_from_slice(&displacement.to_le_bytes());
    }
    if !lowered.exports.iter().any(|(name, _)| name == entry) {
        return Err(format!("native ELF entry `{entry}` has no function body"));
    }
    // Linux supplies a 16-byte aligned initial stack. CALL supplies the SysV
    // function-entry return address; the source entry's result becomes status.
    let start = lowered.code.len();
    let displacement = i32::try_from(i64::from(lowered.entry_offset) - start as i64 - 5)
        .map_err(|_| "native ELF entry call exceeds rel32 range")?;
    lowered.code.push(0xe8);
    lowered.code.extend_from_slice(&displacement.to_le_bytes());
    lowered
        .code
        .extend_from_slice(&[0x48, 0x89, 0xc7, 0xb8, 60, 0, 0, 0, 0x0f, 0x05, 0x0f, 0x0b]);

    let data_offset = (PAGE_SIZE + lowered.code.len() as u64)
        .checked_add(PAGE_SIZE - 1)
        .ok_or("native ELF data layout overflow")?
        & !(PAGE_SIZE - 1);
    let data_base = TEXT_VADDR
        .checked_add(data_offset - PAGE_SIZE)
        .ok_or("native ELF data address overflow")?;
    for reference in &lowered.absolute_refs {
        let base = match reference.target_section {
            NativeSection::Code => TEXT_VADDR,
            NativeSection::Data => data_base,
        };
        patch_absolute_reference(&mut lowered.code, *reference, base, false)?;
    }
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
    // Explicitly non-executable stack, including kernels with legacy defaults.
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

// malloc(size): mmap(NULL,size,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0).
// Linux errors occupy [-4095,-1]; translate them to the malloc NULL contract.
fn linux_malloc() -> Vec<u8> {
    use crate::native_emit::x86_64;
    let mut code = vec![0x48, 0x89, 0xfe]; // mov rsi,rdi
    for (register, value) in [(7, 0), (2, 3), (10, 0x22), (8, -1), (9, 0), (0, 9)] {
        code.extend_from_slice(&x86_64::load_i64(register, value));
    }
    code.extend_from_slice(&[
        0x0f, 0x05, 0x48, 0x3d, 0x01, 0xf0, 0xff, 0xff, 0x72, 0x02, 0x31, 0xc0, 0xc3,
    ]);
    code
}
