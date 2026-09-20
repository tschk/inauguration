const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_REL: u16 = 1;
const EM_386: u16 = 3;
const EM_ARM: u16 = 40;
const EM_X86_64: u16 = 62;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PF_R: u32 = 4;
const PF_X: u32 = 1;
const EHDR32_SIZE: u32 = 52;
const EHDR_SIZE: u64 = 64;
const PHDR_SIZE: u64 = 56;
const TEXT_VADDR: u64 = 0x400_000;
const PAGE_SIZE: u64 = 0x1000;

pub const ELF_LINUX_TRIPLE: &str = "x86_64-unknown-linux-gnu";
pub const AARCH64_LINUX_TRIPLE: &str = "aarch64-unknown-linux-gnu";
pub const ARMV7_LINUX_GNUEABIHF_TRIPLE: &str = "armv7-unknown-linux-gnueabihf";
pub const THUMBV8M_MAIN_NONE_EABI_TRIPLE: &str = "thumbv8m.main-none-eabi";

pub struct ElfExecutable {
    pub code: Vec<u8>,
    pub entry_offset: u32,
}

pub struct ElfObject {
    pub code: Vec<u8>,
    pub export_name: String,
    /// Additional exported symbols (name, offset).
    /// The primary export_name is always included as the first export.
    pub exports: Vec<(String, u32)>,
    /// Undefined symbols (externs) — must be resolved at link time.
    pub undefs: Vec<String>,
    /// x86_64 call sites needing R_X86_64_PLT32 relocations:
    /// (offset of the disp32 within .text, symbol).
    pub x86_plt_relocs: Vec<(u32, String)>,
    /// Thumb BL sites needing R_ARM_THM_CALL relocation: (offset, symbol).
    pub thumb_calls: Vec<(u32, String)>,
    /// Thumb function-address movw/movt pairs needing R_ARM_THM_MOVW_ABS_NC +
    /// R_ARM_THM_MOVT_ABS: (movw offset, movt offset, symbol).
    pub thumb_addr_refs: Vec<(u32, u32, String)>,
}

pub fn x86_64_return_i32_object_code(value: u8) -> Vec<u8> {
    vec![0xB8, value, 0x00, 0x00, 0x00, 0xC3]
}

pub fn aarch64_return_i32_object_code(value: u8) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&(0xD280_0000u32 | ((value as u32) << 5)).to_le_bytes());
    code.extend_from_slice(&0xD65F_03C0u32.to_le_bytes());
    code
}

pub fn arm32_return_i32_object_code(value: u8) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&(0xE3A0_0000u32 | value as u32).to_le_bytes());
    code.extend_from_slice(&0xE12F_FF1Eu32.to_le_bytes());
    code
}

pub fn x86_64_linux_exit_code(status: u8) -> Vec<u8> {
    vec![
        0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00, 0x48, 0xC7, 0xC7, status, 0x00, 0x00, 0x00, 0x0F,
        0x05,
    ]
}

pub fn aarch64_linux_exit_code(status: u8) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&(0xD280_0000u32 | (93u32 << 5) | 8).to_le_bytes());
    code.extend_from_slice(&(0xD280_0000u32 | ((status as u32) << 5)).to_le_bytes());
    code.extend_from_slice(&0xD400_0001u32.to_le_bytes());
    code
}

pub fn arm32_linux_exit_code(status: u8) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&(0xE3A0_7000u32 | 1).to_le_bytes());
    code.extend_from_slice(&(0xE3A0_0000u32 | status as u32).to_le_bytes());
    code.extend_from_slice(&0xEF00_0000u32.to_le_bytes());
    code
}

pub fn write_x86_64_relocatable_object(object: &ElfObject, out: &mut Vec<u8>) {
    write_elf64_relocatable_object(object, EM_X86_64, out);
}

pub fn write_aarch64_relocatable_object(object: &ElfObject, out: &mut Vec<u8>) {
    write_elf64_relocatable_object(object, EM_AARCH64, out);
}

fn build_elf64_exports(object: &ElfObject) -> Vec<(&str, u64)> {
    let mut all_exports = Vec::new();
    all_exports.push((object.export_name.as_str(), 0));
    for (name, offset) in &object.exports {
        if name == &object.export_name && *offset == 0 {
            continue;
        }
        all_exports.push((name.as_str(), *offset as u64));
    }
    all_exports.sort_by_key(|a| a.1);
    all_exports
}

fn build_elf64_strtab(
    object: &ElfObject,
    all_exports: &[(&str, u64)],
) -> (Vec<u8>, Vec<u32>, Vec<u32>) {
    let mut strtab: Vec<u8> = vec![0u8];
    let mut name_indices: Vec<u32> = vec![0u32];
    for (name, _) in all_exports {
        let idx = strtab.len() as u32;
        name_indices.push(idx);
        strtab.extend_from_slice(name.as_bytes());
        strtab.push(0u8);
    }
    let mut undef_indices: Vec<u32> = Vec::new();
    for name in &object.undefs {
        let idx = strtab.len() as u32;
        undef_indices.push(idx);
        strtab.extend_from_slice(name.as_bytes());
        strtab.push(0u8);
    }
    (strtab, name_indices, undef_indices)
}

fn write_elf64_header(out: &mut Vec<u8>, machine: u16, shoff: u64) {
    let shentsize = 64u16;
    let shnum = 5u16;
    let shstrndx = 4u16;

    out.clear();
    out.extend_from_slice(&ELF_MAGIC);
    out.push(ELFCLASS64);
    out.push(ELFDATA2LSB);
    out.push(EV_CURRENT);
    out.extend_from_slice(&[0u8; 9]);
    out.extend_from_slice(&ET_REL.to_le_bytes());
    out.extend_from_slice(&machine.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&shoff.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&shentsize.to_le_bytes());
    out.extend_from_slice(&shnum.to_le_bytes());
    out.extend_from_slice(&shstrndx.to_le_bytes());
}

fn write_elf64_relocatable_object(object: &ElfObject, machine: u16, out: &mut Vec<u8>) {
    let rela_section = !object.x86_plt_relocs.is_empty();
    let shstrtab: &[u8] = if rela_section {
        b"\0.text\0.symtab\0.strtab\0.shstrtab\0.rela.text\0"
    } else {
        b"\0.text\0.symtab\0.strtab\0.shstrtab\0"
    };

    let all_exports = build_elf64_exports(object);
    let (strtab, name_indices, undef_indices) = build_elf64_strtab(object, &all_exports);

    let sym_count = all_exports.len() as u64 + object.undefs.len() as u64 + 1;
    let symtab_entry_size = 24u64;
    let symtab_size = sym_count * symtab_entry_size;
    let text_name = 1u32;
    let symtab_name = 7u32;
    let strtab_name = 15u32;
    let shstrtab_name = 23u32;
    let rela_text_name = 34u32;
    let rela_entry_size = 24u64;
    let rela_size = (object.x86_plt_relocs.len() as u64) * rela_entry_size;
    let text_offset = EHDR_SIZE;
    let symtab_offset = text_offset + object.code.len() as u64;
    let strtab_offset = symtab_offset + symtab_size;
    let shstrtab_offset = strtab_offset + strtab.len() as u64;
    let rela_offset = shstrtab_offset + shstrtab.len() as u64;
    let shoff = rela_offset + rela_size;

    write_elf64_header(out, machine, shoff);

    out.extend_from_slice(&object.code);

    // Null symbol
    write_symbol(out, 0, 0, 0, 0, 0, 0);
    // Defined symbols (shndx=1 = .text)
    for i in 0..all_exports.len() {
        let (name_idx_val, size) = if i + 1 < all_exports.len() {
            (name_indices[i + 1], all_exports[i + 1].1 - all_exports[i].1)
        } else {
            (
                name_indices[i + 1],
                object.code.len() as u64 - all_exports[i].1,
            )
        };
        write_symbol(out, name_idx_val, 0x12, 0, 1, all_exports[i].1, size);
    }
    // Undefined symbols (shndx=0 = UND)
    for (i, _name) in object.undefs.iter().enumerate() {
        if i < undef_indices.len() {
            write_symbol(out, undef_indices[i], 0x12, 0, 0, 0, 0);
        }
    }

    out.extend_from_slice(&strtab);
    out.extend_from_slice(shstrtab);

    // R_X86_64_PLT32 call relocations: { r_offset, r_info, r_addend } where
    // r_offset points at the disp32 inside .text, the symbol index selects
    // the undefined extern, and addend -4 accounts for the disp32 being
    // relative to the next instruction.
    if rela_section {
        let undef_base = 1u32 + all_exports.len() as u32;
        for (site, symbol) in &object.x86_plt_relocs {
            let sym_index = undef_base
                + object
                    .undefs
                    .iter()
                    .position(|name| name == symbol)
                    .unwrap_or_else(|| panic!("PLT32 relocation against unknown symbol `{symbol}`"))
                    as u32;
            let r_info = ((sym_index as u64) << 32) | 4u64; // R_X86_64_PLT32
            out.extend_from_slice(&(*site as u64).to_le_bytes());
            out.extend_from_slice(&r_info.to_le_bytes());
            out.extend_from_slice(&(-4i64).to_le_bytes());
        }
    }

    write_section_header(out, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    write_section_header(
        out,
        text_name,
        1,
        0x6,
        0,
        text_offset,
        object.code.len() as u64,
        16,
        0,
        0,
        0,
    );
    write_section_header(
        out,
        symtab_name,
        2,
        0,
        0,
        symtab_offset,
        symtab_size,
        8,
        24,
        3,
        1,
    );
    write_section_header(
        out,
        strtab_name,
        3,
        0,
        0,
        strtab_offset,
        strtab.len() as u64,
        1,
        0,
        0,
        0,
    );
    write_section_header(
        out,
        shstrtab_name,
        3,
        0,
        0,
        shstrtab_offset,
        shstrtab.len() as u64,
        1,
        0,
        0,
        0,
    );
    if rela_section {
        // SHT_RELA with SHF_INFO_LINK: sh_link = .symtab (2), sh_info =
        // .text (1), entsize = 24.
        write_section_header(
            out,
            rela_text_name,
            4,
            0x40,
            0,
            rela_offset,
            rela_size,
            8,
            2,
            1,
            24,
        );
    }
}

fn gather_arm32_exports_and_undefs(object: &ElfObject) -> (Vec<(&str, u32)>, Vec<String>) {
    let mut all_exports: Vec<(&str, u32)> = Vec::new();
    all_exports.push((&object.export_name, 0));
    for (name, offset) in &object.exports {
        if name == &object.export_name && *offset == 0 {
            continue;
        }
        all_exports.push((name, *offset));
    }
    all_exports.sort_by_key(|a| a.1);

    // Undefs must cover both declared externs and any call relocation targets.
    let mut all_undefs: Vec<String> = Vec::new();
    for name in &object.undefs {
        if !all_undefs.contains(name) {
            all_undefs.push(name.clone());
        }
    }
    for (_, name) in &object.thumb_calls {
        if !all_undefs.contains(name) {
            all_undefs.push(name.clone());
        }
    }

    (all_exports, all_undefs)
}

fn write_arm32_elf_header(out: &mut Vec<u8>, shoff: u32) {
    // EF_ARM_EABI_VER5 — Thumb freestanding objects need EABI flags so linkers
    // treat the stream as Thumb-capable rather than pure ARM.
    const EF_ARM_EABI_VER5: u32 = 0x0500_0000;
    out.clear();
    out.extend_from_slice(&ELF_MAGIC);
    out.push(ELFCLASS32);
    out.push(ELFDATA2LSB);
    out.push(EV_CURRENT);
    out.extend_from_slice(&[0u8; 9]);
    out.extend_from_slice(&ET_REL.to_le_bytes());
    out.extend_from_slice(&EM_ARM.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&shoff.to_le_bytes());
    out.extend_from_slice(&EF_ARM_EABI_VER5.to_le_bytes());
    out.extend_from_slice(&(EHDR32_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&40u16.to_le_bytes());
    out.extend_from_slice(&6u16.to_le_bytes()); // null + .text + .symtab + .rel.text + .strtab + .shstrtab
    out.extend_from_slice(&5u16.to_le_bytes()); // .shstrtab section index
}

fn write_arm32_section_headers(
    out: &mut Vec<u8>,
    object: &ElfObject,
    symtab_size: u32,
    n_local: u32,
    rel_size: u32,
    strtab_len: u32,
    shstrtab_len: u32,
    text_offset: u32,
    symtab_offset: u32,
    rel_offset: u32,
    strtab_offset: u32,
    shstrtab_offset: u32,
) {
    const SHT_REL: u32 = 9;
    let rel_name = 33u32;
    let text_name = 1u32;
    let symtab_name = 7u32;
    let strtab_name = 15u32;
    let shstrtab_name = 23u32;

    write_section_header32(out, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    write_section_header32(
        out,
        text_name,
        1,
        0x6,
        0,
        text_offset,
        object.code.len() as u32,
        0,
        0,
        4,
        0,
    );
    write_section_header32(
        out,
        symtab_name,
        2,
        0,
        0,
        symtab_offset,
        symtab_size,
        4,
        n_local,
        4,
        16,
    );
    write_section_header32(
        out, rel_name, SHT_REL, 0, 0, rel_offset, rel_size, 2, 1, 4, 8,
    );
    write_section_header32(
        out,
        strtab_name,
        3,
        0,
        0,
        strtab_offset,
        strtab_len,
        0,
        0,
        1,
        0,
    );
    write_section_header32(
        out,
        shstrtab_name,
        3,
        0,
        0,
        shstrtab_offset,
        shstrtab_len,
        0,
        0,
        1,
        0,
    );
}

pub fn write_arm32_relocatable_object(object: &ElfObject, out: &mut Vec<u8>) {
    const R_ARM_THM_CALL: u32 = 10;
    const R_ARM_THM_MOVW_ABS_NC: u32 = 47;
    const R_ARM_THM_MOVT_ABS: u32 = 48;
    let shstrtab = b"\0.text\0.symtab\0.strtab\0.shstrtab\0.rel.text\0";

    let (all_exports, all_undefs) = gather_arm32_exports_and_undefs(object);

    let mut strtab: Vec<u8> = vec![0u8];
    let mut name_indices: Vec<u32> = Vec::new();
    let mut symbol_index: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for (name, _) in &all_exports {
        symbol_index.insert(*name, name_indices.len() as u32 + 1);
        name_indices.push(strtab.len() as u32);
        strtab.extend_from_slice(name.as_bytes());
        strtab.push(0);
    }
    for name in &all_undefs {
        symbol_index.insert(name.as_str(), name_indices.len() as u32 + 1);
        name_indices.push(strtab.len() as u32);
        strtab.extend_from_slice(name.as_bytes());
        strtab.push(0);
    }

    let n_local = 1u32; // null symbol only
    let n_exports = all_exports.len() as u32;
    let n_undefs = all_undefs.len() as u32;
    let sym_count = 1 + n_exports + n_undefs;
    let symtab_size = sym_count * 16;
    // Two relocations per address ref (movw + movt).
    let n_addr_relocs = object.thumb_addr_refs.len() as u32 * 2;
    let rel_size = (object.thumb_calls.len() as u32 + n_addr_relocs) * 8;
    let text_offset = EHDR32_SIZE;
    let symtab_offset = text_offset + object.code.len() as u32;
    let rel_offset = symtab_offset + symtab_size;
    let strtab_offset = rel_offset + rel_size;
    let shstrtab_offset = strtab_offset + strtab.len() as u32;
    let shoff = shstrtab_offset + shstrtab.len() as u32;

    write_arm32_elf_header(out, shoff);

    out.extend_from_slice(&object.code);
    write_symbol32(out, 0, 0, 0, 0, 0, 0);
    for (i, (_, offset)) in all_exports.iter().enumerate() {
        // Thumb function symbols carry the low address bit set (st_value | 1).
        let value = offset | 1;
        let next_off = all_exports
            .get(i + 1)
            .map(|e| e.1)
            .unwrap_or(object.code.len() as u32);
        let size = next_off.saturating_sub(*offset);
        write_symbol32(out, name_indices[i], value, size, 0x12, 0, 1);
    }
    for i in 0..all_undefs.len() {
        let idx = name_indices[n_exports as usize + i];
        write_symbol32(out, idx, 0, 0, 0x10, 0, 0);
    }

    // .rel.text entries: R_ARM_THM_CALL for each extern BL, then
    // R_ARM_THM_MOVW_ABS_NC / R_ARM_THM_MOVT_ABS for each function-address load.
    for (offset, name) in &object.thumb_calls {
        let sym = *symbol_index
            .get(name.as_str())
            .expect("thumb call symbol in strtab");
        out.extend_from_slice(&offset.to_le_bytes());
        let info = (sym << 8) | R_ARM_THM_CALL;
        out.extend_from_slice(&info.to_le_bytes());
    }
    for (movw_off, movt_off, name) in &object.thumb_addr_refs {
        let sym = *symbol_index
            .get(name.as_str())
            .expect("thumb addr symbol in strtab");
        out.extend_from_slice(&movw_off.to_le_bytes());
        let info = (sym << 8) | R_ARM_THM_MOVW_ABS_NC;
        out.extend_from_slice(&info.to_le_bytes());
        out.extend_from_slice(&movt_off.to_le_bytes());
        let info = (sym << 8) | R_ARM_THM_MOVT_ABS;
        out.extend_from_slice(&info.to_le_bytes());
    }

    out.extend_from_slice(&strtab);
    out.extend_from_slice(shstrtab);

    write_arm32_section_headers(
        out,
        object,
        symtab_size,
        n_local,
        rel_size,
        strtab.len() as u32,
        shstrtab.len() as u32,
        text_offset,
        symtab_offset,
        rel_offset,
        strtab_offset,
        shstrtab_offset,
    );
}

fn write_symbol32(
    out: &mut Vec<u8>,
    name: u32,
    value: u32,
    size: u32,
    info: u8,
    other: u8,
    shndx: u16,
) {
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.push(info);
    out.push(other);
    out.extend_from_slice(&shndx.to_le_bytes());
}

fn write_symbol(
    out: &mut Vec<u8>,
    name: u32,
    info: u8,
    other: u8,
    shndx: u16,
    value: u64,
    size: u64,
) {
    out.extend_from_slice(&name.to_le_bytes());
    out.push(info);
    out.push(other);
    out.extend_from_slice(&shndx.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
}

fn write_section_header(
    out: &mut Vec<u8>,
    name: u32,
    typ: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    addralign: u64,
    entsize: u64,
    link: u32,
    info: u32,
) {
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&typ.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&addr.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&link.to_le_bytes());
    out.extend_from_slice(&info.to_le_bytes());
    out.extend_from_slice(&addralign.to_le_bytes());
    out.extend_from_slice(&entsize.to_le_bytes());
}

fn write_section_header32(
    out: &mut Vec<u8>,
    name: u32,
    typ: u32,
    flags: u32,
    addr: u32,
    offset: u32,
    size: u32,
    link: u32,
    info: u32,
    addralign: u32,
    entsize: u32,
) {
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&typ.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&addr.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&link.to_le_bytes());
    out.extend_from_slice(&info.to_le_bytes());
    out.extend_from_slice(&addralign.to_le_bytes());
    out.extend_from_slice(&entsize.to_le_bytes());
}

pub fn write_executable(exe: &ElfExecutable, out: &mut Vec<u8>) {
    write_elf64_executable(exe, EM_X86_64, out);
}

pub fn write_aarch64_executable(exe: &ElfExecutable, out: &mut Vec<u8>) {
    write_elf64_executable(exe, EM_AARCH64, out);
}

fn write_elf64_executable(exe: &ElfExecutable, machine: u16, out: &mut Vec<u8>) {
    let text_fileoff = PAGE_SIZE;
    let text_size = exe.code.len() as u64;
    let file_size = text_fileoff + text_size;
    let entry_vaddr = TEXT_VADDR + u64::from(exe.entry_offset);

    out.clear();
    out.extend_from_slice(&ELF_MAGIC);
    out.push(ELFCLASS64);
    out.push(ELFDATA2LSB);
    out.push(EV_CURRENT);
    out.extend_from_slice(&[0u8; 9]);
    // Statically linked: one PT_LOAD, no PT_INTERP / PT_DYNAMIC.
    out.extend_from_slice(&ET_EXEC.to_le_bytes());
    out.extend_from_slice(&machine.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&entry_vaddr.to_le_bytes());
    out.extend_from_slice(&EHDR_SIZE.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    out.extend_from_slice(&PT_LOAD.to_le_bytes());
    out.extend_from_slice(&(PF_R | PF_X).to_le_bytes());
    out.extend_from_slice(&text_fileoff.to_le_bytes());
    out.extend_from_slice(&TEXT_VADDR.to_le_bytes());
    out.extend_from_slice(&TEXT_VADDR.to_le_bytes());
    out.extend_from_slice(&text_size.to_le_bytes());
    out.extend_from_slice(&text_size.to_le_bytes());
    out.extend_from_slice(&PAGE_SIZE.to_le_bytes());

    while (out.len() as u64) < text_fileoff {
        out.push(0);
    }
    out.extend_from_slice(&exe.code);
    while (out.len() as u64) < file_size {
        out.push(0);
    }
}

pub fn write_i386_relocatable_object(object: &ElfObject, out: &mut Vec<u8>) {
    let shstrtab = b"\0.text\0.symtab\0.strtab\0.shstrtab\0";
    let strtab = format!("\0{}\0", object.export_name).into_bytes();
    let symtab_size = 32u32;
    let text_name = 1u32;
    let symtab_name = 7u32;
    let strtab_name = 15u32;
    let shstrtab_name = 23u32;
    let text_offset = EHDR32_SIZE;
    let symtab_offset = text_offset + object.code.len() as u32;
    let strtab_offset = symtab_offset + symtab_size;
    let shstrtab_offset = strtab_offset + strtab.len() as u32;
    let shoff = shstrtab_offset + shstrtab.len() as u32;

    out.clear();
    out.extend_from_slice(&ELF_MAGIC);
    out.push(ELFCLASS32);
    out.push(ELFDATA2LSB);
    out.push(EV_CURRENT);
    out.extend_from_slice(&[0u8; 9]);
    out.extend_from_slice(&ET_REL.to_le_bytes());
    out.extend_from_slice(&EM_386.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&shoff.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(EHDR32_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&40u16.to_le_bytes());
    out.extend_from_slice(&5u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());

    out.extend_from_slice(&object.code);
    write_symbol32(out, 0, 0, 0, 0, 0, 0);
    write_symbol32(out, 1, 0, object.code.len() as u32, 0x12, 0, 1);
    out.extend_from_slice(&strtab);
    out.extend_from_slice(shstrtab);

    write_section_header32(out, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    write_section_header32(
        out,
        text_name,
        1,
        0x6,
        0,
        text_offset,
        object.code.len() as u32,
        0,
        0,
        4,
        0,
    );
    write_section_header32(
        out,
        symtab_name,
        2,
        0,
        0,
        symtab_offset,
        symtab_size,
        3,
        1,
        4,
        16,
    );
    write_section_header32(
        out,
        strtab_name,
        3,
        0,
        0,
        strtab_offset,
        strtab.len() as u32,
        0,
        0,
        1,
        0,
    );
    write_section_header32(
        out,
        shstrtab_name,
        3,
        0,
        0,
        shstrtab_offset,
        shstrtab.len() as u32,
        0,
        0,
        1,
        0,
    );
}

pub fn write_arm32_executable(exe: &ElfExecutable, out: &mut Vec<u8>) {
    let text_fileoff = 0x1000u32;
    let text_vaddr = 0x10000u32;
    let text_size = exe.code.len() as u32;
    let file_size = text_fileoff + text_size;
    let entry_vaddr = text_vaddr + exe.entry_offset;

    out.clear();
    out.extend_from_slice(&ELF_MAGIC);
    out.push(ELFCLASS32);
    out.push(ELFDATA2LSB);
    out.push(EV_CURRENT);
    out.extend_from_slice(&[0u8; 9]);
    out.extend_from_slice(&ET_EXEC.to_le_bytes());
    out.extend_from_slice(&EM_ARM.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&entry_vaddr.to_le_bytes());
    out.extend_from_slice(&EHDR32_SIZE.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0x0500_0000u32.to_le_bytes());
    out.extend_from_slice(&(EHDR32_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());

    out.extend_from_slice(&PT_LOAD.to_le_bytes());
    out.extend_from_slice(&text_fileoff.to_le_bytes());
    out.extend_from_slice(&text_vaddr.to_le_bytes());
    out.extend_from_slice(&text_vaddr.to_le_bytes());
    out.extend_from_slice(&text_size.to_le_bytes());
    out.extend_from_slice(&text_size.to_le_bytes());
    out.extend_from_slice(&(PF_R | PF_X).to_le_bytes());
    out.extend_from_slice(&0x1000u32.to_le_bytes());

    while (out.len() as u32) < text_fileoff {
        out.push(0);
    }
    out.extend_from_slice(&exe.code);
    while (out.len() as u32) < file_size {
        out.push(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_magic_and_program_headers() {
        let exe = ElfExecutable {
            code: vec![
                0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00, 0x48, 0xC7, 0xC7, 0x2A, 0x00, 0x00, 0x00,
                0x0F, 0x05,
            ],
            entry_offset: 0,
        };
        let mut out = Vec::new();
        write_executable(&exe, &mut out);
        assert_eq!(&out[0..4], &ELF_MAGIC);
        assert_eq!(out.len(), PAGE_SIZE as usize + exe.code.len());
        assert_eq!(out[4], ELFCLASS64);
        assert_eq!(u16::from_le_bytes([out[16], out[17]]), ET_EXEC);
        assert_eq!(u16::from_le_bytes([out[18], out[19]]), EM_X86_64);
    }

    #[test]
    fn writes_x86_64_relocatable_object() {
        let object = ElfObject {
            code: x86_64_return_i32_object_code(42),
            export_name: "answer".to_string(),
            exports: vec![],
            undefs: vec!["rtos_tick".to_string()],
            x86_plt_relocs: vec![(5, "rtos_tick".to_string())],
            thumb_calls: vec![],
            thumb_addr_refs: vec![],
        };
        let mut out = Vec::new();
        write_x86_64_relocatable_object(&object, &mut out);
        assert_eq!(&out[0..4], &ELF_MAGIC);
        assert_eq!(u16::from_le_bytes([out[16], out[17]]), ET_REL);
        assert_eq!(u16::from_le_bytes([out[18], out[19]]), EM_X86_64);
        assert!(out.windows(5).any(|window| window == b".text"));
        assert!(out.windows(7).any(|window| window == b".symtab"));
        assert!(out.windows(7).any(|window| window == b".strtab"));
        assert!(out.windows(9).any(|window| window == b".shstrtab"));
        assert!(out.windows(10).any(|window| window == b".rela.text"));
        // The single relocation sits just before the section headers
        // (e_shoff at header offset 0x28): r_offset=5, r_info=(sym 2)<<32|4,
        // addend -4.
        let shoff = u64::from_le_bytes(out[40..48].try_into().unwrap()) as usize;
        let entry = &out[shoff - 24..shoff];
        assert_eq!(u64::from_le_bytes(entry[0..8].try_into().unwrap()), 5);
        assert_eq!(
            u64::from_le_bytes(entry[8..16].try_into().unwrap()),
            (2u64 << 32) | 4
        );
        assert_eq!(i64::from_le_bytes(entry[16..24].try_into().unwrap()), -4);
    }

    #[test]
    fn x86_64_relocatable_without_relocs_has_no_rela_section() {
        let object = ElfObject {
            code: x86_64_return_i32_object_code(42),
            export_name: "answer".to_string(),
            exports: vec![],
            undefs: vec![],
            x86_plt_relocs: vec![],
            thumb_calls: vec![],
            thumb_addr_refs: vec![],
        };
        let mut out = Vec::new();
        write_x86_64_relocatable_object(&object, &mut out);
        assert!(!out.windows(10).any(|window| window == b".rela.text"));
    }

    #[test]
    fn writes_aarch64_relocatable_object() {
        let object = ElfObject {
            code: aarch64_return_i32_object_code(42),
            export_name: "answer".to_string(),
            exports: vec![],
            undefs: vec![],
            x86_plt_relocs: vec![],
            thumb_calls: vec![],
            thumb_addr_refs: vec![],
        };
        let mut out = Vec::new();
        write_aarch64_relocatable_object(&object, &mut out);
        assert_eq!(&out[0..4], &ELF_MAGIC);
        assert_eq!(out[4], ELFCLASS64);
        assert_eq!(u16::from_le_bytes([out[16], out[17]]), ET_REL);
        assert_eq!(u16::from_le_bytes([out[18], out[19]]), EM_AARCH64);
        assert!(out.windows(5).any(|window| window == b".text"));
        assert!(out.windows(7).any(|window| window == b".symtab"));
        assert!(out.windows(7).any(|window| window == b".strtab"));
        assert!(out.windows(9).any(|window| window == b".shstrtab"));
        assert!(out.windows(6).any(|window| window == b"answer"));
        assert!(
            out.windows(8)
                .any(|window| window == [0x40, 0x05, 0x80, 0xD2, 0xC0, 0x03, 0x5F, 0xD6])
        );
    }

    #[test]
    fn writes_arm32_relocatable_object() {
        let object = ElfObject {
            code: arm32_return_i32_object_code(42),
            export_name: "answer".to_string(),
            exports: vec![],
            undefs: vec![],
            x86_plt_relocs: vec![],
            thumb_calls: vec![],
            thumb_addr_refs: vec![],
        };
        let mut out = Vec::new();
        write_arm32_relocatable_object(&object, &mut out);
        assert_eq!(&out[0..4], &ELF_MAGIC);
        assert_eq!(out[4], ELFCLASS32);
        assert_eq!(u16::from_le_bytes([out[16], out[17]]), ET_REL);
        assert_eq!(u16::from_le_bytes([out[18], out[19]]), EM_ARM);
        assert_eq!(
            u32::from_le_bytes([out[36], out[37], out[38], out[39]]),
            0x0500_0000
        );
        assert!(out.windows(5).any(|window| window == b".text"));
        assert!(out.windows(7).any(|window| window == b".symtab"));
        assert!(out.windows(7).any(|window| window == b".strtab"));
        assert!(out.windows(9).any(|window| window == b".shstrtab"));
        assert!(out.windows(6).any(|window| window == b"answer"));
        assert!(
            out.windows(8)
                .any(|window| window == [0x2A, 0x00, 0xA0, 0xE3, 0x1E, 0xFF, 0x2F, 0xE1])
        );
        // Thumb function symbol st_value has low bit set (value starts at 1 for offset 0).
        let text_off = EHDR32_SIZE as usize;
        let sym_off = text_off + object.code.len();
        let answer_sym = &out[sym_off + 16..sym_off + 32];
        let st_value = u32::from_le_bytes(answer_sym[4..8].try_into().unwrap());
        assert_eq!(st_value & 1, 1);
    }

    #[test]
    fn writes_arm32_thumb_addr_relocations() {
        // Function-address movw/movt pair → R_ARM_THM_MOVW_ABS_NC (47) +
        // R_ARM_THM_MOVT_ABS (48) plus the R_ARM_THM_CALL (10) for the extern.
        let object = ElfObject {
            code: vec![0u8; 64],
            export_name: "entry".to_string(),
            exports: vec![("entry".to_string(), 0), ("helper".to_string(), 32)],
            undefs: vec!["rtos_pendsv".to_string()],
            x86_plt_relocs: vec![],
            thumb_calls: vec![(12, "rtos_pendsv".to_string())],
            thumb_addr_refs: vec![(20, 24, "helper".to_string())],
        };
        let mut out = Vec::new();
        write_arm32_relocatable_object(&object, &mut out);

        // 3 relocs = 24 bytes starting at the rel section.
        let text_off = EHDR32_SIZE as usize;
        let symtab_size = (1 + 2 + 1) * 16;
        let rel_off = text_off + 64 + symtab_size;
        let rels = &out[rel_off..rel_off + 24];
        // offsets + info(sym<<8|type)
        assert_eq!(&rels[0..4], &12u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(rels[4..8].try_into().unwrap()) & 0xFF,
            10
        );
        assert_eq!(&rels[8..12], &20u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(rels[12..16].try_into().unwrap()) & 0xFF,
            47
        );
        assert_eq!(&rels[16..20], &24u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(rels[20..24].try_into().unwrap()) & 0xFF,
            48
        );
    }

    #[test]
    fn writes_x86_64_exit_executable() {
        let exe = ElfExecutable {
            code: x86_64_linux_exit_code(42),
            entry_offset: 0,
        };
        let mut out = Vec::new();
        write_executable(&exe, &mut out);
        assert_eq!(&out[0..4], &ELF_MAGIC);
        assert_eq!(u16::from_le_bytes([out[16], out[17]]), ET_EXEC);
        assert_eq!(u16::from_le_bytes([out[18], out[19]]), EM_X86_64);
        assert_eq!(u32::from_le_bytes(out[64..68].try_into().unwrap()), PT_LOAD);
        assert_eq!(
            u32::from_le_bytes(out[68..72].try_into().unwrap()),
            PF_R | PF_X
        );
        assert!(out.windows(16).any(|window| window
            == [
                0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00, 0x48, 0xC7, 0xC7, 0x2A, 0x00, 0x00, 0x00,
                0x0F, 0x05
            ]));
    }

    #[test]
    fn writes_aarch64_exit_executable() {
        let exe = ElfExecutable {
            code: aarch64_linux_exit_code(42),
            entry_offset: 0,
        };
        let mut out = Vec::new();
        write_aarch64_executable(&exe, &mut out);
        assert_eq!(&out[0..4], &ELF_MAGIC);
        assert_eq!(out[4], ELFCLASS64);
        assert_eq!(u16::from_le_bytes([out[16], out[17]]), ET_EXEC);
        assert_eq!(u16::from_le_bytes([out[18], out[19]]), EM_AARCH64);
        assert_eq!(u32::from_le_bytes(out[64..68].try_into().unwrap()), PT_LOAD);
        assert_eq!(
            u32::from_le_bytes(out[68..72].try_into().unwrap()),
            PF_R | PF_X
        );
        assert!(out.windows(12).any(|window| window
            == [
                0xA8, 0x0B, 0x80, 0xD2, 0x40, 0x05, 0x80, 0xD2, 0x01, 0x00, 0x00, 0xD4
            ]));
    }

    #[test]
    fn writes_arm32_exit_executable() {
        let exe = ElfExecutable {
            code: arm32_linux_exit_code(42),
            entry_offset: 0,
        };
        let mut out = Vec::new();
        write_arm32_executable(&exe, &mut out);
        assert_eq!(&out[0..4], &ELF_MAGIC);
        assert_eq!(out[4], ELFCLASS32);
        assert_eq!(u16::from_le_bytes([out[16], out[17]]), ET_EXEC);
        assert_eq!(u16::from_le_bytes([out[18], out[19]]), EM_ARM);
        assert_eq!(u32::from_le_bytes(out[52..56].try_into().unwrap()), PT_LOAD);
        assert_eq!(
            u32::from_le_bytes(out[76..80].try_into().unwrap()),
            PF_R | PF_X
        );
        assert!(out.windows(12).any(|window| window
            == [
                0x01, 0x70, 0xA0, 0xE3, 0x2A, 0x00, 0xA0, 0xE3, 0x00, 0x00, 0x00, 0xEF
            ]));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn roundtrip_answer_code_layout() {
        let code = vec![
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00, 0x48, 0xC7, 0xC7, 0x2A, 0x00, 0x00, 0x00,
            0x0F, 0x05,
        ];
        let exe = ElfExecutable {
            code,
            entry_offset: 0,
        };
        let path = std::path::PathBuf::from("/tmp/inauguration-elf-roundtrip");
        let mut out = Vec::new();
        write_executable(&exe, &mut out);
        std::fs::write(&path, &out).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let status = std::process::Command::new(&path).status().expect("run");
        match status.code() {
            Some(42) => {}
            other => panic!("unexpected native exit {other:?}"),
        }
        let _ = std::fs::remove_file(path);
    }
}
