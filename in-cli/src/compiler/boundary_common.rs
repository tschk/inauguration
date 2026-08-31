//! Shared utilities for boundary language frontends.
//!
//! Most boundary fronts (Clojure, Crystal, D, Hare, Nim, Odin, VB) follow the
//! same scaffolding: read a file, parse source into a `UnifiedModule`, extract
//! an inline boundary annotation from the first line, and combine into a
//! `CompileArtifact`. This module factors that boilerplate into reusable helpers
//! so each frontend only needs to provide its language-specific parsing logic.

use crate::boundary_ir::{BoundaryModule, CompileArtifact};
use crate::core_ir::{Decl, Stmt, Typ, UnifiedModule};
use std::path::Path;

/// Read a source file and parse it with the given `parse_source` function.
pub fn parse_file_with<F>(path: &Path, parse_source: F) -> Result<UnifiedModule, String>
where
    F: FnOnce(&str) -> Result<UnifiedModule, String>,
{
    let src = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse_source(&src)
}

/// Read a source file and produce a `CompileArtifact` using the given parsing
/// and boundary-extraction functions.
pub fn parse_artifact_with<P, B>(
    path: &Path,
    parse_source: P,
    extract_boundary: B,
) -> Result<CompileArtifact, String>
where
    P: FnOnce(&str) -> Result<UnifiedModule, String>,
    B: FnOnce(&str) -> Option<BoundaryModule>,
{
    let src = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    artifact_from_source(&src, parse_source, extract_boundary)
}

/// Combine a parsed `UnifiedModule` with an optional boundary into a
/// `CompileArtifact`.
pub fn artifact_from_source<P, B>(
    src: &str,
    parse_source: P,
    extract_boundary: B,
) -> Result<CompileArtifact, String>
where
    P: FnOnce(&str) -> Result<UnifiedModule, String>,
    B: FnOnce(&str) -> Option<BoundaryModule>,
{
    let semantic = parse_source(src)?;
    let boundary = extract_boundary(src);
    Ok(match boundary {
        Some(boundary) => CompileArtifact::with_boundary(semantic, boundary),
        None => CompileArtifact::from_semantic(semantic),
    })
}

/// Extract an inline `BoundaryModule` JSON annotation from the first line of
/// source code. The annotation is detected by trying each prefix in order
/// (e.g. `"//? in_boundary"` then `"// in_boundary"`).
///
/// Returns `None` if no matching prefix is found or if JSON deserialization fails.
pub fn extract_boundary_from_comment(src: &str, prefixes: &[&str]) -> Option<BoundaryModule> {
    let line = src.lines().next()?;
    let trimmed = line.trim();
    let payload = prefixes
        .iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))?;
    let module: BoundaryModule = serde_json::from_str(payload.trim()).ok()?;
    Some(if module.layout_hash.is_empty() {
        module.with_layout_hash()
    } else {
        module
    })
}

/// If no `main` function is present in `decls`, append a default empty main.
/// This is the standard behavior for boundary frontends that require a main
/// entry point.
pub fn ensure_main(decls: &mut Vec<Decl>) {
    let has_main = decls
        .iter()
        .any(|d| matches!(d, Decl::Function { name, .. } if name == "main"));
    if !has_main {
        decls.push(Decl::Function {
            name: "main".to_string(),
            params: vec![],
            ret: Typ::Void,
            body: vec![Stmt::Return(None)],
            type_params: vec![],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_boundary_empty_src() {
        assert_eq!(extract_boundary_from_comment("", &["//? "]), None);
    }

    #[test]
    fn test_extract_boundary_no_match() {
        assert_eq!(extract_boundary_from_comment("some text", &["//? "]), None);
    }

    #[test]
    fn test_extract_boundary_invalid_json() {
        assert_eq!(
            extract_boundary_from_comment("//? in_boundary { invalid }", &["//? in_boundary "]),
            None
        );
    }

    #[test]
    fn test_extract_boundary_valid_json_no_hash() {
        let src =
            r#"//? in_boundary {"abi_version": 1, "module": "test", "layouts": [], "symbols": []}"#;
        let result = extract_boundary_from_comment(src, &["//? in_boundary "]).unwrap();
        assert_eq!(result.module, "test");
        assert!(!result.layout_hash.is_empty());
    }

    #[test]
    fn test_extract_boundary_valid_json_with_hash() {
        let src = r#"//? in_boundary {"abi_version": 1, "module": "test", "layouts": [], "symbols": [], "layout_hash": "custom_hash"}"#;
        let result = extract_boundary_from_comment(src, &["//? in_boundary "]).unwrap();
        assert_eq!(result.module, "test");
        assert_eq!(result.layout_hash, "custom_hash");
    }

    #[test]
    fn test_extract_boundary_leading_whitespace() {
        let src = r#"   //? in_boundary {"abi_version": 1, "module": "test", "layouts": [], "symbols": []}"#;
        let result = extract_boundary_from_comment(src, &["//? in_boundary "]).unwrap();
        assert_eq!(result.module, "test");
        assert!(!result.layout_hash.is_empty());
    }

    #[test]
    fn test_extract_boundary_multiple_prefixes() {
        let src =
            r#"// in_boundary {"abi_version": 1, "module": "test", "layouts": [], "symbols": []}"#;
        let result =
            extract_boundary_from_comment(src, &["//? in_boundary ", "// in_boundary "]).unwrap();
        assert_eq!(result.module, "test");
        assert!(!result.layout_hash.is_empty());
    }

    #[test]
    fn test_parse_file_with_read_error() {
        let path = Path::new("does_not_exist.txt");
        let result = parse_file_with(path, |_| unreachable!("Should not parse on read error"));

        match result {
            Err(e) => {
                assert!(e.starts_with("read does_not_exist.txt: "));
            }
            Ok(_) => panic!("Expected read error, got Ok"),
        }
    }
}
