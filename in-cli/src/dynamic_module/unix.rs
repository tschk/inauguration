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
    use std::process::Command;

    fn compile_c_fixture(c_src: &str, lib_name: &str) -> std::path::PathBuf {
        let out_dir = std::env::temp_dir().join(format!(
            "in-dyn-unix-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&out_dir).expect("temp dir");
        let lib = out_dir.join(lib_name);
        let src = out_dir.join("src.c");
        std::fs::write(&src, c_src).expect("write src");

        let status = Command::new("cc")
            .args([
                "-shared",
                "-fPIC",
                "-O0",
                "-o",
                lib.to_str().expect("lib path"),
                src.to_str().expect("source path"),
            ])
            .status()
            .expect("cc");
        assert!(status.success(), "cc failed to build fixture");
        lib
    }

    #[test]
    fn test_load_dynamic_module_invalid_library() {
        let path = Path::new("/path/to/nonexistent_library.so");
        let result = load_dynamic_module(path);
        assert!(matches!(result, Err(DynamicModuleError::LoadFailed { .. })));
    }

    #[test]
    fn test_load_dynamic_module_entry_missing() {
        let lib = compile_c_fixture("void some_other_function() {}", "libmissing.so");
        let result = load_dynamic_module(&lib);
        assert!(matches!(
            result,
            Err(DynamicModuleError::EntryMissing { .. })
        ));
    }

    #[test]
    fn test_load_dynamic_module_null_vtable() {
        let lib = compile_c_fixture("void* in_module_vtable() { return 0; }", "libnull.so");
        let result = load_dynamic_module(&lib);
        assert!(matches!(
            result,
            Err(DynamicModuleError::LoadFailed { ref reason, .. }) if reason == "entry returned null vtable"
        ));
    }
}
