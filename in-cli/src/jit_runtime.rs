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

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe extern "sysv64" fn clone_array(
    error_page: *mut u8,
    source: *const u64,
    len: usize,
) -> *mut u64 {
    unsafe { clone_array_words(error_page, source, len) }
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe extern "C" fn clone_array(
    error_page: *mut u8,
    source: *const u64,
    len: usize,
) -> *mut u64 {
    unsafe { clone_array_words(error_page, source, len) }
}

unsafe fn clone_array_words(error_page: *mut u8, source: *const u64, len: usize) -> *mut u64 {
    let destination = unsafe { allocate_array_words(error_page, len) };
    if len != 0 {
        unsafe { std::ptr::copy_nonoverlapping(source, destination.add(2), len) };
    }
    destination
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe extern "sysv64" fn push_array(
    error_page: *mut u8,
    source: *mut u64,
    value: u64,
) -> *mut u64 {
    unsafe { push_array_words(error_page, source, value) }
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe extern "C" fn push_array(
    error_page: *mut u8,
    source: *mut u64,
    value: u64,
) -> *mut u64 {
    unsafe { push_array_words(error_page, source, value) }
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe extern "sysv64" fn extend_array(
    error_page: *mut u8,
    destination: *mut u64,
    source: *mut u64,
) -> *mut u64 {
    unsafe { extend_array_words(error_page, destination, source) }
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe extern "C" fn extend_array(
    error_page: *mut u8,
    destination: *mut u64,
    source: *mut u64,
) -> *mut u64 {
    unsafe { extend_array_words(error_page, destination, source) }
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe extern "sysv64" fn reserve_array(error_page: *mut u8, capacity: u64) -> *mut u64 {
    unsafe { reserve_array_words(error_page, capacity) }
}
#[cfg(not(target_arch = "x86_64"))]
pub(crate) unsafe extern "C" fn reserve_array(error_page: *mut u8, capacity: u64) -> *mut u64 {
    unsafe { reserve_array_words(error_page, capacity) }
}
unsafe fn reserve_array_words(error_page: *mut u8, capacity: u64) -> *mut u64 {
    if capacity >= 1u64 << 59 {
        return std::ptr::null_mut();
    }
    let Some(words) = usize::try_from(capacity)
        .ok()
        .and_then(|cap| cap.checked_add(2))
        .filter(|words| {
            words
                .checked_mul(8)
                .is_some_and(|bytes| bytes <= isize::MAX as usize)
        })
    else {
        return std::ptr::null_mut();
    };
    let mut payload = Vec::new();
    if payload.try_reserve_exact(words).is_err() {
        return std::ptr::null_mut();
    }
    payload.resize(words, 0u64);
    payload[1] = capacity;
    let mut payload = payload.into_boxed_slice();
    let pointer = payload.as_mut_ptr();
    let arena =
        unsafe { error_page.add(24).cast::<usize>().read() as *const Mutex<Vec<Box<[u64]>>> };
    unsafe { &*arena }.lock().unwrap().push(payload);
    pointer
}

unsafe fn extend_array_words(
    error_page: *mut u8,
    mut destination: *mut u64,
    source: *mut u64,
) -> *mut u64 {
    let arena =
        unsafe { error_page.add(24).cast::<usize>().read() as *const Mutex<Vec<Box<[u64]>>> };
    let len = {
        let arrays = unsafe { &*arena }.lock().unwrap();
        let Some(words) = arrays.iter().find(|words| words.as_ptr() == source) else {
            return std::ptr::null_mut();
        };
        let Ok(len) = usize::try_from(words[0]) else {
            return std::ptr::null_mut();
        };
        if len > words.len() - 2
            || !arrays
                .iter()
                .any(|words| words.as_ptr() == destination && words[0] <= (words.len() - 2) as u64)
        {
            return std::ptr::null_mut();
        }
        len
    };
    for index in 0..len {
        let value = unsafe { source.add(index + 2).read() };
        destination = unsafe { push_array_words(error_page, destination, value) };
        if destination.is_null() {
            return destination;
        }
    }
    destination
}

unsafe fn push_array_words(error_page: *mut u8, source: *mut u64, value: u64) -> *mut u64 {
    let arena =
        unsafe { error_page.add(24).cast::<usize>().read() as *const Mutex<Vec<Box<[u64]>>> };
    let mut arrays = unsafe { &*arena }.lock().unwrap();
    let Some(words) = arrays.iter_mut().find(|words| words.as_ptr() == source) else {
        return std::ptr::null_mut();
    };
    let Ok(len) = usize::try_from(words[0]) else {
        return std::ptr::null_mut();
    };
    let capacity = words.len() - 2;
    if len > capacity {
        return std::ptr::null_mut();
    }
    if len < capacity {
        words[len + 2] = value;
        words[0] += 1;
        return words.as_mut_ptr();
    }
    let Some(capacity) = capacity.max(2).checked_mul(2).filter(|cap| {
        cap.checked_add(2)
            .and_then(|words| words.checked_mul(8))
            .is_some_and(|bytes| bytes <= isize::MAX as usize)
    }) else {
        return std::ptr::null_mut();
    };
    let mut grown = vec![0_u64; capacity + 2].into_boxed_slice();
    grown[0] = (len + 1) as u64;
    grown[1] = capacity as u64;
    grown[2..len + 2].copy_from_slice(&words[2..len + 2]);
    grown[len + 2] = value;
    let pointer = grown.as_mut_ptr();
    arrays.push(grown);
    pointer
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
    #[cfg(unix)]
    fn owned_array_push_grows_and_checks_allocations() {
        let runtime = JitRuntime::new();
        let context = runtime.error_page.as_ref().unwrap().ptr;
        unsafe {
            let mut array = allocate_array(context, 0);
            for value in 0..10 {
                array = push_array(context, array, value);
                assert!(!array.is_null());
                assert_eq!(*array, value + 1);
            }
            assert_eq!(
                std::slice::from_raw_parts(array.add(2), 10),
                &(0..10).collect::<Vec<_>>()
            );
            assert!(push_array(context, std::ptr::null_mut(), 42).is_null());
            let capacity = *array.add(1);
            *array = capacity + 1;
            assert!(push_array(context, array, 42).is_null());
        }
    }

    #[test]
    #[cfg(unix)]
    fn owned_array_reservation_keeps_length_zero_and_checks_capacity() {
        let runtime = JitRuntime::new();
        let context = runtime.error_page.as_ref().unwrap().ptr;
        unsafe {
            let array = reserve_array(context, 10);
            assert!(!array.is_null());
            assert_eq!(*array, 0);
            assert_eq!(*array.add(1), 10);
            assert_eq!(push_array(context, array, 42), array);
            assert_eq!(*array, 1);
            assert_eq!(*array.add(2), 42);
            let empty = reserve_array(context, 0);
            assert!(!empty.is_null());
            assert_eq!(*empty, 0);
            assert!(reserve_array(context, u64::MAX).is_null());
            assert!(reserve_array(context, 1u64 << 59).is_null());
        }
    }

    #[test]
    #[cfg(unix)]
    fn owned_array_extend_checks_sources_and_snapshots_alias_length() {
        let runtime = JitRuntime::new();
        let context = runtime.error_page.as_ref().unwrap().ptr;
        unsafe {
            let array = allocate_array(context, 2);
            *array.add(2) = 20;
            *array.add(3) = 22;
            let grown = extend_array(context, array, array);
            assert!(!grown.is_null());
            assert_eq!(*grown, 4);
            assert_eq!(
                std::slice::from_raw_parts(grown.add(2), 4),
                &[20, 22, 20, 22]
            );
            assert!(extend_array(context, grown, std::ptr::null_mut()).is_null());
            assert!(extend_array(context, std::ptr::null_mut(), grown).is_null());
            *array = 99;
            assert!(extend_array(context, grown, array).is_null());
        }
    }

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
                        "let values=[value]; match values {{ [{pattern:?}] {{ return 42; }} _ {{ return 7; }} }}"
                    ),
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
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn global_struct_fields_preserve_reads_writes_and_local_shadowing() {
        for source in [
            include_str!("../../conformance/types/global-struct-scalars.in"),
            "struct Counter { Int value }\nvar counter: Counter = 0;\nfn bump() -> Int { counter.value += 2; return counter.value; }\nfn main() -> Int { counter.value = 40; return bump(); }",
            "struct Counter { Int value }\nvar counter: Counter = 0;\nfn read() -> Int { return counter.value; }\nfn main() -> Int { counter.value = 20; var counter: Counter = Counter { value: 22 }; return counter.value + read(); }",
            "struct State { Bool flag; Float value }\nvar state: State = 0;\nfn main() -> Int { state.flag = true; state.value = 42.5; if state.flag && state.value == 42.5 { return 42; } return 0; }",
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
    fn global_struct_values_copy_return_and_pass_complete_fields() {
        for source in [
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn main() -> Int { var point: Point = Point { x: 20, y: 22 }; var copied: Point = point; return copied.x + copied.y; }",
            include_str!("../../conformance/types/global-struct-values.in"),
            "struct State { Bool flag; Float value }\nvar state: State = 0;\nfn copy() -> State { return state; }\nfn main() -> Int { state.flag = true; state.value = 42.5; var saved: State = copy(); state.flag = false; state.value = 0.0; if saved.flag && saved.value == 42.5 { return 42; } return 0; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn main() -> Int { point.x = 20; point.y = 22; var copied: Point = point; point.x = 0; return copied.x + copied.y; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn make() -> Point { return point; }\nfn main() -> Int { point.x = 20; point.y = 22; var copied: Point = make(); point.x = 0; return copied.x + copied.y; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn sum(value: Point) -> Int { return value.x + value.y; }\nfn main() -> Int { point.x = 20; point.y = 22; return sum(point); }",
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
    fn global_struct_assignments_copy_values_after_evaluating_sources() {
        for source in [
            "struct Empty {}\nvar empty: Empty = 0;\nvar calls: Int = 0;\nfn make() -> Empty { calls += 1; return Empty {}; }\nfn main() -> Int { empty = Empty {}; empty = empty; empty = make(); return calls + 41; }",
            include_str!("../../conformance/types/global-struct-assignment.in"),
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn main() -> Int { point.x = 7; point.y = 8; point = 0; return point.x + point.y + 42; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn fail() -> Int { throw 9; }\nfn main() -> Int { point.x = 20; point.y = 22; try { point = Point { x: 0, y: fail() }; } catch 9 { return point.x + point.y; } return 0; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn main() -> Int { point = Point { x: 20, y: 22 }; return point.x + point.y; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn main() -> Int { var local: Point = Point { x: 20, y: 22 }; point = local; local.x = 0; return point.x + point.y; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nvar other: Point = 0;\nfn main() -> Int { other.x = 20; other.y = 22; point = other; other.x = 0; point = point; return point.x + point.y; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn make() -> Point { return Point { x: 20, y: 22 }; }\nfn main() -> Int { point = make(); return point.x + point.y; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nvar calls: Int = 0;\nfn first() -> Int { calls += 1; return 20; }\nfn second() -> Int { calls += 1; return point.x + 21; }\nfn main() -> Int { point.x = 1; point.y = 2; point = Point { x: first(), y: second() }; return point.x + point.y + calls - 2; }",
            "struct State { Bool flag; Float value }\nvar state: State = 0;\nfn main() -> Int { state = State { flag: true, value: 42.5 }; if state.flag && state.value == 42.5 { return 42; } return 0; }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn read() -> Int { return point.x; }\nfn main() -> Int { point.x = 20; var point: Point = Point { x: 0, y: 0 }; point = Point { x: 20, y: 2 }; return point.x + point.y + read(); }",
            "var value: Int = 20;\nfn read() -> Int { return value; }\nfn set(value: Int) -> Int { value = 22; return value; }\nfn main() -> Int { return set(0) + read(); }",
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
    fn global_struct_primitive_field_aliases_preserve_complete_values() {
        let mut module = crate::in_lang_parse::parse_in_source("struct Point { Int x; Int y }\nvar point: Point = 0;\nfn main() -> Int { point = Point { x: 20, y: 22 }; var copied: Point = point; return copied.x + copied.y; }").unwrap();
        for decl in &mut module.decls {
            if let crate::core_ir::Decl::Struct { fields, .. } = decl {
                for (_, typ) in fields {
                    *typ = crate::core_ir::Typ::Named("Int".into());
                }
            }
        }
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
    fn primitive_global_alias_assignment_uses_scalar_storage() {
        let mut module = crate::in_lang_parse::parse_in_source(
            "var value: Int = 0;\nfn main() -> Int { value = 42; return value; }",
        )
        .unwrap();
        for decl in &mut module.decls {
            if let crate::core_ir::Decl::Global { typ, .. } = decl {
                *typ = crate::core_ir::Typ::Named("Int".into());
            }
        }
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
    fn call_arguments_capture_struct_values_and_evaluate_left_to_right() {
        let sources = [
            include_str!("../../conformance/control-flow/struct-argument-capture.in"),
            include_str!("../../conformance/control-flow/stack-argument-order.in"),
            "struct State { Bool flag; Float value }\nvar state: State = 0;\nfn mutate() -> Int { state.flag = false; state.value = 0.0; return 0; }\nfn check(value: State, after: Int) -> Int { if value.flag && value.value == 42.5 { return 42 + after; } return 0; }\nfn main() -> Int { state.flag = true; state.value = 42.5; return check(state, mutate()); }",
            "struct Point { Int x; Int y }\nvar point: Point = 0;\nfn mutate() -> Int { point.x = 0; return 0; }\nfn sum(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int, value: Point, after: Int) -> Int { return a + b + c + d + e + f + value.x + value.y + after; }\nfn main() -> Int { point.x = 20; point.y = 22; return sum(0, 0, 0, 0, 0, 0, point, mutate()); }",
            "var trace: Int = 0;\nfn fail() -> Int { trace = 1; throw 9; }\nfn later() -> Int { trace = trace * 10 + 2; return 0; }\nfn take(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int, g: Int, h: Int) -> Int { trace = 999; return 0; }\nfn main() -> Int { try { take(fail(), 0, 0, 0, 0, 0, 0, later()); } catch 9 { if trace == 1 { return 42; } return trace; } return 0; }",
            include_str!("../../conformance/control-flow/empty-struct-argument-effects.in"),
            "struct Empty {}\nvar calls: Int = 0;\nfn make() -> Empty { calls += 1; return Empty {}; }\nfn take(value: Empty, after: Int) -> Int { return calls + after; }\nfn main() -> Int { var local: Empty = make(); return take(local, 41); }",
        ];
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let actual = sources.map(|source| invoke_source(source, profile, &[]));
            assert!(
                actual.iter().all(|value| *value == Some(42)),
                "{profile:?}: {actual:?}"
            );
        }
    }

    #[test]
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn nested_struct_argument_producers_preserve_fields_effects_and_throws() {
        for source in [
            "struct Empty {}\nstruct Wrapper { Empty inner }\nvar calls: Int = 0;\nfn make() -> Empty { calls += 1; return Empty {}; }\nfn take(value: Wrapper, after: Int) -> Int { return calls + after; }\nfn main() -> Int { var inner: Empty = make(); return take(Wrapper { inner: inner }, 41); }",
            "struct Empty {}\nstruct Wrapper { Empty inner }\nvar calls: Int = 0;\nfn make() -> Empty { calls += 1; return Empty {}; }\nfn take(value: Wrapper, after: Int) -> Int { return calls + after; }\nfn main() -> Int { var local: Wrapper = Wrapper { inner: make() }; return take(local, 41); }",
            "struct Inner { Int value }\nstruct Wrapper { Inner inner; Int tail }\nvar calls: Int = 0;\nfn make() -> Inner { calls += 1; return Inner { value: 20 }; }\nfn take(value: Wrapper) -> Int { return value.inner.value + value.tail; }\nfn main() -> Int { return take(Wrapper { inner: make(), tail: 22 }) + calls - 1; }",
            "struct Empty {}\nstruct Wrapper { Empty inner }\nvar calls: Int = 0;\nfn make() -> Empty { calls += 1; return Empty {}; }\nfn take(value: Wrapper, after: Int) -> Int { return calls + after; }\nfn main() -> Int { return take(Wrapper { inner: make() }, 41); }",
            "struct Empty {}\nstruct Wrapper { Empty inner }\nvar calls: Int = 0;\nfn fail() -> Empty { calls += 1; throw 9; }\nfn next() -> Int { calls += 10; return 0; }\nfn take(value: Wrapper, after: Int) -> Int { return calls + after; }\nfn main() -> Int { try { take(Wrapper { inner: fail() }, next()); } catch 9 { return calls + 41; } return 0; }",
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
    fn class_method_receivers_preserve_layout_and_argument_positions() {
        for source in [
            "class Pair { x: Int; y: Int; fn answer() -> Int { var copied: Pair = self; return copied.x + copied.y; } }\nfn main() -> Int { var pair: Pair = Pair { x: 20, y: 22 }; return pair.answer(); }",
            "class State { flag: Bool; value: Float; fn answer(expected: Float) -> Int { if flag && value == expected { return 42; } return 0; } }\nfn main() -> Int { var state: State = State { flag: true, value: 42.5 }; return state.answer(42.5); }",
            "class Pair { x: Int; y: Int; fn answer(value: Int) -> Int { return x + y + value; } }\nfn main() -> Int { var pair: Pair = Pair { x: 20, y: 15 }; return pair.answer(7); }",
            "class Empty { fn answer(a: Int, b: Int, c: Int, d: Int, e: Int, f: Int, g: Int, h: Int) -> Int { return a + b + c + d + e + f + g + h; } }\nfn main() -> Int { var empty: Empty = Empty {}; return empty.answer(1, 2, 3, 4, 5, 6, 7, 14); }",
            "class Empty { fn answer(value: Int) -> Int { return value; } fn forward(value: Int) -> Int { return self.answer(value); } }\nfn main() -> Int { var empty: Empty = Empty {}; return empty.forward(42); }",
            "class Empty { fn answer(value: Int) -> Int { return value; } }\nvar empty: Empty = 0;\nfn main() -> Int { return empty.answer(42); }",
            "class Pair { x: Int; y: Int; fn answer(value: Int) -> Int { return x + y + value; } }\nvar pair: Pair = 0;\nfn main() -> Int { pair.x = 20; pair.y = 15; return pair.answer(7); }",
            "class Empty { fn answer() -> Int { return 42; } }\nfn main() -> Int { var empty: Empty = Empty {}; return empty.answer(); }",
            "class Empty { fn answer(value: Int) -> Int { return value; } }\nfn main() -> Int { var empty: Empty = Empty {}; return empty.answer(42); }",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                crate::lower_core::desugar_module(&mut module);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn nested_class_receiver_fields_preserve_method_addresses() {
        for source in [
            "class Inner { x: Int; y: Int; fn get() -> Int { return x + y; } }\nclass Wrapper { left: Inner; right: Inner; fn answer() -> Int { return right.get(); } }\nfn take(value: Wrapper) -> Int { return value.answer(); }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { left: Inner { x: 0, y: 0 }, right: Inner { x: 20, y: 22 } }; return take(wrapper); }",
            "class Inner { value: Int; fn get(after: Int) -> Int { return value + after; } }\nclass Wrapper { inner: Inner; fn answer() -> Int { return inner.get(22); } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { value: 20 } }; return wrapper.answer(); }",
            "class Inner { x: Int; y: Int; fn get() -> Int { return x + y; } }\nclass Wrapper { inner: Inner; tail: Int; fn answer() -> Int { return inner.get(); } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { x: 20, y: 22 }, tail: 0 }; return wrapper.answer(); }",
            "class Empty {}\nclass Wrapper { inner: Empty; value: Int; fn answer() -> Int { return value; } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Empty {}, value: 42 }; return wrapper.answer(); }",
            "class Empty {}\nclass Wrapper { inner: Empty; value: Int; fn answer() -> Int { return value; } }\nfn take(value: Wrapper) -> Int { return value.answer(); }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Empty {}, value: 42 }; return take(wrapper); }",
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn recursive_receiver_layout_compilation_is_bounded() {
        let module = crate::in_lang_parse::parse_in_source("struct Node { Node next }\nfn unused(self: Node) -> Int { return 0; }\nfn main() -> Int { return 42; }").unwrap();
        assert_eq!(
            invoke_module(&module, crate::emit_profile::EmitProfile::Default, &[]),
            Some(42)
        );
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn class_method_mutation_updates_the_callers_object() {
        let sources = [
            "class Counter { value: Int; fn add(delta: Int) -> Int { value += delta; return value; } }\nfn main() -> Int { var counter: Counter = Counter { value: 0 }; counter.add(42); return counter.value; }",
            "class Counter { value: Int; fn add(delta: Int) -> Int { value += delta; return value; } fn run(delta: Int) -> Int { self.add(delta); return value; } }\nfn main() -> Int { var counter: Counter = Counter { value: 0 }; var result: Int = counter.run(21); return result + counter.value; }",
            "class Counter { value: Int; fn add(delta: Int) -> Int { value += delta; return value; } }\nvar counter: Counter = 0;\nfn main() -> Int { counter.add(42); return counter.value; }",
            "class Point { x: Int; y: Int; fn set(value: Int) -> Int { x = value; return x; } fn snapshot(value: Int) -> Point { self.set(value); return self; } }\nfn main() -> Int { var point: Point = Point { x: 0, y: 22 }; var copied: Point = point.snapshot(20); point.set(0); return copied.x + copied.y; }",
            "class Point { x: Int; y: Int; fn replace() -> Int { self = Point { x: 20, y: 22 }; return x + y; } }\nfn main() -> Int { var point: Point = Point { x: 0, y: 0 }; point.replace(); return point.x + point.y; }",
            "fn fail() -> Int { throw 9; }\nclass Point { x: Int; y: Int; fn replace() -> Int { self = Point { x: 0, y: fail() }; return 0; } }\nfn main() -> Int { var point: Point = Point { x: 20, y: 22 }; try { point.replace(); } catch 9 { return point.x + point.y; } return 0; }",
            "class Counter { value: Int; fn fail() -> Int { value = 42; throw 9; } }\nfn main() -> Int { var counter: Counter = Counter { value: 0 }; try { counter.fail(); } catch 9 { return counter.value; } return 0; }",
            "class State { flag: Bool; value: Float; fn set() -> Int { flag = true; value = 42.5; return 0; } }\nfn main() -> Int { var state: State = State { flag: false, value: 0.0 }; state.set(); if state.flag && state.value == 42.5 { return 42; } return 0; }",
            "class Counter { value: Int; fn add(delta: Int) -> Int { value += delta; return value; } }\nfn mutate(value: Counter) -> Int { value.add(42); return value.value; }\nfn main() -> Int { var counter: Counter = Counter { value: 0 }; return mutate(counter) + counter.value; }",
            "fn sum_point(value: Point) -> Int { return value.x + value.y; }\nclass Point { x: Int; y: Int; fn set(value: Int) -> Int { x = value; return x; } fn answer() -> Int { self.set(20); return sum_point(self); } }\nfn main() -> Int { var point: Point = Point { x: 0, y: 22 }; return point.answer(); }",
        ];
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let actual = sources.map(|source| {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                crate::lower_core::desugar_module(&mut module);
                invoke_module(&module, profile, &[])
            });
            assert!(
                actual.iter().all(|value| *value == Some(42)),
                "{profile:?}: {actual:?}"
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn receiver_replacement_constructor_fields_evaluate_in_source_order() {
        let source = "var trace: Int = 0;\nfn first() -> Int { trace = trace * 10 + 1; return 20; }\nfn second() -> Int { trace = trace * 10 + 2; return 22; }\nclass Point { x: Int; y: Int; fn replace() -> Int { self = Point { x: first(), y: second() }; return trace; } }\nfn main() -> Int { var point: Point = Point { x: 0, y: 0 }; var order: Int = point.replace(); if order == 12 && point.x + point.y == 42 { return 42; } return order; }";
        for _ in 0..8 {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                crate::lower_core::desugar_module(&mut module);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn raw_vector_identifier_store_spills_call_operands() {
        use crate::core_ir::{Decl, Expr, Stmt};
        let parsed = crate::compiler::rust_front::parse_rust_source("fn mutate(mut items: Vec<i64>) -> i64 { items[0] = 20; return 1; } fn main() -> i64 { let mut items: Vec<i64> = vec![0, 0]; return items[0] + items[1]; }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        for decl in &mut module.decls {
            if let Decl::Function { name, body, .. } = decl
                && name == "main"
            {
                body.insert(
                    1,
                    Stmt::IndexAssign {
                        base: Expr::Ident("items".into()),
                        index: Expr::Call {
                            callee: Box::new(Expr::Ident("mutate".into())),
                            args: vec![Expr::Ident("items".into())],
                        },
                        value: Expr::IntLit(22),
                    },
                );
            }
        }
        crate::lower_core::desugar_module(&mut module);
        let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
        assert!(report.ok, "{report:?}");
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
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn raw_vector_field_index_store_uses_shared_preparation() {
        use crate::core_ir::{Decl, Expr, Stmt};
        for (source, effectful) in [
            (
                "struct Bag { items: Vec<i64> } struct Wrapper { bag: Bag } fn main() -> i64 { let mut wrapper: Wrapper = Wrapper { bag: Bag { items: vec![0, 22] } }; return wrapper.bag.items[0] + wrapper.bag.items[1]; }",
                false,
            ),
            (
                "struct Bag { items: Vec<i64> } struct Wrapper { bag: Bag } impl Wrapper { fn replace(&mut self) -> i64 { self.bag.items = vec![20, 22]; return 0; } } fn main() -> i64 { let mut wrapper: Wrapper = Wrapper { bag: Bag { items: vec![0, 0] } }; return wrapper.bag.items[0] + wrapper.bag.items[1]; }",
                true,
            ),
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            for decl in &mut module.decls {
                if let Decl::Function { name, body, .. } = decl
                    && name == "main"
                {
                    let index = if effectful {
                        Expr::Call {
                            callee: Box::new(Expr::Ident("Wrapper::replace".into())),
                            args: vec![Expr::Ident("wrapper".into())],
                        }
                    } else {
                        Expr::IntLit(0)
                    };
                    body.insert(
                        1,
                        Stmt::IndexAssign {
                            base: Expr::Field {
                                base: Box::new(Expr::Field {
                                    base: Box::new(Expr::Ident("wrapper".into())),
                                    name: "bag".into(),
                                }),
                                name: "items".into(),
                            },
                            index,
                            value: Expr::IntLit(if effectful { 1 } else { 20 }),
                        },
                    );
                }
            }
            assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn failed_vector_field_replacement_preserves_caller_header() {
        use crate::core_ir::{CatchArm, Decl, Expr, Stmt};
        let parsed = crate::compiler::rust_front::parse_rust_source("struct Bag { items: Vec<i64> } fn fail() -> i64 { return 0; } impl Bag { fn replace(&mut self) -> i64 { self.items = vec![0, fail()]; return 0; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![20, 22] }; Bag::replace(&mut bag); return bag.items[0] + bag.items[1]; }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        for decl in &mut module.decls {
            if let Decl::Function { name, body, .. } = decl {
                if name == "fail" {
                    *body = vec![Stmt::Throw(Expr::IntLit(9))];
                }
                if name == "main" {
                    body[1] = Stmt::Try {
                        body: vec![body[1].clone()],
                        catches: vec![CatchArm {
                            pattern: "9".into(),
                            body: vec![body[2].clone()],
                        }],
                    };
                }
            }
        }
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
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn vector_field_index_writes_mutate_receiver_storage() {
        for source in [
            "struct Bag { items: Vec<i64> } impl Bag { fn set(&mut self) -> i64 { self.items[0] = 20; return self.items[0] + self.items[1]; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0, 22] }; return Bag::set(&mut bag); }",
            "struct Bag { items: Vec<i64> } struct Wrapper { tag: i64, bag: Bag } fn main() -> i64 { let mut wrapper: Wrapper = Wrapper { tag: 0, bag: Bag { items: vec![0, 22] } }; wrapper.bag.items[0] = 20; return wrapper.bag.items[0] + wrapper.bag.items[1]; }",
            "struct Bag { items: Vec<i64> } impl Bag { fn replace(&mut self) -> i64 { self.items = vec![20, 22]; return 0; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; Bag::replace(&mut bag); return bag.items[0] + bag.items[1]; }",
            "struct Bag { items: Vec<i64> } struct Wrapper { tag: i64, bag: Bag } impl Wrapper { fn replace(&mut self) -> i64 { self.bag.items = vec![20, 22]; return self.bag.items[0] + self.bag.items[1]; } } fn main() -> i64 { let mut wrapper: Wrapper = Wrapper { tag: 0, bag: Bag { items: vec![0] } }; return Wrapper::replace(&mut wrapper); }",
            "struct Bag { items: Vec<i64> } impl Bag { fn replace(&mut self, values: Vec<i64>) -> i64 { self.items = values; return self.items[0] + self.items[1]; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; return Bag::replace(&mut bag, vec![20, 22]); }",
            "struct Bag { items: Vec<i64> } impl Bag { fn replace(&mut self, other: Bag) -> i64 { self.items = other.items; return self.items[0] + self.items[1]; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; let other: Bag = Bag { items: vec![20, 22] }; return Bag::replace(&mut bag, other); }",
            "struct Bag { items: Vec<i64> } struct Wrapper { tag: i64, bag: Bag } fn main() -> i64 { let mut wrapper: Wrapper = Wrapper { tag: 0, bag: Bag { items: vec![0] } }; wrapper.bag.items = vec![20, 22]; return wrapper.bag.items[0] + wrapper.bag.items[1]; }",
            "struct Bag { items: Vec<f64> } impl Bag { fn replace(&mut self) -> i64 { self.items = vec![20.0, 22.0]; return 0; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0.0] }; Bag::replace(&mut bag); return (bag.items[0] + bag.items[1]) as i64; }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn saturating_cast_operand_effects_run_once() {
        use crate::core_ir::{Decl, Expr, Stmt};
        let mut module = crate::in_lang_parse::parse_in_source("var count: Int = 0;\nfn value() -> Float { count = count + 1; return 42.9; }\nfn main() -> Int { return 0; }").unwrap();
        for decl in &mut module.decls {
            if let Decl::Function { name, body, .. } = decl
                && name == "main"
            {
                *body = vec![Stmt::Return(Some(Expr::Binary {
                    op: "-".into(),
                    lhs: Box::new(Expr::Binary {
                        op: "+".into(),
                        lhs: Box::new(Expr::Unary {
                            op: "float-saturate-u8".into(),
                            expr: Box::new(Expr::Call {
                                callee: Box::new(Expr::Ident("value".into())),
                                args: vec![],
                            }),
                        }),
                        rhs: Box::new(Expr::Ident("count".into())),
                    }),
                    rhs: Box::new(Expr::IntLit(1)),
                }))];
            }
        }
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
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
    fn saturating_numeric_casts_match_host_rust() {
        for (target, bits, signed) in [
            ("i8", 8, true),
            ("i16", 16, true),
            ("i32", 32, true),
            ("i64", 64, true),
            ("u8", 8, false),
            ("u16", 16, false),
            ("u32", 32, false),
            ("u64", 64, false),
        ] {
            let source =
                format!("fn main(value: f64) -> i64 {{ return (value as {target}) as i64; }}");
            let parsed = crate::compiler::rust_front::parse_rust_source(&source).unwrap();
            let module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
            let upper = 2.0f64.powi(i32::from(bits) - i32::from(signed));
            let lower = if signed { -upper } else { 0.0 };
            for value in [
                0.0,
                -0.0,
                42.9,
                -42.9,
                -1.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                lower,
                upper,
                f64::from_bits(upper.to_bits() - 1),
                f64::from_bits(lower.to_bits() + 1),
                2.0f64.powi(63),
                2.0f64.powi(63) + 2048.0,
            ] {
                let expected = crate::core_ir::saturating_float_cast(value, bits, signed);
                for profile in [
                    crate::emit_profile::EmitProfile::Default,
                    crate::emit_profile::EmitProfile::Lean,
                    crate::emit_profile::EmitProfile::Harden,
                ] {
                    assert_eq!(
                        invoke_module(&module, profile, &[value.to_bits() as i64]),
                        Some(expected),
                        "{target} {value:?} {profile:?}"
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn scalar_vector_clone_owns_independent_storage() {
        let mut sources = vec![
            "fn main() -> i64 { let items: Vec<i64> = vec![20,22]; let mut copied: Vec<i64> = items.clone(); copied[0] = 0; return items[0] + items[1]; }",
            "fn main() -> i64 { let items: Vec<f64> = vec![20.0,22.0]; let mut copied: Vec<f64> = items.clone(); copied[0]=0.0; return (items[0]+items[1]) as i64; }",
            "fn main() -> i64 { let items: Vec<bool> = vec![true]; let mut copied: Vec<bool> = items.clone(); copied[0]=false; if items[0] { return 42; } return 0; }",
            "fn values() -> Vec<i64> { return vec![20,22]; } fn main() -> i64 { let copied: Vec<i64> = values().clone(); return copied[0] + copied[1]; }",
            "fn accept(prefix: i64, items: Vec<i64>) -> i64 { return prefix+items[0]+items[1]; } fn main() -> i64 { let items: Vec<i64> = vec![20,22]; return accept(0,items.clone()); }",
            "fn accept(items: Vec<i64>) -> i64 { return items[0]+items[1]; } fn main() -> i64 { let items: Vec<i64> = vec![20,22]; return accept(items.clone()); }",
            "fn values(items: Vec<i64>) -> Vec<i64> { return items.clone(); } fn main() -> i64 { let items: Vec<i64> = vec![20,22]; let copied: Vec<i64> = values(items); return copied[0]+copied[1]; }",
            "fn main() -> i64 { let items: Vec<i64> = vec![]; let copied: Vec<i64> = items.clone(); return copied.len + 42; }",
        ];
        if cfg!(target_arch = "aarch64") {
            sources.push("struct Bag { items: Vec<i64> } impl Bag { fn answer(&mut self) -> i64 { self.items = vec![20,22]; let mut copied: Vec<i64> = self.items.clone(); copied[0]=0; return self.items[0]+self.items[1]; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; return Bag::answer(&mut bag); }");
        }
        for source in sources {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(
                crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {:?}",
                crate::core_ir_verifier::verify_for_entry(&module, "main")
            );
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    fn vector_allocation_emitters_reject_non_integer_capacity() {
        use crate::core_ir::{Decl, Expr, Stmt};
        for value in [
            Expr::BoolLit(true),
            Expr::FloatLit(crate::core_ir::FloatVal(1.0)),
        ] {
            let mut module =
                crate::compiler::rust_front::parse_rust_source("fn main()->i64{42}").unwrap();
            let Decl::Function { body, .. } = &mut module.decls[0] else {
                panic!("missing main")
            };
            body.insert(
                0,
                Stmt::Expr(Expr::Call {
                    callee: Box::new(Expr::Ident("vector-with-capacity-int".into())),
                    args: vec![value],
                }),
            );
            assert!(
                crate::native_emit::lower::lower_module(
                    &module,
                    "main",
                    crate::native_emit::NativeLinkage::Executable
                )
                .err()
                .unwrap()
                .contains("capacity must be an integer")
            );
            assert!(
                crate::native_emit::x86_64_lower::lower_module(&module, "main")
                    .err()
                    .unwrap()
                    .contains("capacity must be an integer")
            );
        }
    }

    #[test]
    fn scalar_vector_clear_refuses_unsupported_storage_and_c_projection() {
        for source in [
            "fn main()->i64{let mut v=[1,2];v.clear();42}",
            "fn main()->i64{let mut v=vec![\"hello\"];v.clear();42}",
            "struct Item{value:i64} fn main()->i64{let mut v=vec![Item{value:42}];v.clear();42}",
            "fn main()->i64{let mut v=vec![vec![42]];v.clear();42}",
            "fn main()->i64{let mut v=Vec::new();v.clear();42}",
            "fn main()->i64{let mut v=vec![42];v.clear(1);42}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}: {module:?}"
            );
        }
        let parsed = crate::compiler::rust_front::parse_rust_source(
            "fn main()->i64{let mut v=vec![42];v.clear();42}",
        )
        .unwrap();
        let module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        assert!(
            crate::native_emit::c_backend::emit_c_module(&module)
                .unwrap_err()
                .contains("vector clear requires owned native lowering")
        );
    }

    #[test]
    fn scalar_vector_clone_rejects_unsupported_operands_and_c_projection() {
        use crate::core_ir::{Decl, Expr, Stmt};
        let parsed = crate::compiler::rust_front::parse_rust_source("fn main() -> i64 { let items: Vec<i64> = vec![20,22]; let copied: Vec<i64> = items.clone(); return copied[0]+copied[1]; }").unwrap();
        let module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        assert!(
            crate::native_emit::c_backend::emit_c_module(&module)
                .unwrap_err()
                .contains("vector clone requires owned native lowering")
        );
        let native = crate::compiler::rust_front::parse_rust_source(
            "fn main(items: Vec<i64>) -> Vec<i64> { return items.clone(); }",
        )
        .unwrap();
        let native =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &native);
        #[cfg(target_arch = "aarch64")]
        crate::native_emit::lower::lower_native_module(
            &native,
            "main",
            crate::native_emit::NativeLinkage::Executable,
        )
        .expect("native vector cloning is supported");
        #[cfg(target_arch = "x86_64")]
        assert!(
            crate::native_emit::x86_64_lower::lower_native_module(&native, "main")
                .err()
                .unwrap()
                .contains("vector clone requires the 64-bit JIT runtime")
        );
        let declared = crate::compiler::rust_front::parse_rust_source("fn clone(items: Vec<i64>) -> Vec<i64> { return items; } fn main() -> i64 { let items: Vec<i64> = vec![42]; let copied: Vec<i64> = items.clone(); return copied[0]; }").unwrap();
        let declared =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &declared);
        assert!(!format!("{declared:?}").contains("vector-clone"));
        for operand in [Expr::IntLit(42), Expr::ArrayLit(vec![Expr::IntLit(42)])] {
            let mut invalid = module.clone();
            for decl in &mut invalid.decls {
                if let Decl::Function { body, .. } = decl {
                    *body = vec![Stmt::Return(Some(Expr::Unary {
                        op: "vector-clone".into(),
                        expr: Box::new(operand.clone()),
                    }))];
                }
            }
            assert!(!crate::core_ir_verifier::verify_for_entry(&invalid, "main").ok);
            assert!(
                crate::typecheck::TypeChecker::new()
                    .check_module(&invalid)
                    .is_err()
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn scalar_vector_clone_evaluates_producer_once() {
        use crate::core_ir::{Decl, Expr, Stmt, Typ};
        let parsed = crate::compiler::rust_front::parse_rust_source("fn values() -> Vec<i64> { return vec![20,22]; } fn main() -> i64 { let copied: Vec<i64> = values().clone(); return copied[0]+copied[1]; }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        module.decls.insert(
            0,
            Decl::Global {
                name: "calls".into(),
                typ: Typ::Int,
                init: Some(Box::new(Expr::IntLit(0))),
                mutable: true,
                constant_value: false,
            },
        );
        for decl in &mut module.decls {
            if let Decl::Function { name, body, .. } = decl {
                if name == "values" {
                    body.insert(
                        0,
                        Stmt::Assign(
                            "calls".into(),
                            Expr::Binary {
                                op: "+".into(),
                                lhs: Box::new(Expr::Ident("calls".into())),
                                rhs: Box::new(Expr::IntLit(1)),
                            },
                        ),
                    );
                } else if name == "main" {
                    *body.last_mut().unwrap() = Stmt::Return(Some(Expr::Ident("calls".into())));
                }
            }
        }
        crate::lower_core::desugar_module(&mut module);
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(1));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn typed_vector_length_reads_the_live_header() {
        let mut sources = vec![
            "fn len(items: Vec<i64>) -> i64 { return 42; } fn main() -> i64 { let items: Vec<i64> = vec![0]; return items.len(); }",
            "fn main() -> i64 { let items: [i64;2] = [20,22]; return items.len()+40; }",
            "fn values() -> [i64;2] { return [20,22]; } fn main() -> i64 { return values().len()+40; }",
            "fn main() -> i64 { let items: Vec<i64> = vec![20,22]; return items.len()+40; }",
            "fn main() -> i64 { let items: Vec<i64> = vec![]; return items.len()+42; }",
            "fn values() -> Vec<i64> { return vec![20,22]; } fn main() -> i64 { return values().len()+40; }",
            "fn main() -> i64 { let items: Vec<i64> = vec![20,22]; return items.clone().len()+40; }",
            "fn accept(items: Vec<i64>) -> i64 { return items.len()+40; } fn main() -> i64 { return accept(vec![20,22]); }",
        ];
        if cfg!(target_arch = "aarch64") {
            sources.push("struct Bag { items: Vec<i64> } fn main() -> i64 { let bag: Bag = Bag { items: vec![20,22] }; return bag.items.len()+40; }");
            sources.push("struct Bag { items: Vec<i64> } struct Outer { bag: Bag } fn main() -> i64 { let outer: Outer = Outer { bag: Bag { items: vec![20,22] } }; return outer.bag.items.len()+40; }");
            sources.push("struct Bag { items: Vec<i64> } fn main() -> i64 { let bag: Bag = Bag { items: vec![20,22] }; let mut copied: Vec<i64> = bag.items.clone(); copied[0]=0; return bag.items[0]+bag.items[1]; }");
            sources.push("struct Bag { items: Vec<i64> } impl Bag { fn answer(&mut self) -> i64 { self.items=vec![20,22]; return self.items.len()+40; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![0] }; return Bag::answer(&mut bag); }");
        }
        for source in sources {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn array_length_evaluates_producer_once() {
        let mut module = crate::in_lang_parse::parse_in_source("var calls: Int = 0;\nfn values() -> [Int] { calls += 1; return [20,22]; }\nfn main() -> Int { return array_len(values()) + calls; }").unwrap();
        crate::lower_core::desugar_module(&mut module);
        let split = crate::in_lang_parse::parse_in_source(
            "fn main() -> Int { return array_len(str_split_lines(\"a\\nb\")); }",
        )
        .unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(3));
            assert_eq!(invoke_module(&split, profile, &[]), Some(2));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn typed_sequence_empty_uses_length() {
        let mut sources = vec![
            "fn clone(items: Vec<i64>) -> Vec<i64> { return vec![20,22]; } fn main() -> i64 { let items: Vec<i64> = vec![0]; let copied: Vec<i64> = items.clone(); return copied[0]+copied[1]; }",
            "fn is_empty(items: Vec<i64>) -> bool { return false; } fn main() -> i64 { let items: Vec<i64> = vec![]; if items.is_empty() { return 0; } return 42; }",
            "fn main() -> i64 { let items: Vec<i64> = vec![]; if items.is_empty() { return 42; } return 0; }",
            "fn main() -> i64 { let items: Vec<i64> = vec![1]; if items.is_empty() { return 0; } return 42; }",
            "fn values() -> Vec<i64> { return vec![]; } fn main() -> i64 { if values().is_empty() { return 42; } return 0; }",
            "fn main() -> i64 { let items: Vec<i64> = vec![]; if items.clone().is_empty() { return 42; } return 0; }",
            "fn main() -> i64 { let items: [i64;2] = [20,22]; if items.is_empty() { return 0; } return 42; }",
            "fn values() -> [i64;2] { return [20,22]; } fn main() -> i64 { if values().is_empty() { return 0; } return 42; }",
        ];
        if cfg!(target_arch = "aarch64") {
            sources.push("struct Bag { items: Vec<i64> } impl Bag { fn answer(&mut self) -> i64 { self.items=vec![]; if self.items.is_empty() { return 42; } return 0; } } fn main() -> i64 { let mut bag: Bag = Bag { items: vec![1] }; return Bag::answer(&mut bag); }");
            sources.push("struct Bag { items: Vec<i64> } fn main() -> i64 { let bag: Bag = Bag { items: vec![] }; if bag.items.is_empty() { return 42; } return 0; }");
        }
        for source in sources {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn sequence_empty_evaluates_producer_once() {
        let mut module = crate::in_lang_parse::parse_in_source("var calls: Int = 0;\nfn values() -> [Int] { calls += 1; return []; }\nfn main() -> Int { if is_empty(values()) { return calls; } return 0; }").unwrap();
        crate::lower_core::desugar_module(&mut module);
        let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
        assert!(verified.ok, "{verified:?}");
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(1));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn typed_string_length_and_empty_preserve_bytes() {
        let mut sources = vec![
            "fn len(text: String) -> i64 { return 42; } fn main() -> i64 { let text: String = \"\"; return text.len(); }",
            "fn accept(text: String) -> i64 { return text.len()+40; } fn main() -> i64 { return accept(\"hi\"); }",
            "struct Bag { text: String } fn main() -> i64 { let bag: Bag = Bag { text: \"hi\" }; return bag.text.len()+40; }",
            "fn main() -> i64 { let text: String = \"hi\"; return text.len()+40; }",
            "fn main() -> i64 { let text: String = \"é\"; return text.len()+40; }",
            "fn main() -> i64 { let text: String = \"a\\0b\"; return text.len()+39; }",
            "fn values() -> String { return \"hi\"; } fn main() -> i64 { return values().len()+40; }",
            "fn main() -> i64 { let text: String = \"\"; if text.is_empty() { return 42; } return 0; }",
            "fn main() -> i64 { let text: String = \"hi\"; if text.is_empty() { return 0; } return 42; }",
            "fn values() -> String { return \"\"; } fn main() -> i64 { if values().is_empty() { return 42; } return 0; }",
            "fn is_empty(text: String) -> bool { return false; } fn main() -> i64 { let text: String = \"\"; if text.is_empty() { return 0; } return 42; }",
        ];
        if cfg!(target_arch = "aarch64") {
            sources.push("struct Bag { text: String } impl Bag { fn answer(&mut self) -> i64 { self.text=\"hi\"; return self.text.len()+40; } } fn main() -> i64 { let mut bag: Bag = Bag { text: \"\" }; return Bag::answer(&mut bag); }");
        }
        for source in sources {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            crate::typecheck::TypeChecker::new()
                .check_module(&module)
                .unwrap();
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn typed_string_methods_evaluate_producer_once() {
        for body in [
            "return len(values()) + calls;",
            "return values().len() + calls;",
            "if is_empty(values()) { return calls; } return 0;",
            "if values().is_empty() { return calls; } return 0;",
        ] {
            let source = format!(
                "var calls: Int = 0;\nfn values() -> String {{ calls += 1; return \"\"; }}\nfn main() -> Int {{ {body} }}"
            );
            let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            crate::typecheck::TypeChecker::new()
                .check_module(&module)
                .unwrap();
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(invoke_module(&module, profile, &[]), Some(1));
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn rust_unsafe_statement_and_return_blocks_execute() {
        for source in [
            "fn main() -> i64 { let mut value: i64=0; (unsafe { value=42; }); return value; }",
            "fn main() -> i64 { unsafe { return 42; }; }",
            "fn main() -> i64 { let mut value: i64 = 0; unsafe { value=42; } return value; }",
            "fn main() -> i64 { unsafe { 42 } }",
            "fn main() -> i64 { return unsafe { 42 }; }",
            "fn main() -> i64 { unsafe { return 42; } return 0; }",
            "fn main() -> i64 { let value: i64=42; unsafe { let mut value: i64=0; value=1; } return value; }",
            "fn main() -> i64 { let mut count: i64=0; while count<3 { unsafe { count+=1; continue; } } return count+39; }",
            "fn main() -> i64 { let mut count: i64=0; loop { unsafe { count=42; break; } } return count; }",
            "fn main() -> i64 { let mut count: i64=0; unsafe { unsafe { count+=42; } } return count; }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            crate::typecheck::TypeChecker::new()
                .check_module(&module)
                .unwrap();
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    fn rust_unsafe_value_blocks_preserve_values_without_closure_substitution() {
        for source in [
            "fn main() -> i64 { let value: i64 = unsafe { 42 }; return value; }",
            "fn main() -> i64 { let value: i64 = (unsafe { 42 }); return value; }",
            "fn main() -> i64 { let value: i64 = { unsafe { 42 } }; return value; }",
            "fn main() -> i64 { let value: i64 = { 42 }; return value; }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{source}: {verified:?}");
            assert!(
                crate::typecheck::TypeChecker::new()
                    .check_module(&module)
                    .is_ok()
            );
            assert!(!format!("{module:?}").contains("__closure_"));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn rust_statics_use_shared_scalar_storage() {
        for source in [
            "static VALUE: i64=42; fn main() -> i64 { let VALUE: i64=0; return self::VALUE+VALUE; }",
            "mod state { pub static mut VALUE: i64=0; use self::VALUE as DATA; pub fn read() -> i64 { unsafe { DATA=42; } return self::VALUE; } } fn main() -> i64 { return state::read(); }",
            "static VALUE: i64=42; mod child { pub fn read() -> i64 { return crate::VALUE; } } fn main() -> i64 { return child::read(); }",
            "static mut VALUE: i64=0; fn main() -> i64 { unsafe { crate::VALUE=42; } return unsafe { self::VALUE }; }",
            "mod state { pub static mut VALUE: i64=0; pub fn add() -> i64 { unsafe { self::VALUE+=21; } return unsafe { self::VALUE }; } } fn main() -> i64 { state::add(); return state::add(); }",
            "mod state { pub static mut VALUE: i64=0; } fn main() -> i64 { unsafe { crate::state::VALUE=42; } return unsafe { crate::state::VALUE }; }",
            "mod state { pub static mut VALUE: i64=0; } use crate::state::VALUE as DATA; fn main() -> i64 { unsafe { DATA=42; } return unsafe { crate::state::VALUE }; }",
            "mod state { pub static VALUE: i64=42; pub struct Point {} impl Point { pub fn read() -> i64 { return self::VALUE; } } } fn main() -> i64 { return state::Point::read(); }",
            "mod state { pub static VALUE: i64=42; pub mod nested { pub static VALUE: i64=0; pub fn read() -> i64 { return self::VALUE; } } pub fn read() -> i64 { return self::VALUE+ self::nested::VALUE; } } fn main() -> i64 { return state::read(); }",
            "mod state { pub static mut VALUE: i64=0; } fn main() -> i64 { unsafe { state::VALUE=21; state::VALUE+=21; } return unsafe { state::VALUE }; }",
            "mod state { pub static mut VALUE: i64=0; pub fn read() -> i64 { unsafe { VALUE } } } use state::VALUE as DATA; fn main() -> i64 { unsafe { DATA=42; } return state::read(); }",
            "mod state { pub static VALUE: i64=42; pub fn read() -> i64 { return VALUE; } } use state::VALUE; fn main() -> i64 { let VALUE: i64=0; return state::read()+VALUE; }",
            "mod state { pub static VALUE: i64=20; pub fn add(VALUE: i64) -> i64 { return VALUE+22; } } fn main() -> i64 { return state::add(20); }",
            "mod outer { pub mod state { pub static VALUE: i64=42; } pub fn read() -> i64 { return state::VALUE; } } fn main() -> i64 { return outer::read(); }",
            "static VALUE: i64=42; fn main() -> i64 { return VALUE; }",
            "static mut VALUE: i64=0; fn main() -> i64 { unsafe { VALUE=42; } return unsafe { VALUE }; }",
            "static mut VALUE: i64=0; fn add() -> i64 { unsafe { VALUE+=21; } return unsafe { VALUE }; } fn main() -> i64 { add(); return add(); }",
            "static VALUE: f64=42.9; fn main() -> i64 { return VALUE as i64; }",
            "static VALUE: bool=true; fn main() -> i64 { if VALUE { return 42; } return 0; }",
            "static VALUE: i64=42; fn read() -> i64 { return VALUE; } fn main() -> i64 { let VALUE: i64=0; return read()+VALUE; }",
            "mod left { pub static VALUE: i64=20; pub fn read() -> i64 { return VALUE; } } mod right { pub static VALUE: i64=22; pub fn read() -> i64 { return VALUE; } } fn main() -> i64 { return left::read()+right::read(); }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            crate::typecheck::TypeChecker::new()
                .check_module(&module)
                .unwrap();
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    fn rust_static_namespace_rewrites_do_not_capture_parent_bindings() {
        for source in [
            "static VALUE: i64=42; mod child { pub fn read() -> i64 { return VALUE; } } fn main() -> i64 { return child::read(); }",
            "mod state { pub mod nested { pub static VALUE: i64=0; pub fn read() -> i64 { return nested::VALUE; } } } fn main() -> i64 { return state::nested::read(); }",
            "static VALUE: i64=42; mod child { pub fn read() -> i64 { return self::VALUE; } } fn main() -> i64 { return child::read(); }",
            "mod parent { pub static VALUE: i64=42; pub mod child { pub fn read() -> i64 { return VALUE; } } } fn main() -> i64 { return parent::child::read(); }",
            "mod state { pub static VALUE: i64=42; } use state::VALUE as DATA; mod child { pub fn read() -> i64 { return DATA; } } fn main() -> i64 { return child::read(); }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(!verified.ok, "{source}");
            assert_eq!(
                verified.reason_code.as_deref(),
                Some("unresolved-symbol"),
                "{verified:?}"
            );
        }
    }

    #[test]
    fn rust_static_immutability_and_runtime_initializers_are_preserved() {
        for source in [
            "static VALUE: i64=0; fn main() -> i64 { unsafe { VALUE=42; } return VALUE; }",
            "mod state { pub static VALUE: i64=0; } use state::VALUE as DATA; fn main() -> i64 { unsafe { DATA=42; } return DATA; }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert_eq!(
                verified.reason_code.as_deref(),
                Some("immutable-global"),
                "{verified:?}"
            );
        }
        let parsed = crate::compiler::rust_front::parse_rust_source("static mut CALLS: i64=0; static VALUE: i64=make(); fn make() -> i64 { unsafe { CALLS+=1; } return 42; } fn main() -> i64 { return VALUE; }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        assert!(
            crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .err()
            .unwrap()
            .contains("unsupported native initializer for global `VALUE`")
        );
        assert!(
            crate::native_emit::x86_64_lower::lower_module(&module, "main")
                .err()
                .unwrap()
                .contains("unsupported native initializer for global `VALUE`")
        );
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn rust_blocks_preserve_scoped_bindings_and_control_flow() {
        for source in [
            "fn main() -> i64 { let value: i64=42; { let value: i64=0; } return value; }",
            "fn main() -> i64 { let mut value: i64=20; { value+=22; } return value; }",
            "fn main() -> i64 { let value: i64=42; { let value: i64=0; { let value: i64=1; } } return value; }",
            "fn main() -> i64 { { let value: i64=42; value } }",
            "fn main() -> i64 { return { let value: i64=42; value }; }",
            "fn main() -> i64 { { return 42; }; }",
            "fn main() -> i64 { let mut count: i64=0; while count<3 { { count+=1; continue; } } return count+39; }",
            "fn main() -> i64 { let mut count: i64=0; loop { { count=42; break; } } return count; }",
            "static VALUE: i64=42; fn main() -> i64 { { let VALUE: i64=0; } return VALUE; }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            crate::typecheck::TypeChecker::new()
                .check_module(&module)
                .unwrap();
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn literal_global_arrays_preserve_storage_bounds_and_scalar_types() {
        for (source, expected) in [
            (
                "var calls: Int = 0;\nvar items: [Int] = [0,22];\nfn index() -> Int { calls += 1; return 0; }\nfn value() -> Int { calls += 10; return 20; }\nfn main() -> Int { items[index()] = value(); return items[0]+items[1]+calls - 11; }",
                42,
            ),
            (
                "var before: Int = 1;\nvar items: [Int] = [20,22];\nvar after: Int = 2;\nfn main() -> Int { return items[0]+items[1]+before+after - 3; }",
                42,
            ),
            (
                "var items: [Int] = [0,22];\nfn main() -> Int { items[0]=20; return items[0]+items[1]; }",
                42,
            ),
            (
                "var items: [Int] = [20,22];\nfn main() -> Int { return array_len(items)+40; }",
                42,
            ),
            (
                "var items: [Int] = [];\nfn main() -> Int { return array_len(items)+42; }",
                42,
            ),
            (
                "var items: [Int] = [20,22];\nfn main() -> Int { return items[2]; }",
                1,
            ),
            (
                "var items: [Int] = [20,22];\nfn main() -> Int { items[-1]=0; return 42; }",
                1,
            ),
            (
                "var items: [Float] = [20.0,22.0];\nfn main() -> Float { return items[0]+items[1]; }",
                42_f64.to_bits() as i64,
            ),
            (
                "var items: [Bool] = [true,false];\nfn main() -> Int { if items[0] && !items[1] { return 42; } return 0; }",
                42,
            ),
            (
                "var items: [Int] = [20,22];\nfn accept(values: [Int]) -> Int { return values[0]+values[1]; }\nfn main() -> Int { return accept(items); }",
                42,
            ),
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn rust_static_arrays_execute_reads_lengths_and_mutable_elements() {
        for source in [
            "static ITEMS: [i64;2]=[20,22]; fn main() -> i64 { return ITEMS[0]+ITEMS[1]; }",
            "static ITEMS: [i64;2]=[20,22]; fn main() -> i64 { return ITEMS.len()+40; }",
            "static mut ITEMS: [i64;2]=[0,22]; fn main() -> i64 { unsafe { ITEMS[0]=20; } return unsafe { ITEMS[0]+ITEMS[1] }; }",
            "static TABLE: [[i64;2];2]=[[20,22],[1,2]]; fn main() -> i64 { return TABLE[0][0]+TABLE[0][1]; }",
            "static mut TABLE: [[i64;2];1]=[[0,22]]; fn main() -> i64 { unsafe { TABLE[0][0]=20; return TABLE[0][0]+TABLE[0][1]; } }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{verified:?}: {source}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
        for source in [
            "static mut ITEMS: [i64;2]=[20,22]; fn len(mut values: [i64;2]) -> i64 { values[0]=0; return 0; } fn main() -> i64 { unsafe { len(ITEMS); return ITEMS[0]+ITEMS[1]; } }",
            "static ITEMS: [i64;2]=[20,22]; fn main() -> i64 { let mut copy: [i64;2]=ITEMS; copy[0]=0; return ITEMS[0]+ITEMS[1]; }",
            "static ITEMS: [i64;2]=[20,22]; fn value() -> i64 { return 0; } fn main() -> i64 { unsafe { ITEMS[0]=value(); } return 42; }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(!crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        }
    }

    #[test]
    fn global_array_unsupported_uses_refuse_instead_of_corrupting_headers() {
        for body in [
            "let alias = items; alias[0]=0; return 42;",
            "items[step()]=0; return 42;",
        ] {
            let module = crate::in_lang_parse::parse_in_source(&format!("var items: [Int] = [20,22];\nfn step() -> Int {{ return 0; }}\nfn main() -> Int {{ {body} }}")).unwrap();
            let mut module = module;
            crate::lower_core::desugar_module(&mut module);
            for decl in &mut module.decls {
                if let crate::core_ir::Decl::Global { mutable, .. } = decl {
                    *mutable = false;
                }
            }
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{body}: {module:?}"
            );
        }
        let module = crate::in_lang_parse::parse_in_source(
            "var items: [Int] = [20,22];\nfn main() -> Int { items=[1,2]; return items[0]; }",
        )
        .unwrap();
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        assert!(
            crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .err()
            .unwrap()
            .contains("whole global array assignment")
        );
        assert!(
            crate::native_emit::x86_64_lower::lower_module(&module, "main")
                .err()
                .unwrap()
                .contains("whole global array assignment")
        );
        for body in ["return items.missing;", "items.missing=42; return 42;"] {
            let module = crate::in_lang_parse::parse_in_source(&format!(
                "var items: [Int] = [20,22];\nfn main() -> Int {{ {body} }}"
            ))
            .unwrap();
            assert_eq!(
                crate::core_ir_verifier::verify_for_entry(&module, "main")
                    .reason_code
                    .as_deref(),
                Some("invalid-field-base")
            );
            assert!(
                crate::typecheck::TypeChecker::new()
                    .check_module(&module)
                    .is_err()
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn nested_static_arrays_read_checked_rows_on_both_backends() {
        for (source, expected) in [
            (
                "var items: [[Int]] = [[20],[22]];\nfn main() -> Int { return items[0][0]+items[1][0]; }",
                42,
            ),
            (
                "var items: [[[Int]]] = [[[20,22]]];\nfn main() -> Int { return items[0][0][0]+items[0][0][1]; }",
                42,
            ),
            (
                "var items: [[Int]] = [[],[20,22]];\nfn main() -> Int { return array_len(items[0])+items[1][0]+items[1][1]; }",
                42,
            ),
            (
                "var items: [[Int]] = [[],[42]];\nfn main() -> Int { return items[0][0]; }",
                1,
            ),
            (
                "var items: [[Int]] = [[42]];\nfn main() -> Int { return items[2][0]; }",
                1,
            ),
            (
                "var items: [[Int]] = [[20,22]];\nfn row() -> [Int] { return items[0]; }\nfn main() -> Int { return row()[0]+row()[1]; }",
                42,
            ),
            (
                "var items: [[Int]] = [[20,22]];\nfn sum(row: [Int]) -> Int { return row[0]+row[1]; }\nfn main() -> Int { return sum(items[0]); }",
                42,
            ),
            (
                "var items: [[Int]] = [[20,22]];\nfn main() -> Int { let row = items[0]; return row[0]+row[1]; }",
                42,
            ),
            (
                "var items: [[Int]] = [[0,22]];\nfn main() -> Int { items[0][0]=20; return items[0][0]+items[0][1]; }",
                42,
            ),
            (
                "var items: [[Float]] = [[20.0,22.0]];\nfn main() -> Int { if items[0][0]+items[0][1] == 42.0 { return 42; } return 0; }",
                42,
            ),
            (
                "var items: [[Bool]] = [[true,false]];\nfn main() -> Int { if items[0][0] && !items[0][1] { return 42; } return 0; }",
                42,
            ),
            (
                "var items: [[Int]] = [[20,22]];\nvar count: Int = 0;\nfn row_index() -> Int { count+=1; return 0; }\nfn col_index() -> Int { count*=10; return 1; }\nfn main() -> Int { return items[row_index()][col_index()]+count*2; }",
                42,
            ),
            (
                "var items: [[Int]] = [[20,22]];\nfn row() -> [Int] { return items[5]; }\nfn main() -> Int { try { return array_len(row()); } catch 1 { return 42; } return 0; }",
                42,
            ),
            (
                "var items: [[Int]] = [[20,22]];\nfn sum(base: Int, row: [Int]) -> Int { return base+row[0]+row[1]; }\nfn main() -> Int { return sum(0,items[0]); }",
                42,
            ),
            (
                "var before: Int = 7;\nvar items: [[Int]] = [[],[0,22]];\nvar after: Int = 9;\nfn sum(row: [Int]) -> Int { return row[0]+row[1]; }\nfn main() -> Int { items[1][0]=20; return sum(items[1])+array_len(items[0])+before - after+2; }",
                42,
            ),
            (
                "var items: [[Int]] = [[20,22]];\nfn main() -> Int { let sum = 0; for value in items[0] { sum+=value; } return sum; }",
                42,
            ),
            (
                "var items: [[Int]] = [[20,22]];\nfn push(row: [Int], value: Int) -> Int { return row[0]+row[1]; }\nfn main() -> Int { return push(items[0],42); }",
                42,
            ),
            (
                "var items: [[Int]] = [[0,22]];\nfn main() -> Int { let row = items[0]; row[0]=20; return items[0][0]+items[0][1]; }",
                42,
            ),
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            let typed = crate::typecheck::TypeChecker::new().check_module(&module);
            assert!(typed.is_ok(), "{source}: {typed:?}");
            assert!(
                crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}"
            );
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "{source}"
                );
            }
        }
    }

    #[test]
    fn nested_array_literals_reject_scalar_and_depth_mismatches() {
        for value in ["[[],42]", "[42,[]]", "[[[]],[20]]", "[[20],[[]]]"] {
            let source = format!("fn main() -> Int {{ let items = {value}; return 42; }}");
            let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            assert!(
                !crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                "{source}"
            );
            assert!(
                crate::typecheck::TypeChecker::new()
                    .check_module(&module)
                    .is_err(),
                "{source}"
            );
        }
        let module = crate::in_lang_parse::parse_in_source(
            "var items: [[Int]] = [[20,22]];\nfn main() -> Int { items[0]=[42]; return 42; }",
        )
        .unwrap();
        assert!(
            crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .err()
            .unwrap()
            .contains("whole nested array assignment")
        );
        assert!(
            crate::native_emit::x86_64_lower::lower_module(&module, "main")
                .err()
                .unwrap()
                .contains("whole nested array assignment")
        );
        let module = crate::in_lang_parse::parse_in_source("var items: [[Int]] = [[20,22]];\nfn main() -> Int { let row = items[0]; array-push(row,42); return 42; }").unwrap();
        assert!(
            crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .err()
            .unwrap()
            .contains("fixed array storage")
        );
        let module = crate::in_lang_parse::parse_in_source("var items: [[Int]] = [[20,22]];\nfn main() -> Int { let row = items[0]; push(row,42); return 42; }").unwrap();
        assert!(
            crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .err()
            .unwrap()
            .contains("fixed array storage")
        );
        assert!(
            crate::native_emit::x86_64_lower::lower_module(&module, "main")
                .err()
                .unwrap()
                .contains("fixed array storage")
        );
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn pure_static_initializer_blocks_build_native_tables() {
        let source = "const BASE: [i64;4]=[20,22,1,2]; static TABLE: [[i64;2];2] = { let mut out = [[0;2];2]; let mut i=0; while i<2 { out[i]=[BASE[2*i],BASE[2*i+1]]; i+=1; } out }; fn main() -> i64 { return TABLE[0][0]+TABLE[0][1]; }";
        let source = source.replace("fn main() -> i64", "pub fn main() -> i64");
        let source =
            format!("mod tables {{ {source} }} fn main() -> i64 {{ return tables::main(); }}");
        let module = crate::compiler::rust_front::parse_rust_source(&source).unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &module);
        crate::lower_core::desugar_module(&mut module);
        assert!(
            crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
            "{module:?}"
        );
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }
    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn static_initializer_scopes_preserve_local_shadowing() {
        let parsed = crate::compiler::rust_front::parse_rust_source("mod tables { static BASE:i64=20; pub static VALUE:i64={let BASE=40; {let BASE=0;} BASE+2}; } fn main()->i64 { return tables::VALUE; }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn boolean_static_initializer_blocks_close_short_circuit_operations() {
        let parsed = crate::compiler::rust_front::parse_rust_source("static VALUE:bool={true && (false || true)}; fn main()->i64 { if VALUE {return 42;} return 0; }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        assert_eq!(
            invoke_module(&module, crate::emit_profile::EmitProfile::Default, &[]),
            Some(42)
        );
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn closure_parameters_do_not_shadow_generated_receivers() {
        use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule};
        let mut module = UnifiedModule {
            identity: Default::default(),
            decls: vec![Decl::Function {
                name: "main".into(),
                params: vec![],
                ret: Typ::Int,
                type_params: vec![],
                body: vec![
                    Stmt::Let(
                        "answer".into(),
                        None,
                        Expr::Closure {
                            params: vec![("self".into(), Typ::Int)],
                            ret: Typ::Int,
                            captures: vec![],
                            body: vec![Stmt::Return(Some(Expr::Ident("self".into())))],
                        },
                    ),
                    Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident("answer".into())),
                        args: vec![Expr::IntLit(42)],
                    })),
                ],
            }],
        };
        crate::lower_core::desugar_module(&mut module);
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn in_stateless_closure_calls_share_native_lowering() {
        let mut module = crate::in_lang_parse::parse_in_source("fn main() -> Int { let add = fn(a: Int, b: Int) -> Int { return a + b; }; return add(20, 22); }").unwrap();
        crate::lower_core::desugar_module(&mut module);
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn closure_match_and_catch_scopes_preserve_binding_types() {
        for source in [
            "fn main()->i64 { let answer=||{ match 42 { value=>value } }; answer() }",
            "fn main()->i64 { let answer=||{ match 20.5 { value=>value+21.5 } }; answer() as i64 }",
            "fn main()->i64 { let answer=||{ match true { true=>42, false=>0 } }; answer() }",
            "fn main()->i64 { let answer=||{ match 20.5 { value=>{ let f=|x|x+x; f(value)+1.0 } } }; answer() as i64 }",
            "fn fail()->Result<i64,i64> { Err(1) } fn main()->i64 { let answer=||{ if let Err(error)=fail() { let f=|x|x+2; return f(40); } return 0; }; answer() }",
            "fn main()->i64 { let value=40; let answer=||{ match 42 { value=>value } }; answer() }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn native_destructuring_preserves_shapes_bindings_and_guards() {
        for source in [
            "fn main()->i64 { let values=[20,22]; match values { [a,b]=>a+b, _=>0 } }",
            "fn main()->i64 { let pair=(20,22); match pair { (a,b)=>{return a+b;} } return 0; }",
            "fn main()->i64 { let pair=(20.5,21.5); match pair { (a,b)=>{return (a+b) as i64;} } return 0; }",
            "fn main()->i64 { let pair=(1,42); match pair { (2,a)=>{return 0;}, (1,a)=>{return a;}, _=>{return 0;} } }",
            "fn main()->i64 { let pair=(20,1,22); match pair { (a,..,b)=>{return a+b;} } return 0; }",
            "fn main()->i64 { let pair=(20,22); match pair { (a,b) if a==0=>{return 0;}, (a,b)=>{return a+b;} } return 0; }",
            "fn main()->i64 { let values: [i64;0]=[]; match values { [a]=>a, []=>42, _=>0 } }",
            "fn main()->i64 { let values=[true,false]; match values { [a,b]=>if a && !b {42} else {0}, _=>0 } }",
            "fn main()->i64 { let values=[20.5,21.5]; match values { [a,b]=>(a+b) as i64, _=>0 } }",
            "static mut CALLS:i64=0; fn values()->[i64;2] { unsafe {CALLS+=1;} [20,21] } fn main()->i64 { match values() { [a,b]=>unsafe {a+b+CALLS}, _=>0 } }",
            "fn main()->i64 { let values=[42]; match values { [a,b]=>a+b, [a]=>a, _=>0 } }",
            "fn main()->i64 { let values=[20,1,2,22]; match values { [a,..,b]=>a+b, _=>0 } }",
            "fn main()->i64 { let values=[1,42]; match values { [2,a]=>0, [1,a]=>a, _=>0 } }",
            "fn main()->i64 { let values=[20,22]; match values { [a,b] if a==0=>0, [a,b]=>a+b, _=>0 } }",
            "struct Point { x:i64,y:i64 } fn main()->i64 { let p=Point{x:20,y:22}; match p { Point{x:a,y:b}=>{return a+b;} } return 0; }",
            "struct Point { x:i64,y:i64 } fn main()->i64 { let p=Point{x:20,y:22}; match p { Point{x:a,y:b}=>a+b } }",
            "struct Point { x:i64,y:i64 } fn main()->i64 { let f=|| { let p=Point{x:20,y:22}; match p { Point{x:a,y:b}=>a+b } }; f() }",
            "struct Point { x:i64,y:i64 } fn main()->i64 { let p=Point{x:1,y:42}; match p { Point{x:2,y:a}=>0, Point{x:1,y:a}=>a, _=>0 } }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn tuple_constructor_does_not_shadow_user_functions() {
        let source = "fn tuple-value(a: Int,b: Int)->Int {return a+b;}\nfn main()->Int {return tuple-value(20,22);}";
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn typed_tuples_preserve_fields_calls_returns_and_order() {
        for source in [
            "fn main()->i64{let v=vec![42];v.clone().clear();v[0]}",
            "struct Bag{items:Vec<i64>} impl Bag{fn reset(&mut self){self.items.clear();self.items.push(42);}} fn main()->i64{let mut bag=Bag{items:vec![1,2]};bag.reset();bag.items[0]}",
            "static mut CALLS:i64=0;fn source()->Vec<i64>{unsafe{CALLS+=1;}vec![1]} fn main()->i64{source().clear();unsafe{CALLS+41}}",
            "fn main()->i64{let mut v=Vec::<i64>::with_capacity(10);v.push(1);let cap=v.capacity();v.clear();if v.len()!=0 || v.capacity()!=cap{return 0;}v.push(42);v[0]}",
            "fn main()->i64{let mut v=Vec::<i64>::new();v.clear();v.clear();v.push(42);v[0]}",
            "struct Bag{items:Vec<i64>} struct Outer{bag:Bag} fn main()->i64{let mut o=Outer{bag:Bag{items:vec![1,2]}};o.bag.items.clear();o.bag.items.push(42);o.bag.items[0]}",
            "fn main()->i64{let mut v=vec![1.0];v.clear();v.push(42.0);v[0] as i64}",
            "fn main()->i64{let mut v=vec![false];v.clear();v.push(true);if v.len()==1 && v[0]{42}else{0}}",
            "fn main()->i64{let mut v=Vec::<i64>::new();let mut i=0;while i<10{v.push(i);i+=1;}v.clear();v.extend([20,22]);v[0]+v[1]}",
            "fn clear(v:Vec<i64>)->i64{42} fn main()->i64{let v=vec![1];v.clear()}",
            "fn clear(flag:bool)->i64{0} fn main()->i64{let mut v=vec![1];v.clear();v.push(42);v[0]}",
            "struct Bag{answer:i64} impl Bag{fn clear(&self)->i64{self.answer}} fn clear(flag:bool)->i64{0} fn main()->i64{let bag=Bag{answer:42};bag.clear()}",
            "fn main()->i64{let mut items=vec![20];items.extend(vec![22]);items[0]+items[1]}",
            "mod Vec{pub fn with_capacity(value:i64)->i64{value}} fn main()->i64{Vec::with_capacity(42)}",
            "struct Reader{value:i64} impl Reader{fn take(&self,items:Vec<i64>)->i64{if items.capacity()>=10{self.value}else{0}}} fn main()->i64{let reader=Reader{value:42};reader.take(Vec::with_capacity(10))}",
            "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:Vec::new()};bag.items=Vec::with_capacity(10);if bag.items.capacity()>=10{42}else{0}}",
            "fn main()->i64{let items:Vec<i64>=Vec::with_capacity(10).clone();if items.capacity()==0 && items.len()==0{42}else{0}}",
            "fn main()->i64{let mut items:Vec<i64>=Vec::with_capacity(10);let before=items.len();items.push(42);if before==0 && items.capacity()>=10{items[0]}else{0}}",
            "fn main()->i64{let mut items=Vec::with_capacity(2);items.push(20);items.push(22);if items.capacity()>=2{items[0]+items[1]}else{0}}",
            "fn main()->i64{let mut items=std::vec::Vec::<f64>::with_capacity(3);items.push(42.0);items[0] as i64}",
            "fn main()->i64{let mut items=alloc::vec::Vec::<_>::with_capacity(3);items.push(true);if items[0]{42}else{0}}",
            "fn source()->Vec<i64>{Vec::with_capacity(10)} fn main()->i64{let mut items=source();items.push(42);if items.capacity()>=10{items[0]}else{0}}",
            "fn take(items:Vec<i64>)->i64{if items.len()==0 && items.capacity()>=10{42}else{0}} fn main()->i64{take(Vec::with_capacity(10))}",
            "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:Vec::with_capacity(10)};bag.items.push(42);if bag.items.capacity()>=10{bag.items[0]}else{0}}",
            "fn main()->i64{let make=||->Vec<i64>{Vec::with_capacity(10)};let mut items=make();items.push(42);if items.capacity()>=10{items[0]}else{0}}",
            "static mut CALLS:i64=0;fn count()->i64{unsafe{CALLS+=1;}10} fn main()->i64{let mut items=Vec::<i64>::with_capacity(count());items.push(41);unsafe{items[0]+CALLS}}",
            "fn main()->i64{let mut items=Vec::<i64>::with_capacity(0);if items.capacity()!=0{return 0;}items.push(20);items.push(22);items[0]+items[1]}",
            "fn main()->i64{let mut items=Vec::new();items.push(42);if items.capacity()>items.len(){42}else{0}}",
            "fn main()->i64{let mut items:Vec<i64>=Vec::new();let before=items.capacity();items.extend(vec![1,2,3,4,5,6,7,8,9,10]);if before==0 && items.capacity()>=items.len(){42}else{0}}",
            "struct Bag{items:Vec<i64>} fn main()->i64{let bag=Bag{items:vec![20,22]};if bag.items.capacity()>=2{42}else{0}}",
            "static mut CALLS:i64=0;fn source()->Vec<i64>{unsafe{CALLS+=1;}vec![42]} fn main()->i64{let capacity=source().capacity();unsafe{if capacity>=1 && CALLS==1{42}else{0}}}",
            "fn main()->i64{let items=vec![42];if items.clone().capacity()>=1{42}else{0}}",
            "fn capacity(flag:bool)->i64{0} fn main()->i64{let items=vec![42];if items.capacity()>=1{42}else{0}}",
            "fn capacity(items:Vec<i64>)->i64{42} fn main()->i64{let items=vec![1];items.capacity()}",
            "fn main()->i64{let mut items=vec![20,22];items.extend(vec![]);items[0]+items[1]}",
            "fn main()->i64{let mut items=vec![20.0,22.0];items.extend(vec![]);(items[0]+items[1]) as i64}",
            "fn main()->i64{let mut items=vec![true];items.extend([]);if items[0]{42}else{0}}",
            "fn main()->i64{let mut items=vec![20];let source=[22];items.extend(source);items[0]+items[1]}",
            "fn source()->Vec<i64>{vec![20,22]} fn main()->i64{let mut items:Vec<i64>=Vec::new();items.extend(source());items[0]+items[1]}",
            "struct Bag{items:Vec<i64>} fn main()->i64{let mut bag=Bag{items:vec![20]};bag.items.extend(vec![22]);bag.items[0]+bag.items[1]}",
            "fn main()->i64{let mut items=Vec::new();items.extend(vec![20,22]);items[0]+items[1]}",
            "fn main()->i64{let mut items=vec![true];items.extend(vec![false]);if items[0] && !items[1]{42}else{0}}",
            "fn main()->i64{let mut items=vec![20.0];items.extend(vec![22.0]);(items[0]+items[1]) as i64}",
            "fn main()->i64 { let pair=(20,22.0); pair.0+pair.1 as i64 }",
            "fn main()->i64 { let pair=(42,true); if pair.1 {pair.0} else {0} }",
            "fn make()->(i64,f64) { (20,22.0) } fn main()->i64 { let pair=make(); pair.0+pair.1 as i64 }",
            "fn take(pair:(i64,f64))->i64 {pair.0+pair.1 as i64} fn main()->i64 {take((20,22.0))}",
            "fn main()->i64 { let make=|| (20,22.0); let pair=make(); pair.0+pair.1 as i64 }",
            "fn main()->i64 { let make=|| ->(i64,f64) {(20,22.0)}; let pair=make(); pair.0+pair.1 as i64 }",
            "fn main()->i64 { let pair=(20,22.0); match pair {(a,b)=>{return a+b as i64;}} return 0; }",
            "fn main()->i64 { let pair=(1,20.0,22); match pair {(..,a,b)=>{return a as i64+b;}} return 0; }",
            "fn main()->i64 { let pair=(42,); pair.0 }",
            "struct Vec {value:i64} impl Vec {fn new()->i64 {42}} fn main()->i64 {Vec::new()}",
            "struct Bag {items:Vec<i64>} fn main()->i64 {let mut bag=Bag{items:Vec::new()};bag.items.push(20);bag.items.push(22);bag.items[0]+bag.items[1]}",
            "struct Bag {items:Vec<i64>} struct Outer {bag:Bag} fn main()->i64 {let mut outer=Outer{bag:Bag{items:Vec::new()}};outer.bag.items.push(20);outer.bag.items.push(22);outer.bag.items[0]+outer.bag.items[1]}",
            "struct Bag {items:Vec<i64>} impl Bag {fn fill(&mut self) {self.items.push(20);self.items.push(22);}} fn main()->i64 {let mut bag=Bag{items:Vec::new()};bag.fill();bag.items[0]+bag.items[1]}",
            "struct Bag {items:Vec<i64>} struct Outer {id:i64,bag:Bag} impl Outer {fn fill(&mut self) {let mut index=0;while index<10 {self.bag.items.push(index);index+=1;}}} fn main()->i64 {let mut outer=Outer{id:15,bag:Bag{items:Vec::new()}};outer.fill();outer.bag.items.len()+outer.bag.items[8]+outer.bag.items[9]+outer.id}",
            "struct Bag {items:Vec<f64>} fn main()->i64 {let mut bag=Bag{items:Vec::new()};bag.items.push(20.0);bag.items.push(22.0);(bag.items[0]+bag.items[1]) as i64}",
            "struct Bag {items:Vec<bool>} fn main()->i64 {let mut bag=Bag{items:Vec::new()};bag.items.push(true);bag.items.push(false);if bag.items[0] && !bag.items[1] {42} else {0}}",
            "static mut CALLS:i64=0; fn next()->i64 {unsafe{CALLS+=1;}22} struct Bag {items:Vec<i64>} fn main()->i64 {let mut bag=Bag{items:Vec::new()};bag.items.push(20);bag.items.push(next());unsafe{bag.items[0]+bag.items[1]+CALLS-1}}",
            "fn main()->i64 {let mut items:Vec<i64>=Vec::new();items.push(20);items.push(22);items[0]+items[1]}",
            "fn main()->i64 {let mut items=Vec::new();items.push(20);items.push(22);items[0]+items[1]}",
            "fn push(items:Vec<bool>,value:i64)->i64 {value+41} fn main()->i64 {let items=Vec::new();items.push(1)}",
            "fn main()->i64 {let mut items=vec![];items.push(20.0);items.push(22.0);(items[0]+items[1]) as i64}",
            "fn main()->i64 {let mut items=Vec::new();if true {items.push(20);} else {items.push(0);} items.push(22);items[0]+items[1]}",
            "fn main()->i64 {let mut items=Vec::new();if true {let mut items=Vec::new();items.push(true);if !items[0] {return 0;}} items.push(20);items.push(22);items[0]+items[1]}",
            "fn take(items:Vec<i64>)->i64 {items.len()+42} fn main()->i64 {let items=Vec::new();take(items)}",
            "fn values()->Vec<i64> {let items=Vec::new();items} fn main()->i64 {values().len()+42}",
            "fn main()->i64 {let values=||->Vec<i64> {let items=Vec::new();items};values().len()+42}",
            "fn main()->i64 {let mut items=Vec::new();items=vec![20,22];items[0]+items[1]}",
            "fn main()->i64 {let mut first=Vec::new();let mut second=Vec::new();first.push(20);second.push(first[0]);second[0]+22}",
            "fn main()->i64 {let mut items:Vec<i64>=Vec::new();let mut index=0;while index<10 {items.push(index);index+=1;} items.len()+items[8]+items[9]+15}",
            "fn main()->i64 {let mut items:Vec<f64>=Vec::new();items.push(20.0);items.push(22.0);(items[0]+items[1]) as i64}",
            "fn main()->i64 {let mut items:Vec<bool>=Vec::new();items.push(true);items.push(false);if items[0] && !items[1] {42} else {0}}",
            "static mut CALLS:i64=0; fn next()->i64 {unsafe{CALLS+=1;} 22} fn main()->i64 {let mut items:Vec<i64>=Vec::new();items.push(20);items.push(next());unsafe{items[0]+items[1]+CALLS-1}}",
            "fn main()->i64 {let items:Vec<i64>=Vec::new();if items.is_empty() {42} else {0}}",
            "fn values()->Vec<i64> {Vec::new()} fn main()->i64 {if values().is_empty() {42} else {0}}",
            "fn main()->i64 {let mut items:Vec<i64>=std::vec::Vec::new();items.push(42);items[0]}",
            "fn main()->i64 {let items=vec![20,22];items[0]+items[1]}",
            "fn take(prefix:i64,items:Vec<i64>)->i64 {prefix+items[0]+items[1]} fn main()->i64 {let items=vec![20,21];take(1,items.clone())}",
            "fn main()->i64 {let items=vec![42];items.clone()[0]}",
            "fn main()->i64 {let items:Vec<_>=vec![20,22];items[0]+items[1]}",
            "fn main()->i64 {let items=vec![20.0,22.0];(items[0]+items[1]) as i64}",
            "fn main()->i64 {let items=vec![true,false];if items[0] && !items[1] {42} else {0}}",
            "fn main()->i64 {let items=vec![20,22];items.clone().len()+40}",
            "fn values()->Vec<i64> {vec![20,22]} fn main()->i64 {let items:Vec<_>=values();items[0]+items[1]}",
            "fn len(items:Vec<i64>)->i64 {42} fn main()->i64 {vec![0].len()}",
            "fn len(flag:bool)->i64 {if flag {7} else {0}} fn main()->i64 {let items:Vec<i64>=vec![20,22];items.len()+40}",
            "fn len(items:Vec<i64>,extra:i64)->i64 {extra} fn main()->i64 {let items:Vec<i64>=vec![20,22];items.len()+40}",
            "fn len(text:String)->i64 {0} fn main()->i64 {let items:[i64;2]=[20,22];items.len()+40}",
            "fn len(items:Vec<i64>)->i64 {0} fn main()->i64 {let text:String=\"hi\";text.len()+40}",
            "fn clone(flag:bool)->Vec<i64> {vec![0]} fn main()->i64 {let items:Vec<i64>=vec![20,22];let copied:Vec<i64>=items.clone();copied[0]+copied[1]}",
            "fn is_empty(flag:bool)->bool {false} fn main()->i64 {let items:Vec<i64>=vec![];if items.is_empty() {42} else {0}}",
            "struct Bucket {first:i64,value:i64} impl Bucket {fn answer(&self)->i64 {self.first+self.value}} fn main()->i64 {let pair=(1,Bucket{first:20,value:22});pair.1.answer()}",
            "struct Bucket {value:i64} impl Bucket {fn set(&mut self) {self.value=42;}} fn main()->i64 {let mut pair=(1,Bucket{value:0});pair.1.set();pair.1.value}",
            "struct Bucket {value:i64} impl Bucket {fn set(&mut self) {self.value=42;}} fn main()->i64 {let mut pair=(1,Bucket{value:0});Bucket::set(&mut pair.1);pair.1.value}",
            "struct Bucket {value:i64} impl Bucket {fn len(&self)->i64 {self.value}} fn len(_bucket:Bucket)->i64 {0} fn main()->i64 {let bucket=Bucket{value:40};bucket.len()+2}",
            "struct Bucket {value:i64} impl Bucket {fn collect(&self)->i64 {self.value}} fn main()->i64 {let bucket=Bucket{value:40};bucket.collect()+2}",
            "struct Bucket {value:i64} impl Bucket {fn answer(&self)->i64 {0}} fn answer(bucket:Bucket)->i64 {bucket.value} fn main()->i64 {answer(Bucket{value:40})+2}",
            "struct Bucket {value:i64} impl Bucket {fn answer(&self)->i64 {self.value}} fn answer(_bucket:Bucket)->i64 {0} fn main()->i64 {let(bucket,extra)=(Bucket{value:40},2);bucket.answer()+extra}",
            "struct Bucket {value:i64} impl Bucket {fn answer(&self)->i64 {self.value}} fn main()->i64 {let f=|bucket|bucket.answer()+2;f(Bucket{value:40})}",
            "struct Bucket {value:i64} impl Bucket {fn answer(&self)->i64 {self.value}} fn main()->i64 {let pair=(Bucket{value:40},2);pair.0.answer()+pair.1}",
            "fn main()->i64 { let pair=(20,22); match pair {(a,b)=>a+b} }",
            "fn main()->i64 { let f=|| {let pair=(20,22.0);match pair {(a,b)=>a+b as i64}};f() }",
            "fn main()->i64 { let pair=(1,(20,21));match pair {(a,(b,c))=>a+b+c} }",
            "fn main()->i64 { let pair=(true,20,22);match pair {(..,a,b)=>a+b} }",
            "fn main()->i64 { let f=|x| (x,true); let pair=f(42); if pair.1 {pair.0} else {0} }",
            "struct Inner { first:i64,answer:i64 } struct Outer { inner:Inner } fn main()->i64 { let outer=Outer{inner:Inner{first:7,answer:42}}; outer.inner.answer }",
            "struct Inner { x:i64 } struct Outer { inner:Inner } fn main()->i64 { let mut source=Outer{inner:Inner{x:42}}; let copied=source; source=Outer{inner:Inner{x:0}}; copied.inner.x }",
            "fn take(pair:((i64,i64),bool))->i64 {pair.0.0+pair.0.1} fn main()->i64 {take(((20,22),true))}",
            "fn main()->i64 { let pair=(1,(20,22)); match pair {(_,(a,b))=>{return a+b;}} return 0; }",
            "static mut CALLS:i64=0; fn next()->i64 {unsafe{CALLS+=1;CALLS}} fn make()->(i64,i64) {(next(),next())} fn main()->i64 {let pair=make(); unsafe{pair.0*10+pair.1+CALLS+28}}",
            "fn main()->i64 { let pair=((20,22),true); pair.0.0+pair.0.1 }",
            "fn make()->((i64,i64),bool) { ((20,22),true) } fn main()->i64 { let pair=make(); pair.0.0+pair.0.1 }",
            "fn main()->i64 { let pair=(\"abc\",42); pair.1 }",
            "fn main()->i64 { let pair=(20,22.0); let alias=pair; alias.0+alias.1 as i64 }",
            "static mut CALLS:i64=0; fn next()->i64 {unsafe{CALLS+=1;CALLS}} fn main()->i64 { let pair=(next(),next()); unsafe {pair.0*10+pair.1+CALLS+28} }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn inline_constants_execute_only_folded_values() {
        for source in [
            "fn main()->i64{let value=const {let base=20;base+22};value}",
            "fn main()->i64{let value=const{let mut x=20;let y={x+=1;x};x+y};value}",
            "fn main()->i64{let value=const {let mut sum=0;let mut i=0;while i<7{sum+=i;i+=1;}sum*2};value}",
            "const BASE:i64=20;fn main()->i64{let value=const{BASE+22};value}",
            "fn main()->i64{let value=const{if true{42}else{0}};value}",
            "fn main()->i64{let value=const{let a=[20,22];a[0]+a[1]};value}",
            "struct Pair{x:i64,y:i64}fn main()->i64{let value=const{Pair{x:20,y:22}};value.x+value.y}",
            "fn main()->i64{let value=const{(const{20})+22};value}",
            "fn main()->i64{let value=const{let yes=true;yes && true};if value{42}else{0}}",
            "fn main()->i64{let value=const{\"answer\"};if value==\"answer\"{42}else{0}}",
            "fn main()->i64{let value=const{let mut a=[20,0];a[1]=22;a};value[0]+value[1]}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn optimized_indirect_calls_preserve_addresses_arguments_and_errors() {
        for source in [
            "fn add(a:Int,b:Int)->Int{return a+b;}\nfn pointer()->Int{if true{return add;}return add;}\nfn main()->Int{return invoke2(pointer(),20,22);}",
            "fn fail(value:Int)->Int{throw value;return 0;}\nfn bridge()->Int{let pointer:Int=fail;return invoke1(pointer,7);}\nfn main()->Int{try {bridge();return 0;} catch 7 {return 42;} return 0;}",
        ] {
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
                crate::lower_core::desugar_module(&mut module);
                crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn declared_symbol_names_are_not_replaced_by_fabricated_values() {
        for name in ["bad value", "bad..value", "данные", "正常"] {
            use crate::core_ir::{Decl, Expr, Stmt, Typ, UnifiedModule};
            let module = UnifiedModule::new(vec![
                Decl::Function {
                    name: "take".into(),
                    params: vec![(name.into(), Typ::Int)],
                    ret: Typ::Int,
                    type_params: vec![],
                    body: vec![Stmt::If {
                        cond: Expr::Binary {
                            op: "==".into(),
                            lhs: Box::new(Expr::Ident(name.into())),
                            rhs: Box::new(Expr::IntLit(42)),
                        },
                        then_body: vec![Stmt::Return(Some(Expr::Ident(name.into())))],
                        else_body: vec![Stmt::Return(Some(Expr::IntLit(0)))],
                    }],
                },
                Decl::Function {
                    name: "main".into(),
                    params: vec![],
                    ret: Typ::Int,
                    type_params: vec![],
                    body: vec![Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident("take".into())),
                        args: vec![Expr::IntLit(42)],
                    }))],
                },
            ]);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{name} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn unit_values_preserve_types_effects_parameters_and_returns() {
        for source in [
            "fn main()->i64{let x=match (){()=>42};x}",
            "fn main()->i64{match (){()=>42}}",
            "fn main()->i64{let mut x=0;match (){()=>{x=42;}}x}",
            "static mut CALLS:i64=0;fn unit(){unsafe{CALLS+=1;}}fn main()->i64{match unit(){()=>unsafe{CALLS+41}}}",
            "fn unit()->(){()}fn main()->i64{let x=match unit(){()=>42};x}",
            "fn main()->i64{let u:()=if true{()}else{()};let x=match u{()=>42};x}",
            "fn take(unit:())->i64{let alias=unit;match alias{()=>42}}fn main()->i64{take(())}",
            "fn take(a:i64,unit:(),b:i64)->i64{match unit{()=>a+b}}fn main()->i64{take(20,(),22)}",
            "static mut CALLS:i64=0;fn touch()->(){unsafe{CALLS+=1;}return ();}fn main()->i64{let value=touch();match value{()=>unsafe{CALLS+41}}}",
            "static mut CALLS:i64=0;fn touch(){unsafe{CALLS+=1;}}fn relay(){return touch();}fn main()->i64{relay();unsafe{CALLS+41}}",
            "static mut CALLS:i64=0;fn touch(){unsafe{CALLS+=1;}}fn take(a:i64,unit:(),b:i64)->i64{match unit{()=>a+b}}fn main()->i64{take(20,touch(),unsafe{CALLS+21})}",
            "fn main()->i64{let x=match (){u if u==()=>42,_=>0};x}",
            "fn main()->i64{if ()==(){42}else{0}}",
            "fn main()->i64{let x=const{()};match x{()=>42}}",
            "fn main()->i64{let x=const{let y=42;};match x{()=>42}}",
            "fn choose(flag:bool)->(){if flag{return ();}()}fn main()->i64{let x=choose(true);match x{()=>42}}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn match_values_preserve_selected_effects_bindings_and_control_flow() {
        for source in [
            "fn main()->i64{let tag=1;let value=match tag{0=>20,1=>42,_=>0};value}",
            "fn main()->i64{let x=match 1{n if {return 42;}=>0,_=>0};x}",
            "fn main()->i64{let x=match true{true=>42,false=>0};x}",
            "static mut CALLS:i64=0;fn source()->i64{unsafe{CALLS+=1;}2}fn main()->i64{let x=match source(){0=>0,n if n==2=>41,_=>0};unsafe{x+CALLS}}",
            "fn main()->i64{let mut x=20;let y=match 1{0=>{x=0;0},_=>{x+=1;x}};x+y}",
            "fn main()->i64{let x=20;let y=match 22{x=>x};x+y}",
            "fn main()->i64{let x=match 0{0=>return 42,_=>0};x}",
            "fn main()->i64{let x:i64=match true{true=>return 42,false=>return 0};x}",
            "fn main()->i64{let mut v=match 1{0=>vec![0],_=>vec![20]};v.push(22);v[0]+v[1]}",
            "fn take(prefix:i64,v:Vec<i64>)->i64{prefix+v[0]+v[1]}fn main()->i64{take(1,match 1{0=>vec![0],_=>vec![20,21]})}",
            "struct Pair{x:i64,y:i64}fn main()->i64{let p=match false{true=>Pair{x:0,y:0},false=>Pair{x:20,y:22}};p.x+p.y}",
            "fn main()->i64{let p=match 1{0=>(0,0),_=>(20,22)};p.0+p.1}",
            "fn main()->i64{let x=match (20,22){(a,b) if a==20=>a+b,_=>0};x}",
            "fn main()->i64{let x=match [20,22]{[a,b] if a==20=>a+b,_=>0};x}",
            "fn main()->i64{let x=if let (20,value)=(20,42){value}else{0};x}",
            "fn main()->i64{let mut sum=0;for i in 0..3{let x=match i{1=>continue,_=>i};sum+=x;}sum+40}",
            "fn main()->i64{let mut sum=0;while sum<1{let x=match sum{0=>{sum=42;break;},_=>0};sum+=x;}sum}",
            "static mut CALLS:i64=0;fn main()->i64{let x=match 1{n if {unsafe{CALLS+=1;}false}=>0,n if n==1=>41,_=>0};unsafe{x+CALLS}}",
            "fn main()->i64{let mut values=[20,22];let x=match values{[a,b] if {values[0]=0;true}=>a+b,_=>0};x}",
            "fn main()->i64{let x=match 1{0=>0,_=>match true{true=>42,false=>0}};x}",
            "fn main()->i64{let x=match false{true=>0.0,false=>42.0};x as i64}",
            "fn main()->i64{let x=match 1{0=>\"no\",_=>\"yes\"};if x==\"yes\"{42}else{0}}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn multiline_string_bytes_survive_native_data_emission() {
        let expected = "before  \n//inside\nprotocol:\n  λ\n\nend}";
        let source = format!(
            "fn main()->Int{{let text:String=\"{expected}\";if text=={expected:?}{{return 42;}}return 0;}}"
        );
        for source in [
            &source,
            &crate::in_canonical::canonicalize_in_source(&source).unwrap(),
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{report:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn in_struct_pattern_guards_and_bindings_execute_on_native_backends() {
        for body in [
            r#"match entry { Entry {value:x,text:"a,b{c}"} if false -> {return 0;} Entry {value:x,text:"a,b{c}"} if x==20 -> {return x+22;} _ {return 0;} }"#,
            r#"return match entry { Entry {value:x,text:"a,b{c}"} if false -> {0} Entry {value:x,text:"a,b{c}"} if x==20 -> {x+22} _ {0} };"#,
        ] {
            let source = format!(
                "struct Entry {{\n Int value\n String text\n}}\nfn main()->Int{{let entry:Entry=Entry{{value:20,text:\"a,b{{c}}\"}};{body}}}"
            );
            let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{body}: {report:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut optimized = module.clone();
                crate::core_opt::optimize_with_profile(&mut optimized.decls, Some("main"), profile);
                assert_eq!(
                    invoke_module(&optimized, profile, &[]),
                    Some(42),
                    "{body}: {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn conditional_and_block_values_preserve_scope_effects_and_control_flow() {
        for source in [
            "static mut CALLS:i64=0;fn take(unit:())->i64{unsafe{CALLS+41}}fn main()->i64{take({unsafe{CALLS+=1;}})}",
            "fn main()->i64{let x=if {return 42;}{0}else{1};x}",
            "fn main()->i64{let x=if true{{return 42;}}else{0};x}",
            "static mut CALLS:i64=0;fn touch(){unsafe{CALLS+=1;}}fn main()->i64{let unit=touch();unsafe{CALLS+41}}",
            "static mut CALLS:i64=0;fn main()->i64{let mut n=0;for i in 0..3{unsafe{CALLS+=1;if CALLS>3{return 0;}}let x=if i<2{continue;}else{42};n=x;}n}",
            "fn take(prefix:i64,v:Vec<i64>)->i64{prefix+v[0]+v[1]} fn main()->i64{take(1,if false{vec![0]}else{vec![20,21]})}",
            "struct Pair{x:i64,y:i64} fn take(prefix:i64,p:Pair)->i64{prefix+p.x+p.y} fn main()->i64{take(1,if true{Pair{x:20,y:21}}else{Pair{x:0,y:0}})}",
            "fn values()->Vec<i64>{let v=if true{return Vec::with_capacity(10);}else{vec![0]};v} fn main()->i64{let mut v=values();v.push(42);if v.capacity()>=10{v[0]}else{0}}",
            "fn main()->i64{let choose=|flag:bool| if flag{let value=42;value}else{0};choose(true)}",
            "fn main()->i64{let y={let x=20;x+22};y}",
            "fn main()->i64{let value=if false{0.0}else{42.0};value as i64}",
            "fn main()->i64{let value=if false{false}else{true};if value{42}else{0}}",
            "fn main()->i64{let text=if false{\"wrong\"}else{let name=\"answer\";name};if text==\"answer\"{42}else{0}}",
            "fn main()->i64{let pair=if false{(0,0)}else{let x=20;(x,22)};pair.0+pair.1}",
            "fn main()->i64{(if true{[20,22]}else{[0,0]})[1]+20}",
            "fn main()->i64{let ignored={return 42;};0}",
            "fn main()->i64{let ignored={return 42;1}*0;0}",
            "fn main()->i64{let mut x=0;let y={x=42;7}*0;x+y}",
            "fn main()->i64{let ignored={return 42;true}&&false;0}",
            "struct Pair{x:i64,y:i64} impl Pair{fn sum(&self)->i64{self.x+self.y}} fn main()->i64{{let p=Pair{x:20,y:22};p}.sum()}",
            "fn main()->i64{let mut v=if true{vec![20]}else{vec![0]};v.push(22);v[0]+v[1]}",
            "fn main()->i64{let v:Vec<i64>=if false{vec![0]}else{vec![20,22]};v[0]+v[1]}",
            "fn main()->i64{let mut x=20;let y={x+=1;x};x+y}",
            "fn main()->i64{let x=20;let y={let x=22;x};x+y}",
            "fn main()->i64{let x=if false{0}else if true{42}else{1};x}",
            "fn take(x:i64,y:i64)->i64{x+y} fn main()->i64{let mut x=20;take({x+=1;x},{x+=1;x})-1}",
            "static mut CALLS:i64=0;fn count()->bool{unsafe{CALLS+=1;}false} fn main()->i64{let x=if count(){unsafe{CALLS+=100;}0}else{41};unsafe{x+CALLS}}",
            "fn main()->i64{let x=if true{return 42;}else{0};x}",
            "fn main()->i64{let x:i64=if true{return 42;}else{return 0;};x}",
            "fn take(v:Vec<i64>)->i64{v[0]+v[1]} fn main()->i64{take(if false{vec![0]}else{vec![20,22]})}",
            "struct Pair{x:i64,y:i64} fn main()->i64{let p=if false{Pair{x:0,y:0}}else{let x=20;Pair{x,y:22}};p.x+p.y}",
            "fn main()->i64{let mut i=0;let mut n=0;while i<3{i+=1;let x=if i<3{continue;}else{42};n=x;}n}",
            "fn main()->i64{let mut x=0;while true{let y=if true{x=42;break;}else{0};x=y;}x}",
            "fn main()->i64{let values={let x=20;[x,22]};values[0]+values[1]}",
            "fn main()->i64{let mut v:Vec<i64>=if false{Vec::with_capacity(1)}else{Vec::with_capacity(10)};v.push(42);if v.capacity()>=10{v[0]}else{0}}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn tuple_let_bindings_preserve_types_shapes_scope_and_order() {
        for source in [
            "fn main()->i64 {let (a,b)=(20,22);a+b}",
            "fn main()->i64 {let (a,b):(i64,f64)=(20,22.0);a+b as i64}",
            "fn main()->i64 {let (a,(b,c))=(1,(20,21));a+b+c}",
            "fn main()->i64 {let (a,..,b)=(20,3,true,22);a+b}",
            "fn main()->i64 {let (..,a,b)=(true,20.0,22);a as i64+b}",
            "static mut CALLS:i64=0; fn make()->(i64,i64) {unsafe{CALLS+=1;} (20,21)} fn main()->i64 {let (a,b)=make();unsafe{a+b+CALLS}}",
            "fn main()->i64 {let a=20;let (a,b)=(a,22);a+b}",
            "fn main()->i64 {let (mut a,b)=(20,21);a+=1;a+b}",
            "fn main()->i64 {let (pair,_)=((20,22),true);pair.0+pair.1}",
            "fn main()->i64 {let f=|pair| {let(a,b)=pair;a+b};f((20,22))}",
            "fn main()->i64 {let (a,)=(42,);a}",
            "fn main()->i64 {let ()=();42}",
            "static mut CALLS:i64=0; fn unit() {unsafe{CALLS=42;}} fn main()->i64 {let ()=unit();unsafe{CALLS}}",
            "struct Bucket {value:i64} impl Bucket {fn load(&self)->i64 {self.value}} fn main()->i64 {let bucket:i64=0;let(bucket,extra)=(Bucket{value:40},2);bucket.load()+extra}",
            "fn make()->Result<(i64,i64),i64> {Ok((20,22))} fn main()->Result<i64,i64> {let(a,b)=make()?;Ok(a+b)}",
            "static mut CALLS:i64=0; fn make()->Result<(i64,i64),i64> {Err(42)} fn answer()->Result<i64,i64> {let(a,b)=make()?;unsafe{CALLS=99;} Ok(a+b)} fn main()->i64 {if let Err(error)=answer() {unsafe{return error+CALLS;}} 0}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn closure_parameters_infer_monomorphic_call_types() {
        for source in [
            "fn main()->i64 { let f=|x|x+2; f(40) }",
            "fn main()->i64 { let outer=|g|g(10.5)+g(10.5); let make=|| |x|x; let id=make(); (outer(id)*2.0) as i64 }",
            "fn main()->i64 { let f=|x|x; f(42) }",
            "fn main()->i64 { let f=|x|x+x; f(21.0) as i64 }",
            "fn main()->i64 { let f=|x|x; if f(true) { return 42; } return 0; }",
            "fn main()->i64 { let f=|x|x[0]+x[1]; f([20,22]) }",
            "struct Point { value:i64 } fn main()->i64 { let f=|x|x.value; f(Point { value:42 }) }",
            "fn main()->i64 { let f=|x|x; let alias=f; alias(42) }",
            "fn main()->i64 { let factory=|| |x|x; let f=factory(); f(42) }",
            "fn main()->i64 { let f=|x| { let inner=|y|y+2; inner(x) }; f(40) }",
            "fn main()->i64 { let id=|x|x; let f=|x|x; f(id(42)) }",
            "fn main()->i64 { let id=|x|x; let f=|x| { let inner=|y|y; inner(x+x) }; f(id(21.0)) as i64 }",
            "fn main()->i64 { let id=|x|x; let f=|x| { let sum=x+x; let inner=|y|y; inner(sum) }; f(id(21.0)) as i64 }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source}: {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn local_stateless_closures_execute_through_generated_functions() {
        for source in [
            "fn main()->i64 { let answer=||42; answer() }",
            "fn helper()->i64 { 42 } fn main()->i64 { let answer=||helper(); answer() }",
            "static VALUE:i64=42; fn main()->i64 { let answer=||VALUE; answer() }",
            "fn main()->i64 { let answer=||{ let inner=||42; inner() }; answer() }",
            "fn main()->i64 { let factory=|| ||42; let answer=factory(); answer() }",
            "fn main()->i64 { let answer=||{ if true { 42 } else { 7 } }; answer() }",
            "fn main()->i64 { let answer=||{ let mut x=0; while x<42 { x+=1; } x }; answer() }",
            "fn main()->i64 { let answer=||20.5; (answer() as i64)+22 }",
            "mod alpha { pub struct Point { pub value:i64 } } fn main()->i64 { let answer=|point:alpha::Point|point.value; answer(alpha::Point { value:42 }) }",
            "fn answer()->i64 { 7 } fn main()->i64 { let answer=||42; answer() }",
            "fn main()->i64 { (||42)() }",
            "fn main()->i64 { let answer=||{}; answer(); return 42; }",
            "fn main()->i64 { let answer=||\"ok\"; if answer()==\"ok\" { return 42; } return 0; }",
            "fn main()->i64 { let answer=||40; if true { let answer=||0; answer(); } answer()+2 }",
            "static mut COUNT:i64=0; fn next()->i64 { unsafe { COUNT+=1; return COUNT; } } fn main()->i64 { let pair=|x:i64,y:i64| -> i64 { x*10+y }; pair(next(),next())+30 }",
            "fn main()->i64 { let answer=|x:i64| -> i64 { x+2 }; answer(40) }",
            "fn main()->i64 { let answer=||42; let alias=answer; alias() }",
            "fn __closure_0()->i64 { 7 } struct __closure_1_captures { value:i64 } fn main()->i64 { let answer=||42; answer() }",
            "fn main()->i64 { let answer=||true; if answer() { return 42; } return 0; }",
            "fn main()->i64 { let answer=||20.5; (answer()+21.5) as i64 }",
            "fn main()->i64 { let answer=||[20,22]; let items=answer(); items[0]+items[1] }",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let report = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(report.ok, "{source}: {report:?}; module={module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn qualified_struct_paths_execute_through_native_parameters() {
        let parsed = crate::compiler::rust_front::parse_rust_source("mod alpha { pub struct Point { pub value:i64 } } fn answer(point:alpha::Point)->i64 { point.value } fn main()->i64 { answer(alpha::Point { value:42 }) }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn known_struct_field_reads_execute_actual_values() {
        let parsed = crate::compiler::rust_front::parse_rust_source("struct Point { first:i64, answer:i64 } fn main()->i64 { let point:Point=Point { first:7, answer:42 }; return point.answer; }").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            assert_eq!(invoke_module(&module, profile, &[]), Some(42));
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn standard_atomic_static_cells_execute_real_load_store_and_add() {
        for source in [
            "use std::sync::atomic::{AtomicU32,Ordering}; static VALUE:AtomicU32=AtomicU32::new(1); fn main()->i64 { let old=VALUE.fetch_add(41,Ordering::Relaxed); return old+VALUE.load(Ordering::SeqCst)-1; }",
            "use std::sync::atomic::{AtomicU64,Ordering}; static VALUE:AtomicU64=AtomicU64::new(0); fn main()->i64 { VALUE.store(42,Ordering::Release); return VALUE.load(Ordering::Acquire); }",
            "use std::sync::atomic::{AtomicUsize as Word,Ordering::SeqCst}; static VALUE:Word=Word::new(20); fn main()->i64 { return VALUE.fetch_add(22,SeqCst)+VALUE.load(SeqCst)-20; }",
            "use std::sync::atomic::{AtomicU32,Ordering}; static VALUE:AtomicU32=AtomicU32::new(4294967295); fn main()->i64 { let old=VALUE.fetch_add(1,Ordering::SeqCst); if old==4294967295 && VALUE.load(Ordering::Relaxed)==0 {return 42;} return 0; }",
            "use std::sync::atomic as atom; static VALUE:atom::AtomicU32=atom::AtomicU32::new(20); fn main()->i64 {return atom::AtomicU32::fetch_add(&VALUE,22,atom::Ordering::SeqCst)+VALUE.load(atom::Ordering::Relaxed)-20;}",
            "struct Bucket { value:i64 } impl Bucket {fn load(&self)->i64{return self.value;}} fn main()->i64{let bucket=Bucket{value:42};return bucket.load();}",
            "use std::sync::atomic::Ordering::*; static VALUE:std::sync::atomic::AtomicU32=std::sync::atomic::AtomicU32::new(20); fn main()->i64{return VALUE.fetch_add(22,SeqCst)+VALUE.load(Relaxed)-20;}",
            "use std::sync::atomic::{self,Ordering}; static VALUE:atomic::AtomicU32=atomic::AtomicU32::new(20); fn main()->i64{return VALUE.fetch_add(22,Ordering::SeqCst)+VALUE.load(Ordering::Relaxed)-20;}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            let verified = crate::core_ir_verifier::verify_for_entry(&module, "main");
            assert!(verified.ok, "{source}: {verified:?}: {module:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(invoke_module(&module, profile, &[]), Some(42), "{source}");
            }
        }
    }

    #[test]
    fn atomic_invalid_orders_and_unproven_receivers_are_refused() {
        for body in [
            "return VALUE.load(Ordering::Release);",
            "VALUE.store(42,Ordering::Acquire); return 42;",
            "let alias=&VALUE; return alias.load(Ordering::Relaxed);",
        ] {
            let parsed=crate::compiler::rust_front::parse_rust_source(&format!("use std::sync::atomic::{{AtomicU32,Ordering}}; static VALUE:AtomicU32=AtomicU32::new(1); fn main()->i64 {{{body}}}")).unwrap();
            let mut module =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut module);
            assert!(
                crate::native_emit::lower::lower_module(
                    &module,
                    "main",
                    crate::native_emit::NativeLinkage::Executable
                )
                .is_err(),
                "{body}"
            );
            assert!(
                crate::native_emit::x86_64_lower::lower_module(&module, "main").is_err(),
                "{body}"
            );
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn atomic_fetch_add_has_no_lost_updates_under_contention() {
        use std::sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        };
        let parsed=crate::compiler::rust_front::parse_rust_source("use std::sync::atomic::{AtomicU32,Ordering}; static VALUE:AtomicU32=AtomicU32::new(0); fn main()->i64 {return VALUE.fetch_add(1,Ordering::SeqCst);}").unwrap();
        let mut module =
            crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
        crate::lower_core::desugar_module(&mut module);
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let _profile = crate::native_emit::antidecomp::ProfileGuard::new(profile);
            #[cfg(target_arch = "aarch64")]
            let lowered = crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable,
            )
            .unwrap();
            #[cfg(target_arch = "x86_64")]
            let lowered = crate::native_emit::x86_64_lower::lower_module(&module, "main").unwrap();
            let cell = Arc::new(AtomicU32::new(0));
            let address = cell.as_ptr() as u64;
            let mut code = lowered.code.clone();
            for &(site, offset) in &lowered.global_relocs {
                assert_eq!(offset, 0);
                code[site as usize..site as usize + 8].copy_from_slice(&address.to_le_bytes());
            }
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
                .filter(|site| {
                    !lowered
                        .global_relocs
                        .iter()
                        .any(|(global, _)| global == *site)
                })
                .map(|site| (*site, lowered.codegen_base))
                .collect::<Vec<_>>();
            let workers = (0..4)
                .map(|_| {
                    let cell = cell.clone();
                    let code = code.clone();
                    let offsets = offsets.clone();
                    let relocations = relocations.clone();
                    std::thread::spawn(move || {
                        let mut rt = JitRuntime::new();
                        rt.load(&code, &offsets, &relocations).unwrap();
                        let mut sum = 0;
                        for _ in 0..1000 {
                            sum += unsafe { rt.invoke("main", &[]).unwrap() };
                        }
                        drop(rt);
                        drop(cell);
                        sum
                    })
                })
                .collect::<Vec<_>>();
            let sum = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .sum::<i64>();
            assert_eq!(cell.load(Ordering::SeqCst), 4000);
            assert_eq!(sum, 3999 * 4000 / 2);
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn reference_return_checked_failure_does_not_escape_as_a_pointer() {
        if std::env::var_os("IN_ARRAY_RETURN_BOUNDS_CHILD").is_some() {
            for (body, consumer) in [
                (
                    "let items = [20]; let value = items[position]; return [value];",
                    "return values()[0];",
                ),
                (
                    "let items = [20]; let value = items[position]; return [value];",
                    "return array_len(values());",
                ),
                (
                    "let items = [20]; items[position] = 22; return items;",
                    "let items = values(); items[0] = 42; return items[0];",
                ),
                (
                    "let value = 20 / (position - 5); return [value];",
                    "return values()[0];",
                ),
                (
                    "let value = 20 % (position - 5); return [value];",
                    "return array_len(values());",
                ),
            ] {
                let source = format!(
                    "var position: Int = 5;\nfn values() -> [Int] {{ {body} }}\nfn main() -> Int {{ {consumer} }}"
                );
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                crate::lower_core::desugar_module(&mut module);
                assert!(
                    crate::core_ir_verifier::verify_for_entry(&module, "main").ok,
                    "{source}"
                );
                for profile in [
                    crate::emit_profile::EmitProfile::Default,
                    crate::emit_profile::EmitProfile::Lean,
                    crate::emit_profile::EmitProfile::Harden,
                ] {
                    let rt = runtime_for_module(&module, profile);
                    assert_eq!(unsafe { rt.invoke("main", &[]) }, Some(1), "{source}");
                    assert_eq!(rt.invocation_error(), Some(1), "{source}");
                }
            }
            for body in [
                "let items = [20]; let value = items[position]; if value == 20 { return \"hi\"; } return \"no\";",
                "let value = 20 / (position - 5); if value == 20 { return \"hi\"; } return \"no\";",
            ] {
                for position in [0, 5] {
                    for caught in [false, true] {
                        let consumer = if caught {
                            "try { return str_len(text()); } catch 1 { return 42; } return 0;"
                        } else {
                            "return str_len(text());"
                        };
                        let source = format!(
                            "var position: Int = {position};\nfn text() -> String {{ {body} }}\nfn main() -> Int {{ {consumer} }}"
                        );
                        let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                        crate::lower_core::desugar_module(&mut module);
                        assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
                        for profile in [
                            crate::emit_profile::EmitProfile::Default,
                            crate::emit_profile::EmitProfile::Lean,
                            crate::emit_profile::EmitProfile::Harden,
                        ] {
                            let rt = runtime_for_module(&module, profile);
                            let expected = if position == 0 {
                                2
                            } else if caught {
                                42
                            } else {
                                1
                            };
                            assert_eq!(
                                unsafe { rt.invoke("main", &[]) },
                                Some(expected),
                                "{source}"
                            );
                            assert_eq!(
                                rt.invocation_error(),
                                (position == 5 && !caught).then_some(1),
                                "{source}"
                            );
                            if position == 5 {
                                assert_eq!(unsafe { rt.invoke("text", &[]) }, Some(0), "{source}");
                                assert_eq!(rt.invocation_error(), Some(1), "{source}");
                            }
                        }
                    }
                }
            }
            for position in [0, 5] {
                let source = format!(
                    "var position: Int = {position};\nfn values() -> [Int] {{ let items = [42]; return [items[position]]; }}\nfn relay() -> [Int] {{ return values(); }}\nfn main() -> Int {{ try {{ return relay()[0]; }} catch 1 {{ return 42; }} return 0; }}"
                );
                let mut module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                crate::lower_core::desugar_module(&mut module);
                for profile in [
                    crate::emit_profile::EmitProfile::Default,
                    crate::emit_profile::EmitProfile::Lean,
                    crate::emit_profile::EmitProfile::Harden,
                ] {
                    let rt = runtime_for_module(&module, profile);
                    assert_eq!(unsafe { rt.invoke("main", &[]) }, Some(42), "{source}");
                    assert_eq!(rt.invocation_error(), None, "{source}");
                }
            }
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(
                "jit_runtime::tests::reference_return_checked_failure_does_not_escape_as_a_pointer",
            )
            .arg("--nocapture")
            .env("IN_ARRAY_RETURN_BOUNDS_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "status: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn scalar_vector_literals_preserve_call_and_return_values() {
        for source in [
            "fn values() -> Vec<i64> { return vec![20, 22]; } fn main() -> i64 { let items: Vec<i64> = values(); let mut sum = 0; for value in items { sum += value; } return sum; }",
            "fn accept(items: Vec<i64>) -> i64 { return items[0] + items[1]; } fn main() -> i64 { return accept(vec![20, 22]); }",
            "fn accept(items: Vec<i64>) -> i64 { return items[0] + items[1]; } fn main() -> i64 { return accept(vec![accept(vec![20, 22]), 0]); }",
        ] {
            let mut module = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            assert!(crate::core_ir_verifier::verify_for_entry(&module, "main").ok);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn vector_receiver_index_uses_live_length() {
        for (body, main_body) in [
            (
                "self.items.len = 0; return 0;",
                "Bag::answer(&mut bag); return bag.items[0];",
            ),
            (
                "self.items.len = 0; return self.items[0];",
                "return Bag::answer(&mut bag);",
            ),
            (
                "self.items.len = 1; return self.items.len;",
                "return Bag::answer(&mut bag) + bag.items.len;",
            ),
        ] {
            let source = format!(
                "struct Bag {{ items: Vec<i64> }} impl Bag {{ fn answer(&mut self) -> i64 {{ {body} }} }} fn main() -> i64 {{ let mut bag = Bag {{ items: vec![20, 22] }}; {main_body} }}"
            );
            let mut module = crate::compiler::rust_front::parse_rust_source(&source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let expected = if body.contains("return self.items.len") {
                    2
                } else {
                    1
                };
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(expected),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, target_arch = "aarch64"))]
    fn nested_receiver_mutation_preserves_addresses_and_value_copies() {
        for source in [
            "class Inner { x: Int; y: Int; }\nclass Wrapper { inner: Inner; fn replace() -> Int { inner = Inner { x: 20, y: 22 }; return inner.x + inner.y; } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { x: 0, y: 0 } }; wrapper.replace(); return wrapper.inner.x + wrapper.inner.y; }",
            "fn fail() -> Int { throw 9; }\nclass Inner { x: Int; y: Int; }\nclass Wrapper { inner: Inner; fn replace() -> Int { inner = Inner { x: 0, y: fail() }; return 0; } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { x: 20, y: 22 } }; try { wrapper.replace(); } catch 9 { return wrapper.inner.x + wrapper.inner.y; } return 0; }",
            "class Inner { x: Int; y: Int; fn set(value: Int) -> Int { x = value; return x; } }\nclass Middle { tag: Int; inner: Inner; }\nclass Wrapper { tag: Int; middle: Middle; fn answer() -> Int { middle.inner.set(20); return middle.inner.x + middle.inner.y; } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { tag: 0, middle: Middle { tag: 0, inner: Inner { x: 0, y: 22 } } }; wrapper.answer(); return wrapper.middle.inner.x + wrapper.middle.inner.y; }",
            "class Inner { x: Int; y: Int; fn set(value: Int) -> Int { x = value; return x; } }\nclass Middle { tag: Int; inner: Inner; }\nclass Wrapper { tag: Int; middle: Middle; }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { tag: 0, middle: Middle { tag: 0, inner: Inner { x: 0, y: 22 } } }; wrapper.middle.inner.set(20); return wrapper.middle.inner.x + wrapper.middle.inner.y; }",
            "class Inner { x: Int; y: Int; }\nclass Wrapper { inner: Inner; }\nfn copy_inner(value: Wrapper) -> Inner { return value.inner; }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { x: 20, y: 22 } }; var copied: Inner = copy_inner(wrapper); wrapper.inner.x = 0; return copied.x + copied.y; }",
            "class Inner { x: Int; y: Int; fn set(value: Int) -> Int { x = value; return x; } }\nclass Wrapper { inner: Inner; fn answer() -> Int { inner.set(20); return inner.x + inner.y; } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { x: 0, y: 22 } }; wrapper.answer(); return wrapper.inner.x + wrapper.inner.y; }",
            "class Inner { x: Int; y: Int; }\nclass Wrapper { inner: Inner; fn answer() -> Int { inner.x = 20; return inner.x + inner.y; } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { x: 0, y: 22 } }; wrapper.answer(); return wrapper.inner.x + wrapper.inner.y; }",
            "fn sum_inner(value: Inner) -> Int { return value.x + value.y; }\nclass Inner { x: Int; y: Int; fn set(value: Int) -> Int { x = value; return x; } }\nclass Wrapper { inner: Inner; fn answer() -> Int { inner.set(20); return sum_inner(inner); } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { x: 0, y: 22 } }; return wrapper.answer(); }",
            "class Inner { x: Int; y: Int; fn set(value: Int) -> Int { x = value; return x; } }\nclass Wrapper { inner: Inner; fn snapshot() -> Inner { inner.set(20); return inner; } }\nfn main() -> Int { var wrapper: Wrapper = Wrapper { inner: Inner { x: 0, y: 22 } }; var copied: Inner = wrapper.snapshot(); wrapper.inner.x = 0; return copied.x + copied.y; }",
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn empty_receiver_copies_and_replacements_do_not_access_value_storage() {
        for source in [
            "struct Empty {}\nfn main(self: Empty) -> Int { var copied: Empty = self; return 42; }",
            "struct Empty {}\nfn main(self: Empty) -> Int { self = Empty {}; return 42; }",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[8]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    fn recursive_aggregate_field_replacement_is_refused_without_recursing_forever() {
        let module = crate::in_lang_parse::parse_in_source("struct Node { Node next }\nfn unsupported(self: Node, value: Node) -> Int { self.next = value; return 0; }\nfn main() -> Int { return 42; }").unwrap();
        let error = crate::native_emit::lower::lower_module(
            &module,
            "unsupported",
            crate::native_emit::NativeLinkage::Executable,
        )
        .err()
        .expect("recursive values are unsupported");
        assert!(
            error.contains("recursive aggregate value layout"),
            "{error}"
        );
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn temporary_method_receivers_are_materialized_once_in_argument_order() {
        for source in [
            "class Counter { value: Int; fn add(delta: Int) -> Int { value += delta; return value; } }\nclass Point { value: Int; }\nfn make() -> Point { return Point { value: 42 }; }\nfn main() -> Int { var counter: Counter = Counter { value: 0 }; counter.add(make().value); return counter.value; }",
            "class Counter { value: Int; }\nclass Point { value: Int; }\nfn make() -> Point { return Point { value: 42 }; }\nfn main() -> Int { var counter: Counter = Counter { value: 0 }; counter.value = make().value; return counter.value; }",
            "var calls: Int = 0;\nclass Point { value: Int; fn truth() -> Bool { return value > 0; } }\nfn make() -> Point { calls += 1; return Point { value: 1 }; }\nfn main() -> Int { if false && make().truth() { return 0; } if true || make().truth() { return calls + 42; } return 0; }",
            "var calls: Int = 0;\nclass Point { value: Int; fn below() -> Bool { return value < 4; } }\nfn make() -> Point { calls += 1; return Point { value: calls }; }\nfn main() -> Int { var count: Int = 0; while make().below() { count += 1; continue; } if count == 3 && calls == 4 { return 42; } return calls; }",
            "var calls: Int = 0;\nclass Point { value: Int; fn get(after: Int) -> Int { return value + after; } }\nfn fail() -> Point { calls += 1; throw 9; }\nfn later() -> Int { calls += 100; return 0; }\nfn main() -> Int { try { fail().get(later()); } catch 9 { return calls + 41; } return 0; }",
            "var calls: Int = 0;\nclass Empty { fn get(after: Int) -> Int { return calls + after; } }\nfn make() -> Empty { calls += 1; return Empty {}; }\nfn main() -> Int { return make().get(41); }",
            "class Point { x: Int; y: Int; fn get() -> Int { return x + y; } }\nfn make() -> Point { return Point { x: 20, y: 22 }; }\nfn main() -> Int { return make().get(); }",
            "class Point { x: Int; y: Int; fn get() -> Int { return x + y; } }\nfn main() -> Int { return Point { x: 20, y: 22 }.get(); }",
            "var trace: Int = 0;\nclass Point { x: Int; y: Int; fn get(after: Int) -> Int { return x + y + after; } }\nfn make() -> Point { trace = trace * 10 + 1; return Point { x: 20, y: 22 }; }\nfn later() -> Int { trace = trace * 10 + 2; return 0; }\nfn main() -> Int { var result: Int = make().get(later()); if trace == 12 { return result; } return trace; }",
            "var calls: Int = 0;\nclass Point { x: Int; y: Int; fn set(value: Int) -> Point { x = value; return self; } fn get() -> Int { return x + y; } }\nfn make() -> Point { calls += 1; return Point { x: 0, y: 22 }; }\nfn main() -> Int { return make().set(20).get() + calls - 1; }",
        ] {
            let mut module = crate::in_lang_parse::parse_in_source(source).unwrap();
            crate::lower_core::desugar_module(&mut module);
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                assert_eq!(
                    invoke_module(&module, profile, &[]),
                    Some(42),
                    "{profile:?}: {source}"
                );
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn receiver_sentinels_return_failure_before_reading_fields() {
        for source in [
            "struct Empty {}\nfn main(self: Empty) -> Int { return 42; }",
            "struct Point { Int value }\nfn main(self: Point) -> Int { return self.value; }",
        ] {
            let module = crate::in_lang_parse::parse_in_source(source).unwrap();
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                for pointer in [0, 1] {
                    assert_eq!(
                        invoke_module(&module, profile, &[pointer]),
                        Some(1),
                        "{profile:?}: {pointer}: {source}"
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn empty_receivers_do_not_read_address_only_storage() {
        let module = crate::in_lang_parse::parse_in_source(
            "struct Empty {}\nfn main(self: Empty) -> Int { return 42; }",
        )
        .unwrap();
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            // A zero-width receiver may have an aligned dangling address; it has no data to read.
            assert_eq!(
                invoke_module(&module, profile, &[8]),
                Some(42),
                "{profile:?}"
            );
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
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn resolved_rust_modules_keep_private_helpers_globals_structs_and_addresses_distinct() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "inauguration-resolved-jit-{}-{nonce}",
            std::process::id()
        ));
        let sources = root.join("callback_dep/src");
        std::fs::create_dir_all(&sources).unwrap();
        std::fs::write(sources.join("lib.rs"), "pub mod alpha; pub mod beta;").unwrap();
        for (name, value) in [("alpha", 20), ("beta", 22)] {
            std::fs::write(sources.join(format!("{name}.rs")), format!(
                "static VALUE:i64={value}; pub struct Entry{{pub value:i64}} fn helper(value:i64)->i64{{if value>0{{return value;}}0}} impl Entry{{fn read(&self)->i64{{helper(self.value)}}}} fn make()->Entry{{Entry{{value:VALUE}}}} pub fn answer()->i64{{let entry=make();Entry::read(&entry)}}"
            )).unwrap();
        }
        let mut db = crate::crate_db::CrateDb::new();
        db.search_roots = vec![root.clone()];
        db.register_crate("callback_dep", root.join("callback_dep"));
        for source in [
            "fn main()->i64{callback_dep::alpha::answer()+callback_dep::beta::answer()}",
            "fn main()->i64{let first=callback_dep::alpha::answer;let second=callback_dep::beta::answer;invoke(first)+invoke(second)}",
            "fn main()->i64{invoke(callback_dep::alpha::answer)+invoke(callback_dep::beta::answer)}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let resolved = crate::dep_resolver::resolve_deps(&parsed, &db)
                .expect("resolve local module dependencies");
            for prefix in ["callback_dep::alpha", "callback_dep::beta"] {
                for suffix in ["answer", "helper"] {
                    let expected = format!("{prefix}::{suffix}");
                    assert!(resolved.module.decls.iter().any(|decl| matches!(decl, crate::core_ir::Decl::Function { name, .. } if name == &expected)), "missing {expected}: {:?}", resolved.module);
                }
            }
            let mut normalized = crate::typecheck::normalize_module(
                crate::parser_registry::ParserId::Rust,
                &resolved.module,
            );
            crate::lower_core::desugar_module(&mut normalized);
            let report = crate::core_ir_verifier::verify_for_entry(&normalized, "main");
            assert!(report.ok, "{source}: {report:?}; {normalized:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut optimized = normalized.clone();
                crate::core_opt::optimize_with_profile(&mut optimized.decls, Some("main"), profile);
                let report = crate::core_ir_verifier::verify_for_entry(&optimized, "main");
                assert!(report.ok, "{source} {profile:?}: {report:?}; {optimized:?}");
                assert_eq!(
                    invoke_module(&optimized, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(all(unix, any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn rust_self_types_follow_the_owning_impl_through_runtime_values() {
        for source in [
            "mod outer{pub struct Thing{pub value:i64} trait Read{fn read(value:Self)->i64;} impl Read for Thing{fn read(value:Self)->i64{value.value}} impl Thing{pub fn answer()->i64{<Self as Read>::read(Self{value:42})}}} fn main()->i64{outer::Thing::answer()}",
            "mod outer{pub mod outer{pub struct Thing{pub value:i64} impl Thing{fn from(value:i64)->Self{Self{value}} pub fn new()->Self{Self::from(42)}}}} fn main()->i64{outer::outer::Thing::new().value}",
            "struct Thing{value:i64} impl Thing{fn new()->Self{Self{value:42}}} fn main()->i64{Thing::new().value}",
            "struct Thing{value:i64} impl Thing{fn from(value:i64)->Self{Self{value}} fn new()->Self{Self::from(42)}} fn main()->i64{Thing::new().value}",
            "struct Thing{value:i64} impl Thing{fn identity(value:Self)->Self{value} fn new()->Self{Self::identity(Self{value:42})}} fn main()->i64{Thing::new().value}",
            "struct Thing{value:i64} impl Thing{fn new()->Self{let build=||->Self{Self{value:42}};build()}} fn main()->i64{Thing::new().value}",
            "struct Thing{value:i64} impl Thing{fn new()->Self{let build=||{let inner=||->Self{Self{value:42}};inner()};build()}} fn main()->i64{Thing::new().value}",
            "struct Thing{value:i64} impl Thing{fn read(value:Self)->i64{match value{Self{value}=>value}}} fn main()->i64{Thing::read(Thing{value:42})}",
            "struct Thing{value:i64} impl Thing{fn read(&self)->i64{self.value} fn new()->Self{Self{value:42}}} fn main()->i64{let thing=Thing::new();Thing::read(&thing)}",
            "mod outer{pub struct Thing{pub value:i64} impl Thing{pub fn new()->Self{let build=||->Self{Self{value:42}};build()}}} fn main()->i64{outer::Thing::new().value}",
            "struct Thing{value:i64,Selfish:i64} impl Thing{fn new()->Self{Self{value:20,Selfish:22}} fn read(&self)->i64{if \"Self::read\"==\"Self::read\"{self.value+self.Selfish}else{0}}} fn main()->i64{let thing=Thing::new();Thing::read(&thing)}",
        ] {
            let parsed = crate::compiler::rust_front::parse_rust_source(source).unwrap();
            let mut normalized =
                crate::typecheck::normalize_module(crate::parser_registry::ParserId::Rust, &parsed);
            crate::lower_core::desugar_module(&mut normalized);
            let report = crate::core_ir_verifier::verify_for_entry(&normalized, "main");
            assert!(report.ok, "{source}: {report:?}; {normalized:?}");
            for profile in [
                crate::emit_profile::EmitProfile::Default,
                crate::emit_profile::EmitProfile::Lean,
                crate::emit_profile::EmitProfile::Harden,
            ] {
                let mut optimized = normalized.clone();
                crate::core_opt::optimize_with_profile(&mut optimized.decls, Some("main"), profile);
                let report = crate::core_ir_verifier::verify_for_entry(&optimized, "main");
                assert!(report.ok, "{source} {profile:?}: {report:?}; {optimized:?}");
                assert_eq!(
                    invoke_module(&optimized, profile, &[]),
                    Some(42),
                    "{source} {profile:?}"
                );
            }
        }
    }

    #[test]
    fn jit_runtime_new_is_empty() {
        let rt = JitRuntime::new();
        assert_eq!(rt.function_count(), 0);
    }
}
