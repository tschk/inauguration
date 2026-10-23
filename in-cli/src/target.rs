use serde::Serialize;
use std::sync::OnceLock;

pub const NATIVE_BACKEND_NOT_IMPLEMENTED: &str = "native-backend-not-implemented";
pub const NATIVE_AARCH64_SUBSET: &str = "native-aarch64-subset";
pub const NATIVE_OBJECT_SUBSET: &str = "native-object-subset";

/// Integer carriers used by the implemented native target paths.
pub fn integer_semantics(triple: Option<&str>) -> crate::integer_semantics::IntegerSemantics {
    use crate::integer_semantics::IntegerSemantics;
    match triple.and_then(|triple| triple.split('-').next()) {
        Some("thumbv8m.main" | "i386" | "i686" | "armv7" | "wasm32") => IntegerSemantics::Word32,
        _ => IntegerSemantics::Word64,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TargetSpec {
    pub name: &'static str,
    pub implemented: bool,
    pub stage: &'static str,
    pub reason_code: &'static str,
    pub reason: &'static str,
    pub input_stage: &'static str,
    pub artifact_kind: &'static str,
    pub host_triple: Option<&'static str>,
    pub backend_artifact_supported: bool,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const NATIVE_TARGET: TargetSpec = TargetSpec {
    name: "native",
    implemented: true,
    stage: "owned-native-subset",
    reason_code: NATIVE_AARCH64_SUBSET,
    reason: "inauguration owns a Mach-O executable emitter and AArch64 lowering path for a checked Core IR scalar subset on Apple ARM64 hosts",
    input_stage: "core-ir",
    artifact_kind: "mach-o-executable",
    host_triple: Some("aarch64-apple-darwin"),
    backend_artifact_supported: false,
};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const NATIVE_TARGET: TargetSpec = TargetSpec {
    name: "native",
    implemented: true,
    stage: "owned-native-subset",
    reason_code: "native-x86_64-linux-subset",
    reason: "inauguration owns x86_64 Linux ELF executable lowering with writable globals, internal calls and process exit; runtime imports fail closed",
    input_stage: "core-ir",
    artifact_kind: "elf-executable",
    host_triple: Some("x86_64-unknown-linux-gnu"),
    backend_artifact_supported: false,
};

#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
const NATIVE_TARGET: TargetSpec = TargetSpec {
    name: "native",
    implemented: false,
    stage: "contract-only",
    reason_code: NATIVE_BACKEND_NOT_IMPLEMENTED,
    reason: "inauguration currently has no in-tree object-file emitter, linker driver, ABI lowering, or owned machine runtime for native code generation",
    input_stage: "core-ir-or-textual-sil",
    artifact_kind: "none",
    host_triple: None,
    backend_artifact_supported: false,
};

const IN_EQUIVALENT_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "aarch64-apple-ios",
    "aarch64-apple-ios-macabi",
    "aarch64-apple-ios-sim",
    "aarch64-apple-tvos",
    "aarch64-apple-tvos-sim",
    "aarch64-apple-visionos",
    "aarch64-apple-visionos-sim",
    "aarch64-apple-watchos",
    "aarch64-apple-watchos-sim",
    "aarch64-kmc-solid_asp3",
    "aarch64-linux-android",
    "aarch64-nintendo-switch-freestanding",
    "aarch64-pc-windows-gnullvm",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-freebsd",
    "aarch64-unknown-fuchsia",
    "aarch64-unknown-helenos",
    "aarch64-unknown-hermit",
    "aarch64-unknown-illumos",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu_ilp32",
    "aarch64-unknown-linux-musl",
    "aarch64-unknown-linux-ohos",
    "aarch64-unknown-managarm-mlibc",
    "aarch64-unknown-netbsd",
    "aarch64-unknown-none",
    "aarch64-unknown-none-softfloat",
    "aarch64-unknown-nto-qnx700",
    "aarch64-unknown-nto-qnx710",
    "aarch64-unknown-nto-qnx710_iosock",
    "aarch64-unknown-nto-qnx800",
    "aarch64-unknown-nuttx",
    "aarch64-unknown-openbsd",
    "aarch64-unknown-redox",
    "aarch64-unknown-teeos",
    "aarch64-unknown-trusty",
    "aarch64-unknown-uefi",
    "aarch64-uwp-windows-msvc",
    "aarch64-wrs-vxworks",
    "aarch64_be-unknown-hermit",
    "aarch64_be-unknown-linux-gnu",
    "aarch64_be-unknown-linux-gnu_ilp32",
    "aarch64_be-unknown-linux-musl",
    "aarch64_be-unknown-netbsd",
    "aarch64_be-unknown-none-softfloat",
    "amdgcn-amd-amdhsa",
    "arm-linux-androideabi",
    "arm-unknown-linux-gnueabi",
    "arm-unknown-linux-gnueabihf",
    "arm-unknown-linux-musleabi",
    "arm-unknown-linux-musleabihf",
    "arm64_32-apple-watchos",
    "arm64e-apple-darwin",
    "arm64e-apple-ios",
    "arm64e-apple-tvos",
    "arm64ec-pc-windows-msvc",
    "armeb-unknown-linux-gnueabi",
    "armebv7r-none-eabi",
    "armebv7r-none-eabihf",
    "armv4t-none-eabi",
    "armv4t-unknown-linux-gnueabi",
    "armv5te-none-eabi",
    "armv5te-unknown-linux-gnueabi",
    "armv5te-unknown-linux-musleabi",
    "armv5te-unknown-linux-uclibceabi",
    "armv6-unknown-freebsd",
    "armv6-unknown-netbsd-eabihf",
    "armv6k-nintendo-3ds",
    "armv7-linux-androideabi",
    "armv7-rtems-eabihf",
    "armv7-sony-vita-newlibeabihf",
    "armv7-unknown-freebsd",
    "armv7-unknown-linux-gnueabi",
    "armv7-unknown-linux-gnueabihf",
    "armv7-unknown-linux-musleabi",
    "armv7-unknown-linux-musleabihf",
    "armv7-unknown-linux-ohos",
    "armv7-unknown-linux-uclibceabi",
    "armv7-unknown-linux-uclibceabihf",
    "armv7-unknown-netbsd-eabihf",
    "armv7-unknown-trusty",
    "armv7-wrs-vxworks-eabihf",
    "armv7a-kmc-solid_asp3-eabi",
    "armv7a-kmc-solid_asp3-eabihf",
    "armv7a-none-eabi",
    "armv7a-none-eabihf",
    "armv7a-nuttx-eabi",
    "armv7a-nuttx-eabihf",
    "armv7a-vex-v5",
    "armv7k-apple-watchos",
    "armv7r-none-eabi",
    "armv7r-none-eabihf",
    "armv7s-apple-ios",
    "armv8r-none-eabihf",
    "avr-none",
    "bpfeb-unknown-none",
    "bpfel-unknown-none",
    "csky-unknown-linux-gnuabiv2",
    "csky-unknown-linux-gnuabiv2hf",
    "hexagon-unknown-linux-musl",
    "hexagon-unknown-none-elf",
    "hexagon-unknown-qurt",
    "i386-apple-ios",
    "i586-unknown-linux-gnu",
    "i586-unknown-linux-musl",
    "i586-unknown-netbsd",
    "i586-unknown-redox",
    "i686-apple-darwin",
    "i686-linux-android",
    "i686-pc-nto-qnx700",
    "i686-pc-windows-gnu",
    "i686-pc-windows-gnullvm",
    "i686-pc-windows-msvc",
    "i686-unknown-freebsd",
    "i686-unknown-haiku",
    "i686-unknown-helenos",
    "i686-unknown-hurd-gnu",
    "i686-unknown-linux-gnu",
    "i686-unknown-linux-musl",
    "i686-unknown-netbsd",
    "i686-unknown-openbsd",
    "i686-unknown-uefi",
    "i686-uwp-windows-gnu",
    "i686-uwp-windows-msvc",
    "i686-win7-windows-gnu",
    "i686-win7-windows-msvc",
    "i686-wrs-vxworks",
    "loongarch32-unknown-none",
    "loongarch32-unknown-none-softfloat",
    "loongarch64-unknown-linux-gnu",
    "loongarch64-unknown-linux-musl",
    "loongarch64-unknown-linux-ohos",
    "loongarch64-unknown-none",
    "loongarch64-unknown-none-softfloat",
    "m68k-unknown-linux-gnu",
    "m68k-unknown-none-elf",
    "mips-mti-none-elf",
    "mips-unknown-linux-gnu",
    "mips-unknown-linux-musl",
    "mips-unknown-linux-uclibc",
    "mips64-openwrt-linux-musl",
    "mips64-unknown-linux-gnuabi64",
    "mips64-unknown-linux-muslabi64",
    "mips64el-unknown-linux-gnuabi64",
    "mips64el-unknown-linux-muslabi64",
    "mipsel-mti-none-elf",
    "mipsel-sony-psp",
    "mipsel-sony-psx",
    "mipsel-unknown-linux-gnu",
    "mipsel-unknown-linux-musl",
    "mipsel-unknown-linux-uclibc",
    "mipsel-unknown-netbsd",
    "mipsel-unknown-none",
    "mipsisa32r6-unknown-linux-gnu",
    "mipsisa32r6el-unknown-linux-gnu",
    "mipsisa64r6-unknown-linux-gnuabi64",
    "mipsisa64r6el-unknown-linux-gnuabi64",
    "msp430-none-elf",
    "nvptx64-nvidia-cuda",
    "powerpc-unknown-freebsd",
    "powerpc-unknown-helenos",
    "powerpc-unknown-linux-gnu",
    "powerpc-unknown-linux-gnuspe",
    "powerpc-unknown-linux-musl",
    "powerpc-unknown-linux-muslspe",
    "powerpc-unknown-netbsd",
    "powerpc-unknown-openbsd",
    "powerpc-wrs-vxworks",
    "powerpc-wrs-vxworks-spe",
    "powerpc64-ibm-aix",
    "powerpc64-unknown-freebsd",
    "powerpc64-unknown-linux-gnu",
    "powerpc64-unknown-linux-musl",
    "powerpc64-unknown-openbsd",
    "powerpc64-wrs-vxworks",
    "powerpc64le-unknown-freebsd",
    "powerpc64le-unknown-linux-gnu",
    "powerpc64le-unknown-linux-musl",
    "riscv32-wrs-vxworks",
    "riscv32e-unknown-none-elf",
    "riscv32em-unknown-none-elf",
    "riscv32emc-unknown-none-elf",
    "riscv32gc-unknown-linux-gnu",
    "riscv32gc-unknown-linux-musl",
    "riscv32i-unknown-none-elf",
    "riscv32im-risc0-zkvm-elf",
    "riscv32im-unknown-none-elf",
    "riscv32ima-unknown-none-elf",
    "riscv32imac-esp-espidf",
    "riscv32imac-unknown-none-elf",
    "riscv32imac-unknown-nuttx-elf",
    "riscv32imac-unknown-xous-elf",
    "riscv32imafc-esp-espidf",
    "riscv32imafc-unknown-none-elf",
    "riscv32imafc-unknown-nuttx-elf",
    "riscv32imc-esp-espidf",
    "riscv32imc-unknown-none-elf",
    "riscv32imc-unknown-nuttx-elf",
    "riscv64-linux-android",
    "riscv64-wrs-vxworks",
    "riscv64a23-unknown-linux-gnu",
    "riscv64gc-unknown-freebsd",
    "riscv64gc-unknown-fuchsia",
    "riscv64gc-unknown-hermit",
    "riscv64gc-unknown-linux-gnu",
    "riscv64gc-unknown-linux-musl",
    "riscv64gc-unknown-managarm-mlibc",
    "riscv64gc-unknown-netbsd",
    "riscv64gc-unknown-none-elf",
    "riscv64gc-unknown-nuttx-elf",
    "riscv64gc-unknown-openbsd",
    "riscv64gc-unknown-redox",
    "riscv64im-unknown-none-elf",
    "riscv64imac-unknown-none-elf",
    "riscv64imac-unknown-nuttx-elf",
    "s390x-unknown-linux-gnu",
    "s390x-unknown-linux-musl",
    "sparc-unknown-linux-gnu",
    "sparc-unknown-none-elf",
    "sparc64-unknown-helenos",
    "sparc64-unknown-linux-gnu",
    "sparc64-unknown-netbsd",
    "sparc64-unknown-openbsd",
    "sparcv9-sun-solaris",
    "thumbv4t-none-eabi",
    "thumbv5te-none-eabi",
    "thumbv6m-none-eabi",
    "thumbv6m-nuttx-eabi",
    "thumbv7a-nuttx-eabi",
    "thumbv7a-nuttx-eabihf",
    "thumbv7a-pc-windows-msvc",
    "thumbv7a-uwp-windows-msvc",
    "thumbv7em-none-eabi",
    "thumbv7em-none-eabihf",
    "thumbv7em-nuttx-eabi",
    "thumbv7em-nuttx-eabihf",
    "thumbv7m-none-eabi",
    "thumbv7m-nuttx-eabi",
    "thumbv7neon-linux-androideabi",
    "thumbv7neon-unknown-linux-gnueabihf",
    "thumbv7neon-unknown-linux-musleabihf",
    "thumbv8m.base-none-eabi",
    "thumbv8m.base-nuttx-eabi",
    "thumbv8m.main-none-eabi",
    "thumbv8m.main-none-eabihf",
    "thumbv8m.main-nuttx-eabi",
    "thumbv8m.main-nuttx-eabihf",
    "wasm32-unknown-emscripten",
    "wasm32-unknown-unknown",
    "wasm32-wali-linux-musl",
    "wasm32-wasip1",
    "wasm32-wasip1-threads",
    "wasm32-wasip2",
    "wasm32-wasip3",
    "wasm32v1-none",
    "wasm64-unknown-unknown",
    "x86_64-apple-darwin",
    "x86_64-apple-ios",
    "x86_64-apple-ios-macabi",
    "x86_64-apple-tvos",
    "x86_64-apple-watchos-sim",
    "x86_64-fortanix-unknown-sgx",
    "x86_64-linux-android",
    "x86_64-lynx-lynxos178",
    "x86_64-pc-cygwin",
    "x86_64-pc-nto-qnx710",
    "x86_64-pc-nto-qnx710_iosock",
    "x86_64-pc-nto-qnx800",
    "x86_64-pc-solaris",
    "x86_64-pc-windows-gnu",
    "x86_64-pc-windows-gnullvm",
    "x86_64-pc-windows-msvc",
    "x86_64-unikraft-linux-musl",
    "x86_64-unknown-dragonfly",
    "x86_64-unknown-freebsd",
    "x86_64-unknown-fuchsia",
    "x86_64-unknown-haiku",
    "x86_64-unknown-helenos",
    "x86_64-unknown-hermit",
    "x86_64-unknown-hurd-gnu",
    "x86_64-unknown-illumos",
    "x86_64-unknown-l4re-uclibc",
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnux32",
    "x86_64-unknown-linux-musl",
    "x86_64-unknown-linux-none",
    "x86_64-unknown-linux-ohos",
    "x86_64-unknown-managarm-mlibc",
    "x86_64-unknown-motor",
    "x86_64-unknown-netbsd",
    "x86_64-unknown-none",
    "x86_64-unknown-openbsd",
    "x86_64-unknown-redox",
    "x86_64-unknown-trusty",
    "x86_64-unknown-uefi",
    "x86_64-uwp-windows-gnu",
    "x86_64-uwp-windows-msvc",
    "x86_64-win7-windows-gnu",
    "x86_64-win7-windows-msvc",
    "x86_64-wrs-vxworks",
    "x86_64h-apple-darwin",
    "xtensa-esp32-espidf",
    "xtensa-esp32-none-elf",
    "xtensa-esp32s2-espidf",
    "xtensa-esp32s2-none-elf",
    "xtensa-esp32s3-espidf",
    "xtensa-esp32s3-none-elf",
];
const RUST_STYLE_TARGET_REASON: &str = "inauguration recognizes this Rust-style target triple as a compiler target identity, but it has no in-tree object-file emitter, linker driver, ABI lowering, or owned machine runtime for this target yet";
static TARGET_REGISTRY: OnceLock<Vec<TargetSpec>> = OnceLock::new();

#[must_use]
pub fn all_target_specs() -> &'static [TargetSpec] {
    TARGET_REGISTRY
        .get_or_init(build_target_registry)
        .as_slice()
}

#[must_use]
pub fn native_target_spec() -> TargetSpec {
    NATIVE_TARGET
}

#[must_use]
pub fn target_spec_by_name(name: &str) -> Option<TargetSpec> {
    all_target_specs()
        .iter()
        .find(|spec| spec.name == name)
        .cloned()
}

#[must_use]
pub fn native_subset_host_available() -> bool {
    cfg!(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    ))
}

fn build_target_registry() -> Vec<TargetSpec> {
    let mut specs = vec![NATIVE_TARGET];
    for &target in IN_EQUIVALENT_TARGETS {
        if specs.iter().any(|spec| spec.name == target) {
            continue;
        }
        specs.push(TargetSpec {
            name: target,
            implemented: implemented_target(target),
            stage: if target == "x86_64-unknown-linux-gnu" {
                "owned-native-and-object-subset"
            } else if target == "x86_64-pc-windows-msvc" {
                "owned-native-subset"
            } else if implemented_target(target) {
                "owned-object-subset"
            } else {
                "contract-only"
            },
            reason_code: if implemented_target(target) {
                NATIVE_OBJECT_SUBSET
            } else {
                NATIVE_BACKEND_NOT_IMPLEMENTED
            },
            reason: if target == "x86_64-unknown-linux-gnu" {
                "inauguration owns ELF64 relocatable object emission and a Linux syscall-exit executable subset for const-evaluable scalar entry functions on this target"
            } else if target == "aarch64-unknown-linux-gnu" {
                "inauguration owns ELF64 AArch64 relocatable object emission for const-evaluable scalar entry functions on this target"
            } else if target == "aarch64-apple-darwin" {
                "inauguration owns Mach-O static archive and .app bundle emission for const-evaluable scalar entry functions on this target"
            } else if target == "x86_64-pc-windows-msvc" {
                "inauguration owns PE32+ .exe emission for const-evaluable scalar entry functions on this target"
            } else if target == "armv7-unknown-linux-gnueabihf" {
                "inauguration owns ELF32 ARM relocatable object emission for const-evaluable scalar entry functions on this target"
            } else if target == "thumbv8m.main-none-eabi" {
                "inauguration owns ELF32 Thumb freestanding object emission for const-evaluable scalar entry functions on this target"
            } else if target == "wasm32-unknown-unknown" {
                "inauguration owns WebAssembly module emission for const-evaluable scalar entry functions on this target"
            } else {
                RUST_STYLE_TARGET_REASON
            },
            input_stage: "core-ir-or-textual-sil",
            artifact_kind: if target == "x86_64-unknown-linux-gnu" {
                "elf-relocatable-object+elf-executable"
            } else if target == "aarch64-unknown-linux-gnu" {
                "elf-relocatable-object"
            } else if target == "aarch64-apple-darwin" {
                "mach-o-static-archive+app-bundle"
            } else if target == "x86_64-pc-windows-msvc" {
                "pe-executable"
            } else if target == "armv7-unknown-linux-gnueabihf" {
                "elf32-relocatable-object"
            } else if target == "thumbv8m.main-none-eabi" {
                "elf32-relocatable-object+raw+uf2"
            } else if target == "wasm32-unknown-unknown" {
                "wasm-module"
            } else {
                "none"
            },
            host_triple: Some(target),
            backend_artifact_supported: implemented_target(target),
        });
    }
    specs
}

fn implemented_target(target: &str) -> bool {
    matches!(
        target,
        "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "aarch64-apple-darwin"
            | "armv7-unknown-linux-gnueabihf"
            | "thumbv8m.main-none-eabi"
            | "x86_64-pc-windows-msvc"
            | "wasm32-unknown-unknown"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_native_target() {
        let specs = all_target_specs();
        assert!(specs.len() > 1);
        assert_eq!(specs[0].name, "native");
    }

    #[test]
    fn registry_lists_rust_style_compiler_targets() {
        let specs = all_target_specs();
        assert_eq!(specs.len(), IN_EQUIVALENT_TARGETS.len() + 1);
        let names: Vec<&str> = specs.iter().map(|spec| spec.name).collect();
        for target in [
            "x86_64-unknown-none",
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "armv7-unknown-linux-gnueabihf",
            "thumbv8m.main-none-eabi",
            "x86_64-pc-windows-msvc",
            "wasm32-unknown-unknown",
            "riscv64gc-unknown-none-elf",
        ] {
            assert!(names.contains(&target), "{target}");
            let spec = target_spec_by_name(target).expect("target spec");
            if target == "x86_64-unknown-linux-gnu" {
                assert!(spec.implemented, "{target}");
                assert_eq!(spec.stage, "owned-native-and-object-subset");
                assert_eq!(spec.artifact_kind, "elf-relocatable-object+elf-executable");
                assert!(spec.backend_artifact_supported);
                continue;
            }
            if target == "aarch64-apple-darwin" {
                assert!(spec.implemented, "{target}");
                assert_eq!(spec.stage, "owned-object-subset");
                assert_eq!(spec.artifact_kind, "mach-o-static-archive+app-bundle");
                assert!(spec.backend_artifact_supported);
                continue;
            }
            if target == "x86_64-pc-windows-msvc" {
                assert!(spec.implemented, "{target}");
                assert_eq!(spec.stage, "owned-native-subset");
                assert_eq!(spec.artifact_kind, "pe-executable");
                assert!(spec.backend_artifact_supported);
                continue;
            }
            if target == "aarch64-unknown-linux-gnu" {
                assert!(spec.implemented, "{target}");
                assert_eq!(spec.stage, "owned-object-subset");
                assert_eq!(spec.artifact_kind, "elf-relocatable-object");
                assert!(spec.backend_artifact_supported);
                continue;
            }
            if target == "armv7-unknown-linux-gnueabihf" {
                assert!(spec.implemented, "{target}");
                assert_eq!(spec.stage, "owned-object-subset");
                assert_eq!(spec.artifact_kind, "elf32-relocatable-object");
                assert!(spec.backend_artifact_supported);
                continue;
            }
            if target == "thumbv8m.main-none-eabi" {
                assert!(spec.implemented, "{target}");
                assert_eq!(spec.stage, "owned-object-subset");
                assert_eq!(spec.artifact_kind, "elf32-relocatable-object+raw+uf2");
                assert!(spec.backend_artifact_supported);
                continue;
            }
            if target == "wasm32-unknown-unknown" {
                assert!(spec.implemented, "{target}");
                assert_eq!(spec.stage, "owned-object-subset");
                assert_eq!(spec.artifact_kind, "wasm-module");
                assert!(spec.backend_artifact_supported);
                continue;
            }
            assert!(!spec.implemented, "{target}");
            assert_eq!(spec.stage, "contract-only");
            assert_eq!(spec.reason_code, NATIVE_BACKEND_NOT_IMPLEMENTED);
            assert_eq!(spec.input_stage, "core-ir-or-textual-sil");
            assert_eq!(spec.artifact_kind, "none");
            assert!(!spec.backend_artifact_supported);
        }
    }

    #[cfg(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "linux", target_arch = "x86_64")
    )))]
    #[test]
    fn reports_native_target_contract_on_unsupported_hosts() {
        let spec = native_target_spec();
        assert!(!spec.implemented);
        assert_eq!(spec.stage, "contract-only");
        assert_eq!(spec.reason_code, NATIVE_BACKEND_NOT_IMPLEMENTED);
        assert_eq!(spec.input_stage, "core-ir-or-textual-sil");
        assert_eq!(spec.artifact_kind, "none");
        assert!(!spec.backend_artifact_supported);
        assert!(spec.host_triple.is_none());
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn reports_native_target_aarch64_subset_on_host() {
        let spec = native_target_spec();
        assert!(spec.implemented);
        assert_eq!(spec.stage, "owned-native-subset");
        assert_eq!(spec.reason_code, NATIVE_AARCH64_SUBSET);
        assert_eq!(spec.input_stage, "core-ir");
        assert_eq!(spec.artifact_kind, "mach-o-executable");
        assert_eq!(spec.host_triple, Some("aarch64-apple-darwin"));
        assert!(!spec.backend_artifact_supported);
    }
}

#[cfg(test)]
mod integer_semantics_tests {
    use super::integer_semantics;
    use crate::integer_semantics::IntegerSemantics::{Word32, Word64};

    #[test]
    fn implemented_native_integer_carriers_follow_the_selected_target() {
        for triple in [
            "thumbv8m.main-none-eabi",
            "i386-unknown-none",
            "i686-unknown-none",
            "armv7-unknown-linux-gnueabihf",
            "wasm32-unknown-unknown",
        ] {
            assert_eq!(integer_semantics(Some(triple)), Word32, "{triple}");
        }
        for triple in [
            None,
            Some("native"),
            Some("x86_64-unknown-none"),
            Some("aarch64-apple-darwin"),
        ] {
            assert_eq!(integer_semantics(triple), Word64);
        }
    }
}
