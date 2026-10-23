use crate::core_ir::UnifiedModule;
use serde::{Deserialize, Serialize};
use std::hash::Hasher;

pub const IN_ABI_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryRepr {
    #[serde(rename = "c")]
    C,
    #[serde(rename = "transparent")]
    Transparent,
    #[serde(rename = "packed")]
    Packed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryTransfer {
    #[serde(rename = "copy")]
    Copy,
    #[serde(rename = "borrow")]
    Borrow,
    #[serde(rename = "owned")]
    Owned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryOwnership {
    #[serde(rename = "returns-owned-handle")]
    ReturnsOwnedHandle,
    #[serde(rename = "borrowed")]
    Borrowed,
    #[serde(rename = "owned-buffer")]
    OwnedBuffer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryField {
    pub name: String,
    pub offset: u64,
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<BoundaryTransfer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryLayout {
    pub name: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repr: Option<BoundaryRepr>,
    pub size: u64,
    pub align: u64,
    pub stride: u64,
    pub fields: Vec<BoundaryField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundarySymbol {
    pub name: String,
    pub signature_hash: String,
    pub ownership: BoundaryOwnership,
    #[serde(default = "default_calling_convention")]
    pub calling_convention: String,
}

fn default_calling_convention() -> String {
    "c".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryAllocator {
    pub id: u64,
    pub kind: String,
    pub free_with: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryModule {
    pub abi_version: u32,
    pub module: String,
    pub layouts: Vec<BoundaryLayout>,
    pub symbols: Vec<BoundarySymbol>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allocators: Vec<BoundaryAllocator>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub layout_hash: String,
}

impl Default for BoundaryModule {
    fn default() -> Self {
        Self {
            abi_version: IN_ABI_VERSION,
            module: String::new(),
            layouts: Vec::new(),
            symbols: Vec::new(),
            allocators: Vec::new(),
            layout_hash: String::new(),
        }
    }
}

impl BoundaryModule {
    pub fn compute_layout_hash(&self) -> String {
        let mut payload = serde_json::Map::new();
        let layouts = serde_json::to_value(&self.layouts).unwrap_or(serde_json::Value::Null);
        let symbols = serde_json::to_value(&self.symbols).unwrap_or(serde_json::Value::Null);
        payload.insert("layouts".to_string(), layouts);
        payload.insert("symbols".to_string(), symbols);
        let canonical = serde_json::to_string(&payload).unwrap_or_default();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&canonical, &mut h);
        format!("siphash-{:016x}", h.finish())
    }

    pub fn with_layout_hash(mut self) -> Self {
        self.layout_hash = self.compute_layout_hash();
        self
    }
}

/// Frontend-owned proof for unary method dispatch. Keys are exact receiver
/// identities and canonical operators; values are exact callable targets or
/// explicit unresolved/unsupported reasons. Ordered storage also supports
/// deterministic resolved-artifact cache fingerprints.
pub type UnaryMethodBindings = std::collections::BTreeMap<(String, String), Result<String, String>>;

/// Merge retained proofs without silently replacing an unsupported or conflicting proof.
pub(crate) fn merge_unary_method_bindings(
    destination: &mut UnaryMethodBindings,
    incoming: UnaryMethodBindings,
) -> Result<(), String> {
    for (key, value) in &incoming {
        if let Some(previous) = destination.get(key)
            && previous != value
        {
            return Err(format!(
                "dependency unary-method binding conflict for `{}` `{}`",
                key.0, key.1
            ));
        }
    }
    destination.extend(incoming);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileArtifact {
    pub semantic: UnifiedModule,
    pub boundary: Option<BoundaryModule>,
    pub unary_methods: UnaryMethodBindings,
}

impl CompileArtifact {
    /// Restore semantic-only callers' runtime representations and discard private frontend proof.
    pub fn into_semantic(mut self) -> UnifiedModule {
        crate::core_ir::rust_references::erase_declarations(&mut self.semantic.decls);
        self.semantic
    }

    #[must_use]
    pub fn from_semantic(semantic: UnifiedModule) -> Self {
        Self {
            semantic,
            boundary: None,
            unary_methods: UnaryMethodBindings::new(),
        }
    }

    #[must_use]
    pub fn with_boundary(semantic: UnifiedModule, boundary: BoundaryModule) -> Self {
        Self {
            semantic,
            boundary: Some(boundary),
            unary_methods: UnaryMethodBindings::new(),
        }
    }
}

// ── Component Metadata (generic SCI-like sidecar) ──────────────

/// Code section descriptor for a compiled component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSection {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub flags: String,
}

/// Data section descriptor for a compiled component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSection {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub flags: String,
}

/// Capability declaration in component metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDecl {
    pub name: String,
    pub capability_type: String,
    pub args: Vec<String>,
}

/// Object schema (struct layout) referenced by a component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectSchema {
    pub name: String,
    pub fields: Vec<ObjectField>,
    pub size: u64,
    pub align: u64,
}

/// Field within an object schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectField {
    pub name: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub offset: u64,
    pub size: u64,
}

/// Memory requirements for a component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRequirements {
    pub stack: u64,
    pub heap: u64,
    pub static_data: u64,
}

/// Build provenance metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub compiler: String,
    pub compiler_version: String,
    pub source_hash: String,
}

/// Service import declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceImport {
    pub name: String,
    pub interface: String,
}

/// Service export declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceExport {
    pub name: String,
    pub interface: String,
}

/// Generic component metadata sidecar.
///
/// Emitted alongside compiled artifacts when the source contains
/// component declarations. This is the generic metadata shape
/// that component-loading contracts can consume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentMetadata {
    pub component: String,
    pub target: String,
    pub entry: Option<String>,
    pub code_sections: Vec<CodeSection>,
    pub data_sections: Vec<DataSection>,
    /// Exact raw SCI v2 layout, when this artifact was emitted as SCI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sci_layout: Option<SciLayout>,
    pub imports: Vec<ServiceImport>,
    pub exports: Vec<ServiceExport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities_required: Vec<CapabilityDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities_exported: Vec<CapabilityDecl>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub object_schemas: Vec<ObjectSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryRequirements>,
    pub checkpoint: String,
    pub deterministic: bool,
    pub provenance: Provenance,
}

/// Image-relative ranges of a raw SCI artifact, bound to its exact bytes.
///
/// Text includes read-only literals. Padding and binding tables are outside
/// both text and mutable data. Consumers choose and enforce page permissions;
/// this description does not grant authority or authenticate the producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SciLayout {
    pub version: u64,
    pub image_size: u64,
    pub load_base: u64,
    pub page_size: u64,
    pub text_offset: u64,
    pub text_size: u64,
    pub data_offset: u64,
    pub data_size: u64,
    pub binding_offset: u64,
    pub binding_size: u64,
    pub artifact_sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── BoundaryModule ────────────────────────────────────────────────

    #[test]
    fn boundary_module_default() {
        let m = BoundaryModule::default();
        assert_eq!(m.abi_version, IN_ABI_VERSION);
        assert!(m.module.is_empty());
        assert!(m.layouts.is_empty());
        assert!(m.symbols.is_empty());
        assert!(m.allocators.is_empty());
        assert!(m.layout_hash.is_empty());
    }

    #[test]
    fn boundary_module_layout_hash_deterministic() {
        let m = BoundaryModule {
            layouts: vec![BoundaryLayout {
                name: "Foo".to_string(),
                kind: "struct".to_string(),
                repr: Some(BoundaryRepr::C),
                size: 16,
                align: 8,
                stride: 16,
                fields: vec![BoundaryField {
                    name: "x".to_string(),
                    offset: 0,
                    typ: "i32".to_string(),
                    transfer: None,
                }],
            }],
            symbols: vec![BoundarySymbol {
                name: "foo_create".to_string(),
                signature_hash: "abc123".to_string(),
                ownership: BoundaryOwnership::ReturnsOwnedHandle,
                calling_convention: "c".to_string(),
            }],
            ..BoundaryModule::default()
        };
        let h1 = m.compute_layout_hash();
        let h2 = m.compute_layout_hash();
        assert_eq!(h1, h2);
        assert!(h1.starts_with("siphash-"));
    }

    #[test]
    fn boundary_module_with_layout_hash() {
        let m = BoundaryModule::default().with_layout_hash();
        assert!(!m.layout_hash.is_empty());
        assert!(m.layout_hash.starts_with("siphash-"));
    }

    #[test]
    fn boundary_module_different_layouts_different_hash() {
        let m1 = BoundaryModule::default().with_layout_hash();
        let m2 = BoundaryModule {
            symbols: vec![BoundarySymbol {
                name: "bar".to_string(),
                signature_hash: "xyz".to_string(),
                ownership: BoundaryOwnership::Borrowed,
                calling_convention: "c".to_string(),
            }],
            ..BoundaryModule::default()
        }
        .with_layout_hash();
        assert_ne!(m1.layout_hash, m2.layout_hash);
    }

    #[test]
    fn boundary_module_empty_vectors_hash() {
        let m = BoundaryModule {
            layouts: vec![],
            symbols: vec![],
            ..BoundaryModule::default()
        };
        let hash = m.compute_layout_hash();
        assert!(!hash.is_empty());
        assert!(hash.starts_with("siphash-"));
        assert_eq!(hash, m.compute_layout_hash());
    }

    // ─── Serde Round-Trip ──────────────────────────────────────────────

    #[test]
    fn boundary_repr_serde() {
        let json = serde_json::to_string(&BoundaryRepr::C).unwrap();
        assert_eq!(json, "\"c\"");
        let r: BoundaryRepr = serde_json::from_str(&json).unwrap();
        assert_eq!(r, BoundaryRepr::C);
    }

    #[test]
    fn boundary_transfer_serde() {
        let json = serde_json::to_string(&BoundaryTransfer::Borrow).unwrap();
        assert_eq!(json, "\"borrow\"");
        let t: BoundaryTransfer = serde_json::from_str(&json).unwrap();
        assert_eq!(t, BoundaryTransfer::Borrow);
    }

    #[test]
    fn boundary_ownership_serde() {
        let json = serde_json::to_string(&BoundaryOwnership::OwnedBuffer).unwrap();
        assert_eq!(json, "\"owned-buffer\"");
        let o: BoundaryOwnership = serde_json::from_str(&json).unwrap();
        assert_eq!(o, BoundaryOwnership::OwnedBuffer);
    }

    #[test]
    fn boundary_module_json_round_trip() {
        let m = BoundaryModule {
            abi_version: 1,
            module: "test".to_string(),
            layouts: vec![],
            symbols: vec![],
            allocators: vec![BoundaryAllocator {
                id: 1,
                kind: "arena".to_string(),
                free_with: "arena_free".to_string(),
            }],
            layout_hash: String::new(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let m2: BoundaryModule = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
    }

    // ─── CompileArtifact ───────────────────────────────────────────────

    #[test]
    fn compile_artifact_from_semantic() {
        let module = crate::core_ir::UnifiedModule::new(vec![]);
        let a = CompileArtifact::from_semantic(module.clone());
        assert!(a.boundary.is_none());
        assert!(a.unary_methods.is_empty());
        assert_eq!(a.semantic, module);
    }

    #[test]
    fn compile_artifact_with_boundary() {
        let module = crate::core_ir::UnifiedModule::new(vec![]);
        let bm = BoundaryModule::default();
        let a = CompileArtifact::with_boundary(module.clone(), bm.clone());
        assert!(a.boundary.is_some());
        assert!(a.unary_methods.is_empty());
        assert_eq!(a.boundary.unwrap(), bm);
    }

    // ─── default_calling_convention ────────────────────────────────────

    #[test]
    fn default_calling_convention_is_c() {
        assert_eq!(default_calling_convention(), "c");
    }

    // ─── ComponentMetadata ─────────────────────────────────────────────

    #[test]
    fn component_metadata_serde() {
        let cm = ComponentMetadata {
            component: "test".to_string(),
            target: "x86_64".to_string(),
            entry: Some("main".to_string()),
            code_sections: vec![CodeSection {
                name: ".text".to_string(),
                offset: 0,
                size: 100,
                flags: "rx".to_string(),
            }],
            data_sections: vec![],
            sci_layout: None,
            imports: vec![],
            exports: vec![],
            capabilities_required: vec![],
            capabilities_exported: vec![],
            object_schemas: vec![],
            memory: Some(MemoryRequirements {
                stack: 4096,
                heap: 0,
                static_data: 0,
            }),
            checkpoint: "none".to_string(),
            deterministic: true,
            provenance: Provenance {
                compiler: "in".to_string(),
                compiler_version: "0.1.0".to_string(),
                source_hash: "abc".to_string(),
            },
        };
        let json = serde_json::to_string(&cm).unwrap();
        let cm2: ComponentMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(cm, cm2);
    }

    // ─── IN_ABI_VERSION ────────────────────────────────────────────────

    #[test]
    fn abi_version_constant() {
        assert_eq!(IN_ABI_VERSION, 1);
    }
}

#[cfg(test)]
mod unary_binding_merge_tests {
    use super::*;
    #[test]
    fn unsupported_and_conflicting_proofs_are_not_overwritten() {
        let key = ("Row".into(), "-".into());
        let mut bindings: UnaryMethodBindings = [(key.clone(), Err("ambiguous".into()))]
            .into_iter()
            .collect();
        assert!(
            merge_unary_method_bindings(
                &mut bindings,
                [(key.clone(), Err("ambiguous".into()))]
                    .into_iter()
                    .collect()
            )
            .is_ok()
        );
        let previous = bindings.clone();
        assert!(
            merge_unary_method_bindings(
                &mut bindings,
                [(key, Ok("Row::neg".into()))].into_iter().collect()
            )
            .is_err()
        );
        assert_eq!(bindings, previous);
    }
}
