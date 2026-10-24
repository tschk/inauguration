use crate::boundary_emit::emit_component_metadata;
use crate::boundary_ir::{
    CapabilityDecl, CodeSection, ComponentMetadata, DataSection, MemoryRequirements, ObjectField,
    ObjectSchema, Provenance, SciLayout, ServiceExport, ServiceImport,
};
use crate::core_ir::{Decl, UnifiedModule};
use std::path::Path;

/// Emit a JSON component metadata sidecar alongside the compiled artifact.
/// Returns the path if emitted, or None if the module has no component declarations.
pub fn emit_component_metadata_sidecar(
    module: &UnifiedModule,
    entry: &str,
    out_path: &Path,
) -> Option<String> {
    emit_component_metadata_with_layout(module, entry, out_path, None)
}

pub fn emit_component_metadata_with_layout(
    module: &UnifiedModule,
    entry: &str,
    out_path: &Path,
    layout: Option<&SciLayout>,
) -> Option<String> {
    let meta_path = out_path.with_extension("component-metadata.json");
    // Find the first component declaration.
    let component = module.decls.iter().find_map(|d| match d {
        Decl::Component {
            name,
            target,
            deterministic,
            checkpoint,
            imports,
            exports,
            capabilities,
        } => Some((
            name.clone(),
            target.clone(),
            *deterministic,
            checkpoint.clone(),
            imports.clone(),
            exports.clone(),
            capabilities.clone(),
        )),
        _ => None,
    });
    let Some(component) = component else {
        // The artifact was successfully replaced: its previous generated
        // component metadata must not describe this component-free module.
        if let Err(error) = std::fs::remove_file(&meta_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "[metadata] warning: failed to remove stale component metadata {}: {error}",
                meta_path.display()
            );
        }
        return None;
    };

    let (name, target, deterministic, checkpoint, imports, exports, capabilities) = component;

    // Collect object schemas from struct declarations
    let object_schemas: Vec<ObjectSchema> = module
        .decls
        .iter()
        .filter_map(|d| match d {
            Decl::Struct {
                name: sn, fields, ..
            } => {
                let mut offset = 0u64;
                let obj_fields: Vec<ObjectField> = fields
                    .iter()
                    .map(|(fn_, typ)| {
                        let size = schema_field_size(typ);
                        let off = offset;
                        offset += size;
                        ObjectField {
                            name: fn_.clone(),
                            typ: schema_type_name(typ),
                            offset: off,
                            size,
                        }
                    })
                    .collect();
                let size = offset.next_multiple_of(8);
                Some(ObjectSchema {
                    name: sn.clone(),
                    fields: obj_fields,
                    size,
                    align: 8,
                })
            }
            _ => None,
        })
        .collect();

    let component_name = format!("{}", name);
    let comp = module
        .identity
        .package
        .as_deref()
        .map(|pkg| format!("{pkg}/{name}"))
        .unwrap_or_else(|| component_name);

    let metadata = ComponentMetadata {
        component: comp,
        target: target.clone(),
        entry: Some(entry.to_string()),
        code_sections: vec![CodeSection {
            name: ".text".to_string(),
            offset: layout.map_or(0, |layout| layout.text_offset),
            size: layout.map_or(0, |layout| layout.text_size),
            flags: "rx".to_string(),
        }],
        data_sections: layout.filter(|layout| layout.data_size != 0).map_or_else(
            Vec::new,
            |layout| {
                vec![DataSection {
                    name: ".data".to_string(),
                    offset: layout.data_offset,
                    size: layout.data_size,
                    flags: "rw".to_string(),
                }]
            },
        ),
        sci_layout: layout.cloned(),
        imports: imports
            .into_iter()
            .map(|i: crate::core_ir::ComponentImport| ServiceImport {
                name: i.name,
                interface: i.interface,
            })
            .collect(),
        exports: exports
            .into_iter()
            .map(|e: crate::core_ir::ComponentExport| ServiceExport {
                name: e.name,
                interface: e.interface,
            })
            .collect(),
        capabilities_required: capabilities
            .iter()
            .map(|c: &crate::core_ir::ComponentCapability| CapabilityDecl {
                name: c.name.clone(),
                capability_type: c.capability_type.clone(),
                args: c.args.clone(),
            })
            .collect(),
        capabilities_exported: capabilities
            .iter()
            .map(|c: &crate::core_ir::ComponentCapability| CapabilityDecl {
                name: c.name.clone(),
                capability_type: c.capability_type.clone(),
                args: c.args.clone(),
            })
            .collect(),
        object_schemas,
        memory: Some(MemoryRequirements {
            stack: 16384,
            heap: 0,
            static_data: layout.map_or(0, |layout| layout.data_size),
        }),
        checkpoint: checkpoint.clone(),
        deterministic,
        provenance: Provenance {
            compiler: "inauguration".to_string(),
            compiler_version: env!("CARGO_PKG_VERSION").to_string(),
            source_hash: String::new(),
        },
    };

    let json = emit_component_metadata(&metadata);
    match std::fs::write(&meta_path, &json) {
        Ok(()) => Some(meta_path.display().to_string()),
        Err(e) => {
            eprintln!(
                "[metadata] warning: failed to write component metadata {}: {e}",
                meta_path.display()
            );
            None
        }
    }
}

/// Get the size of a Core IR type for schema purposes.
pub fn schema_field_size(typ: &crate::core_ir::Typ) -> u64 {
    match typ {
        crate::core_ir::Typ::Int | crate::core_ir::Typ::Float | crate::core_ir::Typ::Bool => 8,
        crate::core_ir::Typ::String => 16,
        crate::core_ir::Typ::Void => 0,
        crate::core_ir::Typ::Array(_) => 16,
        crate::core_ir::Typ::Vector(_) => 24,
        crate::core_ir::Typ::Named(_) => 8,
        crate::core_ir::Typ::Generic(_) => 8,
    }
}

/// Convert a Core IR type to a human-readable schema type name.
pub fn schema_type_name(typ: &crate::core_ir::Typ) -> String {
    match typ {
        crate::core_ir::Typ::Int => "Int".to_string(),
        crate::core_ir::Typ::Float => "Float".to_string(),
        crate::core_ir::Typ::String => "String".to_string(),
        crate::core_ir::Typ::Bool => "Bool".to_string(),
        crate::core_ir::Typ::Void => "void".to_string(),
        crate::core_ir::Typ::Named(name) => name.clone(),
        crate::core_ir::Typ::Generic(name) => name.clone(),
        crate::core_ir::Typ::Array(elem) => format!("[{}]", schema_type_name(elem)),
        crate::core_ir::Typ::Vector(elem) => format!("Vec<{}>", schema_type_name(elem)),
    }
}
