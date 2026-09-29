use std::path::Path;

use libloading::Library;

use super::{
    DynamicModule, DynamicModuleError, IN_MODULE_ENTRY_SYMBOL, InCallStatus, ModuleDescriptor,
    validate_descriptor,
};

#[repr(C)]
struct RawModuleVTable {
    abi_version: u32,
    pointer_width: u32,
    endian: u32,
    layout_hash: u32,
    alloc: Option<unsafe extern "C" fn(u64, u64, u64) -> *mut ()>,
    dealloc: Option<unsafe extern "C" fn(*mut (), u64, u64, u64)>,
    init: Option<unsafe extern "C" fn(*const ()) -> InCallStatus>,
    shutdown: Option<unsafe extern "C" fn() -> InCallStatus>,
    symbol: Option<unsafe extern "C" fn(*const u8, u64) -> *const ()>,
    manifest: Option<unsafe extern "C" fn() -> *const ()>,
}

type VtableFn = unsafe extern "C" fn() -> *const RawModuleVTable;

pub struct UnixDynamicModule {
    _library: Library,
    vtable: *const RawModuleVTable,
}

impl DynamicModule for UnixDynamicModule {
    fn descriptor(&self) -> ModuleDescriptor {
        unsafe {
            ModuleDescriptor {
                abi_version: (*self.vtable).abi_version,
                pointer_width: (*self.vtable).pointer_width,
                endian: (*self.vtable).endian,
                layout_hash: (*self.vtable).layout_hash,
            }
        }
    }

    fn init(&self, host: *const ()) -> InCallStatus {
        unsafe {
            match (*self.vtable).init {
                Some(init) => init(host),
                None => InCallStatus::ok(),
            }
        }
    }

    fn shutdown(&self) -> InCallStatus {
        unsafe {
            match (*self.vtable).shutdown {
                Some(shutdown) => shutdown(),
                None => InCallStatus::ok(),
            }
        }
    }

    fn symbol(&self, name: &str) -> Option<*const ()> {
        unsafe {
            let lookup = (*self.vtable).symbol?;
            let ptr = lookup(name.as_ptr(), name.len() as u64);
            if ptr.is_null() { None } else { Some(ptr) }
        }
    }
}

pub fn load_dynamic_module(path: &Path) -> Result<Box<dyn DynamicModule>, DynamicModuleError> {
    unsafe {
        let library = Library::new(path).map_err(|err| DynamicModuleError::LoadFailed {
            path: path.display().to_string(),
            reason: err.to_string(),
        })?;
        let vtable_fn: libloading::Symbol<VtableFn> = library
            .get(IN_MODULE_ENTRY_SYMBOL.as_bytes())
            .map_err(|_| DynamicModuleError::EntryMissing {
                path: path.display().to_string(),
                symbol: IN_MODULE_ENTRY_SYMBOL.to_string(),
            })?;
        let vtable = vtable_fn();
        if vtable.is_null() {
            return Err(DynamicModuleError::LoadFailed {
                path: path.display().to_string(),
                reason: "entry returned null vtable".to_string(),
            });
        }
        let descriptor = ModuleDescriptor {
            abi_version: (*vtable).abi_version,
            pointer_width: (*vtable).pointer_width,
            endian: (*vtable).endian,
            layout_hash: (*vtable).layout_hash,
        };
        validate_descriptor(&descriptor)?;
        Ok(Box::new(UnixDynamicModule {
            _library: library,
            vtable,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    fn compile_dummy_library(c_code: &str, output_path: &Path) {
        let mut child = Command::new("cc")
            .args([
                "-shared",
                "-fPIC",
                "-o",
                output_path.to_str().unwrap(),
                "-xc",
                "-",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("Failed to spawn cc");

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(c_code.as_bytes())
                .expect("Failed to write to stdin");
        }

        let status = child.wait().expect("Failed to wait for cc");
        assert!(status.success(), "cc failed");
    }

    #[test]
    fn test_load_dynamic_module_invalid_library() {
        let path = Path::new("/path/to/nonexistent_library.so");
        let result = load_dynamic_module(path);
        assert!(matches!(result, Err(DynamicModuleError::LoadFailed { .. })));
    }

    #[test]
    fn test_load_dynamic_module_missing_symbol() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_missing_symbol.so");
        compile_dummy_library("void dummy() {}", &path);

        let result = load_dynamic_module(&path);

        if let Err(DynamicModuleError::EntryMissing { symbol, .. }) = result {
            assert_eq!(symbol, super::IN_MODULE_ENTRY_SYMBOL);
        } else {
            panic!("Expected EntryMissing error, got: {:?}", result.err());
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_load_dynamic_module_null_vtable() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_null_vtable.so");
        compile_dummy_library("void* in_module_vtable() { return 0; }", &path);

        let result = load_dynamic_module(&path);

        if let Err(DynamicModuleError::LoadFailed { reason, .. }) = result {
            assert_eq!(reason, "entry returned null vtable");
        } else {
            panic!(
                "Expected LoadFailed error with null vtable reason, got: {:?}",
                result.err()
            );
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_load_dynamic_module_abi_mismatch() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_abi_mismatch.so");
        let c_code = r#"
            #include <stdint.h>
            struct RawModuleVTable {
                uint32_t abi_version;
                uint32_t pointer_width;
                uint32_t endian;
                uint32_t layout_hash;
                void* alloc;
                void* dealloc;
                void* init;
                void* shutdown;
                void* symbol;
                void* manifest;
            };
            static struct RawModuleVTable vtable = {
                .abi_version = 999,
                .pointer_width = 64,
                .endian = 0,
                .layout_hash = 0,
                .alloc = 0,
                .dealloc = 0,
                .init = 0,
                .shutdown = 0,
                .symbol = 0,
                .manifest = 0,
            };
            void* in_module_vtable() {
                return &vtable;
            }
        "#;
        compile_dummy_library(c_code, &path);

        let result = load_dynamic_module(&path);

        if let Err(DynamicModuleError::AbiVersionMismatch { expected, found }) = result {
            assert_eq!(expected, crate::boundary_ir::IN_ABI_VERSION);
            assert_eq!(found, 999);
        } else {
            panic!("Expected AbiVersionMismatch error, got: {:?}", result.err());
        }

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_load_dynamic_module_success() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_success.so");
        let c_code = format!(
            r#"
            #include <stdint.h>
            struct RawModuleVTable {{
                uint32_t abi_version;
                uint32_t pointer_width;
                uint32_t endian;
                uint32_t layout_hash;
                void* alloc;
                void* dealloc;
                void* init;
                void* shutdown;
                void* symbol;
                void* manifest;
            }};
            static struct RawModuleVTable vtable = {{
                .abi_version = {},
                .pointer_width = 64,
                .endian = 0,
                .layout_hash = 0,
                .alloc = 0,
                .dealloc = 0,
                .init = 0,
                .shutdown = 0,
                .symbol = 0,
                .manifest = 0,
            }};
            void* in_module_vtable() {{
                return &vtable;
            }}
        "#,
            crate::boundary_ir::IN_ABI_VERSION
        );
        compile_dummy_library(&c_code, &path);

        let result = load_dynamic_module(&path);
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());

        let module = result.unwrap();
        let descriptor = module.descriptor();
        assert_eq!(descriptor.abi_version, crate::boundary_ir::IN_ABI_VERSION);

        let init_status = module.init(std::ptr::null());
        assert!(init_status.is_ok());

        let shutdown_status = module.shutdown();
        assert!(shutdown_status.is_ok());

        let symbol = module.symbol("nonexistent");
        assert!(symbol.is_none());

        let _ = std::fs::remove_file(path);
    }
}
