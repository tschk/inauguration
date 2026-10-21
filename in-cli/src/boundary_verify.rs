use crate::boundary_ir::{
    BoundaryLayout, BoundaryModule, BoundaryOwnership, BoundaryRepr, BoundaryTransfer,
    IN_ABI_VERSION,
};

const MANAGED_ABI_TYPES: &[&str] = &["Vec", "String", "Array", "seq", "string", "InBufU8"];

pub struct BoundaryVerifyReport {
    pub ok: bool,
    pub diagnostics: Vec<String>,
}

pub fn boundary_ir_verify(module: &BoundaryModule) -> BoundaryVerifyReport {
    let mut diagnostics = Vec::new();

    if module.abi_version != IN_ABI_VERSION {
        diagnostics.push(format!(
            "abi_version mismatch: expected {IN_ABI_VERSION}, got {}",
            module.abi_version
        ));
    }

    if module.module.is_empty() {
        diagnostics.push("module id is empty".to_string());
    }

    for layout in &module.layouts {
        verify_layout(layout, &mut diagnostics);
    }

    for symbol in &module.symbols {
        if symbol.name.is_empty() {
            diagnostics.push("symbol name is empty".to_string());
        }
        if symbol.signature_hash.is_empty() {
            diagnostics.push(format!("symbol `{}` missing signature_hash", symbol.name));
        }
        if matches!(symbol.ownership, BoundaryOwnership::Borrowed)
            && symbol.calling_convention != "c"
        {
            diagnostics.push(format!(
                "symbol `{}` borrowed ownership requires c calling convention",
                symbol.name
            ));
        }
    }

    if !module.layout_hash.is_empty() {
        let expected = module.compute_layout_hash();
        if module.layout_hash != expected {
            diagnostics.push("layout_hash does not match canonical layouts+symbols".to_string());
        }
    }

    BoundaryVerifyReport {
        ok: diagnostics.is_empty(),
        diagnostics,
    }
}

fn verify_layout(layout: &BoundaryLayout, diagnostics: &mut Vec<String>) {
    if layout.name.is_empty() {
        diagnostics.push("layout name is empty".to_string());
        return;
    }

    if layout.kind != "struct" && layout.kind != "enum" {
        diagnostics.push(format!(
            "layout `{}` unsupported kind `{}`",
            layout.name, layout.kind
        ));
    }

    if layout.size == 0 {
        diagnostics.push(format!("layout `{}` size must be non-zero", layout.name));
    }

    if layout.align == 0 || !layout.align.is_power_of_two() {
        diagnostics.push(format!(
            "layout `{}` align must be a power of two",
            layout.name
        ));
    }

    if layout.stride < layout.size {
        diagnostics.push(format!(
            "layout `{}` stride {} < size {}",
            layout.name, layout.stride, layout.size
        ));
    }

    if let Some(repr) = &layout.repr
        && matches!(repr, BoundaryRepr::Packed)
        && layout.align > 1
    {
        diagnostics.push(format!(
            "layout `{}` packed repr with align > 1",
            layout.name
        ));
    }

    let mut prev_end = 0u64;
    for field in &layout.fields {
        if field.offset < prev_end && prev_end > 0 {
            diagnostics.push(format!(
                "layout `{}` field `{}` overlaps prior field",
                layout.name, field.name
            ));
        }
        if is_managed_abi_type(&field.typ) {
            diagnostics.push(format!(
                "layout `{}` field `{}` uses managed type `{}` across ABI",
                layout.name, field.name, field.typ
            ));
        }
        if let Some(transfer) = &field.transfer
            && matches!(transfer, BoundaryTransfer::Borrow)
            && field.typ != "InSliceU8"
        {
            diagnostics.push(format!(
                "layout `{}` field `{}` borrow transfer requires InSliceU8",
                layout.name, field.name
            ));
        }
        prev_end = field.offset.saturating_add(size_hint_for_type(&field.typ));
    }
}

fn is_managed_abi_type(typ: &str) -> bool {
    let base = typ.split('<').next().unwrap_or(typ).trim();
    MANAGED_ABI_TYPES
        .iter()
        .any(|m| base.to_ascii_lowercase() == m.to_ascii_lowercase())
}

fn size_hint_for_type(typ: &str) -> u64 {
    match typ {
        "u8" | "i8" | "bool" => 1,
        "u16" | "i16" => 2,
        "u32" | "i32" | "float" => 4,
        "u64" | "i64" | "f64" | "InSliceU8" | "InBufU8" | "InBorrowToken" | "InArenaHandle" => 8,
        _ => 8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary_ir::{
        BoundaryField, BoundaryLayout, BoundaryOwnership, BoundaryRepr, BoundarySymbol,
    };

    fn sample_layout() -> BoundaryLayout {
        BoundaryLayout {
            name: "Person".to_string(),
            kind: "struct".to_string(),
            repr: Some(BoundaryRepr::C),
            size: 24,
            align: 8,
            stride: 24,
            fields: vec![
                BoundaryField {
                    name: "name".to_string(),
                    offset: 0,
                    typ: "InSliceU8".to_string(),
                    transfer: Some(BoundaryTransfer::Borrow),
                },
                BoundaryField {
                    name: "age".to_string(),
                    offset: 16,
                    typ: "u32".to_string(),
                    transfer: Some(BoundaryTransfer::Copy),
                },
            ],
        }
    }

    #[test]
    fn verify_accepts_valid_boundary_module() {
        let module = BoundaryModule {
            abi_version: IN_ABI_VERSION,
            module: "pkg.person".to_string(),
            layouts: vec![sample_layout()],
            symbols: vec![BoundarySymbol {
                name: "person_new".to_string(),
                signature_hash: "abc123".to_string(),
                ownership: BoundaryOwnership::ReturnsOwnedHandle,
                calling_convention: "c".to_string(),
            }],
            allocators: vec![],
            layout_hash: String::new(),
        }
        .with_layout_hash();
        let report = boundary_ir_verify(&module);
        assert!(report.ok, "{:?}", report.diagnostics);
    }

    #[test]
    fn verify_rejects_managed_container_field() {
        let mut layout = sample_layout();
        layout.fields.push(BoundaryField {
            name: "tags".to_string(),
            offset: 20,
            typ: "Vec".to_string(),
            transfer: None,
        });
        let module = BoundaryModule {
            abi_version: IN_ABI_VERSION,
            module: "pkg.person".to_string(),
            layouts: vec![layout],
            symbols: vec![],
            allocators: vec![],
            layout_hash: String::new(),
        };
        let report = boundary_ir_verify(&module);
        assert!(!report.ok);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.contains("managed type"))
        );
    }

    #[test]
    fn verify_rejects_stale_layout_hash() {
        let module = BoundaryModule {
            abi_version: IN_ABI_VERSION,
            module: "pkg.person".to_string(),
            layouts: vec![sample_layout()],
            symbols: vec![],
            allocators: vec![],
            layout_hash: "siphash-deadbeef".to_string(),
        };
        let report = boundary_ir_verify(&module);
        assert!(!report.ok);
    }
}
