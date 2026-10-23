use crate::boundary_ir::{BoundaryModule, ComponentMetadata};

pub fn emit_abi_manifest(module: &BoundaryModule) -> String {
    emit_abi_manifest_with_package(module, None)
}

pub fn emit_abi_manifest_with_package(module: &BoundaryModule, package: Option<&str>) -> String {
    let module = super::prepared_module(module);
    let mut value = serde_json::to_value(&module).unwrap_or(serde_json::Value::Null);
    if let Some(package) = package
        && !package.is_empty()
        && let serde_json::Value::Object(map) = &mut value
    {
        map.insert(
            "package".to_string(),
            serde_json::Value::String(package.to_string()),
        );
    }
    serde_json::to_string_pretty(&value).unwrap_or_default()
}

/// Emit component metadata as a JSON sidecar.
pub fn emit_component_metadata(metadata: &ComponentMetadata) -> String {
    serde_json::to_string_pretty(metadata).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary_emit::sample_module;

    #[test]
    fn manifest_includes_layout_hash() {
        let module = sample_module();
        let manifest = emit_abi_manifest(&module);
        let parsed: BoundaryModule = serde_json::from_str(&manifest).expect("json");
        assert!(!parsed.layout_hash.is_empty());
        assert!(parsed.layout_hash.starts_with("siphash-"));
    }

    #[test]
    fn manifest_omits_absent_package_identity() {
        let module = sample_module();
        let manifest = emit_abi_manifest(&module);
        let parsed: serde_json::Value = serde_json::from_str(&manifest).expect("json");
        assert!(parsed.get("package").is_none());
    }

    #[test]
    fn manifest_includes_package_identity() {
        let module = sample_module();
        let manifest = emit_abi_manifest_with_package(&module, Some("my-org/my-pkg"));
        let parsed: serde_json::Value = serde_json::from_str(&manifest).expect("json");
        assert_eq!(
            parsed.get("package").and_then(|v| v.as_str()),
            Some("my-org/my-pkg")
        );
    }

    #[test]
    fn component_metadata_emits_valid_json() {
        let metadata = ComponentMetadata {
            component: "test_component".to_string(),
            target: "wasm32-unknown-unknown".to_string(),
            entry: Some("main".to_string()),
            code_sections: vec![],
            data_sections: vec![],
            sci_layout: None,
            imports: vec![],
            exports: vec![],
            capabilities_required: vec![],
            capabilities_exported: vec![],
            object_schemas: vec![],
            memory: None,
            checkpoint: "test_checkpoint".to_string(),
            deterministic: true,
            provenance: crate::boundary_ir::Provenance {
                compiler: "test_compiler".to_string(),
                compiler_version: "1.0.0".to_string(),
                source_hash: "abcd1234".to_string(),
            },
        };
        let json = emit_component_metadata(&metadata);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");

        assert_eq!(
            parsed.get("component").and_then(|v| v.as_str()),
            Some("test_component")
        );
        assert_eq!(
            parsed.get("target").and_then(|v| v.as_str()),
            Some("wasm32-unknown-unknown")
        );
        assert_eq!(parsed.get("entry").and_then(|v| v.as_str()), Some("main"));
        assert_eq!(
            parsed.get("deterministic").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            parsed
                .get("provenance")
                .and_then(|v| v.as_object())
                .map(|p| p.len()),
            Some(3)
        );
    }
}
