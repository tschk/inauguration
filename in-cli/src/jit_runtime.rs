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
#[cfg(not(windows))]
use std::ffi::c_void;
use std::sync::{Mutex, RwLock};

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
    code_pages: Vec<MappedPage>,
    /// Mapped read-write pages holding the lowered data section (globals).
    /// Kept alive for the lifetime of the runtime so executed code can
    /// read and write global slots.
    data_pages: Vec<MappedPage>,
    /// Function dispatch table: name → entry point.
    functions: RwLock<HashMap<String, JitFunction>>,
    /// Context page: error flag at +0, value at +8, type at +16, array arena at +24.
    /// Set by invoke() into X27 (AArch64) or R15 (x86_64).
    error_page: Option<MappedPage>,
    // Box keeps the allocation arena address stable when the runtime moves.
    _arrays: Box<Mutex<Vec<Box<[u64]>>>>,
}

// The x86 lowerer uses SysV registers even on Windows hosts.
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe extern "sysv64" fn allocate_array(error_page: *mut u8, len: usize) -> *mut u64 {
    unsafe { allocate_array_words(error_page, len) }
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe extern "C" fn allocate_array(error_page: *mut u8, len: usize) -> *mut u64 {
    unsafe { allocate_array_words(error_page, len) }
}

/// The generated caller supplies its runtime's writable context page.
/// Returned arrays remain valid until that runtime is dropped.
unsafe fn allocate_array_words(error_page: *mut u8, len: usize) -> *mut u64 {
    let arena =
        unsafe { error_page.add(24).cast::<usize>().read() as *const Mutex<Vec<Box<[u64]>>> };
    let mut words =
        vec![0_u64; len.checked_add(2).expect("array allocation overflow")].into_boxed_slice();
    words[0] = len as u64;
    words[1] = len as u64;
    let ptr = words.as_mut_ptr();
    unsafe { &*arena }.lock().unwrap().push(words);
    ptr
}

struct MappedPage {
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

// SAFETY: each mapping is exclusively owned and remains valid when moved.
unsafe impl Send for MappedPage {}

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
fn alloc_writable_pages(size: usize) -> Option<*mut u8> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    (ptr != libc::MAP_FAILED).then_some(ptr as *mut u8)
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
    alloc_windows_pages(size, 0x40) // PAGE_EXECUTE_READWRITE
}

#[cfg(windows)]
fn alloc_writable_pages(size: usize) -> Option<*mut u8> {
    alloc_windows_pages(size, 0x04) // PAGE_READWRITE
}

#[cfg(windows)]
fn alloc_windows_pages(size: usize, protection: u32) -> Option<*mut u8> {
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_COMMIT: u32 = 0x1000;
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
            protection,
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

impl MappedPage {
    fn writable(size: usize) -> Option<Self> {
        Some(Self {
            ptr: alloc_writable_pages(size)?,
            size,
            used: 0,
        })
    }

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
            unsafe extern "C" {
                fn __clear_cache(start: *const u8, end: *const u8);
            }
            __clear_cache(self.ptr, self.ptr.add(self.used));
        }
        make_executable(self.ptr, self.size)
    }
}

impl Drop for MappedPage {
    fn drop(&mut self) {
        // Revoke execution before address reuse so Rosetta discards translated code.
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        if self.used > 0
            && unsafe { libc::mprotect(self.ptr.cast(), self.size, libc::PROT_NONE) } != 0
        {
            // Keep the address reserved if translated code cannot be invalidated safely.
            return;
        }
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
        let error_page = MappedPage::writable(64);
        let arrays = Box::new(Mutex::new(Vec::new()));
        if let Some(page) = &error_page {
            unsafe {
                page.ptr
                    .add(24)
                    .cast::<usize>()
                    .write((&*arrays as *const Mutex<Vec<Box<[u64]>>>) as usize);
            }
        }
        Self {
            code_pages: Vec::new(),
            data_pages: Vec::new(),
            functions: RwLock::new(HashMap::new()),
            error_page,
            _arrays: arrays,
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
        if self.error_page.is_none() {
            return Err("jit: error page allocation failed".into());
        }
        // Allocate a code page large enough
        let mut page = MappedPage::new(code.len()).ok_or_else(|| "jit: mmap failed".to_string())?;
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
        let mut mapped_data = None;
        if !data.is_empty() {
            let rounded = data.len().next_multiple_of(0x1000);
            let data_page = MappedPage::writable(rounded)
                .ok_or_else(|| "jit: data section allocation failed".to_string())?;
            let data_ptr = data_page.ptr;
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), data_ptr, data.len());
            }
            mapped_data = Some(data_page);
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
        if let Some(data_page) = mapped_data {
            self.data_pages.push(data_page);
        }
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
    /// responsible for ensuring the function signature matches the arguments
    /// and for invoking a runtime exclusively while its writable state is in use.
    pub unsafe fn invoke(&self, name: &str, args: &[i64]) -> Option<i64> {
        let funcs = self.functions.read().unwrap();
        let func = funcs.get(name)?;
        let entry = func.entry as *const ();
        let error_page = self.error_page.as_ref()?.ptr;
        unsafe {
            error_page.cast::<u64>().write(0);
        }

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
                    in("x16") entry, in("x17") error_page as usize,
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
                    in("r11") entry, in("r10") error_page as usize,
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

    /// Error carried by the last invocation, before decoding its return register.
    pub(crate) fn invocation_error(&self) -> Option<i64> {
        let page = self.error_page.as_ref()?;
        unsafe { (page.ptr.read() != 0).then(|| page.ptr.add(8).cast::<i64>().read()) }
    }

    /// Returns the number of loaded functions.
    pub fn function_count(&self) -> usize {
        self.functions.read().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_error_page_refuses_loading_before_publishing_functions() {
        let mut rt = JitRuntime::new();
        rt.error_page = None;
        assert_eq!(
            rt.load(&[], &[], &[]),
            Err("jit: error page allocation failed".into())
        );
        assert_eq!(rt.function_count(), 0);
        assert!(rt.code_pages.is_empty());
        assert!(rt.data_pages.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_pages_are_released_after_runtime_drop() {
        const CHILD: &str = "IN_TEST_MAPPED_PAGE_DROP_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Isolate the address check from parallel tests that could reuse freed mappings.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "jit_runtime::tests::runtime_pages_are_released_after_runtime_drop",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let mut rt = JitRuntime::new();
        rt.load_with_globals(&[0], &[], &[], &[1], &[]).unwrap();
        let error = rt.error_page.as_ref().unwrap();
        let data = &rt.data_pages[0];
        let code = &rt.code_pages[0];
        let mappings = [
            (error.ptr, error.size),
            (data.ptr, data.size),
            (code.ptr, code.size),
        ];
        drop(rt);
        for (ptr, size) in mappings {
            assert_ne!(
                unsafe { libc::mprotect(ptr.cast(), size, libc::PROT_READ) },
                0,
                "runtime mapping remains allocated after drop"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn executable_page_protection_reports_failures() {
        let page = MappedPage::new(1).expect("code page");
        let error = make_executable(unsafe { page.ptr.add(1) }, page.size - 1)
            .expect_err("unaligned page protection must fail");
        assert!(error.starts_with("jit: executable page protection failed:"));
        page.finalize().expect("valid page protection");
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
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
                        let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                        let rt = runtime_for_module(&module, profile);
                        let page = &rt.code_pages[0];
                        let snapshot = unsafe {
                            std::slice::from_raw_parts(page.ptr, page.used).to_vec()
                        };
                        let result = unsafe { rt.invoke("main", &args) };
                        assert_eq!(
                            unsafe { std::slice::from_raw_parts(page.ptr, page.used) },
                            snapshot,
                            "mapped code changed during invocation"
                        );
                        assert_eq!(
                            result,
                            Some(expected),
                            "worker={worker} iteration={iteration} profile={profile:?} page={:p} bytes={snapshot:02x?}",
                            page.ptr
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
    fn string_patterns_match_independent_payloads_by_content() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (value, pattern, expected) in [
                ("abcdefghijk", "abcdefghijk", 42),
                ("éabc", "éabc", 42),
                ("line\n\t\r\"\\end", "line\n\t\r\"\\end", 42),
                ("", "", 42),
                ("abcdefghijk", "abcdefghijl", 7),
                ("abc", "abcd", 7),
            ] {
                let mut storage = (value.len() as u64).to_ne_bytes().to_vec();
                storage.extend_from_slice(value.as_bytes());
                for body in [
                    format!("match value {{ {pattern:?} {{ return 42; }} _ {{ return 7; }} }}"),
                    format!(
                        "match value {{ captured {{ match captured {{ {pattern:?} {{ return 42; }} _ {{ return 7; }} }} }} }}"
                    ),
                    format!(
                        "try {{ throw value; }} catch caught {{ match caught {{ {pattern:?} {{ return 42; }} _ {{ return 7; }} }} }}"
                    ),
                    format!(
                        "try {{ throw value; }} catch {pattern:?} {{ return 42; }} catch _ {{ return 7; }}"
                    ),
                    format!(
                        "try {{ try {{ throw value; }} catch caught {{ let alias = caught; throw alias; }} }} catch {pattern:?} {{ return 42; }} catch _ {{ return 7; }}"
                    ),
                ] {
                    let source = format!("fn main(value: String) -> Int {{ {body} return 9; }}");
                    let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                    let args = [storage.as_ptr() as i64];
                    assert_eq!(
                        invoke_module(&module, profile, &args),
                        Some(expected),
                        "raw {profile:?}: {source}"
                    );
                    crate::core_opt::optimize_with_profile(
                        &mut module.decls,
                        Some("main"),
                        profile,
                    );
                    assert_eq!(
                        invoke_module(&module, profile, &args),
                        Some(expected),
                        "optimized {profile:?}: {source}"
                    );
                }
            }
            let mut storage = 3u64.to_ne_bytes().to_vec();
            storage.extend_from_slice(b"abc");
            let mut module = crate::in_lang_parse::parse_in_source("fn main(value: String) -> Int { match value { \"abc\" { return 42; } _ { return 7; } } return 9; }").unwrap();
            if let crate::core_ir::Decl::Function { params, .. } = &mut module.decls[0] {
                params[0].1 = crate::core_ir::Typ::Named("str".into());
            }
            assert_eq!(
                invoke_module(&module, profile, &[storage.as_ptr() as i64]),
                Some(42)
            );
            for body in [
                "match value { \"\" { return 1; } _ { return 7; } }",
                "try { throw value; } catch \"\" { return 1; } catch _ { return 7; }",
            ] {
                let module = crate::in_lang_parse::parse_in_source(&format!(
                    "fn main(value: String) -> Int {{ {body} return 9; }}"
                ))
                .unwrap();
                assert_eq!(invoke_module(&module, profile, &[0]), Some(7));
            }
            let module = crate::in_lang_parse::parse_in_source("fn main() -> Int { try { throw 0; } catch \"text\" { return 1; } catch 0 { return 42; } return 7; }").unwrap();
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
            let module = crate::in_lang_parse::parse_in_source("fn main() -> Int { try { throw 123; } catch caught { match caught { \"text\" { return 1; } _ { return 42; } } } return 7; }").unwrap();
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }

    #[cfg(all(unix, target_arch = "x86_64"))]
    #[test]
    fn array_literal_indexing_evaluates_all_elements_once() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (index, expected) in [(0, 13), (1, 24), (9, 1), (-1, 1)] {
                let source = format!(
                    "var calls: Int = 0;\nfn next(digit: Int) -> Int {{ calls = calls * 10 + digit; return calls; }}\nfn main() -> Int {{ let item = [next(1), next(2)][{index}]; return item + calls; }}"
                );
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                assert_eq!(invoke_module(&module, profile, &[]), Some(expected));
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(invoke_module(&module, profile, &[]), Some(expected));
                if index < 0 || index >= 2 {
                    let source = source.replace("fn main()", "fn invalid()");
                    let source = format!(
                        "{source}\nfn main() -> Int {{ let result = invalid(); if result == 1 && calls == 12 {{ return 42; }} return 0; }}"
                    );
                    assert_eq!(invoke_source(&source, profile, &[]), Some(42));
                }
            }
            let items = (0..400)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let scratch = vec!["9"; 64].join(",");
            for (source, expected) in [
                (format!("fn index() -> Int {{ let scratch: [Int] = [{scratch}]; return scratch[0] - 8; }}\nfn main() -> Int {{ return [{items}][index()]; }}"), 1),
                ("var calls: Int = 0;\nfn fail() -> Int { calls += 1; throw 9; }\nfn next() -> Int { calls += 10; return 1; }\nfn main() -> Int { try { return [fail(), next()][0]; } catch 9 { return [calls, 99][0]; } return 0; }".into(), 1),
                ("var calls: Int = 0;\nfn index() -> Int { calls += 1; return 0; }\nfn main() -> Int { let item = [][index()]; return item + calls; }".into(), 1),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                assert_eq!(invoke_module(&module, profile, &[]), Some(expected));
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(invoke_module(&module, profile, &[]), Some(expected));
            }
        }
    }

    #[cfg(all(unix, target_arch = "x86_64"))]
    #[test]
    fn array_dynamic_stores_check_bounds_and_evaluate_index_before_value() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (index, expected) in [(1, 29), (9, 1), (-1, 1)] {
                let source = format!(
                    "var calls: Int = 0;\nfn index() -> Int {{ calls = calls * 10 + 1; return {index}; }}\nfn value() -> Int {{ calls = calls * 10 + 2; return 7; }}\nfn main() -> Int {{ let xs: [Int] = [10, 20]; xs[index()] = value(); return xs[0] + xs[1] + calls; }}"
                );
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                assert_eq!(invoke_module(&module, profile, &[]), Some(expected));
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(invoke_module(&module, profile, &[]), Some(expected));
                if index < 0 || index >= 2 {
                    let source = source.replace("fn main()", "fn invalid()");
                    let source = format!(
                        "{source}\nfn main() -> Int {{ let result = invalid(); if result == 1 && calls == 12 {{ return 42; }} return 0; }}"
                    );
                    assert_eq!(invoke_source(&source, profile, &[]), Some(42));
                }
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn array_for_evaluates_collection_once() {
        use crate::core_ir::{Decl, Expr, LoopKind, Stmt};
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut module = crate::in_lang_parse::parse_in_source("var calls: Int = 0;\nfn next(digit: Int) -> Int { calls = calls * 10 + digit; return calls; }\nfn main() -> Int { var sum: Int = 0; return sum + calls; }").unwrap();
            let body = module
                .decls
                .iter_mut()
                .find_map(|decl| match decl {
                    Decl::Function { name, body, .. } if name == "main" => Some(body),
                    _ => None,
                })
                .unwrap();
            body.insert(
                1,
                Stmt::Loop {
                    kind: LoopKind::For {
                        binding: "item".into(),
                    },
                    cond: Some(Expr::ArrayLit(
                        [1, 2]
                            .into_iter()
                            .map(|digit| Expr::Call {
                                callee: Box::new(Expr::Ident("next".into())),
                                args: vec![Expr::IntLit(digit)],
                            })
                            .collect(),
                    )),
                    body: vec![Stmt::Assign(
                        "sum".into(),
                        Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Ident("sum".into())),
                            rhs: Box::new(Expr::Ident("item".into())),
                        },
                    )],
                },
            );
            #[cfg(target_arch = "x86_64")]
            assert_eq!(invoke_module(&module, profile, &[]), Some(25));
            crate::lower_core::desugar_module(&mut module);
            assert_eq!(invoke_module(&module, profile, &[]), Some(25));
            crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            assert_eq!(invoke_module(&module, profile, &[]), Some(25));
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn desugared_for_continues_advance_only_their_own_loop() {
        use crate::core_ir::{Decl, Expr, LoopKind, Stmt};
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (continuation, expected) in [
                ("if item == 2 { continue; }", 43),
                ("if item == 2 { if visits == 2 { continue; } }", 43),
                ("match item { 2 { continue; } _ { } }", 43),
                (
                    "if item == 2 { try { throw 9; } catch 9 { continue; } }",
                    43,
                ),
                (
                    "if item == 2 { try { continue; } catch _ { return 998; } }",
                    43,
                ),
                (
                    "var inner: Int = 0; while inner < 2 { inner += 1; if inner == 1 { continue; } } if item == 2 { continue; }",
                    43,
                ),
                ("if item == 2 { break; }", 12),
                ("if item == 2 { return 42; }", 42),
            ] {
                for iter in [
                    Expr::ArrayLit(vec![Expr::IntLit(1), Expr::IntLit(2), Expr::IntLit(3)]),
                    Expr::Call {
                        callee: Box::new(Expr::Ident("range".into())),
                        args: vec![Expr::IntLit(1), Expr::IntLit(4)],
                    },
                ] {
                    let source = format!(
                        "fn main(item: Int) -> Int {{ var sum: Int = 0; var visits: Int = 0; while true {{ visits += 1; if visits > 5 {{ return 999; }} {continuation} sum += item; }} return sum * 10 + visits; }}"
                    );
                    let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                    let body = module
                        .decls
                        .iter_mut()
                        .find_map(|decl| match decl {
                            Decl::Function { name, body, .. } if name == "main" => Some(body),
                            _ => None,
                        })
                        .unwrap();
                    let Stmt::Loop { kind, cond, .. } = &mut body[2] else {
                        panic!("loop");
                    };
                    *kind = LoopKind::For {
                        binding: "item".into(),
                    };
                    *cond = Some(iter);
                    let verify = |module: &crate::core_ir::UnifiedModule| {
                        let rt = runtime_for_module(module, profile);
                        assert_eq!(
                            unsafe { rt.invoke("main", &[0]) },
                            Some(expected),
                            "{continuation}"
                        );
                        assert!(rt.invocation_error().is_none(), "{continuation}");
                    };
                    #[cfg(target_arch = "x86_64")]
                    verify(&module);
                    crate::lower_core::desugar_module(&mut module);
                    verify(&module);
                    crate::core_opt::optimize_with_profile(
                        &mut module.decls,
                        Some("main"),
                        profile,
                    );
                    verify(&module);
                }
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn loop_jumps_unwind_only_exited_try_scopes() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (source, expected) in [
                (
                    "fn main() -> Int { var sum: Int = 0; var i: Int = 0; try { while i < 3 { i += 1; try { if i == 2 { continue; } } catch _ { return 998; } sum += i; } } catch _ { return 999; } return sum * 10 + i; }",
                    43,
                ),
                (
                    "fn main() -> Int { var sum: Int = 0; var i: Int = 0; while i < 3 { i += 1; try { try { if i == 2 { throw 9; } } catch 9 { break; } } catch _ { return 999; } sum += i; } return sum * 10 + i; }",
                    12,
                ),
                (
                    "fn main() -> Int { var sum: Int = 0; var i: Int = 0; try { throw 9; } catch 9 { while i < 3 { i += 1; try { if i == 2 { continue; } } catch _ { return 998; } sum += i; } } return sum * 10 + i; }",
                    43,
                ),
                (
                    "fn fail() -> Int { throw 7; }\nfn main() -> Int { var sum: Int = 0; var i: Int = 0; try { while i < 3 { i += 1; try { if i == 2 { continue; } } catch _ { return 998; } if i == 3 { fail(); } sum += i; } } catch 7 { return sum * 10 + i; } return 999; }",
                    13,
                ),
                (
                    "fn main() -> Int { var sum: Int = 0; var i: Int = 0; try { while i < 3 { i += 1; var inner: Int = 0; while inner < 3 { inner += 1; try { if inner == 2 { break; } } catch _ { return 998; } sum += 10; } sum += i; } } catch _ { return 999; } return sum; }",
                    36,
                ),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                let verify = |module: &crate::core_ir::UnifiedModule| {
                    let rt = runtime_for_module(module, profile);
                    assert_eq!(
                        unsafe { rt.invoke("main", &[]) },
                        Some(expected),
                        "{source}"
                    );
                    assert!(rt.invocation_error().is_none(), "{source}");
                };
                verify(&module);
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                verify(&module);
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn ranges_snapshot_bounds_and_keep_iteration_separate_from_bindings() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (source, expected) in [
                (
                    "var calls: Int = 0;\nfn start() -> Int { calls = calls * 10 + 1; return 0; }\nfn limit() -> Int { calls = calls * 10 + 2; return 3; }\nfn main() -> Int { var sum: Int = 0; for item in start()..limit() { sum += item; } return sum + calls; }",
                    15,
                ),
                (
                    "fn main() -> Int { var sum: Int = 0; for item in 1..4 { sum += item; item = 99; } return sum; }",
                    6,
                ),
                (
                    "fn main() -> Int { var visits: Int = 0; var count: Int = 0; for item in 0..2 { visits += 1; if visits > 3 { return 999; } for item in 0..2 { count += 1; } } return count; }",
                    4,
                ),
                (
                    "fn main() -> Int { var __for_0: Int = 100; var __for_end_0: Int = 200; var sum: Int = 0; for item in 0..3 { sum += item; } return sum + __for_0 + __for_end_0; }",
                    303,
                ),
                (
                    "var calls: Int = 0;\nfn start() -> Int { calls += 1; throw 7; }\nfn limit() -> Int { calls += 10; return 3; }\nfn main() -> Int { try { for item in start()..limit() { } } catch 7 { return calls; } return 999; }",
                    1,
                ),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "{source}"
                );
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "{source}"
                );
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn rust_range_bindings_do_not_control_iteration() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut module = crate::compiler::rust_front::parse_rust_source("fn main() -> i64 { let mut sum = 0; for mut item in 1..4 { sum = sum + item; item = 99; } return sum; }").unwrap();
            crate::lower_core::desugar_module(&mut module);
            assert_eq!(invoke_module(&module, profile, &[]), Some(6));
            crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            assert_eq!(invoke_module(&module, profile, &[]), Some(6));
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn integer_address_indexing_keeps_headerless_word_layout() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (source, expected, writes) in [
                (
                    "fn main(base: Int) -> Int { base[1] = 7; return base[1]; }",
                    7,
                    true,
                ),
                (
                    "var calls: Int = 0;\nfn address(base: Int) -> Int { calls = calls * 10 + 1; return base; }\nfn index() -> Int { calls = calls * 10 + 2; return 1; }\nfn value() -> Int { calls = calls * 10 + 3; return 7; }\nfn main(base: Int) -> Int { address(base)[index()] = value(); return base[1] + calls; }",
                    130,
                    true,
                ),
                (
                    "fn main(base: Int) -> Int { base[1] = 7; return (base + 16)[-1]; }",
                    7,
                    true,
                ),
                (
                    "var calls: Int = 0;\nfn address(base: Int) -> Int { calls = calls * 10 + 1; return base; }\nfn index() -> Int { calls = calls * 10 + 2; throw 9; }\nfn value() -> Int { calls = calls * 10 + 3; return 7; }\nfn main(base: Int) -> Int { try { address(base)[index()] = value(); } catch 9 { return calls; } return 999; }",
                    12,
                    false,
                ),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                for optimized in [false, true] {
                    if optimized {
                        crate::core_opt::optimize_with_profile(
                            &mut module.decls,
                            Some("main"),
                            profile,
                        );
                    }
                    let rt = runtime_for_module(&module, profile);
                    let mut words = [10_i64, 20, 30, 40];
                    assert_eq!(
                        unsafe { rt.invoke("main", &[words.as_mut_ptr() as i64]) },
                        Some(expected)
                    );
                    assert_eq!(
                        words,
                        if writes {
                            [10, 7, 30, 40]
                        } else {
                            [10, 20, 30, 40]
                        }
                    );
                    assert!(rt.invocation_error().is_none());
                }
            }
        }
    }

    #[cfg(all(unix, target_arch = "x86_64"))]
    #[test]
    fn array_length_calls_preserve_returned_lengths_and_effects() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (source, expected) in [
                (
                    "var calls: Int = 0;\nfn values() -> [Int] { calls += 1; return [1, 2]; }\nfn main() -> Int { return array_len(values()) + calls; }",
                    3,
                ),
                (
                    "var calls: Int = 0;\nfn next(digit: Int) -> Int { calls = calls * 10 + digit; return calls; }\nfn values() -> [Int] { let xs = [next(1), next(2)]; return xs; }\nfn main() -> Int { let xs = values(); return xs[0] * 100 + xs[1] + calls; }",
                    124,
                ),
                (
                    "fn values() -> [Int] { return [1, 2]; }\nfn forward() -> [Int] { return values(); }\nfn update(xs: [Int]) -> void { xs[0] = 42; }\nfn main() -> Int { let xs = forward(); let alias = xs; update(alias); return xs[0] + array_len(alias); }",
                    44,
                ),
                (
                    "fn values() -> [Int] { return [10, 20]; }\nfn index() -> Int { let scratch = [8, 9]; return scratch[1] - 8; }\nfn main() -> Int { return values()[index()]; }",
                    20,
                ),
                (
                    "fn values() -> [Int] { return []; }\nfn main() -> Int { return array_len(values()) + values()[0]; }",
                    1,
                ),
                (
                    "fn values() -> [String] { return [\"a\", \"bc\"]; }\nfn main() -> Int { return str_len(values()[1]); }",
                    2,
                ),
                (
                    "fn values() -> [Bool] { return [true, false]; }\nfn main() -> Int { if values()[0] { return 7; } return 0; }",
                    7,
                ),
                (
                    "fn values() -> [Float] { return [1.25, 2.5]; }\nfn main() -> Float { return values()[0] + values()[1]; }",
                    3.75_f64.to_bits() as i64,
                ),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "{source}"
                );
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "{source}"
                );
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    #[test]
    fn native_array_allocation_uses_aligned_system_calls() {
        unsafe extern "sysv64" fn allocation_failure(_: usize) -> usize {
            0
        }
        unsafe extern "sysv64" fn abort_observed() -> ! {
            std::process::exit(73)
        }
        let oom = std::env::var_os("IN_TEST_NATIVE_ARRAY_OOM").is_some();
        if !oom {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "jit_runtime::tests::native_array_allocation_uses_aligned_system_calls",
                ])
                .env("IN_TEST_NATIVE_ARRAY_OOM", "1")
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(73),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let _profile = crate::native_emit::antidecomp::ProfileGuard::new(profile);
            let module = crate::in_lang_parse::parse_in_source("fn values() -> [Int] { return [1, 2, 3]; }\nfn main() -> [Int] { return values(); }").unwrap();
            let mut lowered =
                crate::native_emit::x86_64_lower::lower_native_module(&module, "main").unwrap();
            assert_eq!(lowered.externs.len(), 2);
            for name in ["malloc", "abort"] {
                let stub = lowered.code.len() as u32;
                let ptr = if oom {
                    match name {
                        "malloc" => allocation_failure as *const () as usize,
                        "abort" => abort_observed as *const () as usize,
                        _ => unreachable!(),
                    }
                } else {
                    crate::native_emit::native_link::resolve_native_fn(name).unwrap() as usize
                };
                lowered
                    .code
                    .extend(crate::native_emit::x86_64::load_i64(0, ptr as i64));
                lowered.code.extend([0xFF, 0xE0]); // tail call keeps the caller's aligned C frame
                for (target, site) in &lowered.extern_calls {
                    if target == name {
                        let delta = stub as i32 - *site as i32 - 5;
                        lowered.code[*site as usize + 1..*site as usize + 5]
                            .copy_from_slice(&delta.to_le_bytes());
                    }
                }
            }
            let offsets = lowered
                .exports
                .iter()
                .map(|(name, off)| (name.clone(), *off, 0))
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
            .unwrap();
            let ptr = unsafe { rt.invoke("main", &[]) }.unwrap() as *mut u64;
            unsafe {
                assert_eq!(std::slice::from_raw_parts(ptr, 5), &[3, 3, 1, 2, 3]);
                libc::free(ptr.cast());
            }
            assert!(rt._arrays.lock().unwrap().is_empty());
        }
    }

    #[cfg(all(unix, target_arch = "x86_64"))]
    #[test]
    fn returned_arrays_remain_owned_across_runtime_moves_and_calls() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let module = crate::in_lang_parse::parse_in_source("var counter: Int = 0;\nfn values() -> [Int] { counter += 1; return [counter, counter + 10]; }\nfn main() -> Int { return 0; }").unwrap();
            let rt = runtime_for_module(&module, profile);
            let first = unsafe { rt.invoke("values", &[]) }.unwrap() as *const u64;
            let moved = Box::new(rt);
            let second = unsafe { moved.invoke("values", &[]) }.unwrap() as *const u64;
            unsafe {
                assert_eq!(std::slice::from_raw_parts(first, 4), &[2, 2, 1, 11]);
                assert_eq!(std::slice::from_raw_parts(second, 4), &[2, 2, 2, 12]);
            }
            assert_eq!(moved._arrays.lock().unwrap().len(), 2);
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn array_lengths_evaluate_literal_elements_once_in_order() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (source, expected) in [
                (
                    "var calls: Int = 0;\nfn next(digit: Int) -> Int { calls = calls * 10 + digit; return calls; }\nfn main() -> Int { return array-len([next(1), next(2)]) + calls; }",
                    14,
                ),
                ("fn main() -> Int { return array_len([]); }", 0),
                ("fn main() -> Int { return array_len([\"a\", \"b\"]); }", 2),
                (
                    "var calls: Int = 0;\nfn fail() -> Int { calls += 1; throw 9; }\nfn next() -> Int { calls += 10; return 1; }\nfn main() -> Int { try { return array-len([fail(), next()]); } catch 9 { return calls; } return 0; }",
                    1,
                ),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
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
    fn string_lengths_preserve_bytes_nulls_and_single_evaluation() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for name in ["str-len", "str_len", "__inrt_str_len"] {
                for (source, args, expected) in [
                    (
                        format!("fn main() -> Int {{ return {name}(\"é🙂\"); }}"),
                        vec![],
                        6,
                    ),
                    (
                        format!("fn main() -> Int {{ return {name}(\"\"); }}"),
                        vec![],
                        0,
                    ),
                    (
                        format!("fn main(value: String) -> Int {{ return {name}(value); }}"),
                        vec![0],
                        0,
                    ),
                    (
                        format!(
                            "var calls: Int = 0;\nfn text() -> String {{ calls += 1; return \"abc\"; }}\nfn main() -> Int {{ return {name}(text()) + calls; }}"
                        ),
                        vec![],
                        4,
                    ),
                ] {
                    let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                    assert_eq!(
                        invoke_module(&module, profile, &args),
                        Some(expected),
                        "raw {profile:?}: {source}"
                    );
                    crate::core_opt::optimize_with_profile(
                        &mut module.decls,
                        Some("main"),
                        profile,
                    );
                    assert_eq!(
                        invoke_module(&module, profile, &args),
                        Some(expected),
                        "optimized {profile:?}: {source}"
                    );
                }
            }
        }
    }

    #[cfg(all(unix, target_arch = "aarch64"))]
    #[test]
    fn runtime_builtin_arguments_keep_nested_results_and_effects() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let source = "var calls: Int = 0;\nfn text() -> String { calls += 1; return \"abcd\"; }\nfn start() -> Int { calls += 1; return 1; }\nfn count() -> Int { calls += 1; return 2; }\nfn main() -> Int { let value = __inrt_str_substr(text(), start(), count()); if __inrt_str_eq(value, \"bc\") { return __inrt_str_len(value) + calls; } return 0; }";
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            assert_eq!(
                invoke_module(&module, profile, &[]),
                Some(5),
                "raw {profile:?}"
            );
            crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            assert_eq!(
                invoke_module(&module, profile, &[]),
                Some(5),
                "optimized {profile:?}"
            );
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn filesystem_calls_expose_boolean_success_and_failure() {
        crate::native_emit::native_link::bootstrap_jit_native();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (create, remove) in [("create-dir", "remove-file"), ("create_dir", "remove_file")] {
                for optimize in [false, true] {
                    let nonce = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos();
                    let directory = std::env::temp_dir().join(format!(
                        "inauguration-fs-jit-{}-{nonce}",
                        std::process::id()
                    ));
                    let file = directory.join("file");
                    let source = format!(
                        "fn main() -> Bool {{ if !{create}({directory:?}) {{ return false; }} if !write-file({file:?}, \"text\") {{ return false; }} if !{remove}({file:?}) {{ return false; }} if {remove}({file:?}) {{ return false; }} if {create}({directory:?}) {{ return false; }} return true; }}"
                    );
                    let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                    let verification = crate::core_ir_verifier::verify_for_entry(&module, "main");
                    assert!(verification.ok, "{verification:?}");
                    if optimize {
                        crate::core_opt::optimize_with_profile(
                            &mut module.decls,
                            Some("main"),
                            profile,
                        );
                    }
                    let result = invoke_module(&module, profile, &[]);
                    let directory_created = directory.is_dir();
                    let file_removed = !file.exists();
                    if file.exists() {
                        std::fs::remove_file(&file).unwrap();
                    }
                    if directory.exists() {
                        std::fs::remove_dir(&directory).unwrap();
                    }
                    assert_eq!(result, Some(1), "{profile:?}/{create}/{optimize}");
                    assert!(directory_created && file_removed);
                }
            }
        }
    }

    #[cfg(all(unix, target_arch = "x86_64"))]
    #[test]
    fn jit_stdlib_results_survive_nested_argument_calls() {
        crate::native_emit::native_link::bootstrap_jit_native();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for expression in [
                "str-eq(str-concat(\"ab\", \"c\"), \"abc\")",
                "str-eq(\"abc\", str-concat(\"ab\", \"c\"))",
                "str-eq(str-concat(\"ab\", \"c\"), str-concat(\"a\", \"bc\"))",
                "str_eq(str_concat(\"ab\", \"c\"), str_concat(\"a\", \"bc\"))",
                "str-eq(process-run(\"printf abc\"), \"abc\")",
                "str_eq(process_run(\"printf abc\"), \"abc\")",
                "str_eq(path_join(\"/tmp\", \"file\"), \"/tmp/file\")",
            ] {
                let source = format!("fn main() -> Bool {{ return {expression}; }}");
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                let verification = crate::core_ir_verifier::verify_for_entry(&module, "main");
                assert!(verification.ok, "{verification:?}");
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(1),
                    "raw {profile:?}: {source}"
                );
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(1),
                    "optimized {profile:?}: {source}"
                );
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn match_bindings_keep_values_types_scope_and_error_tags() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (source, expected) in [
                (
                    "fn main() -> Int { match 5 { captured { return captured; } 5 { return 9; } } return 0; }",
                    5,
                ),
                (
                    "fn main() -> Int { match false { captured { match captured { false { return 2; } _ { return 1; } } } } return 0; }",
                    2,
                ),
                (
                    "fn main() -> Int { match 3.75 { captured { if captured == 3.75 { return 2; } } } return 0; }",
                    2,
                ),
                (
                    "fn text() -> String { return \"abc\"; }\nfn main() -> Int { match text() { captured { match captured { \"abc\" { return 2; } _ { return 1; } } } } return 0; }",
                    2,
                ),
                (
                    "var calls: Int = 0;\nfn value() -> Int { calls += 1; return 5; }\nfn main() -> Int { match value() { captured { return captured + calls; } } return 0; }",
                    6,
                ),
                (
                    "fn main() -> Int { let captured = 7; match 5 { captured { if captured != 5 { return 0; } } } return captured; }",
                    7,
                ),
                (
                    "fn main() -> Int { try { try { throw false; } catch caught { match caught { captured { throw captured; } } } } catch 0 { return 1; } catch false { return 2; } return 0; }",
                    2,
                ),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
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
    fn scalar_matches_preserve_arm_order_and_payload_types() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (typ, argument, body, expected) in [
                (
                    "Int",
                    5,
                    "match value { _ { count = 1; } 5 { count = 2; } } return count;",
                    1,
                ),
                (
                    "Int",
                    5,
                    "match value { 4 { count = 4; } _ { count = 1; } 5 { count = 2; } } return count;",
                    1,
                ),
                (
                    "Int",
                    5,
                    "match value { 5 { count = 1; } 5 { count = 2; } _ { count = 3; } } return count;",
                    1,
                ),
                (
                    "Bool",
                    0,
                    "match value { true { return 1; } false { return 2; } _ { return 3; } } return 4;",
                    2,
                ),
                (
                    "Bool",
                    1,
                    "match value { false { return 1; } true { return 2; } _ { return 3; } } return 4;",
                    2,
                ),
                (
                    "Bool",
                    0,
                    "try { throw value; } catch caught { match caught { 0 { return 1; } false { return 2; } _ { return 3; } } } return 4;",
                    2,
                ),
                (
                    "Int",
                    0,
                    "try { throw value; } catch caught { match caught { false { return 1; } 0 { return 2; } _ { return 3; } } } return 4;",
                    2,
                ),
                (
                    "Int",
                    0,
                    "try { throw 0.0; } catch caught { match caught { false { return 1; } 0 { return 2; } _ { return 3; } } } return 4;",
                    3,
                ),
                (
                    "Int",
                    5,
                    "let values = [5, 6]; match values[0] { 5 { return 1; } _ { return 2; } } return 3;",
                    1,
                ),
                (
                    "Int",
                    5,
                    "let values = [false, true]; match values[0] { true { return 1; } false { return 2; } _ { return 3; } } return 4;",
                    2,
                ),
            ] {
                let source =
                    format!("var count: Int = 0;\nfn main(value: {typ}) -> Int {{ {body} }}");
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                assert_eq!(
                    invoke_module(&module, profile, &[argument]),
                    Some(expected),
                    "raw {profile:?}: {source}"
                );
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&module, profile, &[argument]),
                    Some(expected),
                    "optimized {profile:?}: {source}"
                );
            }
            for body in [
                "match 5 { _ { return 1; } 5 { return 2; } } return 3;",
                "match false { _ { return 1; } false { return 2; } } return 3;",
                "match \"line\\n\" { \"line\\n\" { return 1; } _ { return 2; } } return 3;",
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(&format!(
                    "fn main() -> Int {{ {body} }}"
                ))
                .unwrap();
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(1),
                    "raw {profile:?}: {body}"
                );
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(1),
                    "optimized {profile:?}: {body}"
                );
            }
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn catches_select_in_order_and_propagate_unmatched_errors() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (body, expected) in [
                (
                    "try { throw 42; } catch 41 { return 1; } catch 42 { return 2; } return 3;",
                    2,
                ),
                (
                    "try { throw 42; } catch 41 { return 1; } catch error { return error; } return 3;",
                    42,
                ),
                (
                    "try { throw 42; } catch _ { return 9; } catch 42 { return 2; } return 3;",
                    9,
                ),
                (
                    "try { throw 42; } catch 41 { count = 1; } catch 42 { count = 2; } count += 3; return count;",
                    5,
                ),
                (
                    "try { try { throw 43; } catch 41 { return 1; } catch 42 { return 2; } return 3; } catch outer { return outer; } return 4;",
                    43,
                ),
                (
                    "try { throw false; } catch true { return 1; } catch false { return 2; } return 3;",
                    2,
                ),
                (
                    "try { throw false; } catch 0 { return 1; } catch false { return 2; } return 3;",
                    2,
                ),
                (
                    "try { throw 0; } catch false { return 1; } catch 0 { return 2; } return 3;",
                    2,
                ),
                (
                    "try { throw 0.0; } catch 0 { return 1; } catch false { return 2; } catch _ { return 4; } return 3;",
                    4,
                ),
                (
                    "try { try { throw false; } catch value { throw value; } } catch 0 { return 1; } catch false { return 2; } return 3;",
                    2,
                ),
                (
                    "try { try { throw false; } catch value { let alias = value; let next = alias; throw next; } } catch 0 { return 1; } catch false { return 2; } return 3;",
                    2,
                ),
                (
                    "try { try { throw false; } catch value { let alias = value; alias = 0; throw alias; } } catch false { return 1; } catch 0 { return 2; } return 3;",
                    2,
                ),
                (
                    "try { try { throw 0; } catch value { value = false; throw value; } } catch 0 { return 1; } catch false { return 2; } return 3;",
                    2,
                ),
                (
                    "try { try { throw false; } catch value { try { throw 42; } catch other { count = other; } throw value; } } catch 0 { return 1; } catch false { return 2; } return 3;",
                    2,
                ),
                (
                    "try { throw -42; } catch 42 { return 1; } catch -42 { return 2; } return 3;",
                    2,
                ),
                (
                    "try { throw 42; } catch count { return count; } return 3;",
                    42,
                ),
                (
                    "try { try { throw 42; } catch 42 { throw 43; } } catch 42 { return 1; } catch 43 { return 2; } return 3;",
                    2,
                ),
            ] {
                let source = format!("var count: Int = 0;\nfn main() -> Int {{ {body} }}");
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
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
            let module = crate::in_lang_parse::parse_in_source(
                "fn main() -> Int { try { throw 42; } catch 41 { return 1; } return 3; }",
            )
            .unwrap();
            let rt = runtime_for_module(&module, profile);
            assert_eq!(unsafe { rt.invoke("main", &[]) }, Some(42));
            assert_eq!(rt.invocation_error(), Some(42));
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn throws_exit_to_the_nearest_handler_and_skip_later_effects() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (helpers, main, expected) in [
                (
                    "",
                    "try { throw 42; count = 7; } catch error { return count; } return -1;",
                    0,
                ),
                (
                    "fn fail() -> Int { throw 42; count = 7; return 0; }",
                    "try { fail(); count = 8; } catch error { return count; } return -1;",
                    0,
                ),
                (
                    "fn fail() -> Int { throw 42; return 0; }",
                    "try { let result = 100 + fail(); count = 8; } catch error { return error; } return -1;",
                    42,
                ),
                (
                    "fn fail() -> Int { throw 42; return 0; }\nfn use(x: Int, y: Int) -> Int { count = 9; return x + y; }",
                    "try { use(1, fail()); count = 8; } catch error { return count; } return -1;",
                    0,
                ),
                (
                    "",
                    "try { try { throw 42; } catch inner { throw 43; count = 7; } count = 8; } catch outer { return outer; } return -1;",
                    43,
                ),
                (
                    "fn fail() -> Int { throw 42; return 0; }\nfn middle() -> Int { return fail() + 7; }",
                    "try { middle(); count = 8; } catch error { return error; } return -1;",
                    42,
                ),
                (
                    "",
                    "try { var n = 0; while n < 3 { n += 1; if n == 2 { throw 42; } count += 1; } count = 9; } catch error { return count; } return -1;",
                    1,
                ),
                (
                    "fn pure() -> Int { return 42; }",
                    "try { pure(); count = 3; } catch error { return -1; } return count;",
                    3,
                ),
                ("", "throw 42; count = 7; return 0;", 42),
            ] {
                let source =
                    format!("var count: Int = 0;\n{helpers}\nfn main() -> Int {{ {main} }}");
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
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
    fn try_without_catches_propagates_to_the_outer_handler() {
        let mut module = crate::in_lang_parse::parse_in_source("fn main() -> Int { try { try { throw 42; } catch inner { return 1; } return 2; } catch outer { return outer; } return 3; }").unwrap();
        for decl in &mut module.decls {
            if let crate::core_ir::Decl::Function { body, .. } = decl
                && let crate::core_ir::Stmt::Try { body, .. } = &mut body[0]
                && let crate::core_ir::Stmt::Try { catches, .. } = &mut body[0]
            {
                catches.clear();
            }
        }
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }

    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    #[test]
    fn new_invocations_clear_previous_error_flags() {
        let mut module = crate::in_lang_parse::parse_in_source("fn fail() -> Int { throw 42; return 0; }\nfn pure() -> Int { return 7; }\nfn main() -> Int { pure(); return 7; }").unwrap();
        for decl in &mut module.decls {
            if let crate::core_ir::Decl::Function { name, body, .. } = decl
                && name == "main"
            {
                body.insert(1, crate::core_ir::Stmt::Propagate);
            }
        }
        let rt = runtime_for_module(&module, crate::emit_profile::EmitProfile::Default);
        assert_eq!(unsafe { rt.invoke("fail", &[]) }, Some(42));
        assert_eq!(rt.invocation_error(), Some(42));
        assert_eq!(unsafe { rt.invoke("main", &[]) }, Some(7));
        assert_eq!(rt.invocation_error(), None);
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
    fn numeric_conversions_execute_without_constant_folding() {
        let narrow =
            crate::in_lang_parse::parse_in_source("fn main(value: Float) -> Int { return value; }")
                .unwrap();
        let narrow =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::C, &narrow);
        let roundtrip = crate::in_lang_parse::parse_in_source("fn widen(value: Int) -> Float { return value; }\nfn main(value: Int) -> Int { return widen(value); }").unwrap();
        let roundtrip =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::C, &roundtrip);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (value, expected) in [
                (42.9f64, 42i64),
                (-42.9, -42),
                (-0.0, 0),
                (i64::MIN as f64, i64::MIN),
                (9223372036854774784.0, 9223372036854774784),
            ] {
                assert_eq!(
                    invoke_module(&narrow, profile, &[value.to_bits() as i64]),
                    Some(expected),
                    "{profile:?}: {value}"
                );
            }
            for (value, expected) in [
                (42, 42),
                (-42, -42),
                (9007199254740993, 9007199254740992),
                (i64::MIN, i64::MIN),
            ] {
                assert_eq!(
                    invoke_module(&roundtrip, profile, &[value]),
                    Some(expected),
                    "{profile:?}: {value}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn numeric_casts_normalize_and_execute_once() {
        let source = "var calls: Int = 0;\nfn next() -> Float { calls += 1; return 41.9; }\nfn main() -> Int { return c-numeric-cast(next(), \"Int\") + calls; }";
        let module = crate::in_lang_parse::parse_in_source(source).unwrap();
        let module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::C, &module);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(
                invoke_module(&module, profile, &[]),
                Some(42),
                "{profile:?}"
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn integer_casts_execute_truncation_and_extension_once() {
        use crate::core_ir::{Decl, Expr, Stmt};
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for (target, input, expected) in [
                ("u8", 298, 42),
                ("i8", 170, -86),
                ("i8", -129, 127),
                ("u8", -1, 255),
                ("u16", 65578, 42),
                ("i16", 65535, -1),
                ("u16", -1, 65535),
                ("u32", 4294967338, 42),
                ("i32", 2147483648, -2147483648),
                ("u32", -1, 4294967295),
                ("i64", i64::MIN, i64::MIN),
                ("u64", i64::MIN, i64::MIN),
                ("isize", -1, -1),
                ("usize", -1, -1),
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(
                    "fn main(value: Int) -> Int { return value; }",
                )
                .unwrap();
                let Decl::Function { body, .. } = &mut module.decls[0] else {
                    panic!("function");
                };
                body[0] = Stmt::Return(Some(Expr::Unary {
                    op: format!("int-cast-{target}"),
                    expr: Box::new(Expr::Ident("value".into())),
                }));
                assert_eq!(
                    invoke_module(&module, profile, &[input]),
                    Some(expected),
                    "{profile:?}: {target}: {input}"
                );
            }
            let mut boolean = crate::in_lang_parse::parse_in_source(
                "fn main(value: Bool) -> Int { return value; }",
            )
            .unwrap();
            let Decl::Function { body, .. } = &mut boolean.decls[0] else {
                panic!("function");
            };
            body[0] = Stmt::Return(Some(Expr::Unary {
                op: "int-cast-u8".into(),
                expr: Box::new(Expr::Ident("value".into())),
            }));
            for value in [0, 1] {
                assert_eq!(
                    invoke_module(&boolean, profile, &[value]),
                    Some(value),
                    "{profile:?}: boolean operand"
                );
            }
            let mut module = crate::in_lang_parse::parse_in_source("var calls: Int = 0;\nfn next() -> Int { calls += 1; return 297; }\nfn main() -> Int { return next() + calls; }").unwrap();
            let Decl::Function { body, .. } = &mut module.decls[2] else {
                panic!("function");
            };
            let Stmt::Return(Some(Expr::Binary { lhs, .. })) = &mut body[0] else {
                panic!("return");
            };
            *lhs = Box::new(Expr::Unary {
                op: "int-cast-u8".into(),
                expr: lhs.clone(),
            });
            assert_eq!(
                invoke_module(&module, profile, &[]),
                Some(42),
                "{profile:?}: evaluation count"
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn floating_width_casts_execute_rounding_without_folding() {
        use crate::core_ir::{Decl, Expr, Stmt};
        for target in ["f32", "f64"] {
            let mut module = crate::in_lang_parse::parse_in_source(
                "fn main(value: Float) -> Float { return value; }",
            )
            .unwrap();
            let Decl::Function { body, .. } = &mut module.decls[0] else {
                panic!("function");
            };
            body[0] = Stmt::Return(Some(Expr::Unary {
                op: format!("float-cast-{target}"),
                expr: Box::new(Expr::Ident("value".into())),
            }));
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                for value in [
                    0.0f64,
                    -0.0,
                    0.1,
                    -0.1,
                    16777217.0,
                    f64::MAX,
                    f64::MIN_POSITIVE,
                    f64::from(f32::from_bits(1)),
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NAN,
                ] {
                    let expected = if target == "f32" {
                        f64::from(value as f32)
                    } else {
                        value
                    };
                    let actual = f64::from_bits(
                        invoke_module(&module, profile, &[value.to_bits() as i64]).unwrap() as u64,
                    );
                    if expected.is_nan() {
                        assert!(actual.is_nan());
                    } else {
                        assert_eq!(
                            actual.to_bits(),
                            expected.to_bits(),
                            "{profile:?}: {target}: {value}"
                        );
                    }
                }
            }
        }
        let mut module = crate::in_lang_parse::parse_in_source("var calls: Int = 0;\nfn next() -> Float { calls += 1; return 16777217.0; }\nfn main() -> Int { let value: Float = next(); if value == 16777216.0 && calls == 1 { return 42; } return 0; }").unwrap();
        let Decl::Function { body, .. } = &mut module.decls[2] else {
            panic!("function");
        };
        let Stmt::Let(_, _, value) = &mut body[0] else {
            panic!("let");
        };
        *value = Expr::Unary {
            op: "float-cast-f32".into(),
            expr: Box::new(value.clone()),
        };
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(
                invoke_module(&module, profile, &[]),
                Some(42),
                "{profile:?}: evaluation count"
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn signed_right_shifts_match_constant_folding_at_runtime() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for body in [
                "return value >> count;",
                "var result: Int = value; result >>= count; return result;",
            ] {
                let source = format!("fn main(value: Int, count: Int) -> Int {{ {body} }}");
                for value in [-84i64, -1, i64::MIN, i64::MAX, 84, 0] {
                    for count in [0, 1, 7, 31, 63] {
                        assert_eq!(
                            invoke_source(&source, profile, &[value, count]),
                            Some(value >> count),
                            "{profile:?}: {body}: {value}, {count}"
                        );
                    }
                }
            }
            let source = "var calls: Int = 0;\nfn count() -> Int { calls += 1; return 1; }\nfn main() -> Int { let value: Int = -82; return -(value >> count()) + calls; }";
            assert_eq!(
                invoke_source(source, profile, &[]),
                Some(42),
                "{profile:?}: count evaluation"
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn hardened_dispatch_preserves_mutable_local_scope() {
        for source in [
            "fn main(value: Int) -> Int { var result: Int = value; result >>= 1; return -result; }",
            "fn main(_pc: Int) -> Int { var result: Int = _pc; result >>= 1; return -result; }",
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            crate::core_opt::optimize_with_profile(
                &mut module.decls,
                Some("main"),
                crate::emit_profile::EmitProfile::Harden,
            );
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{source}: {verified:?}");
            assert_eq!(
                invoke_module(&module, crate::emit_profile::EmitProfile::Harden, &[-84]),
                Some(42),
                "{source}"
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn in_index_compound_places_execute_once_before_value_effects() {
        for source in [
            include_str!("../../conformance/control-flow/in-index-compound-effects.in"),
            include_str!("../../conformance/types/array-return-writable.in"),
            "var calls: Int = 0;\nfn make() -> [Int] { calls += 1; return [40]; }\nfn index() -> Int { calls += 1; return 0; }\nfn main() -> Int { return make()[index()] + calls; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(invoke_source(source, profile, &[]), Some(42), "{profile:?}");
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn direct_array_arguments_preserve_values_and_effect_order() {
        for source in [
            include_str!("../../conformance/types/array-direct-argument.in"),
            "fn empty(values: [Int]) -> Int { return 42; }\nfn main() -> Int { return empty([]); }",
            "fn first(values: [Float]) -> Int { if values[0] == 42.5 { return 42; } return 0; }\nfn main() -> Int { return first([42.5]); }",
            "fn first(values: [Bool]) -> Int { if values[0] { return 42; } return 0; }\nfn main() -> Int { return first([true]); }",
            "fn sum(values: [Int]) -> Int { return values[0] + values[1]; }\nfn main() -> Int { return sum([20, 22]); }",
            "fn make() -> [Int] { return [42]; }\nfn first(values: [Int]) -> Int { return values[0]; }\nfn main() -> Int { return first(make()); }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn local_array_results_survive_frame_reuse() {
        for source in [
            "fn id(values: [Int]) -> [Int] { return values; }\nfn make(value: Int) -> [Int] { var values: [Int] = [value]; return id(values); }\nfn main() -> Int { var first: [Int] = make(20); var second: [Int] = make(22); return first[0] + second[0]; }",
            include_str!("../../conformance/types/array-local-return.in"),
            "fn make() -> [Int] { var values: [Int] = [20, 21]; values[1] += 1; return values; }\nfn main() -> Int { var result: [Int] = make(); return result[0] + result[1]; }",
            "fn make(value: Int) -> [Int] { var values: [Int] = [value]; return values; }\nfn main() -> Int { var first: [Int] = make(20); var second: [Int] = make(22); first[0] += 1; return first[0] + second[0] - 1; }",
            "fn make() -> [Float] { var values: [Float] = [42.5]; return values; }\nfn main() -> Int { var values: [Float] = make(); if values[0] == 42.5 { return 42; } return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn direct_array_literal_index_preserves_element_and_index_effects() {
        for source in [
            include_str!("../../conformance/types/array-literal-index.in"),
            "var calls: Int = 0;\nfn item() -> Int { calls += 1; return 39; }\nfn index() -> Int { calls += 1; return 1; }\nfn main() -> Int { return [item(), 40][index()] + calls; }",
            "var trace: Int = 0;\nfn item(value: Int) -> Int { trace = trace * 10 + value; return value; }\nfn index() -> Int { trace = trace * 10 + 3; return 1; }\nfn main() -> Int { var value: Int = [item(1), item(2)][index()]; return trace + value - 83; }",
            "fn index() -> Int { return 1; }\nfn main() -> Int { if [40.5, 42.5][index()] == 42.5 { return 42; } return 0; }",
            "fn index() -> Int { return 1; }\nfn main() -> Int { if [false, true][index()] { return 42; } return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn direct_array_literal_index_checks_bounds() {
        for index in [-1, 2, i64::MAX] {
            let source = format!(
                "fn index() -> Int {{ return {index}; }}\nfn main() -> Int {{ return [20, 42][index()]; }}"
            );
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(&source, profile, &[]),
                    Some(1),
                    "{profile:?}: {index}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn invalid_array_indices_stop_the_current_function() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(
                invoke_source(
                    include_str!("../../conformance/control-flow/array-bounds-effects.in"),
                    profile,
                    &[]
                ),
                Some(42)
            );
            assert_eq!(
                invoke_source(
                    "fn invalid() -> Int { var values: [Int] = []; return values[0] + 7; }\nfn main() -> Int { return invalid() + 41; }",
                    profile,
                    &[]
                ),
                Some(42)
            );
        }
        for index in [-1, 2, i64::MAX] {
            for body in [
                format!(
                    "var values: [Int] = [20, 42]; var read: Int = values[{index}]; trace = 99; return read + 7;"
                ),
                format!("var read: Int = [20, 42][{index}]; trace = 99; return read + 7;"),
                format!("var read: Int = make()[{index}]; trace = 99; return read + 7;"),
                format!(
                    "var values: [Int] = [20, 42]; values[{index}] = 99; trace = 99; return 7;"
                ),
                format!("var values: [Int] = make(); values[{index}] = 99; trace = 99; return 7;"),
            ] {
                let source = format!(
                    "var trace: Int = 0;\nfn make() -> [Int] {{ return [20, 42]; }}\nfn invalid() -> Int {{ {body} }}\nfn main() -> Int {{ var result: Int = invalid(); if result == 1 && trace == 0 {{ return 42; }} return 0; }}"
                );
                for profile in [
                    crate::emit_profile::EmitProfile::Default,
                    crate::emit_profile::EmitProfile::Lean,
                    crate::emit_profile::EmitProfile::Harden,
                ] {
                    assert_eq!(
                        invoke_source(&source, profile, &[]),
                        Some(42),
                        "{profile:?}: {source}"
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn in_bitwise_compound_updates_preserve_types_and_read_order() {
        for source in [
            include_str!("../../conformance/control-flow/in-bit-compound-effects.in"),
            "fn main() -> Int { var value: Int = -1; value &= 42; return value; }",
            "var calls: Int = 0;\nfn rhs() -> Bool { calls += 1; return true; }\nfn main() -> Int { var value: Bool = false; value &= rhs(); if !value && calls == 1 { return 42; } return 0; }",
            "fn main() -> Int { var value: Int = 63; value &= 47; value ^= 5; value |= 0; return value; }",
            "fn main() -> Int { var value: Bool = true; value &= false; value |= true; value ^= false; if value { return 42; } return 0; }",
            "struct Bits { Int value }\nfn main() -> Int { var bits: Bits = Bits { value: 63 }; bits.value &= 47; bits.value ^= 5; bits.value |= 0; return bits.value; }",
            "var value: Int = 63;\nfn rhs() -> Int { value = 0; return 42; }\nfn main() -> Int { value &= rhs(); return value; }",
            "fn main() -> Int { var values: [Bool] = [true]; values[0] &= false; values[0] |= true; values[0] ^= false; if values[0] { return 42; } return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
        for (operator, before, rhs) in [("&=", 63, 42), ("|=", 40, 2), ("^=", 40, 2)] {
            let source = format!(
                "var trace: Int = 0;\nfn index() -> Int {{ trace = trace * 10 + 1; return 0; }}\nfn rhs(values: [Int]) -> Int {{ trace = trace * 10 + 2; values[0] = 0; return {rhs}; }}\nfn main() -> Int {{ var values: [Int] = [{before}]; values[index()] {operator} rhs(values); if trace == 12 {{ return values[0]; }} return 0; }}"
            );
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(&source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn in_assignment_scans_preserve_unicode_and_quoted_calls() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(
                invoke_source(
                    include_str!("../../conformance/control-flow/in-assignment-scan.in"),
                    profile,
                    &[]
                ),
                Some(42),
                "{profile:?}"
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn in_shifts_and_comparisons_preserve_precedence() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(
                invoke_source(
                    include_str!("../../conformance/control-flow/in-shift-precedence.in"),
                    profile,
                    &[]
                ),
                Some(42)
            );
        }
        for condition in [
            "8 == 16 >> 1",
            "2 < 1 << 2",
            "true == 1 < 2",
            "false == 2 > 3",
            "16 >> 1 >> 1 == 4",
            "1 << 1 + 1 == 4",
            "4 == 1 << 1 + 1",
            "(1 == 2) == false",
            "(1 < 2) == true",
        ] {
            let source =
                format!("fn main() -> Int {{ if {condition} {{ return 42; }} return 0; }}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(&source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn in_scientific_literals_preserve_arithmetic_and_operand_effects() {
        for source in [
            include_str!("../../conformance/types/in-scientific-literals.in"),
            "fn main() -> Int { var name-1e: Int = 39; return name-1e+3; }",
            "fn main() -> Int { var value: Float = 2.0-1e+3; if value == -998.0 { return 42; } return 0; }",
            "fn main() -> Int { var value: Float = 1e+3 * 2.0; if value == 2000.0 { return 42; } return 0; }",
            "fn main() -> Int { var value: Float = -1e+3 * 2.0; if value == -2000.0 { return 42; } return 0; }",
            "fn main() -> Int { var value: Float = 1.25E-2 * 2.0; if value == 0.025 { return 42; } return 0; }",
            "var calls: Int = 0;\nfn value() -> Float { calls += 1; return 2.0; }\nfn main() -> Int { if 1e+3 * value() == 2000.0 && calls == 1 { return 42; } return 0; }",
            "fn main() -> Int { var e: Int = 39; return e+3; }",
            "fn main() -> Int { var caféE: Int = 39; return caféE+3; }",
            "fn main() -> Int { return 0x1e+12; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn inferred_in_arithmetic_and_bitwise_bindings_keep_runtime_types() {
        for source in [
            "fn main() -> Int { var value = 1e+3 * 2.0; if value == 2000.0 { return 42; } return 0; }",
            "fn main() -> Int { var value = -(1e+3 * 2.0); if value == -2000.0 { return 42; } return 0; }",
            "fn main() -> Int { var flag = true & false; var value = flag | true; if value { return 42; } return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn field_reads_materialize_effectful_struct_values_once() {
        for source in [
            "struct Point { Int x; Int y }\nfn main() -> Int { var point: Point = Point { x: 20, y: 22 }; var copied: Point = point; return copied.x + copied.y; }",
            "struct Point { Int x; Int y }\nvar calls: Int = 0;\nfn make() -> Point { calls += 1; return Point { x: 41, y: 10 }; }\nfn main() -> Int { return make().x + calls; }",
            "struct Point { Int x; Int y }\nfn sum(point: Point) -> Int { return point.x + point.y; }\nfn main() -> Int { var point: Point = Point { x: 20, y: 22 }; return sum(point); }",
            "struct Point { Int x; Int y }\nfn make(n: Int) -> Point { var point: Point = Point { x: n, y: 21 }; return point; }\nfn main() -> Int { var first: Point = make(20); var second: Point = make(21); first.y = 22; return first.x + first.y; }",
            "struct Point { Int x; Int y }\nfn make(n: Int) -> Point { return Point { x: n, y: 21 }; }\nfn main() -> Int { var first: Point = Point { x: 0, y: 0 }; first = make(20); var copied: Point = first; copied.y = 0; return first.x + copied.x + first.y + copied.y - 19; }",
            "struct Point { Int x; Int y }\nfn make() -> Point { throw 9; return Point { x: 0, y: 0 }; }\nfn main() -> Int { try { return make().y; } catch 9 { return 42; } return 0; }",
            "struct Point { Int x; Int y }\nvar calls: Int = 0;\nfn fail() -> Int { calls += 1; throw 9; return 0; }\nfn next() -> Int { calls += 10; return 0; }\nfn main() -> Int { try { return Point { x: fail(), y: next() }.y; } catch 9 { if calls == 1 { return 42; } } return 0; }",
            include_str!("../../conformance/control-flow/struct-field-call-effects.in"),
            "struct Point { Bool flag; Int value }\nvar trace: Int = 0;\nfn guard() -> Bool { trace = trace * 10 + 1; return true; }\nfn make() -> Point { trace = trace * 10 + 2; return Point { flag: true, value: 41 }; }\nfn main() -> Int { if guard() && make().flag { return trace + 30; } return 0; }",
            "struct Point { Bool flag; Int value }\nvar trace: Int = 0;\nfn guard() -> Bool { trace += 1; return false; }\nfn make() -> Point { trace += 10; return Point { flag: true, value: 41 }; }\nfn main() -> Int { if guard() && make().flag { return 0; } if trace == 1 { return 42; } return 0; }",
            "struct Point { Int x; Int y }\nvar calls: Int = 0;\nfn make() -> Point { calls += 1; return Point { x: 10, y: 41 }; }\nfn main() -> Int { return make().y + calls; }",
            "struct Point { Int x; Int y }\nvar trace: Int = 0;\nfn item(n: Int) -> Int { trace = trace * 10 + n; return n; }\nfn main() -> Int { var value: Int = Point { x: item(1), y: item(2) }.y; return value + trace + 28; }",
            "struct Point { Bool flag; Float value }\nvar calls: Int = 0;\nfn make() -> Point { calls += 1; return Point { flag: true, value: 42.5 }; }\nfn main() -> Int { if make().value == 42.5 && calls == 1 { return 42; } return 0; }",
            "struct Point { Bool flag; Int value }\nvar calls: Int = 0;\nfn make() -> Point { calls += 1; return Point { flag: true, value: 41 }; }\nfn main() -> Int { if false && make().flag { return 0; } if calls == 0 { return 42; } return 0; }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_source(source, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, target_arch = "x86_64"))]
    fn struct_parameter_sentinels_return_failure_without_reading_memory() {
        let module = crate::in_lang_parse::parse_in_source("struct Point { Int x; Int y }\nfn main(point: Point) -> Int { return point.x + point.y; }").unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            for pointer in [0, 1] {
                assert_eq!(invoke_module(&module, profile, &[pointer]), Some(1));
            }
        }
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
                        "push rbx", "push r15", "mov r15, r10", "mov rbx, 0x12345678", "call r11",
                        "mov rdx, rbx", "pop r15", "pop rbx",
                        in("r11") entry, in("r10") rt.error_page.as_ref().unwrap().ptr,
                        in("rdi") 9_i64, in("rsi") 4_i64,
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
