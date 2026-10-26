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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_target_label() {
        assert_eq!(target_label(CompileTarget::Native), "native");
        assert_eq!(target_label(CompileTarget::Jit), "jit");
    }

    #[test]
    fn test_linkage_label() {
        assert_eq!(linkage_label(NativeLinkage::Executable), "executable");
        assert_eq!(linkage_label(NativeLinkage::Dylib), "dylib");
        assert_eq!(linkage_label(NativeLinkage::StaticLib), "staticlib");
    }

    #[test]
    fn test_default_linkage_label() {
        assert_eq!(default_linkage_label(), "executable");
    }

    #[test]
    fn test_emit_label() {
        assert_eq!(
            emit_label(Some(&OwnedEmit::Sci { base: 0x1000 })),
            "sci:0x1000"
        );
        assert_eq!(emit_label(None), "default");
    }

    #[test]
    fn test_path_extension_is() {
        assert!(path_extension_is(Path::new("foo.txt"), "txt"));
        assert!(path_extension_is(Path::new("foo.TXT"), "txt"));
        assert!(!path_extension_is(Path::new("foo.txt"), "bin"));
        assert!(!path_extension_is(Path::new("foo"), "txt"));
    }

    #[test]
    fn test_artifact_stem() {
        assert_eq!(artifact_stem(Path::new("foo.txt"), "fallback"), "foo");
        assert_eq!(artifact_stem(Path::new(""), "fallback"), "fallback");
        assert_eq!(artifact_stem(Path::new(".hidden"), "fallback"), ".hidden");
    }
}
