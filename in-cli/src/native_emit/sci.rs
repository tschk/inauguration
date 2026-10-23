//! Dynamic SCI (Simple Component Image) v2 binary emitter for freestanding targets.
//!
//! Produces a raw, position-dependent binary with a 64-byte manifest:
//!   [magic(8), required_caps(8), entry_va(8), image_size(8),
//!    export_count(8), export_table_off(8), import_count(8), import_table_off(8)]
//!
//! Table offsets are image-relative. The code section begins at
//! `SCI_MANIFEST_SIZE`; the entry VA is the code base (the entry function is
//! emitted first, at code offset 0).
//!
//! Export entries are 16 bytes: `{ name_off, fn_off }` where `name_off` is
//! image-relative (NUL-terminated name in the trailing name pool) and
//! `fn_off` is relative to the entry VA.
//!
//! Import entries are 16 bytes: `{ name_off, site_off }` where `site_off` is
//! the code offset of the `E8` opcode of a `call rel32 0` placeholder. The
//! loader patches the disp32 at `code + site_off + 1` to
//! `provider_fn_va - (entry_va + site_off) - 5`.
//!
//! String literals use the freestanding C-string layout (NUL-terminated,
//! value address = first byte), matching what freestanding `.in` code walks.

pub use crate::boundary_ir::SciLayout;
use crate::core_ir::{Decl, UnifiedModule};
use crate::native_emit::x86_64_lower::{
    X86_64CompileResult, X86StringLayout, lower_module_with_bases_layout,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const SCI_MAGIC: u64 = 0x5343490000000002;
/// SCI container whose payload is the private INISA (harden profile).
pub const SCI_INISA_MAGIC: u64 = 0x5343490000000049; // ...'I'
pub const SCI_MANIFEST_SIZE: usize = 64;
pub const SCI_EXPORT_ENTRY_SIZE: usize = 16;
pub const SCI_IMPORT_ENTRY_SIZE: usize = 16;
pub const SCI_PAGE_SIZE: u64 = 4096;

pub struct SciImage {
    pub bytes: Vec<u8>,
    pub layout: SciLayout,
}

/// Emit a dynamic SCI v2 binary for `module` with entry point `entry` loaded
/// at `base`. Every defined function becomes an export record; every call to
/// an externally defined function becomes an import record.
pub fn emit_sci_binary(module: &UnifiedModule, entry: &str, base: u64) -> Result<Vec<u8>, String> {
    Ok(emit_sci_image(module, entry, base)?.bytes)
}

/// Emit the compatible v2 wire image and its exact, byte-bound layout.
pub fn emit_sci_image(module: &UnifiedModule, entry: &str, base: u64) -> Result<SciImage, String> {
    let code_base = base
        .checked_add(SCI_MANIFEST_SIZE as u64)
        .ok_or("SCI code address overflow")?;
    // First pass: determine code size so the data section can be placed after it.
    let temp = lower_module_with_bases_layout(
        module,
        entry,
        code_base,
        code_base,
        X86StringLayout::Cstring,
    )?;
    let code_size = temp.code.len();
    let code_end = code_base
        .checked_add(code_size as u64)
        .ok_or("SCI text address overflow")?;
    // Align the virtual address, including for non-page-aligned load bases.
    // No page can then contain both executable text and mutable data.
    let data_base = align_up(code_end, SCI_PAGE_SIZE)?;
    // Second pass: patch globals using the real data section base.
    let result = lower_module_with_bases_layout(
        module,
        entry,
        code_base,
        data_base,
        X86StringLayout::Cstring,
    )?;
    if result.code.len() != code_size {
        return Err("SCI lowering changed text size between layout passes".into());
    }
    let pad = usize::try_from(data_base - code_end)
        .map_err(|_| "SCI padding exceeds host address space")?;
    let data_end = data_base
        .checked_add(result.data.len() as u64)
        .ok_or("SCI data address overflow")?;
    let binding_base = align_up(data_end, SCI_PAGE_SIZE)?;
    let binding_pad = usize::try_from(binding_base - data_end)
        .map_err(|_| "SCI binding padding exceeds host address space")?;
    let bytes = build_image(
        &result,
        base,
        required_capabilities_mask(module),
        pad,
        binding_pad,
    )?;
    base.checked_add(bytes.len() as u64)
        .ok_or("SCI image address overflow")?;
    let binding_offset =
        SCI_MANIFEST_SIZE + result.code.len() + pad + result.data.len() + binding_pad;
    let layout = SciLayout {
        version: 1,
        image_size: bytes.len() as u64,
        load_base: base,
        page_size: SCI_PAGE_SIZE,
        text_offset: SCI_MANIFEST_SIZE as u64,
        text_size: result.code.len() as u64,
        data_offset: data_base - base,
        data_size: result.data.len() as u64,
        binding_offset: binding_offset as u64,
        binding_size: (bytes.len() - binding_offset) as u64,
        artifact_sha256: Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    };
    Ok(SciImage { bytes, layout })
}

fn build_image(
    result: &X86_64CompileResult,
    base: u64,
    required_caps: u64,
    data_pad: usize,
    binding_pad: usize,
) -> Result<Vec<u8>, String> {
    let entry_va = base + SCI_MANIFEST_SIZE as u64;
    let export_count = result.exports.len();
    let import_count = result.extern_calls.len();

    // Sections after the (padded) code and data: export table, import table,
    // then the shared NUL-terminated name pool.
    let body_size = result.code.len() + data_pad + result.data.len() + binding_pad;
    let export_table_off = SCI_MANIFEST_SIZE + body_size;
    let import_table_off = export_table_off + export_count * SCI_EXPORT_ENTRY_SIZE;
    let name_pool_off = import_table_off + import_count * SCI_IMPORT_ENTRY_SIZE;

    let mut name_pool: Vec<u8> = Vec::new();
    let mut name_offsets: HashMap<String, u64> = HashMap::new();
    let intern = |name: &str, pool: &mut Vec<u8>, offsets: &mut HashMap<String, u64>| -> u64 {
        if let Some(&off) = offsets.get(name) {
            return off;
        }
        let off = name_pool_off as u64 + pool.len() as u64;
        offsets.insert(name.to_string(), off);
        pool.extend_from_slice(name.as_bytes());
        pool.push(0);
        off
    };
    let export_records: Vec<(u64, u64)> = result
        .exports
        .iter()
        .map(|(name, fn_off)| {
            (
                intern(name, &mut name_pool, &mut name_offsets),
                *fn_off as u64,
            )
        })
        .collect();
    let import_records: Vec<(u64, u64)> = result
        .extern_calls
        .iter()
        .map(|(name, site)| {
            (
                intern(name, &mut name_pool, &mut name_offsets),
                *site as u64,
            )
        })
        .collect();

    let image_size = name_pool_off + name_pool.len();
    let mut image = Vec::with_capacity(image_size);
    image.extend_from_slice(&SCI_MAGIC.to_le_bytes());
    image.extend_from_slice(&required_caps.to_le_bytes());
    image.extend_from_slice(&entry_va.to_le_bytes());
    image.extend_from_slice(&(image_size as u64).to_le_bytes());
    image.extend_from_slice(&(export_count as u64).to_le_bytes());
    image.extend_from_slice(&(export_table_off as u64).to_le_bytes());
    image.extend_from_slice(&(import_count as u64).to_le_bytes());
    image.extend_from_slice(&(import_table_off as u64).to_le_bytes());
    debug_assert_eq!(image.len(), SCI_MANIFEST_SIZE);

    image.extend_from_slice(&result.code);
    image.resize(image.len() + data_pad, 0);
    image.extend_from_slice(&result.data);
    image.resize(image.len() + binding_pad, 0);

    for (name_off, fn_off) in export_records {
        image.extend_from_slice(&name_off.to_le_bytes());
        image.extend_from_slice(&fn_off.to_le_bytes());
    }
    for (name_off, site) in import_records {
        image.extend_from_slice(&name_off.to_le_bytes());
        image.extend_from_slice(&site.to_le_bytes());
    }
    image.extend_from_slice(&name_pool);
    debug_assert_eq!(image.len(), image_size);
    Ok(image)
}

fn required_capabilities_mask(module: &UnifiedModule) -> u64 {
    let mut mask = 0u64;
    let mut bit = 0u64;
    for decl in &module.decls {
        if let Decl::Component { capabilities, .. } = decl {
            for _cap in capabilities {
                if bit < 64 {
                    mask |= 1u64 << bit;
                    bit += 1;
                }
            }
        }
    }
    mask
}

fn align_up(addr: u64, alignment: u64) -> Result<u64, String> {
    addr.checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or_else(|| "SCI section alignment address overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{CoreModuleIdentity, Decl, Expr, Stmt, Typ};

    fn simple_module() -> UnifiedModule {
        let decls = vec![Decl::Function {
            name: "answer".into(),
            params: vec![],
            ret: Typ::Int,
            body: vec![Stmt::Return(Some(Expr::IntLit(42)))],
            type_params: vec![],
        }];
        UnifiedModule::with_identity(decls, CoreModuleIdentity::default())
    }

    /// Module with one defined function that calls an extern and a string.
    fn binding_module() -> UnifiedModule {
        let decls = vec![
            Decl::Function {
                name: "host".into(),
                params: vec![],
                ret: Typ::Int,
                body: vec![
                    Stmt::Let("s".into(), None, Expr::StringLit("hi".into())),
                    Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident("ext_fn".into())),
                        args: vec![Expr::IntLit(1)],
                    })),
                ],
                type_params: vec![],
            },
            Decl::Function {
                name: "ext_fn".into(),
                params: vec![("x".into(), Typ::Int)],
                ret: Typ::Int,
                body: vec![],
                type_params: vec![],
            },
        ];
        UnifiedModule::with_identity(decls, CoreModuleIdentity::default())
    }

    fn u64_at(image: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(image[off..off + 8].try_into().unwrap())
    }

    fn cstr_at(image: &[u8], off: u64) -> String {
        let start = off as usize;
        let end = image[start..]
            .iter()
            .position(|&b| b == 0)
            .expect("NUL terminator")
            + start;
        String::from_utf8(image[start..end].to_vec()).unwrap()
    }

    #[test]
    fn sci_manifest_layout() {
        let module = simple_module();
        let binary = emit_sci_binary(&module, "answer", 0x40000020).expect("emit sci");
        assert_eq!(u64_at(&binary, 0), SCI_MAGIC);
        let image_size = u64_at(&binary, 24);
        assert_eq!(binary.len(), image_size as usize);
        // Entry VA = base + 64-byte manifest.
        assert_eq!(u64_at(&binary, 16), 0x40000020 + 64);
        assert_eq!(u64_at(&binary, 32), 1); // one export
        let export_table_off = u64_at(&binary, 40) as usize;
        // Image ends with the 16-byte export table plus the "answer\0" pool.
        assert_eq!(export_table_off + 16 + 7, binary.len());
        assert_eq!(u64_at(&binary, 48), 0); // no imports
        assert_eq!(u64_at(&binary, 56) as usize, export_table_off + 16);
        // Export record: name then fn offset from entry VA.
        let name_off = u64_at(&binary, export_table_off);
        let fn_off = u64_at(&binary, export_table_off + 8);
        assert_eq!(cstr_at(&binary, name_off), "answer");
        assert_eq!(fn_off, 0); // entry function is first, at code offset 0
    }

    #[test]
    fn sci_v2_binds_exports_imports_and_cstrings() {
        let module = binding_module();
        let base = 0x40000000u64;
        let binary = emit_sci_binary(&module, "host", base).expect("emit sci");
        assert_eq!(u64_at(&binary, 0), SCI_MAGIC);
        assert_eq!(u64_at(&binary, 32), 1); // exports: host
        assert_eq!(u64_at(&binary, 48), 1); // imports: ext_fn
        let export_table_off = u64_at(&binary, 40) as usize;
        let import_table_off = u64_at(&binary, 56) as usize;

        // Export: host
        let name_off = u64_at(&binary, export_table_off);
        assert_eq!(cstr_at(&binary, name_off), "host");
        assert_eq!(u64_at(&binary, export_table_off + 8), 0); // entry first

        // Import: ext_fn at the site of its call; the E8 opcode sits at
        // code + site and the disp32 placeholder is zero until bound.
        let iname_off = u64_at(&binary, import_table_off);
        let site = u64_at(&binary, import_table_off + 8);
        assert_eq!(cstr_at(&binary, iname_off), "ext_fn");
        let code_start = SCI_MANIFEST_SIZE;
        assert_eq!(binary[code_start + site as usize], 0xE8);
        assert_eq!(
            &binary[code_start + site as usize + 1..code_start + site as usize + 5],
            &[0, 0, 0, 0]
        );

        // Freestanding strings are NUL-terminated: find "hi\0" in the image.
        assert!(binary.windows(3).any(|w| w == [b'h', b'i', 0]));

        // Name pool must live after both tables and inside the image.
        let name_pool_first = name_off.min(iname_off);
        assert!(name_pool_first as usize >= import_table_off + 16);
    }
    #[test]
    fn sci_string_globals_point_to_cstring_payloads_inside_the_image() {
        let module = crate::in_lang_parse::parse_in_source(
            "const label:String=\"λ\";\nvar sentinel:Int=42;\nfn main()->String{return label;}",
        )
        .unwrap();
        let base = 0x40000020;
        let emitted = emit_sci_image(&module, "main", base).unwrap();
        let data = emitted.layout.data_offset as usize;
        let image = emitted.bytes;
        // Two global words followed by one aligned NUL-terminated payload.
        let pointer = u64_at(&image, data);
        assert_eq!(pointer, base + data as u64 + 16);
        assert_eq!(u64_at(&image, data + 8), 42);
        assert_eq!(cstr_at(&image, pointer - base), "λ");
        assert_eq!(&image[data + 16..data + 19], "λ\0".as_bytes());
    }

    #[test]
    fn sci_layout_binds_actual_text_data_and_tables_for_unaligned_bases() {
        let module = crate::in_lang_parse::parse_in_source(
            "var counter:Int=40;\nfn ext(x:Int)->Int;\nfn main()->Int{counter+=1;return ext(counter);}",
        ).unwrap();
        for base in [0x40000000, 0x40000020, 0x40000fc0] {
            let image = emit_sci_image(&module, "main", base).unwrap();
            let layout = &image.layout;
            assert_eq!(layout.version, 1);
            assert_eq!(layout.image_size, image.bytes.len() as u64);
            assert_eq!(layout.load_base, base);
            assert_eq!(layout.page_size, 4096);
            assert_eq!(layout.text_offset, 64);
            assert!(layout.text_size > 0);
            assert_eq!((base + layout.data_offset) % layout.page_size, 0);
            assert!(layout.text_offset + layout.text_size <= layout.data_offset);
            assert_eq!(layout.data_size, 8);
            assert_eq!(u64_at(&image.bytes, layout.data_offset as usize), 40);
            assert!(layout.data_offset + layout.data_size <= layout.binding_offset);
            assert_eq!((base + layout.binding_offset) % layout.page_size, 0);
            assert!(
                image.bytes[(layout.data_offset + layout.data_size) as usize
                    ..layout.binding_offset as usize]
                    .iter()
                    .all(|&byte| byte == 0)
            );
            assert_eq!(layout.binding_offset, u64_at(&image.bytes, 40));
            assert_eq!(
                layout.binding_offset + layout.binding_size,
                layout.image_size
            );
            assert_eq!(
                layout.artifact_sha256,
                Sha256::digest(&image.bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            assert!(
                image.bytes
                    [(layout.text_offset + layout.text_size) as usize..layout.data_offset as usize]
                    .iter()
                    .all(|&byte| byte == 0)
            );
            let imports = u64_at(&image.bytes, 56) as usize;
            assert_eq!(u64_at(&image.bytes, 48), 1);
            let site = u64_at(&image.bytes, imports + 8);
            assert!(site + 5 <= layout.text_size);
            assert_eq!(image.bytes[(layout.text_offset + site) as usize], 0xe8);
            let mut modified = image.bytes.clone();
            modified[layout.data_offset as usize] ^= 1;
            assert_ne!(
                layout.artifact_sha256,
                Sha256::digest(&modified)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            assert_eq!(
                emit_sci_image(&module, "main", base).unwrap().layout,
                *layout
            );
        }
        assert!(emit_sci_image(&module, "main", u64::MAX - 32).is_err());
        assert!(emit_sci_image(&module, "main", u64::MAX - 512).is_err());
    }

    #[test]
    fn sci_empty_data_has_no_mutable_extent() {
        let image = emit_sci_image(&simple_module(), "answer", 0x40000000).unwrap();
        assert_eq!(image.layout.data_size, 0);
        assert_eq!(image.layout.data_offset, image.layout.binding_offset);
    }

    // Exercise the emitted addresses under real page permissions. The test
    // consumer applies the layout; emitting metadata alone enforces nothing.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn sci_global_mutation_runs_with_rx_text_and_rw_nonexecuting_data() {
        unsafe {
            let capacity = 65536;
            let mapping = libc::mmap(
                std::ptr::null_mut(),
                capacity,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            assert_ne!(mapping, libc::MAP_FAILED);
            let module = crate::in_lang_parse::parse_in_source(
                "var counter:Int=40;\nfn external(x:Int)->Int;\nfn main()->Int{counter+=1;return external(counter);}",
            )
            .unwrap();
            let image = emit_sci_image(&module, "main", mapping as u64).unwrap();
            assert!(image.bytes.len() < capacity);
            std::ptr::copy_nonoverlapping(image.bytes.as_ptr(), mapping.cast(), image.bytes.len());
            // A nearby native provider returns its SysV integer argument.
            // Patch the real import call before making text executable.
            let provider = mapping.byte_add(capacity - 4096);
            std::ptr::copy_nonoverlapping([0x48u8, 0x89, 0xf8, 0xc3].as_ptr(), provider.cast(), 4);
            let imports = u64_at(&image.bytes, 56) as usize;
            assert_eq!(u64_at(&image.bytes, 48), 1);
            let call = mapping.byte_add(64 + u64_at(&image.bytes, imports + 8) as usize);
            let displacement = i32::try_from(provider as i64 - call as i64 - 5).unwrap();
            std::ptr::write_unaligned(call.byte_add(1).cast::<i32>(), displacement);
            assert_eq!(
                libc::mprotect(provider, 4096, libc::PROT_READ | libc::PROT_EXEC),
                0
            );
            assert_eq!(
                libc::mprotect(
                    mapping,
                    image.layout.data_offset as usize,
                    libc::PROT_READ | libc::PROT_EXEC
                ),
                0
            );
            let bindings = mapping.byte_add(image.layout.binding_offset as usize);
            assert_eq!(
                libc::mprotect(
                    bindings,
                    image.layout.binding_size as usize,
                    libc::PROT_READ
                ),
                0
            );
            let entry: extern "C" fn() -> i64 = std::mem::transmute(mapping.byte_add(64));
            assert_eq!(entry(), 41);
            assert_eq!(entry(), 42);
            let data = mapping
                .byte_add(image.layout.data_offset as usize)
                .cast::<u8>();
            assert_eq!(std::ptr::read_unaligned(data.cast::<i64>()), 42);

            // A RET would succeed if this writable page were executable.
            std::ptr::write_volatile(data, 0xc3);
            for forbidden in 0..3 {
                let pid = libc::fork();
                assert!(pid >= 0);
                if pid == 0 {
                    let limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    libc::setrlimit(libc::RLIMIT_CORE, &limit);
                    if forbidden == 1 {
                        let function: extern "C" fn() = std::mem::transmute(data);
                        function();
                    } else if forbidden == 0 {
                        std::ptr::write_volatile(mapping.byte_add(64).cast::<u8>(), 0xc3);
                    } else {
                        std::ptr::write_volatile(bindings.cast::<u8>(), 0);
                    }
                    libc::_exit(0);
                }
                let mut status = 0;
                assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
                assert!(libc::WIFSIGNALED(status));
                assert_eq!(libc::WTERMSIG(status), libc::SIGSEGV);
            }
            assert_eq!(libc::munmap(mapping, capacity), 0);
        }
    }
}
