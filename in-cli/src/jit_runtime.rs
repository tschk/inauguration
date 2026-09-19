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
    /// Set by invoke() into X27 before calling JIT code. Throw/try use this.
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
fn make_executable(ptr: *mut u8, size: usize) {
    // ponytail: pthread_jit_write_protect_np regressed JIT on arm64 macOS CI;
    // mprotect on MAP_JIT pages still passes conformance here.
    unsafe {
        libc::mprotect(
            ptr as *mut std::ffi::c_void,
            size,
            libc::PROT_READ | libc::PROT_EXEC,
        );
    }
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
fn make_executable(_ptr: *mut u8, _size: usize) {
    // Already RWX from VirtualAlloc. ARM64 Windows needs FlushInstructionCache
    // but that requires win32_ffi we can add when tested.
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
    fn finalize(&self) {
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
        make_executable(self.ptr, self.size);
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

        // On Apple ARM64, flush the instruction cache after writing code
        self.code_pages.push(page);

        // Make code pages executable
        let last_page = self.code_pages.last().unwrap();
        last_page.finalize();

        let base = self.code_pages.last().unwrap().ptr;
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

    /// Call a compiled function by name.
    ///
    /// # Safety
    /// The function must have been loaded via `load()`. The caller is
    /// responsible for ensuring the function signature matches the arguments.
    pub unsafe fn invoke(&self, name: &str, _args: &[i64]) -> Option<i64> {
        let funcs = self.functions.read().unwrap();
        let func = funcs.get(name)?;
        let entry = func.entry as *const ();

        #[cfg(target_arch = "aarch64")]
        {
            // Set X27 to error page for throw/try/catch, then call through blr.
            // JIT functions no longer emit adr x27 in prologue.
            let ep = self.error_page as usize;
            let result = match _args.len() {
                0 => {
                    let r: i64;
                    unsafe {
                        std::arch::asm!(
                            "mov x27, {e}",
                            "blr {f}",
                            e = in(reg) ep,
                            f = in(reg) entry,
                            lateout("x0") r,
                            clobber_abi("C"),
                        );
                    }
                    r
                }
                1 => {
                    let a0 = _args[0];
                    let r: i64;
                    unsafe {
                        std::arch::asm!(
                            "mov x27, {e}",
                            "blr {f}",
                            e = in(reg) ep,
                            f = in(reg) entry,
                            in("x0") a0,
                            lateout("x0") r,
                            clobber_abi("C"),
                        );
                    }
                    r
                }
                2 => {
                    let a0 = _args[0];
                    let a1 = _args[1];
                    let r: i64;
                    unsafe {
                        std::arch::asm!(
                            "mov x27, {e}",
                            "blr {f}",
                            e = in(reg) ep,
                            f = in(reg) entry,
                            in("x0") a0,
                            in("x1") a1,
                            lateout("x0") r,
                            clobber_abi("C"),
                        );
                    }
                    r
                }
                3 => {
                    let a0 = _args[0];
                    let a1 = _args[1];
                    let a2 = _args[2];
                    let r: i64;
                    unsafe {
                        std::arch::asm!(
                            "mov x27, {e}",
                            "blr {f}",
                            e = in(reg) ep,
                            f = in(reg) entry,
                            in("x0") a0,
                            in("x1") a1,
                            in("x2") a2,
                            lateout("x0") r,
                            clobber_abi("C"),
                        );
                    }
                    r
                }
                _ => 0,
            };
            Some(result)
        }

        #[cfg(target_arch = "x86_64")]
        {
            // System V AMD64 ABI: args in rdi, rsi, rdx, rcx, r8, r9; return in rax.
            // x86_64 JIT uses stack-based error handling — no register setup needed.
            type JitFn = unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64;
            let f: JitFn = unsafe { std::mem::transmute(entry) };
            let result = match _args.len() {
                0 => unsafe { f(0, 0, 0, 0, 0, 0) },
                1 => unsafe { f(_args[0], 0, 0, 0, 0, 0) },
                2 => unsafe { f(_args[0], _args[1], 0, 0, 0, 0) },
                3 => unsafe { f(_args[0], _args[1], _args[2], 0, 0, 0) },
                _ => 0,
            };
            Some(result)
        }
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

    #[test]
    fn jit_runtime_new_is_empty() {
        let rt = JitRuntime::new();
        assert_eq!(rt.function_count(), 0);
    }
}
