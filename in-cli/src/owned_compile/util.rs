use crate::native_emit::NativeLinkage;
use std::path::Path;

use super::{CompileTarget, OwnedEmit};

pub fn target_label(target: CompileTarget) -> &'static str {
    match target {
        CompileTarget::Native => "native",
        CompileTarget::Jit => "jit",
    }
}

pub fn linkage_label(linkage: NativeLinkage) -> &'static str {
    match linkage {
        NativeLinkage::Executable => "executable",
        NativeLinkage::Dylib => "dylib",
        NativeLinkage::StaticLib => "staticlib",
    }
}

pub fn default_linkage_label() -> String {
    linkage_label(NativeLinkage::Executable).to_string()
}

pub fn emit_label(emit: Option<&OwnedEmit>) -> String {
    match emit {
        Some(OwnedEmit::Sci { base }) => format!("sci:0x{base:x}"),
        None => "default".to_string(),
    }
}

pub fn path_extension_is(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}

pub fn artifact_stem(path: &Path, fallback: &str) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}
