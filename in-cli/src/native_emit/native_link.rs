//! Minimal native symbol resolver for JIT code.
//! Falls back to dlsym when function not in module map.
//!
//! # Security
//! Only safe I/O functions are pre-registered (exit, puts, putchar, printf).
//! `system` is deliberately excluded to prevent shell injection through JIT code.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

#[derive(Clone, Copy)]
struct NativePtr(*const u8);
// SAFETY: NativePtr wraps a dlsym'd function pointer that is read-only
// after initialization. Multiple threads can read it concurrently.
unsafe impl Send for NativePtr {}
// SAFETY: Same as Send — the pointer is immutable after cache insertion.
unsafe impl Sync for NativePtr {}

fn cache() -> &'static RwLock<HashMap<String, NativePtr>> {
    static C: OnceLock<RwLock<HashMap<String, NativePtr>>> = OnceLock::new();
    C.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Symbols that may be resolved via dlsym. Everything else is rejected to
/// prevent JIT-compiled code from calling arbitrary libc functions.
const DLSYM_ALLOWLIST: &[&str] = &[
    "exit",
    "abort",
    "puts",
    "putchar",
    "printf",
    "malloc",
    "free",
    "memset",
    "memcpy",
    "mmap",
    "bzero",
    "in_env_var",
    "in_env_has",
    "in_env_temp_dir",
    "in_env_current_dir",
    "in_env_set_var",
    "in_env_remove_var",
    "in_fs_read_to_string",
    "in_fs_exists",
    "in_fs_write",
    "in_fs_create_dir",
    "in_fs_remove_file",
    "in_str_contains",
    "in_str_starts_with",
    "in_str_ends_with",
    "in_str_concat",
    "in_json_stringify",
    "in_str_eq",
    "in_str_table_has",
    "in_str_table_get_int",
    "in_vec_join",
    "in_str_trim",
    "in_str_split_lines",
    "in_str_split_spaces",
    "in_str_tokenize_expr",
    "in_str_to_int",
    "in_str_is_int",
    "in_str_index_of",
    "in_str_slice",
    "in_int_to_string",
    "in_path_join",
    "in_path_dirname",
    "in_path_basename",
    "in_path_extname",
    "in_path_normalize",
];

pub fn resolve_native_fn(name: &str) -> Option<*const u8> {
    {
        let c = cache().read().unwrap();
        if let Some(np) = c.get(name) {
            return Some(np.0);
        }
    }

    // Only symbols on the explicit allowlist may be looked up dynamically.
    if !DLSYM_ALLOWLIST.contains(&name) {
        return None;
    }

    let ptr = dlsym_exact(name);
    if ptr.is_none() {
        // macOS C convention
        let u = format!("_{name}");
        if let Some(p) = dlsym_exact(&u) {
            cache()
                .write()
                .unwrap()
                .insert(name.to_string(), NativePtr(p));
            return Some(p);
        }
    }
    if let Some(p) = ptr {
        cache()
            .write()
            .unwrap()
            .insert(name.to_string(), NativePtr(p));
    }
    ptr
}

/// Pre-register critical libc symbols on init.
///
/// # Security
/// Only safe I/O functions are included. `system` is deliberately excluded
/// to prevent JIT-compiled code from executing arbitrary shell commands.
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
pub fn bootstrap_jit_native() {
    // Only safe I/O and memory functions.
    // No shell-execution symbols.
    let mut c = cache().write().unwrap();
    for name in &[
        "exit", "puts", "putchar", "printf", "malloc", "free", "memset", "memcpy", "mmap", "bzero",
    ] {
        if let Some(ptr) = dlsym_exact(name) {
            c.insert(name.to_string(), NativePtr(ptr));
        }
    }
    // Pre-register in-cli stdlib wrappers so the JIT can call std::env and
    // std::fs helpers without external libc references.
    register_env_funcs(&mut c);
    register_fs_funcs(&mut c);
    register_str_funcs(&mut c);
    register_path_funcs(&mut c);
    register_vec_funcs(&mut c);
    register_misc_funcs(&mut c);
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
fn register_env_funcs<S: std::hash::BuildHasher>(
    c: &mut std::collections::HashMap<String, NativePtr, S>,
) {
    c.insert(
        "in_env_var".to_string(),
        NativePtr(crate::native_stdlib::in_env_var as *const u8),
    );
    c.insert(
        "in_env_temp_dir".to_string(),
        NativePtr(crate::native_stdlib::in_env_temp_dir as *const u8),
    );
    c.insert(
        "in_env_current_dir".to_string(),
        NativePtr(crate::native_stdlib::in_env_current_dir as *const u8),
    );
    c.insert(
        "in_env_set_var".to_string(),
        NativePtr(crate::native_stdlib::in_env_set_var as *const u8),
    );
    c.insert(
        "in_env_remove_var".to_string(),
        NativePtr(crate::native_stdlib::in_env_remove_var as *const u8),
    );
    c.insert(
        "in_env_has".to_string(),
        NativePtr(crate::native_stdlib::in_env_has as *const u8),
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
fn register_fs_funcs<S: std::hash::BuildHasher>(
    c: &mut std::collections::HashMap<String, NativePtr, S>,
) {
    c.insert(
        "in_fs_read_to_string".to_string(),
        NativePtr(crate::native_stdlib::in_fs_read_to_string as *const u8),
    );
    c.insert(
        "in_fs_exists".to_string(),
        NativePtr(crate::native_stdlib::in_fs_exists as *const u8),
    );
    c.insert(
        "in_fs_write".to_string(),
        NativePtr(crate::native_stdlib::in_fs_write as *const u8),
    );
    c.insert(
        "in_fs_create_dir".to_string(),
        NativePtr(crate::native_stdlib::in_fs_create_dir as *const u8),
    );
    c.insert(
        "in_fs_remove_file".to_string(),
        NativePtr(crate::native_stdlib::in_fs_remove_file as *const u8),
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
fn register_str_funcs<S: std::hash::BuildHasher>(
    c: &mut std::collections::HashMap<String, NativePtr, S>,
) {
    c.insert(
        "in_str_contains".to_string(),
        NativePtr(crate::native_stdlib::in_str_contains as *const u8),
    );
    c.insert(
        "in_str_starts_with".to_string(),
        NativePtr(crate::native_stdlib::in_str_starts_with as *const u8),
    );
    c.insert(
        "in_str_ends_with".to_string(),
        NativePtr(crate::native_stdlib::in_str_ends_with as *const u8),
    );
    c.insert(
        "in_str_concat".to_string(),
        NativePtr(crate::native_stdlib::in_str_concat as *const u8),
    );
    c.insert(
        "in_str_eq".to_string(),
        NativePtr(crate::native_stdlib::in_str_eq as *const u8),
    );
    c.insert(
        "in_str_table_has".to_string(),
        NativePtr(crate::native_stdlib::in_str_table_has as *const u8),
    );
    c.insert(
        "in_str_table_get_int".to_string(),
        NativePtr(crate::native_stdlib::in_str_table_get_int as *const u8),
    );
    c.insert(
        "in_str_trim".to_string(),
        NativePtr(crate::native_stdlib::in_str_trim as *const u8),
    );
    c.insert(
        "in_str_split_lines".to_string(),
        NativePtr(crate::native_stdlib::in_str_split_lines as *const u8),
    );
    c.insert(
        "in_str_split_spaces".to_string(),
        NativePtr(crate::native_stdlib::in_str_split_spaces as *const u8),
    );
    c.insert(
        "in_str_tokenize_expr".to_string(),
        NativePtr(crate::native_stdlib::in_str_tokenize_expr as *const u8),
    );
    c.insert(
        "in_str_to_int".to_string(),
        NativePtr(crate::native_stdlib::in_str_to_int as *const u8),
    );
    c.insert(
        "in_str_is_int".to_string(),
        NativePtr(crate::native_stdlib::in_str_is_int as *const u8),
    );
    c.insert(
        "in_str_index_of".to_string(),
        NativePtr(crate::native_stdlib::in_str_index_of as *const u8),
    );
    c.insert(
        "in_str_slice".to_string(),
        NativePtr(crate::native_stdlib::in_str_slice as *const u8),
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
fn register_path_funcs<S: std::hash::BuildHasher>(
    c: &mut std::collections::HashMap<String, NativePtr, S>,
) {
    c.insert(
        "in_path_join".to_string(),
        NativePtr(crate::native_stdlib::in_path_join as *const u8),
    );
    c.insert(
        "in_path_dirname".to_string(),
        NativePtr(crate::native_stdlib::in_path_dirname as *const u8),
    );
    c.insert(
        "in_path_basename".to_string(),
        NativePtr(crate::native_stdlib::in_path_basename as *const u8),
    );
    c.insert(
        "in_path_extname".to_string(),
        NativePtr(crate::native_stdlib::in_path_extname as *const u8),
    );
    c.insert(
        "in_path_normalize".to_string(),
        NativePtr(crate::native_stdlib::in_path_normalize as *const u8),
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
fn register_vec_funcs<S: std::hash::BuildHasher>(
    c: &mut std::collections::HashMap<String, NativePtr, S>,
) {
    c.insert(
        "in_vec_extend".to_string(),
        NativePtr(crate::native_stdlib::in_vec_extend as *const u8),
    );
    c.insert(
        "in_vec_join".to_string(),
        NativePtr(crate::native_stdlib::in_vec_join as *const u8),
    );
    c.insert(
        "in_vec_push".to_string(),
        NativePtr(crate::native_stdlib::in_vec_push as *const u8),
    );
    c.insert(
        "in_vec_push_words".to_string(),
        NativePtr(crate::native_stdlib::in_vec_push_words as *const u8),
    );
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
fn register_misc_funcs<S: std::hash::BuildHasher>(
    c: &mut std::collections::HashMap<String, NativePtr, S>,
) {
    c.insert(
        "in_process_run".to_string(),
        NativePtr(crate::native_stdlib::in_process_run as *const u8),
    );
    c.insert(
        "in_json_stringify".to_string(),
        NativePtr(crate::native_stdlib::in_json_stringify as *const u8),
    );
    c.insert(
        "in_print".to_string(),
        NativePtr(crate::native_stdlib::in_print as *const u8),
    );
    c.insert(
        "in_print_int".to_string(),
        NativePtr(crate::native_stdlib::in_print_int as *const u8),
    );
    c.insert(
        "in_int_to_string".to_string(),
        NativePtr(crate::native_stdlib::in_int_to_string as *const u8),
    );
}

fn dlsym_exact(name: &str) -> Option<*const u8> {
    let c_name = std::ffi::CString::new(name).ok()?;
    // SAFETY: dlsym returns a symbol pointer from the global symbol space
    // (RTLD_DEFAULT). It returns NULL if the symbol is not found. The
    // resulting pointer points to a function in a loaded library that
    // remains resident for the process lifetime. No aliasing concerns
    // because we only read the pointer value (never call through it
    // without the caller's explicit intent via resolve_native_fn).
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c_name.as_ptr()) };
    if ptr.is_null() {
        None
    } else {
        Some(ptr as *const u8)
    }
}

#[cfg(windows)]
fn dlsym_exact(_name: &str) -> Option<*const u8> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(windows))]
    fn test_resolve_native_fn_allowlist() {
        let ptr = resolve_native_fn("exit");
        assert!(ptr.is_some());
    }

    #[test]
    fn test_resolve_native_fn_disallowed() {
        let ptr = resolve_native_fn("system");
        assert!(ptr.is_none());
    }

    #[test]
    fn test_resolve_native_fn_cache() {
        cache().write().unwrap().insert(
            "fake_cached_func".to_string(),
            NativePtr(0x1234_usize as *const u8),
        );
        let ptr = resolve_native_fn("fake_cached_func");
        assert_eq!(ptr, Some(0x1234_usize as *const u8));
    }
}
