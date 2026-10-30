//! Linkable x86 objects with independent executable code and writable data.
use super::*;
use crate::native_emit::{AbsoluteReference, NativeSection};
use std::collections::{HashMap, HashSet};

pub struct X86RelocatableObject<'a> {
    pub code: &'a [u8],
    pub data: &'a [u8],
    pub exports: &'a [(String, u32)],
    pub undefs: &'a [String],
    /// Offsets of call disp32 operands, not instruction starts.
    pub calls: &'a [(u32, String)],
    pub absolute_refs: &'a [AbsoluteReference],
    /// Eight-byte data slots pointing to section-relative data offsets.
    pub data_relocations: &'a [(u64, u64)],
}

fn checked_range(site: u64, width: u64, length: usize) -> Result<std::ops::Range<usize>, String> {
    let end = site
        .checked_add(width)
        .ok_or("x86 ELF relocation range overflow")?;
    if end > length as u64 {
        return Err("x86 ELF relocation site outside section".into());
    }
    Ok(
        usize::try_from(site).map_err(|_| "x86 ELF site exceeds host size")?
            ..usize::try_from(end).map_err(|_| "x86 ELF end exceeds host size")?,
    )
}

fn check_nonoverlap(ranges: &mut [(u64, u64)]) -> Result<(), String> {
    ranges.sort_unstable();
    if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("x86 ELF overlapping relocation sites".into());
    }
    Ok(())
}

fn string_offset(strings: &mut Vec<u8>, name: &str) -> Result<u32, String> {
    if name.is_empty() || name.as_bytes().contains(&0) {
        return Err("x86 ELF symbol names must be nonempty and contain no NUL".into());
    }
    let offset = u32::try_from(strings.len()).map_err(|_| "x86 ELF string table too large")?;
    strings.extend_from_slice(name.as_bytes());
    strings.push(0);
    u32::try_from(strings.len()).map_err(|_| "x86 ELF string table too large")?;
    Ok(offset)
}

pub fn write_x86_data_object(
    object: &X86RelocatableObject<'_>,
    is_64: bool,
) -> Result<Vec<u8>, String> {
    let pointer_width = if is_64 { 8 } else { 4 };
    let mut names = HashSet::new();
    let mut symbols = HashMap::new();
    let mut strings = vec![0];
    let mut symtab = vec![0; if is_64 { 24 } else { 16 }];
    // Local section symbols precede every global; sh_info is therefore 3.
    for section in [1, 2] {
        if is_64 {
            write_symbol(&mut symtab, 0, 3, 0, section, 0, 0);
        } else {
            write_symbol32(&mut symtab, 0, 0, 0, 3, 0, section);
        }
    }
    for (index, (name, offset)) in object.exports.iter().enumerate() {
        if u64::from(*offset) >= object.code.len() as u64 {
            return Err(format!("x86 ELF export `{name}` outside code"));
        }
        if !names.insert(name.as_str()) {
            return Err(format!("x86 ELF duplicate symbol `{name}`"));
        }
        let symbol = u32::try_from(index)
            .ok()
            .and_then(|n| n.checked_add(3))
            .ok_or("x86 ELF too many symbols")?;
        symbols.insert(name.as_str(), symbol);
        let string = string_offset(&mut strings, name)?;
        if is_64 {
            write_symbol(&mut symtab, string, 0x12, 0, 1, u64::from(*offset), 0);
        } else {
            write_symbol32(&mut symtab, string, *offset, 0, 0x12, 0, 1);
        }
    }
    for name in object.undefs {
        if !names.insert(name.as_str()) {
            return Err(format!("x86 ELF duplicate symbol `{name}`"));
        }
        let symbol = u32::try_from(symbols.len())
            .ok()
            .and_then(|n| n.checked_add(3))
            .ok_or("x86 ELF too many symbols")?;
        symbols.insert(name.as_str(), symbol);
        let string = string_offset(&mut strings, name)?;
        if is_64 {
            write_symbol(&mut symtab, string, 0x10, 0, 0, 0, 0);
        } else {
            write_symbol32(&mut symtab, string, 0, 0, 0x10, 0, 0);
        }
    }
    if !is_64 && symbols.len() > 0x00ff_fffc {
        return Err("x86 ELF32 relocation symbol index exceeds 24 bits".into());
    }
    let mut code = object.code.to_vec();
    let mut data = object.data.to_vec();
    let mut text_relocs = Vec::new();
    let mut data_relocs = Vec::new();
    let mut text_ranges = Vec::new();
    let mut data_ranges = Vec::new();
    for (site, name) in object.calls {
        let site = u64::from(*site);
        let range = checked_range(site, 4, code.len())?;
        let symbol = *symbols
            .get(name.as_str())
            .ok_or_else(|| format!("x86 ELF call symbol `{name}` not declared"))?;
        text_ranges.push((site, site + 4));
        if is_64 {
            code[range].fill(0);
            text_relocs.extend_from_slice(&site.to_le_bytes());
            text_relocs.extend_from_slice(&((u64::from(symbol) << 32) | 4).to_le_bytes());
            text_relocs.extend_from_slice(&(-4_i64).to_le_bytes());
        } else {
            code[range].copy_from_slice(&(-4_i32).to_le_bytes());
            text_relocs.extend_from_slice(
                &u32::try_from(site)
                    .map_err(|_| "x86 ELF32 site overflow")?
                    .to_le_bytes(),
            );
            text_relocs.extend_from_slice(&((symbol << 8) | 2).to_le_bytes());
        }
    }
    for reference in object.absolute_refs {
        if u64::from(reference.width) != pointer_width {
            return Err("x86 ELF absolute relocation width does not match target".into());
        }
        let site = u64::from(reference.site);
        let range = checked_range(site, pointer_width, code.len())?;
        let (symbol, length) = match reference.target_section {
            NativeSection::Code => (1_u64, code.len()),
            NativeSection::Data => (2_u64, data.len()),
        };
        if reference.target_offset >= length as u64 {
            return Err("x86 ELF absolute relocation target outside section".into());
        }
        text_ranges.push((site, site + pointer_width));
        if is_64 {
            code[range].fill(0);
            text_relocs.extend_from_slice(&site.to_le_bytes());
            text_relocs.extend_from_slice(&((symbol << 32) | 1).to_le_bytes());
            text_relocs.extend_from_slice(
                &i64::try_from(reference.target_offset)
                    .map_err(|_| "x86 ELF64 addend overflow")?
                    .to_le_bytes(),
            );
        } else {
            code[range].copy_from_slice(
                &u32::try_from(reference.target_offset)
                    .map_err(|_| "x86 ELF32 addend overflow")?
                    .to_le_bytes(),
            );
            text_relocs.extend_from_slice(
                &u32::try_from(site)
                    .map_err(|_| "x86 ELF32 site overflow")?
                    .to_le_bytes(),
            );
            text_relocs.extend_from_slice(
                &((u32::try_from(symbol).map_err(|_| "x86 ELF32 symbol overflow")? << 8) | 1)
                    .to_le_bytes(),
            );
        }
    }
    for &(site, target) in object.data_relocations {
        if !site.is_multiple_of(8) {
            return Err("x86 ELF data relocation slot must be eight-byte aligned".into());
        }
        let range = checked_range(site, 8, data.len())?;
        if target >= data.len() as u64 {
            return Err("x86 ELF data relocation target outside data".into());
        }
        data_ranges.push((site, site + 8));
        data[range.clone()].fill(0);
        if is_64 {
            data_relocs.extend_from_slice(&site.to_le_bytes());
            data_relocs.extend_from_slice(&((2_u64 << 32) | 1).to_le_bytes());
            data_relocs.extend_from_slice(
                &i64::try_from(target)
                    .map_err(|_| "x86 ELF64 data addend overflow")?
                    .to_le_bytes(),
            );
        } else {
            data[range.start..range.start + 4].copy_from_slice(
                &u32::try_from(target)
                    .map_err(|_| "x86 ELF32 data addend overflow")?
                    .to_le_bytes(),
            );
            data_relocs.extend_from_slice(
                &u32::try_from(site)
                    .map_err(|_| "x86 ELF32 data site overflow")?
                    .to_le_bytes(),
            );
            data_relocs.extend_from_slice(&((2_u32 << 8) | 1).to_le_bytes());
        }
    }
    check_nonoverlap(&mut text_ranges)?;
    check_nonoverlap(&mut data_ranges)?;
    let relocation_names = if is_64 {
        [".rela.text", ".rela.data"]
    } else {
        [".rel.text", ".rel.data"]
    };
    let section_names = [
        ".text",
        ".data",
        relocation_names[0],
        relocation_names[1],
        ".symtab",
        ".strtab",
        ".shstrtab",
        ".note.GNU-stack",
    ];
    let mut shstrings = vec![0];
    let mut section_name_offsets = Vec::new();
    for name in section_names {
        section_name_offsets.push(string_offset(&mut shstrings, name)?);
    }
    let sections = [
        code,
        data,
        text_relocs,
        data_relocs,
        symtab,
        strings,
        shstrings,
        Vec::new(),
    ];
    let alignments: [u64; 8] = [16, 8, pointer_width, pointer_width, pointer_width, 1, 1, 1];
    let mut output = vec![0; if is_64 { 64 } else { 52 }];
    let mut offsets = Vec::new();
    for (section, alignment) in sections.iter().zip(alignments) {
        while !(output.len() as u64).is_multiple_of(alignment) {
            output.push(0);
        }
        offsets.push(output.len() as u64);
        output.extend_from_slice(section);
    }
    while !(output.len() as u64).is_multiple_of(pointer_width) {
        output.push(0);
    }
    let shoff = output.len() as u64;
    let final_size = shoff
        .checked_add(if is_64 { 9 * 64 } else { 9 * 40 })
        .ok_or("x86 ELF size overflow")?;
    if !is_64 {
        u32::try_from(final_size).map_err(|_| "x86 ELF32 file exceeds 32-bit offsets")?;
    }
    output.resize(output.len() + if is_64 { 64 } else { 40 }, 0);
    for index in 0..8 {
        let (typ, flags, link, info, entsize) = match index {
            0 => (1, 6, 0, 0, 0),
            1 => (1, 3, 0, 0, 0),
            2 | 3 => (
                if is_64 { 4 } else { 9 },
                0,
                5,
                if index == 2 { 1 } else { 2 },
                if is_64 { 24 } else { 8 },
            ),
            4 => (2, 0, 6, 3, if is_64 { 24 } else { 16 }),
            7 => (1, 0, 0, 0, 0), // non-allocating, non-executable GNU stack note
            _ => (3, 0, 0, 0, 0),
        };
        if is_64 {
            write_section_header(
                &mut output,
                section_name_offsets[index],
                typ,
                flags,
                0,
                offsets[index],
                sections[index].len() as u64,
                alignments[index],
                entsize,
                link,
                info,
            );
        } else {
            write_section_header32(
                &mut output,
                section_name_offsets[index],
                typ,
                u32::try_from(flags).map_err(|_| "x86 ELF32 flags overflow")?,
                0,
                u32::try_from(offsets[index]).map_err(|_| "x86 ELF32 offset overflow")?,
                u32::try_from(sections[index].len()).map_err(|_| "x86 ELF32 section overflow")?,
                link,
                info,
                u32::try_from(alignments[index]).map_err(|_| "x86 ELF32 alignment overflow")?,
                u32::try_from(entsize).map_err(|_| "x86 ELF32 entry size overflow")?,
            );
        }
    }
    let mut header = Vec::new();
    header.extend_from_slice(&ELF_MAGIC);
    header.extend_from_slice(&[
        if is_64 { ELFCLASS64 } else { ELFCLASS32 },
        ELFDATA2LSB,
        EV_CURRENT,
    ]);
    header.extend_from_slice(&[0; 9]);
    header.extend_from_slice(&ET_REL.to_le_bytes());
    header.extend_from_slice(&(if is_64 { EM_X86_64 } else { EM_386 }).to_le_bytes());
    header.extend_from_slice(&1_u32.to_le_bytes());
    if is_64 {
        header.extend_from_slice(&0_u64.to_le_bytes());
        header.extend_from_slice(&0_u64.to_le_bytes());
        header.extend_from_slice(&shoff.to_le_bytes());
    } else {
        header.extend_from_slice(&0_u32.to_le_bytes());
        header.extend_from_slice(&0_u32.to_le_bytes());
        header.extend_from_slice(
            &u32::try_from(shoff)
                .map_err(|_| "x86 ELF32 header offset overflow")?
                .to_le_bytes(),
        );
    }
    header.extend_from_slice(&0_u32.to_le_bytes());
    for value in [
        if is_64 { 64_u16 } else { 52 },
        0,
        0,
        if is_64 { 64 } else { 40 },
        9,
        7,
    ] {
        header.extend_from_slice(&value.to_le_bytes());
    }
    output[..header.len()].copy_from_slice(&header);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    fn section(bytes: &[u8], is_64: bool, index: usize) -> &[u8] {
        let header = if is_64 {
            u64_at(bytes, 40) as usize + index * 64
        } else {
            u32_at(bytes, 32) as usize + index * 40
        };
        let (offset, length) = if is_64 {
            (
                u64_at(bytes, header + 24) as usize,
                u64_at(bytes, header + 32) as usize,
            )
        } else {
            (
                u32_at(bytes, header + 16) as usize,
                u32_at(bytes, header + 20) as usize,
            )
        };
        &bytes[offset..offset + length]
    }

    #[test]
    fn both_x86_classes_keep_data_exports_and_separate_relocations() {
        let code = [0xff; 32];
        let mut data = [0xff; 32];
        data[16..24].copy_from_slice(&41_i64.to_le_bytes());
        let exports = vec![("first".into(), 0), ("second".into(), 4)];
        let undefs = vec!["external".into()];
        let calls = vec![(0, "external".into())];
        for is_64 in [false, true] {
            let references = vec![
                AbsoluteReference {
                    site: 8,
                    width: if is_64 { 8 } else { 4 },
                    target_section: NativeSection::Data,
                    target_offset: 16,
                },
                AbsoluteReference {
                    site: 16,
                    width: if is_64 { 8 } else { 4 },
                    target_section: NativeSection::Code,
                    target_offset: 4,
                },
            ];
            let object = X86RelocatableObject {
                code: &code,
                data: &data,
                exports: &exports,
                undefs: &undefs,
                calls: &calls,
                absolute_refs: &references,
                data_relocations: &[(0, 24)],
            };
            let bytes = write_x86_data_object(&object, is_64).unwrap();
            assert_eq!(&bytes[..4], b"\x7fELF");
            assert_eq!(bytes[4], if is_64 { 2 } else { 1 });
            let shoff = if is_64 {
                u64_at(&bytes, 40) as usize
            } else {
                u32_at(&bytes, 32) as usize
            };
            let stride = if is_64 { 64 } else { 40 };
            assert_eq!(u32_at(&bytes, shoff + stride + 8), 6); // .text AX
            assert_eq!(u32_at(&bytes, shoff + 2 * stride + 8), 3); // .data WA
            let names = section(&bytes, is_64, 7);
            assert!(names.windows(6).any(|name| name == b".data\0"));
            let stack = shoff + 8 * stride;
            let name = u32_at(&bytes, stack) as usize;
            assert_eq!(&names[name..name + 16], b".note.GNU-stack\0");
            assert_eq!(u32_at(&bytes, stack + 4), 1); // SHT_PROGBITS
            assert_eq!(u32_at(&bytes, stack + 8), 0); // no SHF_EXECINSTR
            assert!(section(&bytes, is_64, 8).is_empty());
            let text_relocs = section(&bytes, is_64, 3);
            let data_relocs = section(&bytes, is_64, 4);
            let symbols = section(&bytes, is_64, 5);
            let strings = section(&bytes, is_64, 6);
            let symbol_size = if is_64 { 24 } else { 16 };
            assert_eq!(symbols.len(), 6 * symbol_size);
            for (index, expected) in [(3, "first"), (4, "second"), (5, "external")] {
                let offset = u32_at(symbols, index * symbol_size) as usize;
                assert!(strings[offset..].starts_with(expected.as_bytes()));
            }
            assert_eq!(symbols[symbol_size + if is_64 { 4 } else { 12 }], 3); // STT_SECTION
            assert_eq!(symbols[2 * symbol_size + if is_64 { 4 } else { 12 }], 3);
            let initial_data = section(&bytes, is_64, 2);
            assert_eq!(u64_at(initial_data, 16), 41);
            if is_64 {
                assert_eq!(text_relocs.len(), 3 * 24);
                assert_eq!(u64_at(text_relocs, 8), (5 << 32) | 4); // PLT32 external
                assert_eq!(u64_at(text_relocs, 16) as i64, -4);
                assert_eq!(u64_at(text_relocs, 32), (2 << 32) | 1); // data section
                assert_eq!(u64_at(text_relocs, 40), 16);
                assert_eq!(u64_at(text_relocs, 56), (1 << 32) | 1); // text section
                assert_eq!(data_relocs.len(), 24);
                assert_eq!(u64_at(data_relocs, 8), (2 << 32) | 1);
                assert_eq!(u64_at(data_relocs, 16), 24);
                assert_eq!(u64_at(initial_data, 0), 0);
            } else {
                assert_eq!(text_relocs.len(), 3 * 8);
                assert_eq!(u32_at(text_relocs, 4), (5 << 8) | 2); // PC32 external
                assert_eq!(u32_at(section(&bytes, false, 1), 0) as i32, -4);
                assert_eq!(u32_at(text_relocs, 12), (2 << 8) | 1);
                assert_eq!(u32_at(section(&bytes, false, 1), 8), 16);
                assert_eq!(u32_at(text_relocs, 20), (1 << 8) | 1);
                assert_eq!(data_relocs.len(), 8);
                assert_eq!(u32_at(data_relocs, 4), (2 << 8) | 1);
                assert_eq!(u64_at(initial_data, 0), 24); // low pointer + zero high word
            }
        }
    }

    #[test]
    fn relocation_targets_must_be_inside_nonempty_sections_and_data_slots_aligned() {
        let code = [0; 24];
        let data = [0; 16];
        for is_64 in [false, true] {
            for (section, offset) in [(NativeSection::Code, 24), (NativeSection::Data, 16)] {
                let references = [AbsoluteReference {
                    site: 0,
                    width: if is_64 { 8 } else { 4 },
                    target_section: section,
                    target_offset: offset,
                }];
                let object = X86RelocatableObject {
                    code: &code,
                    data: &data,
                    exports: &[],
                    undefs: &[],
                    calls: &[],
                    absolute_refs: &references,
                    data_relocations: &[],
                };
                assert!(write_x86_data_object(&object, is_64).is_err());
            }
            let references = [AbsoluteReference {
                site: 0,
                width: if is_64 { 8 } else { 4 },
                target_section: NativeSection::Data,
                target_offset: 0,
            }];
            let object = X86RelocatableObject {
                code: &code,
                data: &[],
                exports: &[],
                undefs: &[],
                calls: &[],
                absolute_refs: &references,
                data_relocations: &[],
            };
            assert!(write_x86_data_object(&object, is_64).is_err());
            for relocations in [vec![(0, 16)], vec![(1, 0)]] {
                let object = X86RelocatableObject {
                    code: &code,
                    data: &data,
                    exports: &[],
                    undefs: &[],
                    calls: &[],
                    absolute_refs: &[],
                    data_relocations: &relocations,
                };
                assert!(write_x86_data_object(&object, is_64).is_err());
            }
            let object = X86RelocatableObject {
                code: &code,
                data: &[],
                exports: &[],
                undefs: &[],
                calls: &[],
                absolute_refs: &[],
                data_relocations: &[(0, 0)],
            };
            assert!(write_x86_data_object(&object, is_64).is_err());
        }
    }

    #[test]
    fn malformed_objects_refuse_instead_of_truncating_or_overlapping() {
        let code = [0; 24];
        let data = [0; 16];
        let exports = vec![("answer".into(), 0)];
        let object = X86RelocatableObject {
            code: &code,
            data: &data,
            exports: &exports,
            undefs: &[],
            calls: &[],
            absolute_refs: &[],
            data_relocations: &[],
        };
        for reference in [
            AbsoluteReference {
                site: 20,
                width: 8,
                target_section: NativeSection::Data,
                target_offset: 0,
            },
            AbsoluteReference {
                site: 0,
                width: 4,
                target_section: NativeSection::Data,
                target_offset: 0,
            },
            AbsoluteReference {
                site: 0,
                width: 8,
                target_section: NativeSection::Data,
                target_offset: 17,
            },
            AbsoluteReference {
                site: 0,
                width: 8,
                target_section: NativeSection::Code,
                target_offset: 25,
            },
        ] {
            let invalid = X86RelocatableObject {
                absolute_refs: std::slice::from_ref(&reference),
                ..object
            };
            assert!(write_x86_data_object(&invalid, true).is_err());
        }
        for relocations in [
            vec![(12, 0)],
            vec![(0, 17)],
            vec![(0, 0), (4, 0)],
            vec![(u64::MAX, 0)],
        ] {
            let invalid = X86RelocatableObject {
                data_relocations: &relocations,
                ..object
            };
            assert!(write_x86_data_object(&invalid, true).is_err());
        }
        let calls = vec![(0, "missing".into())];
        let invalid = X86RelocatableObject {
            calls: &calls,
            ..object
        };
        assert!(write_x86_data_object(&invalid, true).is_err());
        for exports in [
            vec![("bad\0name".into(), 0)],
            vec![("".into(), 0)],
            vec![("past".into(), 24)],
            vec![("same".into(), 0), ("same".into(), 1)],
        ] {
            let invalid = X86RelocatableObject {
                exports: &exports,
                ..object
            };
            assert!(write_x86_data_object(&invalid, true).is_err());
        }
        let refs = [AbsoluteReference {
            site: 0,
            width: 8,
            target_section: NativeSection::Data,
            target_offset: 0,
        }];
        let calls = vec![(4, "answer".into())];
        let object = X86RelocatableObject {
            absolute_refs: &refs,
            calls: &calls,
            ..object
        };
        assert!(
            write_x86_data_object(&object, true)
                .unwrap_err()
                .contains("overlapping")
        );
    }
}
