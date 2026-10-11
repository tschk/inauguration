//! In-memory JIT execution runtime.
//!
//! Skips binary formats (Mach-O, ELF) entirely. Takes lowered AArch64/x86-64
//! machine code from `native_emit`, maps it into executable memory via
//! mmap (Unix) or VirtualAlloc (Windows), and executes directly through
//! function pointers.
//!
//! Architecture:
//!   Core IR → Machine IR → raw bytes → mmap/VirtualAlloc → call via fn ptr
//!
//! Systems-level: the JIT runtime IS the executable model. No files, no
//! linker, no dynamic loader. Functions resolve through an in-memory
//! dispatch table. Designed to eventually compile itself.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::RwLock;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn sys_icache_invalidate(start: *const std::ffi::c_void, len: usize);
}

/// A compiled function resident in JIT memory.
struct JitFunction {
    /// Raw pointer to the first instruction in the JIT code page.
    entry: *const u8,
    /// Size of the function in bytes (for debugging / future stack walking).
    _size: u32,
    /// Stack frame size (for debugging / future stack walking).
    _frame_size: u32,
}

// SAFETY: JitFunction pointers reference mmap'd/VirtualAlloc pages that remain
// valid for the lifetime of the JitRuntime. They are never aliased mutably
// after compilation.
unsafe impl Send for JitFunction {}
unsafe impl Sync for JitFunction {}

/// System-level JIT execution runtime.
///
/// Owns executable memory pages and a dispatch table. Functions are compiled
/// lazily: the first call to `invoke()` for a function triggers compilation
/// of the entire module, then all functions become callable.
pub struct JitRuntime {
    /// Mapped executable pages (the code arena).
    code_pages: Vec<CodePage>,
    /// Mapped read-write pages holding the lowered data section (globals).
    /// Kept alive for the lifetime of the runtime so executed code can
    /// read and write global slots.
    data_pages: Vec<(*mut u8, usize)>,
    /// Function dispatch table: name → entry point.
    functions: RwLock<HashMap<String, JitFunction>>,
    /// Writable page for error flag (byte at +0) and error value (ptr at +8).
    /// Set by invoke() into X27 (AArch64) or R15 (x86_64) for throw/try.
    error_page: *mut u8,
}

struct CodePage {
    ptr: *mut u8,
    size: usize,
    /// Bytes written; used for icache flush on macOS and AArch64 Linux.
    #[cfg_attr(
        not(any(
            target_os = "macos",
            all(
                any(target_os = "linux", target_os = "android"),
                target_arch = "aarch64"
            )
        )),
        allow(dead_code)
    )]
    used: usize,
}

// SAFETY: CodePage owns mmap'd memory that is valid until dropped.
unsafe impl Send for CodePage {}
unsafe impl Sync for CodePage {}

#[cfg(not(windows))]
fn alloc_executable_pages(size: usize) -> Option<*mut u8> {
    #[cfg(target_os = "macos")]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT;
    #[cfg(not(target_os = "macos"))]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return None;
    }
    Some(ptr as *mut u8)
}

#[cfg(not(windows))]
fn make_executable(ptr: *mut u8, size: usize) -> Result<(), String> {
    // ponytail: pthread_jit_write_protect_np regressed JIT on arm64 macOS CI;
    // mprotect on MAP_JIT pages still passes conformance here.
    let result = unsafe {
        libc::mprotect(
            ptr as *mut std::ffi::c_void,
            size,
            libc::PROT_READ | libc::PROT_EXEC,
        )
    };
    if result != 0 {
        return Err(format!(
            "jit: executable page protection failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn free_pages(ptr: *mut u8, size: usize) {
    unsafe {
        libc::munmap(ptr as *mut c_void, size);
    }
}

#[cfg(windows)]
fn alloc_executable_pages(size: usize) -> Option<*mut u8> {
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_COMMIT: u32 = 0x1000;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    unsafe extern "system" {
        fn VirtualAlloc(
            lp_address: *mut std::ffi::c_void,
            dw_size: usize,
            fl_allocation_type: u32,
            fl_protect: u32,
        ) -> *mut std::ffi::c_void;
    }
    let ptr = unsafe {
        VirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_EXECUTE_READWRITE,
        )
    };
    if ptr.is_null() {
        return None;
    }
    Some(ptr as *mut u8)
}

#[cfg(windows)]
fn make_executable(_ptr: *mut u8, _size: usize) -> Result<(), String> {
    // Already RWX from VirtualAlloc. ARM64 Windows needs FlushInstructionCache
    // but that requires win32_ffi we can add when tested.
    Ok(())
}

#[cfg(windows)]
fn free_pages(ptr: *mut u8, _size: usize) {
    const MEM_RELEASE: u32 = 0x8000;
    unsafe extern "system" {
        fn VirtualFree(lp_address: *mut std::ffi::c_void, dw_size: usize, dw_free_type: u32)
        -> i32;
    }
    unsafe {
        VirtualFree(ptr as *mut std::ffi::c_void, 0, MEM_RELEASE);
    }
}

impl CodePage {
    fn new(min_size: usize) -> Option<Self> {
        let page_size = 0x4000; // 16KB typical for ARM64, works for x86_64 too
        let size = min_size.max(page_size).next_multiple_of(page_size);

        let ptr = alloc_executable_pages(size)?;

        Some(Self {
            ptr: ptr as *mut u8,
            size,
            used: 0,
        })
    }

    /// Make the written code executable and flush caches.
    fn finalize(&self) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        unsafe {
            sys_icache_invalidate(self.ptr as *const std::ffi::c_void, self.used);
        }
        #[cfg(all(
            any(target_os = "linux", target_os = "android"),
            target_arch = "aarch64"
        ))]
        unsafe {
            extern "C" {
                fn __clear_cache(start: *const u8, end: *const u8);
            }
            __clear_cache(self.ptr, self.ptr.add(self.used));
        }
        make_executable(self.ptr, self.size)
    }
}

impl Drop for CodePage {
    fn drop(&mut self) {
        free_pages(self.ptr, self.size);
    }
}

impl Default for JitRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl JitRuntime {
    pub fn new() -> Self {
        // Bootstrap native symbol cache with critical libc symbols
        crate::native_emit::native_link::bootstrap_jit_native();
        // Allocate a small writable page for error flag/value.
        // Not MAP_JIT — just RW for throw/try/catch writes.
        let error_page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                64,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        let error_page = if error_page == libc::MAP_FAILED {
            std::ptr::null_mut()
        } else {
            error_page as *mut u8
        };
        Self {
            code_pages: Vec::new(),
            data_pages: Vec::new(),
            functions: RwLock::new(HashMap::new()),
            error_page,
        }
    }

    /// Load machine code bytes and register entry points.
    ///
    /// `code` is raw AArch64 machine instructions. `entry_offset` is the
    /// byte offset from the start of `code` to the entry trampoline.
    /// `function_offsets` maps function names to (offset, size) pairs.
    pub fn load(
        &mut self,
        code: &[u8],
        function_offsets: &[(String, u32, u32)], // (name, offset, size)
        relocations: &[(u32, u64)],              // (offset, codegen_base) — patched at load time
    ) -> Result<(), String> {
        self.load_with_globals(code, function_offsets, relocations, &[], &[])
    }

    /// Like [`load`], with the lowered data section mapped for execution.
    ///
    /// `global_relocs` holds `(site, data_offset)` pairs: each `site` is the
    /// imm64/displacement offset of an absolute reference to the global slot
    /// at `data_offset`. The lowering baked `data_base + offset` into the
    /// image; this pass rewrites it to the actually-mapped data address so
    /// executed code reads and writes real globals instead of unmapped
    /// memory. Sites listed here are skipped by the code-relative relocation
    /// pass.
    pub fn load_with_globals(
        &mut self,
        code: &[u8],
        function_offsets: &[(String, u32, u32)],
        relocations: &[(u32, u64)],
        data: &[u8],
        global_relocs: &[(u32, u64)],
    ) -> Result<(), String> {
        // Allocate a code page large enough
        let mut page = CodePage::new(code.len()).ok_or_else(|| "jit: mmap failed".to_string())?;
        page.used = code.len();

        let dest = page.ptr;
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), dest, code.len());
        }

        // Apply relocations: patch each absolute address by adding (actual_base - codegen_base).
        // Global-data sites are excluded — they are rebased against the
        // mapped data section below.
        if !relocations.is_empty() {
            let actual_base = dest as u64;
            let global_sites: std::collections::HashSet<usize> =
                global_relocs.iter().map(|(s, _)| *s as usize).collect();
            for &(offset, codegen_base) in relocations {
                let site = offset as usize;
                if global_sites.contains(&site) {
                    continue;
                }
                if site + 8 <= code.len() {
                    let old_val = u64::from_le_bytes([
                        code[site],
                        code[site + 1],
                        code[site + 2],
                        code[site + 3],
                        code[site + 4],
                        code[site + 5],
                        code[site + 6],
                        code[site + 7],
                    ]);
                    let new_val = old_val.wrapping_sub(codegen_base).wrapping_add(actual_base);
                    let patch = new_val.to_le_bytes();
                    unsafe {
                        std::ptr::copy_nonoverlapping(patch.as_ptr(), dest.add(site), 8);
                    }
                }
            }
        }

        // Map the data section (globals) read-write and rebase every global
        // reference to the mapped address. Without this, executed code would
        // touch the lowered data_base (0x200000) directly, which is unmapped.
        if !data.is_empty() {
            let rounded = data.len().next_multiple_of(0x1000);
            let map = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    rounded,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                )
            };
            if map == libc::MAP_FAILED {
                return Err("jit: data section mmap failed".to_string());
            }
            let data_ptr = map as *mut u8;
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), data_ptr, data.len());
            }
            self.data_pages.push((data_ptr, rounded));
            let actual_data_base = data_ptr as u64;
            for &(site, offset) in global_relocs {
                let site = site as usize;
                if site + 8 <= code.len() {
                    let patch = actual_data_base.wrapping_add(offset).to_le_bytes();
                    unsafe {
                        std::ptr::copy_nonoverlapping(patch.as_ptr(), dest.add(site), 8);
                    }
                }
            }
        }

        // Publish entry points only after the page is executable.
        page.finalize()?;
        let base = page.ptr;
        self.code_pages.push(page);
        let mut funcs = self.functions.write().unwrap();
        for (name, offset, size) in function_offsets {
            funcs.insert(
                name.clone(),
                JitFunction {
                    entry: unsafe { base.add(*offset as usize) },
                    _size: *size,
                    _frame_size: 0,
                },
            );
        }

        Ok(())
    }

    /// Call a compiled function by name. Returns `None` for a missing symbol or
    /// more than eight scalar arguments on AArch64 or six on x86_64.
    ///
    /// # Safety
    /// The function must have been loaded via `load()`. The caller is
    /// responsible for ensuring the function signature matches the arguments.
    pub unsafe fn invoke(&self, name: &str, args: &[i64]) -> Option<i64> {
        let funcs = self.functions.read().unwrap();
        let func = funcs.get(name)?;
        let entry = func.entry as *const ();

        #[cfg(target_arch = "aarch64")]
        {
            // ponytail: marshal stack arguments before supporting more than eight scalars.
            if args.len() > 8 {
                return None;
            }
            let mut values = [0_i64; 8];
            values[..args.len()].copy_from_slice(args);
            let result: i64;
            unsafe {
                std::arch::asm!(
                    "mov x27, x17", "blr x16",
                    in("x16") entry, in("x17") self.error_page as usize,
                    in("x0") values[0], in("x1") values[1], in("x2") values[2], in("x3") values[3],
                    in("x4") values[4], in("x5") values[5], in("x6") values[6], in("x7") values[7],
                    lateout("x0") result, out("x27") _, clobber_abi("C"),
                );
            }
            Some(result)
        }

        #[cfg(target_arch = "x86_64")]
        {
            // The lowerer uses SysV registers, including when the Rust host has another C ABI.
            // ponytail: marshal stack arguments before supporting more than six scalars.
            if args.len() > 6 {
                return None;
            }
            let mut values = [0_i64; 6];
            values[..args.len()].copy_from_slice(args);
            let result: i64;
            unsafe {
                // Keep the saved stack pointer above the callee's frame and align its call.
                std::arch::asm!(
                    "mov r15, r10", "mov r10, rsp", "and rsp, -16", "sub rsp, 16",
                    "mov [rsp], r10", "call r11", "mov rsp, [rsp]",
                    in("r11") entry, in("r10") self.error_page as usize,
                    in("rdi") values[0], in("rsi") values[1], in("rdx") values[2],
                    in("rcx") values[3], in("r8") values[4], in("r9") values[5],
                    lateout("rax") result, out("r15") _, clobber_abi("sysv64"),
                );
            }
            Some(result)
        }

        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            None
        }
    }

    /// Call a compiled function whose result is a `Float`.
    ///
    /// The backend keeps `Float` values in general-purpose registers as their
    /// `f64` bit pattern: a literal is materialized with `movz`/`movk` and an
    /// arithmetic result is moved back with `fmov`. The value therefore comes
    /// out of the integer result register, not the `d` registers.
    ///
    /// # Safety
    /// The function must have been loaded via `load()` and must take no
    /// arguments.
    pub unsafe fn invoke_float(&self, name: &str) -> Option<f64> {
        let bits = unsafe { self.invoke(name, &[]) }?;
        Some(f64::from_bits(bits as u64))
    }

    /// Returns the number of loaded functions.
    pub fn function_count(&self) -> usize {
        self.functions.read().unwrap().len()
    }
}

impl Drop for JitRuntime {
    fn drop(&mut self) {
        if !self.error_page.is_null() {
            unsafe {
                libc::munmap(self.error_page as *mut c_void, 64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn executable_page_protection_reports_failures() {
        let page = CodePage::new(1).expect("code page");
        let error = make_executable(unsafe { page.ptr.add(1) }, page.size - 1)
            .expect_err("unaligned page protection must fail");
        assert!(error.starts_with("jit: executable page protection failed:"));
        page.finalize().expect("valid page protection");
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    #[ignore = "diagnostic stress for mixed JIT code-page reuse"]
    fn jit_code_pages_keep_fresh_results_when_reused() {
        use crate::emit_profile::EmitProfile;
        std::thread::scope(|scope| {
            for worker in 0..32_i64 {
                scope.spawn(move || {
                    for iteration in 0..1024_i64 {
                        let profile = match (iteration / 4) % 3 {
                            0 => EmitProfile::Default,
                            1 => EmitProfile::Lean,
                            _ => EmitProfile::Harden,
                        };
                        let tag = worker * 5000 + iteration;
                        let (source, args, expected) = match iteration % 4 {
                            0 => (format!("fn main() -> Int {{ return {tag}; }}"), vec![], tag),
                            1 => (
                                "fn main() -> Float { return 1.25 + 2.5; }".into(),
                                vec![],
                                3.75_f64.to_bits() as i64,
                            ),
                            2 => (
                                format!("var value: Int = {tag};\nfn read(x: Int) -> Int {{ return x; }}\nfn main() -> Int {{ return read(value); }}"),
                                vec![],
                                tag,
                            ),
                            _ => (
                                "fn main(a: Int, b: Int) -> Int { return a % b; }".into(),
                                vec![i64::MIN, -1],
                                0,
                            ),
                        };
                        assert_eq!(
                            invoke_source(&source, profile, &args),
                            Some(expected),
                            "worker={worker} iteration={iteration} profile={profile:?}"
                        );
                    }
                });
            }
        });
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn concurrent_runtime_errors_keep_their_own_values() {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        let barrier = Barrier::new(8);
        let mismatches = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for worker in 1..=8_i64 {
                let barrier = &barrier;
                let mismatches = &mismatches;
                scope.spawn(move || {
                    let module = crate::in_lang_parse::parse_in_source("fn main(tag: Int) -> Int { try { throw tag; var n = 0; while n < 10000 { n = n + 1; } } catch error { return error; } return -1; }").unwrap();
                    for profile in [crate::emit_profile::EmitProfile::Default, crate::emit_profile::EmitProfile::Lean, crate::emit_profile::EmitProfile::Harden] {
                        let rt = runtime_for_module(&module, profile);
                        for _ in 0..100 {
                            barrier.wait();
                            let result = unsafe { rt.invoke("main", &[worker]) };
                            if result != Some(worker) { mismatches.fetch_add(1, Ordering::Relaxed); }
                            barrier.wait();
                        }
                    }
                });
            }
        });
        assert_eq!(
            mismatches.load(Ordering::Relaxed),
            0,
            "runtime error values crossed between invocations"
        );
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn runtime_error_values_survive_nested_calls() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = "fn fail(tag: Int) -> Int { throw tag; return 0; }\nfn main(tag: Int) -> Int { try { fail(tag); } catch error { return error; } return -1; }";
            assert_eq!(
                invoke_source(source, profile, &[42]),
                Some(42),
                "{profile:?}"
            );
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn shadowed_bindings_keep_their_lexical_identity() {
        use crate::emit_profile::EmitProfile;
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for (source, expected) in [
                (
                    "var count: Int = 0;\nfn read() -> Int { return count; }\nfn main() -> Int { count = 42; let count = 7; return read(); }",
                    42,
                ),
                (
                    "var count: Int = 42;\nfn main() -> Int { let before = count; let count = 7; count = count + 1; return before + count; }",
                    50,
                ),
                (
                    "fn shadow(x: Int) -> Int { let x = x + 1; return x; }\nfn main() -> Int { return shadow(41); }",
                    42,
                ),
                (
                    "fn main() -> Int { let x = 1; let before = x; let x = 7; return before + x; }",
                    8,
                ),
                (
                    "fn main() -> Int { var x = 1; if true { let x = 7; x = x + 1; } return x; }",
                    1,
                ),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).expect("parse");
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "raw {profile:?}: {source}"
                );
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "optimized {profile:?}: {source}"
                );
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn optimized_call_arguments_are_evaluated_once() {
        use crate::emit_profile::EmitProfile;
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for (callee, body, expected) in [
                (
                    "fn twice(x: Int) -> Int { return x + x; }",
                    "return twice(next());",
                    2,
                ),
                (
                    "fn unused(x: Int) -> Void { return; }",
                    "unused(next()); return count;",
                    1,
                ),
                (
                    "fn combine(x: Int) -> Int { return next() + x; }",
                    "return combine(count);",
                    1,
                ),
                (
                    "fn combine() -> Int { let snapshot = count; return next() + snapshot; }",
                    "return combine();",
                    1,
                ),
                (
                    "fn addtwice(x: Int) -> Void { count = count + x; count = count + x; }",
                    "count = 1; addtwice(count); return count;",
                    3,
                ),
                (
                    "fn helper() -> Void { let temp = 7; count = temp; }",
                    "let temp = 42; helper(); return count + temp;",
                    49,
                ),
                (
                    "fn helper() -> Void { return; count = 2; }",
                    "count = 1; helper(); return count;",
                    1,
                ),
                (
                    "var shared: Int = 42;\nfn read() -> Int { return shared; }",
                    "let shared = 7; return read();",
                    42,
                ),
                (
                    "var shared: Int = 42;\nfn read() -> Int { let copy = shared; return copy; }",
                    "let shared = 7; return read();",
                    42,
                ),
            ] {
                let source = format!(
                    "var count: Int = 0;\nfn next() -> Int {{ count = count + 1; return count; }}\n{callee}\nfn main() -> Int {{ {body} }}"
                );
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "{profile:?}: {body}"
                );
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn scalar_float_arithmetic_preserves_fractional_values() {
        use crate::emit_profile::EmitProfile;
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for (expression, expected) in [
                ("1.25 + 2.5", 3.75_f64),
                ("(8.5 - 2.25) * 2.0 / 5.0", 2.5),
                ("2 + 0.5", 2.5),
                ("-(2.5 + 0.75)", -3.25),
                ("-0.0", -0.0),
                ("0.5 + 2", 2.5),
                ("(1.5 + 2.25) / (0.5 + 1.0)", 2.5),
            ] {
                let source = format!("fn main() -> Float {{ return {expression}; }}");
                assert_eq!(
                    invoke_source(&source, profile, &[]),
                    Some(expected.to_bits() as i64),
                    "{profile:?}: {expression}"
                );
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn typed_float_values_flow_through_locals_calls_and_comparisons() {
        use crate::emit_profile::EmitProfile;
        for profile in [EmitProfile::Default, EmitProfile::Lean, EmitProfile::Harden] {
            for source in [
                "fn main() -> Float { let x = 1.25 + 2.5; let y: Float = 0.5; return x * y; }",
                "fn scale(x: Float) -> Float { return x * 0.5; }\nfn main() -> Float { return scale(3.75); }",
                "var x: Float = 3.75;\nfn main() -> Float { return x * 0.5; }",
                "var x: Float = 1.25;\nfn main() -> Float { return x + 0.625; }",
                "fn main() -> Float { var x: Float = 3.75; x *= 0.5; return x; }",
                "struct Value { Float amount }\nfn main() -> Float { let value = Value { amount: 3.75 }; return value.amount * 0.5; }",
            ] {
                assert_eq!(
                    invoke_source(source, profile, &[]),
                    Some(1.875_f64.to_bits() as i64),
                    "{profile:?}: {source}"
                );
            }
            for (expression, expected) in [
                ("1.5 < 2.0", 1),
                ("2.0 >= 1.5", 1),
                ("1.5 == 1.5", 1),
                ("1.5 != 2.0", 1),
                ("(0.0 / 0.0) == 0.0", 0),
                ("(0.0 / 0.0) != 0.0", 1),
                ("(0.0 / 0.0) < 1.0", 0),
            ] {
                let source = format!("fn main() -> Bool {{ return {expression}; }}");
                assert_eq!(
                    invoke_source(&source, profile, &[]),
                    Some(expected),
                    "{profile:?}: {expression}"
                );
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn invoke_source(
        source: &str,
        profile: crate::emit_profile::EmitProfile,
        args: &[i64],
    ) -> Option<i64> {
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        invoke_module(&module, profile, args)
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn invoke_module(
        module: &crate::core_ir::UnifiedModule,
        profile: crate::emit_profile::EmitProfile,
        args: &[i64],
    ) -> Option<i64> {
        let rt = runtime_for_module(module, profile);
        unsafe { rt.invoke("main", args) }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn runtime_for_module(
        module: &crate::core_ir::UnifiedModule,
        profile: crate::emit_profile::EmitProfile,
    ) -> JitRuntime {
        let _profile = crate::native_emit::antidecomp::ProfileGuard::new(profile);
        #[cfg(target_arch = "aarch64")]
        let result = crate::native_emit::lower::lower_module(
            module,
            "main",
            crate::native_emit::NativeLinkage::Executable,
        );
        #[cfg(target_arch = "x86_64")]
        let result = crate::native_emit::x86_64_lower::lower_module(module, "main");
        let lowered = result.unwrap();
        #[cfg(target_arch = "aarch64")]
        assert!(
            lowered.degradations.is_empty(),
            "{:?}",
            lowered.degradations
        );
        #[cfg(target_arch = "aarch64")]
        let offsets = lowered
            .function_offsets
            .iter()
            .map(|(name, offset)| (name.clone(), *offset, 0))
            .collect::<Vec<_>>();
        #[cfg(target_arch = "x86_64")]
        let offsets = lowered
            .exports
            .iter()
            .map(|(name, offset)| (name.clone(), *offset, 0))
            .collect::<Vec<_>>();
        #[cfg(target_arch = "aarch64")]
        let relocations = lowered.relocations;
        #[cfg(target_arch = "x86_64")]
        let relocations = lowered
            .relocations
            .iter()
            .map(|site| (*site, lowered.codegen_base))
            .collect::<Vec<_>>();
        let mut rt = JitRuntime::new();
        rt.load_with_globals(
            &lowered.code,
            &offsets,
            &relocations,
            &lowered.data,
            &lowered.global_relocs,
        )
        .unwrap();
        rt
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn logical_operators_short_circuit_calls() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (expression, expected) in [
                ("false && side()", 0),
                ("true || side()", 1),
                ("true && side()", 11),
                ("false || side()", 11),
                ("(false && side()) || side()", 11),
                ("(true || side()) && side()", 11),
                ("true && !side()", 10),
                ("false || !side()", 10),
                ("(true && !side()) || side()", 21),
            ] {
                let source = format!(
                    "var calls: Int = 0;\nfn side() -> Bool {{ calls += 1; return true; }}\nfn main() -> Int {{ let value: Bool = {expression}; if value {{ return calls * 10 + 1; }} return calls * 10; }}"
                );
                assert_eq!(
                    invoke_source(&source, profile, &[]),
                    Some(expected),
                    "{profile:?}: {expression}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn signed_integer_division_and_remainder() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (body, a, b, expected) in [
                ("return a / b;", -9, 4, -2),
                ("return a / b;", 9, -4, -2),
                ("return a / b;", -9, -4, 2),
                ("return a % b;", -9, 4, -1),
                ("return a % b;", 9, -4, 1),
                ("return a % b;", -9, -4, -1),
                ("return a / b;", i64::MIN, -1, i64::MIN),
                ("return a % b;", i64::MIN, -1, 0),
                ("return 100 + a / b;", 9, 0, 1),
                ("return 100 + a % b;", 9, 0, 1),
                ("var value: Int = a; value /= b; return value;", -9, 4, -2),
                ("var value: Int = a; value %= b; return value;", -9, 4, -1),
            ] {
                let source = format!("fn main(a: Int, b: Int) -> Int {{ {body} }}");
                assert_eq!(
                    invoke_source(&source, profile, &[a, b]),
                    Some(expected),
                    "{profile:?}: {body}, {a}, {b}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn jit_invocation_passes_all_register_arguments() {
        let limit = if cfg!(target_arch = "aarch64") { 8 } else { 6 };
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for count in 1..=limit + 1 {
                let parameters = (0..count)
                    .map(|i| format!("a{i}: Int"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let expression = (0..count)
                    .map(|i| format!("a{i} * {}", i + 1))
                    .collect::<Vec<_>>()
                    .join(" + ");
                let source = format!("fn main({parameters}) -> Int {{ return {expression}; }}");
                let args = (1..=count as i64).collect::<Vec<_>>();
                let expected = (count <= limit).then(|| args.iter().map(|a| a * a).sum());
                assert_eq!(
                    invoke_source(&source, profile, &args),
                    expected,
                    "{profile:?}: {count} arguments"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn jit_invocation_preserves_the_callers_error_register() {
        #[inline(never)]
        unsafe extern "C" fn invoke_rt(rt: *const JitRuntime) -> i64 {
            unsafe { (*rt).invoke("main", &[]).unwrap() }
        }
        let module =
            crate::in_lang_parse::parse_in_source("fn main() -> Int { return 42; }").unwrap();
        let lowered = crate::native_emit::lower::lower_module(
            &module,
            "main",
            crate::native_emit::NativeLinkage::Executable,
        )
        .unwrap();
        let offsets = lowered
            .function_offsets
            .iter()
            .map(|(name, offset)| (name.clone(), *offset, 0))
            .collect::<Vec<_>>();
        let mut rt = JitRuntime::new();
        rt.load(&lowered.code, &offsets, &lowered.relocations)
            .unwrap();
        let result: i64;
        let preserved: u64;
        unsafe {
            // Restore the host register even when the boundary violates its ABI.
            std::arch::asm!(
                "str x27, [sp, #-16]!", "mov x27, #0x1234", "blr x16",
                "mov x9, x27", "ldr x27, [sp], #16",
                in("x16") invoke_rt as *const (), in("x0") &rt as *const JitRuntime,
                lateout("x0") result, lateout("x9") preserved, clobber_abi("C"),
            );
        }
        assert_eq!(result, 42);
        assert_eq!(preserved, 0x1234);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn jit_entry_stack_obeys_sysv_alignment() {
        // Return RSP modulo 16 before a generated prologue can change it.
        let code = [0x48, 0x89, 0xE0, 0x48, 0x83, 0xE0, 0x0F, 0xC3];
        let mut rt = JitRuntime::new();
        rt.load(&code, &[("main".into(), 0, code.len() as u32)], &[])
            .unwrap();
        assert_eq!(unsafe { rt.invoke("main", &[]) }, Some(8));
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn jit_invocation_preserves_the_callers_error_register() {
        #[inline(never)]
        unsafe extern "sysv64" fn invoke_rt(rt: *const JitRuntime) -> i64 {
            unsafe { (*rt).invoke("main", &[]).unwrap() }
        }
        let module =
            crate::in_lang_parse::parse_in_source("fn main() -> Int { return 42; }").unwrap();
        let rt = runtime_for_module(&module, crate::emit_profile::EmitProfile::Default);
        let result: i64;
        let preserved: i64;
        unsafe {
            std::arch::asm!(
                "push r15", "mov r15, 0x12345678", "mov r10, rsp", "and rsp, -16",
                "sub rsp, 16", "mov [rsp], r10", "call r11", "mov rsp, [rsp]",
                "mov rdx, r15", "pop r15",
                in("r11") invoke_rt as *const (), in("rdi") &rt as *const JitRuntime,
                lateout("rax") result, lateout("rdx") preserved, clobber_abi("sysv64"),
            );
        }
        assert_eq!(result, 42);
        assert_eq!(preserved, 0x12345678);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn generated_functions_preserve_callee_saved_registers() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (source, expected) in [
                ("fn main(a: Int, b: Int) -> Int { return a + b; }", 13),
                ("fn main(a: Int, b: Int) -> Int { return a / b; }", 2),
                ("fn main(a: Bool, b: Bool) -> Bool { return a && b; }", 1),
                (
                    "fn main(a: Int, b: Int) -> Int { let values = [1, 2]; let index = 1; values[index] = a + b; return values[index]; }",
                    13,
                ),
            ] {
                let module = crate::in_lang_parse::parse_in_source(source).expect("parse");
                let previous = crate::native_emit::antidecomp::current_profile();
                crate::native_emit::antidecomp::set_profile(profile);
                let lowered = crate::native_emit::x86_64_lower::lower_module(&module, "main");
                crate::native_emit::antidecomp::set_profile(previous);
                let lowered = lowered.expect("lower");
                let offsets = lowered
                    .exports
                    .iter()
                    .map(|(name, offset)| (name.clone(), *offset, 0))
                    .collect::<Vec<_>>();
                let relocations = lowered
                    .relocations
                    .iter()
                    .map(|site| (*site, lowered.codegen_base))
                    .collect::<Vec<_>>();
                let mut rt = JitRuntime::new();
                rt.load_with_globals(
                    &lowered.code,
                    &offsets,
                    &relocations,
                    &lowered.data,
                    &lowered.global_relocs,
                )
                .expect("load");
                let entry = rt.functions.read().unwrap()["main"].entry;
                let result: i64;
                let preserved: u64;
                unsafe {
                    // Restore the host's RBX even when the generated function violates its ABI.
                    std::arch::asm!(
                        "push rbx", "sub rsp, 8", "mov rbx, 0x12345678", "call r11",
                        "mov rdx, rbx", "add rsp, 8", "pop rbx",
                        in("r11") entry, in("rdi") 9_i64, in("rsi") 4_i64,
                        lateout("rax") result, lateout("rdx") preserved,
                        clobber_abi("C"),
                    );
                }
                assert_eq!(result, expected, "{source}");
                assert_eq!(preserved, 0x12345678, "{source}");
            }
        }
    }

    #[test]
    fn jit_runtime_new_is_empty() {
        let rt = JitRuntime::new();
        assert_eq!(rt.function_count(), 0);
    }
}
