pub use crate::target::{NATIVE_AARCH64_SUBSET, NATIVE_BACKEND_NOT_IMPLEMENTED, TargetSpec};

#[must_use]
pub fn native_backend_status() -> TargetSpec {
    crate::target::native_target_spec()
}

#[must_use]
pub fn native_subset_host_available() -> bool {
    crate::target::native_subset_host_available()
}

/// Whether the host executable backend supplies the hosted allocation, error,
/// print and external-call runtime used by the general runtime conformance suite.
/// Linux's static ELF subset deliberately does not claim those facilities.
#[must_use]
pub fn native_hosted_runtime_available() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    )))]
    #[test]
    fn reports_native_backend_not_implemented_contract() {
        let status = native_backend_status();
        assert!(!status.implemented);
        assert_eq!(status.stage, "contract-only");
        assert_eq!(status.reason_code, NATIVE_BACKEND_NOT_IMPLEMENTED);
        assert_eq!(status.input_stage, "core-ir-or-textual-sil");
        assert_eq!(status.artifact_kind, "none");
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn reports_native_backend_linux_x86_64_subset_on_host() {
        let status = native_backend_status();
        assert!(status.implemented);
        assert_eq!(status.stage, "owned-native-subset");
        assert_eq!(status.reason_code, "native-x86_64-linux-subset");
        assert_eq!(status.artifact_kind, "elf-executable");
        assert_eq!(status.host_triple, Some("x86_64-unknown-linux-gnu"));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn reports_native_backend_aarch64_subset_on_host() {
        let status = native_backend_status();
        assert!(status.implemented);
        assert_eq!(status.stage, "owned-native-subset");
        assert_eq!(status.reason_code, NATIVE_AARCH64_SUBSET);
        assert_eq!(status.input_stage, "core-ir");
        assert_eq!(status.artifact_kind, "mach-o-executable");
    }
}
