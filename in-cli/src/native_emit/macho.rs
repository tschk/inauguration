const MH_MAGIC_64: u32 = 0xFEED_FACF;
const CPU_TYPE_ARM64: i32 = 0x0100_000C;
const CPU_SUBTYPE_ARM64_ALL: i32 = 0;
const MH_OBJECT: u32 = 1;
const MH_EXECUTE: u32 = 2;
const MH_DYLIB: u32 = 6;
const MH_NOUNDEFS: u32 = 0x0000_0001;
const MH_PIE: u32 = 0x0020_0000;
const MH_DYLDLINK: u32 = 0x0000_0004;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_ID_DYLIB: u32 = 0xd;
const LC_MAIN: u32 = 0x8000_0028;
const LC_BUILD_VERSION: u32 = 0x32;
const PLATFORM_MACOS: u32 = 1;
const N_SECT: u8 = 0x0e;
const N_EXT: u8 = 0x01;
const N_UNDF: u8 = 0x00;
const ARM64_RELOC_BRANCH26: u32 = 2;

const VM_PROT_NONE: i32 = 0;
const VM_PROT_READ: i32 = 1;
const VM_PROT_EXECUTE: i32 = 4;
const TEXT_PROT: i32 = VM_PROT_READ | VM_PROT_EXECUTE;

const TEXT_VMADDR: u64 = 0x0001_0000_0000;
const PAGEZERO_SIZE: u64 = 0x0001_0000_0000;
const PAGE_SIZE: u64 = 0x1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachOLinkage {
    Executable,
    Dylib,
    StaticLib,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportSymbol {
    /// Source name, before adding the Mach-O C symbol prefix.
    pub name: String,
    pub offset: u32,
}

#[derive(Debug, Clone)]
pub struct MachOImage {
    pub code: Vec<u8>,
    pub entry_offset: Option<u32>,
    pub exports: Vec<ExportSymbol>,
    /// External symbol references: (instruction_offset_in_code, symbol_name)
    pub external_refs: Vec<(u32, String)>,
}

pub fn write_image(
    image: &MachOImage,
    linkage: MachOLinkage,
    install_name: &str,
    out: &mut Vec<u8>,
) {
    match linkage {
        MachOLinkage::StaticLib => write_static_archive(image, install_name, out),
        MachOLinkage::Executable | MachOLinkage::Dylib => {
            write_macho_image(image, linkage, install_name, out)
        }
    }
}

fn write_static_archive(image: &MachOImage, member_name: &str, out: &mut Vec<u8>) {
    let object = write_relocatable_object(image);
    let name = if member_name.is_empty() {
        "object.o".to_string()
    } else if member_name.ends_with(".o") {
        member_name.to_string()
    } else {
        format!("{member_name}.o")
    };
    let mut symbols: Vec<String> = image
        .exports
        .iter()
        .map(|export| format!("_{}", export.name))
        .collect();
    symbols.sort();
    let mut strings = Vec::new();
    let mut offsets = Vec::new();
    for symbol in &symbols {
        offsets.push(u32::try_from(strings.len()).expect("archive symbol table exceeds 4 GiB"));
        strings.extend_from_slice(symbol.as_bytes());
        strings.push(0);
    }
    while !strings.len().is_multiple_of(8) {
        strings.push(0);
    }
    let table_size = offsets.len() * 8;
    let toc_size = 4 + table_size + 4 + strings.len();
    let object_offset =
        u32::try_from(8 + 60 + toc_size + toc_size % 2).expect("archive index exceeds 4 GiB");
    let mut toc = Vec::new();
    toc.extend_from_slice(
        &u32::try_from(table_size)
            .expect("archive table exceeds 4 GiB")
            .to_le_bytes(),
    );
    for offset in offsets {
        toc.extend_from_slice(&offset.to_le_bytes());
        toc.extend_from_slice(&object_offset.to_le_bytes());
    }
    toc.extend_from_slice(
        &u32::try_from(strings.len())
            .expect("archive strings exceed 4 GiB")
            .to_le_bytes(),
    );
    toc.extend_from_slice(&strings);
    write_ar_member(out, "__.SYMDEF SORTED", &toc);
    write_ar_member(out, &name, &object);
}

fn write_ar_member(out: &mut Vec<u8>, name: &str, payload: &[u8]) {
    if out.is_empty() {
        out.extend_from_slice(b"!<arch>\n");
    }
    let name_size = if name.len() > 16 {
        name.len().next_multiple_of(8)
    } else {
        0
    };
    let header_name = if name_size == 0 {
        let mut buf = [b' '; 16];
        buf[..name.len()].copy_from_slice(name.as_bytes());
        buf
    } else {
        let mut buf = [b' '; 16];
        let marker = format!("#1/{name_size}");
        buf[..marker.len()].copy_from_slice(marker.as_bytes());
        buf
    };
    let timestamp = b"0           ";
    let uid = b"0     ";
    let gid = b"0     ";
    let mode: &[u8] = b"644     ";
    let size_field = format!("{:<10}", payload.len() + name_size);
    out.extend_from_slice(&header_name);
    out.extend_from_slice(timestamp);
    out.extend_from_slice(uid);
    out.extend_from_slice(gid);
    out.extend_from_slice(mode);
    out.extend_from_slice(size_field.as_bytes());
    out.extend_from_slice(b"`\n");
    if name_size > 0 {
        out.extend_from_slice(name.as_bytes());
        out.resize(out.len() + name_size - name.len(), 0);
    }
    out.extend_from_slice(payload);
    if out.len() % 2 == 1 {
        out.push(b'\n');
    }
}

fn write_macho_image(
    image: &MachOImage,
    linkage: MachOLinkage,
    install_name: &str,
    out: &mut Vec<u8>,
) {
    let (file_type, flags, text_vmaddr) = match linkage {
        MachOLinkage::Executable => (MH_EXECUTE, MH_NOUNDEFS | MH_PIE | MH_DYLDLINK, TEXT_VMADDR),
        MachOLinkage::Dylib => (MH_DYLIB, MH_NOUNDEFS | MH_DYLDLINK, TEXT_VMADDR),
        MachOLinkage::StaticLib => (MH_OBJECT, MH_NOUNDEFS, 0),
    };

    let pagezero_cmd_size = if linkage == MachOLinkage::StaticLib {
        0
    } else {
        72
    };
    let text_cmd_size = 72 + 80;
    let linkedit_cmd_size = if image.exports.is_empty() && linkage != MachOLinkage::Executable {
        0
    } else {
        72
    };
    let main_cmd_size = if linkage == MachOLinkage::Executable {
        24
    } else {
        0
    };
    let id_dylib_cmd_size = if linkage == MachOLinkage::Dylib {
        align8(24 + install_name.len())
    } else {
        0
    };
    let symtab_cmd_size = if image.exports.is_empty() { 0 } else { 24 };
    let dysymtab_cmd_size = if image.exports.is_empty() { 0 } else { 80 };
    let build_version_cmd_size = if linkage == MachOLinkage::StaticLib {
        0
    } else {
        24
    };

    let ncmds = u32::from(pagezero_cmd_size > 0)
        + 1
        + u32::from(linkedit_cmd_size > 0)
        + u32::from(main_cmd_size > 0)
        + u32::from(id_dylib_cmd_size > 0)
        + u32::from(symtab_cmd_size > 0)
        + u32::from(dysymtab_cmd_size > 0)
        + u32::from(build_version_cmd_size > 0);
    let sizeofcmds = pagezero_cmd_size
        + text_cmd_size
        + linkedit_cmd_size
        + main_cmd_size
        + id_dylib_cmd_size
        + symtab_cmd_size
        + dysymtab_cmd_size
        + build_version_cmd_size;

    let text_fileoff = if linkage == MachOLinkage::StaticLib {
        u64::from(align8(sizeofcmds as usize))
    } else {
        PAGE_SIZE
    };
    let code_size = image.code.len() as u64;
    let text_vmsize = code_size.next_multiple_of(PAGE_SIZE).max(PAGE_SIZE);

    let linkedit = build_linkedit(image, text_vmaddr);
    let linkedit_fileoff = text_fileoff + text_vmsize;
    let linkedit_filesize = if image.exports.is_empty() && linkage == MachOLinkage::Executable {
        PAGE_SIZE
    } else {
        linkedit.total_size()
    };
    let file_size = linkedit_fileoff + linkedit_filesize.next_multiple_of(PAGE_SIZE);

    out.clear();
    out.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
    out.extend_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
    out.extend_from_slice(&CPU_SUBTYPE_ARM64_ALL.to_le_bytes());
    out.extend_from_slice(&file_type.to_le_bytes());
    out.extend_from_slice(&ncmds.to_le_bytes());
    out.extend_from_slice(&sizeofcmds.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    if pagezero_cmd_size > 0 {
        write_pagezero_command(out, pagezero_cmd_size);
    }
    write_text_command(
        out,
        text_cmd_size,
        text_fileoff,
        code_size,
        text_vmsize,
        text_vmaddr,
    );
    if linkedit_cmd_size > 0 {
        write_linkedit_command(
            out,
            linkedit_cmd_size,
            linkedit_fileoff,
            linkedit_filesize,
            text_vmaddr + text_vmsize,
        );
    }
    if id_dylib_cmd_size > 0 {
        write_id_dylib_command(out, id_dylib_cmd_size, install_name);
    }
    if symtab_cmd_size > 0 {
        write_symtab_command(
            out,
            symtab_cmd_size,
            linkedit_fileoff,
            linkedit.symtab.len() as u32,
            linkedit_fileoff + linkedit.symtab.len() as u64,
            linkedit.strtab.len() as u32,
        );
    }
    if dysymtab_cmd_size > 0 {
        write_dysymtab_command(out, dysymtab_cmd_size, image.exports.len() as u32);
    }
    if main_cmd_size > 0 {
        let entryoff = image.entry_offset.unwrap_or(0);
        write_main_command(out, main_cmd_size, text_fileoff + u64::from(entryoff));
    }
    if build_version_cmd_size > 0 {
        write_build_version_command(out, build_version_cmd_size);
    }

    while (out.len() as u64) < text_fileoff {
        out.push(0);
    }
    out.extend_from_slice(&image.code);
    while (out.len() as u64) < linkedit_fileoff {
        out.push(0);
    }
    if !image.exports.is_empty() {
        out.extend_from_slice(&linkedit.symtab);
        out.extend_from_slice(&linkedit.strtab);
    }
    while (out.len() as u64) < file_size {
        out.push(0);
    }
}

struct LinkEditTables {
    symtab: Vec<u8>,
    strtab: Vec<u8>,
}

impl LinkEditTables {
    fn total_size(&self) -> u64 {
        (self.symtab.len() + self.strtab.len()) as u64
    }
}

fn external_symbol_name(name: &str) -> String {
    if name.starts_with('_') {
        name.to_string()
    } else {
        format!("_{name}")
    }
}

fn build_linkedit(image: &MachOImage, text_vmaddr: u64) -> LinkEditTables {
    let mut strtab = vec![0u8];
    let mut symtab = Vec::new();
    for export in &image.exports {
        let sym_name = format!("_{}", export.name);
        let strx = strtab.len() as u32;
        strtab.extend_from_slice(sym_name.as_bytes());
        strtab.push(0);
        symtab.extend_from_slice(&strx.to_le_bytes());
        symtab.push(N_SECT | N_EXT);
        symtab.push(1);
        symtab.extend_from_slice(&0u16.to_le_bytes());
        symtab.extend_from_slice(&(text_vmaddr + u64::from(export.offset)).to_le_bytes());
    }
    LinkEditTables { symtab, strtab }
}

fn align8(value: usize) -> u32 {
    value.next_multiple_of(8) as u32
}

fn write_pagezero_command(out: &mut Vec<u8>, cmdsize: u32) {
    out.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
    out.extend_from_slice(&cmdsize.to_le_bytes());
    write_fixed_name(out, "__PAGEZERO");
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&PAGEZERO_SIZE.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&VM_PROT_NONE.to_le_bytes());
    out.extend_from_slice(&VM_PROT_NONE.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

fn write_text_command(
    out: &mut Vec<u8>,
    cmdsize: u32,
    fileoff: u64,
    code_size: u64,
    vmsize: u64,
    vmaddr: u64,
) {
    out.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
    out.extend_from_slice(&cmdsize.to_le_bytes());
    write_fixed_name(out, "__TEXT");
    out.extend_from_slice(&vmaddr.to_le_bytes());
    out.extend_from_slice(&vmsize.to_le_bytes());
    out.extend_from_slice(&fileoff.to_le_bytes());
    out.extend_from_slice(&vmsize.to_le_bytes());
    out.extend_from_slice(&TEXT_PROT.to_le_bytes());
    out.extend_from_slice(&TEXT_PROT.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    write_fixed_name(out, "__text");
    write_fixed_name(out, "__TEXT");
    out.extend_from_slice(&vmaddr.to_le_bytes());
    out.extend_from_slice(&code_size.to_le_bytes());
    out.extend_from_slice(&(fileoff as u32).to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0x8000_0400u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

fn write_linkedit_command(
    out: &mut Vec<u8>,
    cmdsize: u32,
    fileoff: u64,
    filesize: u64,
    vmaddr: u64,
) {
    out.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
    out.extend_from_slice(&cmdsize.to_le_bytes());
    write_fixed_name(out, "__LINKEDIT");
    out.extend_from_slice(&vmaddr.to_le_bytes());
    out.extend_from_slice(&filesize.to_le_bytes());
    out.extend_from_slice(&fileoff.to_le_bytes());
    out.extend_from_slice(&filesize.to_le_bytes());
    out.extend_from_slice(&VM_PROT_READ.to_le_bytes());
    out.extend_from_slice(&VM_PROT_READ.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

fn write_id_dylib_command(out: &mut Vec<u8>, cmdsize: u32, install_name: &str) {
    out.extend_from_slice(&LC_ID_DYLIB.to_le_bytes());
    out.extend_from_slice(&cmdsize.to_le_bytes());
    out.extend_from_slice(&24u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0x0001_0000u32.to_le_bytes());
    out.extend_from_slice(&0x0001_0000u32.to_le_bytes());
    out.extend_from_slice(install_name.as_bytes());
    let padding = cmdsize as usize - 24 - install_name.len();
    out.extend(std::iter::repeat_n(0u8, padding));
}

fn write_symtab_command(
    out: &mut Vec<u8>,
    cmdsize: u32,
    symoff: u64,
    nsyms: u32,
    stroff: u64,
    strsize: u32,
) {
    out.extend_from_slice(&LC_SYMTAB.to_le_bytes());
    out.extend_from_slice(&cmdsize.to_le_bytes());
    out.extend_from_slice(&(symoff as u32).to_le_bytes());
    out.extend_from_slice(&nsyms.to_le_bytes());
    out.extend_from_slice(&(stroff as u32).to_le_bytes());
    out.extend_from_slice(&strsize.to_le_bytes());
}

fn write_dysymtab_command(out: &mut Vec<u8>, cmdsize: u32, nextdefsym: u32) {
    out.extend_from_slice(&LC_DYSYMTAB.to_le_bytes());
    out.extend_from_slice(&cmdsize.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&nextdefsym.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

fn write_build_version_command(out: &mut Vec<u8>, cmdsize: u32) {
    out.extend_from_slice(&LC_BUILD_VERSION.to_le_bytes());
    out.extend_from_slice(&cmdsize.to_le_bytes());
    out.extend_from_slice(&PLATFORM_MACOS.to_le_bytes());
    out.extend_from_slice(&0x000c_0000u32.to_le_bytes());
    out.extend_from_slice(&0x000f_0000u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
}

fn write_main_command(out: &mut Vec<u8>, cmdsize: u32, entryoff: u64) {
    out.extend_from_slice(&LC_MAIN.to_le_bytes());
    out.extend_from_slice(&cmdsize.to_le_bytes());
    out.extend_from_slice(&entryoff.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
}

fn write_fixed_name(out: &mut Vec<u8>, name: &str) {
    let mut buf = [0u8; 16];
    let bytes = name.as_bytes();
    let len = bytes.len().min(16);
    buf[..len].copy_from_slice(&bytes[..len]);
    out.extend_from_slice(&buf);
}

/// Write a Mach-O relocatable object (.o) suitable for linking with `ld`.
/// Includes proper ARM64_RELOC_BRANCH26 relocation entries for external symbols.
pub fn write_relocatable_object(image: &MachOImage) -> Vec<u8> {
    let mut out = Vec::new();
    let code = &image.code;
    let code_size = code.len();

    let num_def_syms = image.exports.len();

    // Build string table
    let mut strtab = vec![0u8];
    let mut str_offsets: Vec<(String, u32)> = Vec::new();
    let mut exports_sorted = image.exports.clone();
    exports_sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for export in &exports_sorted {
        let off = strtab.len() as u32;
        strtab.extend_from_slice(format!("_{}", export.name).as_bytes());
        strtab.push(0);
        str_offsets.push((export.name.clone(), off));
    }

    let mut ext_str_offsets: Vec<(String, u32)> = Vec::new();
    for (_, name) in &image.external_refs {
        if !ext_str_offsets.iter().any(|(n, _)| n == name) {
            let off = strtab.len() as u32;
            strtab.extend_from_slice(external_symbol_name(name).as_bytes());
            strtab.push(0);
            ext_str_offsets.push((name.clone(), off));
        }
    }

    while !strtab.len().is_multiple_of(8) {
        strtab.push(0);
    }
    let num_syms: u32 = num_def_syms as u32 + ext_str_offsets.len() as u32;
    let num_def_syms_u32 = num_def_syms as u32;
    let num_undef_syms = ext_str_offsets.len() as u32;
    let num_cmds: u32 = 4; // segment + symtab + build_version + dysymtab

    // Calculate layout
    let segment_cmd_size: u32 = 72 + 80; // seg + 1 section
    let symtab_cmd_size: u32 = 24;
    let build_version_cmd_size: u32 = 24;
    let dysymtab_cmd_size: u32 = 80;
    let header_size: u32 = 32; // mach_header_64
    let sizeofcmds =
        segment_cmd_size + symtab_cmd_size + build_version_cmd_size + dysymtab_cmd_size;
    let text_fileoff = header_size + sizeofcmds; // header + load commands
    let text_vmsize = ((code_size as u32) + 3) & !3u32; // align to 4
    let strtab_off = text_fileoff + text_vmsize;
    const NLIST64_SIZE: u32 = 16; // n_strx(4) + n_type(1) + n_sect(1) + n_desc(2) + n_value(8)
    let nlist_off = strtab_off + strtab.len() as u32;
    let file_size = nlist_off + num_syms * NLIST64_SIZE;

    // Relocation entries
    let reloc_data: Vec<(u32, u32)> = image
        .external_refs
        .iter()
        .map(|(site, name)| {
            let undef_idx = num_def_syms_u32
                + ext_str_offsets
                    .iter()
                    .position(|(n, _)| n == name)
                    .unwrap_or(0) as u32;
            (*site, undef_idx)
        })
        .collect();
    let reloc_off = file_size;
    let reloc_count = reloc_data.len() as u32;

    // Mach-O 64 header: magic + cputype + cpusubtype + filetype + ncmds + sizeofcmds + flags + reserved = 32 bytes
    out.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
    out.extend_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
    out.extend_from_slice(&CPU_SUBTYPE_ARM64_ALL.to_le_bytes());
    out.extend_from_slice(&MH_OBJECT.to_le_bytes());
    out.extend_from_slice(&num_cmds.to_le_bytes());
    out.extend_from_slice(&sizeofcmds.to_le_bytes());
    out.extend_from_slice(&0x2000u32.to_le_bytes()); // flags: MH_SUBSECT_VIA_SYMBOLS
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved

    // Segment (MH_OBJECT: segname should be empty, only section has segname)
    out.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
    out.extend_from_slice(&segment_cmd_size.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // segname = empty for MH_OBJECT
    out.extend_from_slice(&0u64.to_le_bytes()); // vmaddr (0 for .o)
    out.extend_from_slice(&(text_vmsize as u64).to_le_bytes()); // vmsize
    out.extend_from_slice(&(text_fileoff as u64).to_le_bytes()); // fileoff = section offset
    out.extend_from_slice(&(text_vmsize as u64).to_le_bytes()); // filesize = section size
    out.extend_from_slice(&(TEXT_PROT | 0x2).to_le_bytes()); // maxprot rwx (for ld compat)
    out.extend_from_slice(&(TEXT_PROT | 0x2).to_le_bytes()); // initprot rwx
    out.extend_from_slice(&1u32.to_le_bytes()); // nsects
    out.extend_from_slice(&0u32.to_le_bytes()); // flags

    // Section __TEXT,__text (segment name = __TEXT, section name = __text)
    write_fixed_name(&mut out, "__text"); // sectname
    write_fixed_name(&mut out, "__TEXT"); // segname
    out.extend_from_slice(&0u64.to_le_bytes()); // addr (0 for .o)
    out.extend_from_slice(&(text_vmsize as u64).to_le_bytes()); // size
    out.extend_from_slice(&text_fileoff.to_le_bytes()); // offset
    out.extend_from_slice(&2u32.to_le_bytes()); // align (2^2 = 4)
    out.extend_from_slice(&reloc_off.to_le_bytes()); // reloff
    out.extend_from_slice(&reloc_count.to_le_bytes()); // nreloc
    out.extend_from_slice(&0x8000_0400u32.to_le_bytes()); // flags: S_REGULAR | ATTR_PURE_INSTRUCTIONS | ATTR_SOME_INSTRUCTIONS
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved1
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved2
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved3

    // LC_BUILD_VERSION (must come before SYMTAB per Apple linker convention)
    out.extend_from_slice(&LC_BUILD_VERSION.to_le_bytes());
    out.extend_from_slice(&build_version_cmd_size.to_le_bytes());
    out.extend_from_slice(&PLATFORM_MACOS.to_le_bytes());
    out.extend_from_slice(&0x000E_0000u32.to_le_bytes()); // minos 14.0
    out.extend_from_slice(&0x000E_0000u32.to_le_bytes()); // sdk 14.0
    out.extend_from_slice(&0u32.to_le_bytes()); // ntools

    // LC_SYMTAB
    out.extend_from_slice(&LC_SYMTAB.to_le_bytes());
    out.extend_from_slice(&symtab_cmd_size.to_le_bytes());
    out.extend_from_slice(&nlist_off.to_le_bytes());
    out.extend_from_slice(&num_syms.to_le_bytes());
    out.extend_from_slice(&strtab_off.to_le_bytes());
    out.extend_from_slice(&(strtab.len() as u32).to_le_bytes());

    // LC_DYSYMTAB — tells linker symbol counts and relocation layout
    // Defined exports precede undefined symbols; no synthetic locals are needed.
    let nlocalsym: u32 = 0;
    let iextdefsym: u32 = nlocalsym;
    let iundefsym: u32 = nlocalsym + num_def_syms_u32;
    out.extend_from_slice(&LC_DYSYMTAB.to_le_bytes());
    out.extend_from_slice(&dysymtab_cmd_size.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // ilocalsym
    out.extend_from_slice(&nlocalsym.to_le_bytes()); // nlocalsym
    out.extend_from_slice(&iextdefsym.to_le_bytes()); // iextdefsym
    out.extend_from_slice(&num_def_syms_u32.to_le_bytes()); // nextdefsym
    out.extend_from_slice(&iundefsym.to_le_bytes()); // iundefsym
    out.extend_from_slice(&num_undef_syms.to_le_bytes()); // nundefsym
    // Zero out the rest (tocoff, ntoc, modtaboff, nmodtab, extrefsymoff, nextrefsyms,
    // indirectsymoff, nindirectsyms, extreloff, nextrel, locreloff, nlocrel)
    out.extend_from_slice(&0u32.to_le_bytes()); // tocoff
    out.extend_from_slice(&0u32.to_le_bytes()); // ntoc
    out.extend_from_slice(&0u32.to_le_bytes()); // modtaboff
    out.extend_from_slice(&0u32.to_le_bytes()); // nmodtab
    out.extend_from_slice(&0u32.to_le_bytes()); // extrefsymoff
    out.extend_from_slice(&0u32.to_le_bytes()); // nextrefsyms
    out.extend_from_slice(&0u32.to_le_bytes()); // indirectsymoff
    out.extend_from_slice(&0u32.to_le_bytes()); // nindirectsyms
    out.extend_from_slice(&0u32.to_le_bytes()); // section owns the relocation table
    out.extend_from_slice(&0u32.to_le_bytes()); // nextrel
    out.extend_from_slice(&0u32.to_le_bytes()); // locreloff
    out.extend_from_slice(&0u32.to_le_bytes()); // nlocrel

    // Code
    out.extend_from_slice(code);
    let pad = text_vmsize as usize - code.len();
    if pad > 0 {
        out.extend_from_slice(&vec![0u8; pad]);
    }

    // String table
    out.extend_from_slice(&strtab);

    // Symbol table — each nlist_64 entry is 16 bytes: n_strx(4) + n_type(1) + n_sect(1) + n_desc(2) + n_value(8)
    // Defined exports
    for export in &exports_sorted {
        let &s_off = str_offsets
            .iter()
            .find(|(n, _)| n == &export.name)
            .map(|(_, o)| o)
            .unwrap_or(&0u32);
        out.extend_from_slice(&s_off.to_le_bytes()); // n_strx
        out.extend_from_slice(&[N_SECT | N_EXT, 1u8, 0u8, 0u8]); // n_type, n_sect, n_desc
        out.extend_from_slice(&(export.offset as u64).to_le_bytes()); // n_value
    }

    // Undefined externals
    for (_, s_off) in &ext_str_offsets {
        out.extend_from_slice(&s_off.to_le_bytes()); // n_strx
        out.extend_from_slice(&[N_UNDF | N_EXT, 0u8, 0u8, 0u8]); // n_type, n_sect, n_desc
        out.extend_from_slice(&0u64.to_le_bytes()); // n_value
    }

    // Relocation entries (generic relocation_info format)
    // r_symbolnum(24), r_pcrel(1), r_length(2), r_extern(1), r_type(4)
    for &(site, sym_idx) in &reloc_data {
        out.extend_from_slice(&site.to_le_bytes());
        let info = sym_idx | (1 << 24) | (2 << 25) | (1 << 27) | (ARM64_RELOC_BRANCH26 << 28);
        out.extend_from_slice(&info.to_le_bytes());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_image() -> MachOImage {
        MachOImage {
            code: vec![0xD6, 0x5F, 0x03, 0xC0],
            entry_offset: Some(0),
            exports: vec![ExportSymbol {
                name: "answer".into(),
                offset: 0,
            }],
            external_refs: Vec::new(),
        }
    }

    #[test]
    fn writes_magic_and_load_commands() {
        let image = MachOImage {
            code: vec![0xD6, 0x5F, 0x03, 0xC0],
            entry_offset: Some(0),
            exports: Vec::new(),
            external_refs: Vec::new(),
        };
        let mut out = Vec::new();
        write_image(&image, MachOLinkage::Executable, "", &mut out);
        assert_eq!(&out[0..4], &MH_MAGIC_64.to_le_bytes());
        assert_eq!(out.len(), (PAGE_SIZE * 3) as usize);
    }

    #[test]
    fn dylib_uses_mh_dylib_file_type() {
        let image = sample_image();
        let mut out = Vec::new();
        write_image(
            &image,
            MachOLinkage::Dylib,
            "@rpath/libsample.dylib",
            &mut out,
        );
        assert_eq!(&out[0..4], &MH_MAGIC_64.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(out[12..16].try_into().unwrap()),
            MH_DYLIB
        );
    }

    #[test]
    fn staticlib_writes_ar_archive() {
        for (member, export) in [
            ("sample", "answer"),
            ("a-long-static-library-object-name", "_answer"),
        ] {
            let mut image = sample_image();
            image.exports[0].name = export.into();
            let mut out = Vec::new();
            write_image(&image, MachOLinkage::StaticLib, member, &mut out);
            assert_eq!(&out[..8], b"!<arch>\n");
            assert_eq!(&out[66..68], b"`\n");
            let toc_size = std::str::from_utf8(&out[56..66])
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            let object_header = 68 + toc_size + toc_size % 2;
            let header_name = std::str::from_utf8(&out[object_header..object_header + 16])
                .unwrap()
                .trim();
            let name_size = header_name
                .strip_prefix("#1/")
                .map(|size| size.parse::<usize>().unwrap())
                .unwrap_or(0);
            let object_offset = object_header + 60 + name_size;
            assert!(object_offset.is_multiple_of(8));
            assert_eq!(&out[object_header + 58..object_header + 60], b"`\n");
            assert_eq!(
                u32::from_le_bytes(out[76..80].try_into().unwrap()) as usize,
                object_header
            );
            assert_eq!(
                u32::from_le_bytes(out[object_offset..object_offset + 4].try_into().unwrap()),
                MH_MAGIC_64
            );
            assert_eq!(
                i32::from_le_bytes(
                    out[object_offset + 4..object_offset + 8]
                        .try_into()
                        .unwrap()
                ),
                CPU_TYPE_ARM64
            );
            assert_eq!(
                u32::from_le_bytes(
                    out[object_offset + 12..object_offset + 16]
                        .try_into()
                        .unwrap()
                ),
                MH_OBJECT
            );
            assert!(String::from_utf8_lossy(&out).contains("_answer"));
            assert!(String::from_utf8_lossy(&out).contains(&format!("_{export}")));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn staticlib_archive_is_readable_by_system_nm() {
        for member in ["sample", "a-long-static-library-object-name"] {
            let mut bytes = Vec::new();
            write_image(&sample_image(), MachOLinkage::StaticLib, member, &mut bytes);
            let path = std::env::temp_dir().join(format!(
                "inauguration-macho-archive-{}-{}.a",
                std::process::id(),
                member.len()
            ));
            std::fs::write(&path, bytes).unwrap();
            let result = std::process::Command::new("/usr/bin/nm")
                .arg(&path)
                .output();
            std::fs::remove_file(&path).unwrap();
            let output = result.expect("run system nm on emitted static archive");
            assert!(
                output.status.success(),
                "{member}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.split_whitespace().last() == Some("_answer")),
                "{member}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    #[test]
    fn dylib_exports_symbols_in_linkedit() {
        let image = sample_image();
        let mut out = Vec::new();
        write_image(
            &image,
            MachOLinkage::Dylib,
            "@rpath/libsample.dylib",
            &mut out,
        );
        let haystack = String::from_utf8_lossy(&out);
        assert!(haystack.contains("_answer"));
    }
}
