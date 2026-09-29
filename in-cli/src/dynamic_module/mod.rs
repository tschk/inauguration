use crate::boundary_ir::IN_ABI_VERSION;

pub const IN_MODULE_ENTRY_SYMBOL: &str = "in_module_vtable";

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InCallStatus {
    pub code: u32,
    pub reserved: u32,
    pub error_len: u64,
    pub error_ptr: *const u8,
}

impl InCallStatus {
    pub const OK: u32 = 0;

    #[must_use]
    pub fn ok() -> Self {
        Self {
            code: Self::OK,
            reserved: 0,
            error_len: 0,
            error_ptr: std::ptr::null(),
        }
    }

    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.code == Self::OK
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleDescriptor {
    pub abi_version: u32,
    pub pointer_width: u32,
    pub endian: u32,
    pub layout_hash: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct DynamicLoaderFixture {
    pub entry_symbol: String,
    pub abi_version: u32,
    pub pointer_width: u32,
    pub endian: u32,
    pub layout_hash: u32,
}

pub trait DynamicModule {
    fn descriptor(&self) -> ModuleDescriptor;
    fn init(&self, host: *const ()) -> InCallStatus;
    fn shutdown(&self) -> InCallStatus;
    fn symbol(&self, name: &str) -> Option<*const ()>;
}

#[derive(Debug, thiserror::Error)]
pub enum DynamicModuleError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("load failed for {path}: {reason}")]
    LoadFailed { path: String, reason: String },
    #[error("entry symbol `{symbol}` missing from {path}")]
    EntryMissing { path: String, symbol: String },
    #[error("abi_version mismatch: expected {expected}, got {found}")]
    AbiVersionMismatch { expected: u32, found: u32 },
    #[error("dynamic module loading requires Unix or Windows")]
    UnsupportedPlatform,
}

pub fn validate_descriptor(descriptor: &ModuleDescriptor) -> Result<(), DynamicModuleError> {
    if descriptor.abi_version != IN_ABI_VERSION {
        return Err(DynamicModuleError::AbiVersionMismatch {
            expected: IN_ABI_VERSION,
            found: descriptor.abi_version,
        });
    }
    Ok(())
}

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::load_dynamic_module;

#[cfg(not(any(unix, windows)))]
pub fn load_dynamic_module(
    _path: &std::path::Path,
) -> Result<Box<dyn DynamicModule>, DynamicModuleError> {
    Err(DynamicModuleError::UnsupportedPlatform)
}

#[must_use]
pub fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("in-cli parent")
        .to_path_buf()
}

pub fn fixture_manifest() -> Result<DynamicLoaderFixture, String> {
    let path = workspace_root().join("fixtures/dynamic-loader/manifest.json");
    let source =
        std::fs::read_to_string(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
    serde_json::from_str(&source).map_err(|err| format!("parse {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;

    #[test]
    fn fixture_manifest_matches_contract() {
        let fixture = fixture_manifest().expect("fixture manifest");
        assert_eq!(fixture.entry_symbol, IN_MODULE_ENTRY_SYMBOL);
        assert_eq!(fixture.abi_version, IN_ABI_VERSION);
        assert_eq!(fixture.pointer_width, 64);
        assert_eq!(fixture.endian, 0);
        assert_eq!(fixture.layout_hash, 0);
    }

    #[test]
    fn validate_descriptor_rejects_abi_mismatch() {
        let descriptor = ModuleDescriptor {
            abi_version: IN_ABI_VERSION + 1,
            pointer_width: 64,
            endian: 0,
            layout_hash: 0,
        };
        let err = validate_descriptor(&descriptor).expect_err("abi mismatch");
        assert!(matches!(
            err,
            DynamicModuleError::AbiVersionMismatch { expected, found }
            if expected == IN_ABI_VERSION && found == IN_ABI_VERSION + 1
        ));
    }

    #[test]
    fn validate_descriptor_accepts_current_abi() {
        let descriptor = ModuleDescriptor {
            abi_version: IN_ABI_VERSION,
            pointer_width: 64,
            endian: 0,
            layout_hash: 0,
        };
        validate_descriptor(&descriptor).expect("current abi");
    }

    #[test]
    fn validate_descriptor_rejects_older_abi() {
        let descriptor = ModuleDescriptor {
            abi_version: 0,
            pointer_width: 64,
            endian: 0,
            layout_hash: 0,
        };
        let err = validate_descriptor(&descriptor).expect_err("older abi");
        assert!(matches!(
            err,
            DynamicModuleError::AbiVersionMismatch { expected, found }
            if expected == IN_ABI_VERSION && found == 0
        ));
    }

    #[test]
    fn validate_descriptor_accepts_varying_metadata_fields() {
        let descriptor = ModuleDescriptor {
            abi_version: IN_ABI_VERSION,
            pointer_width: 32,
            endian: 1,
            layout_hash: 12345,
        };
        validate_descriptor(&descriptor).expect("current abi with varying fields");
    }

    #[cfg(windows)]
    #[test]
    fn windows_loader_is_stub() {
        let err = load_dynamic_module(Path::new("fixture.dll")).expect_err("windows stub");
        assert!(matches!(err, DynamicModuleError::UnsupportedPlatform));
    }

    #[cfg(unix)]
    fn compile_echo_fixture() -> std::path::PathBuf {
        let root = workspace_root();
        let src = root.join("fixtures/dynamic-loader/echo_module.c");
        let out_dir = std::env::temp_dir().join(format!(
            "in-dynamic-loader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&out_dir).expect("temp dir");
        let lib_name = if cfg!(target_os = "macos") {
            "libecho_module.dylib"
        } else {
            "libecho_module.so"
        };
        let lib = out_dir.join(lib_name);
        let status = Command::new("cc")
            .args([
                "-shared",
                "-fPIC",
                "-O2",
                "-I",
                root.join("shared/abi").to_str().expect("abi include"),
                "-o",
                lib.to_str().expect("lib path"),
                src.to_str().expect("source path"),
            ])
            .status()
            .expect("cc");
        assert!(status.success(), "cc failed to build echo fixture");
        lib
    }

    #[cfg(unix)]
    #[test]
    fn unix_loads_echo_fixture_module() {
        let lib = compile_echo_fixture();
        let module = load_dynamic_module(&lib).expect("load echo fixture");
        let descriptor = module.descriptor();
        assert_eq!(descriptor.abi_version, IN_ABI_VERSION);
        assert_eq!(descriptor.pointer_width, 64);
        assert_eq!(descriptor.endian, 0);
        assert_eq!(descriptor.layout_hash, 0);
        let init_status = module.init(std::ptr::null());
        assert!(init_status.is_ok());
        let shutdown_status = module.shutdown();
        assert!(shutdown_status.is_ok());
        let symbol = module.symbol("echo_add").expect("echo_add symbol");
        assert!(!symbol.is_null());
        type EchoAdd = unsafe extern "C" fn(i32, i32) -> i32;
        let echo_add: EchoAdd = unsafe { std::mem::transmute(symbol) };
        assert_eq!(unsafe { echo_add(2, 3) }, 5);
    }
}
