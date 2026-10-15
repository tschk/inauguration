use crate::boundary_ir::{BoundaryLayout, BoundaryModule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutProbes {
    pub rust: String,
    pub zig: String,
}

pub fn emit_layout_probes(module: &BoundaryModule) -> Result<LayoutProbes, String> {
    if module.module.is_empty() {
        return Err("module id is empty".to_string());
    }
    if module.layouts.is_empty() {
        return Err("no layouts to probe".to_string());
    }

    Ok(LayoutProbes {
        rust: emit_rust_probes(module)?,
        zig: emit_zig_probes(module)?,
    })
}

fn emit_rust_probes(module: &BoundaryModule) -> Result<String, String> {
    let mut out = String::new();
    out.push_str("use std::mem::{align_of, offset_of, size_of};\n\n");
    emit_rust_abi_types(&mut out);

    for layout in &module.layouts {
        emit_rust_layout(&mut out, layout)?;
    }

    Ok(out)
}

fn emit_rust_abi_types(out: &mut String) {
    out.push_str(
        "#[repr(C)]\npub struct InSliceU8 {\n    pub ptr: *const u8,\n    pub len: u64,\n}\n\n",
    );
    out.push_str("#[repr(C)]\npub struct InBufU8 {\n    pub ptr: *mut u8,\n    pub len: u64,\n    pub cap: u64,\n    pub allocator_id: u64,\n}\n\n");
    out.push_str("#[repr(C)]\npub struct InBorrowToken {\n    pub arena_id: u64,\n    pub generation: u64,\n    pub start: u64,\n    pub len: u64,\n    pub flags: u64,\n}\n\n");
    out.push_str(
        "#[repr(C)]\npub struct InArenaHandle {\n    pub id: u64,\n    pub generation: u64,\n}\n\n",
    );
}

fn emit_rust_layout(out: &mut String, layout: &BoundaryLayout) -> Result<(), String> {
    if layout.name.is_empty() {
        return Err("layout name is empty".to_string());
    }

    let repr = super::repr_attribute(layout.repr.as_ref());
    out.push_str(&format!("#[repr({repr})]\npub struct {} {{\n", layout.name));
    for field in &layout.fields {
        let rust_type = super::rust_type_name(&field.typ);
        out.push_str(&format!("    pub {}: {},\n", field.name, rust_type));
    }
    out.push_str("}\n\n");

    out.push_str("const _: () = {\n");
    out.push_str(&format!(
        "    assert!(size_of::<{}>() == {});\n",
        layout.name, layout.size
    ));
    out.push_str(&format!(
        "    assert!(align_of::<{}>() == {});\n",
        layout.name, layout.align
    ));
    for field in &layout.fields {
        out.push_str(&format!(
            "    assert!(offset_of!({}, {}) == {});\n",
            layout.name, field.name, field.offset
        ));
    }
    out.push_str("};\n\n");
    Ok(())
}

fn emit_zig_probes(module: &BoundaryModule) -> Result<String, String> {
    let mut out = String::new();
    emit_zig_abi_types(&mut out);

    for layout in &module.layouts {
        emit_zig_layout(&mut out, layout)?;
    }

    Ok(out)
}

fn emit_zig_abi_types(out: &mut String) {
    out.push_str("const InSliceU8 = extern struct {\n    ptr: [*]const u8,\n    len: u64,\n};\n\n");
    out.push_str("const InBufU8 = extern struct {\n    ptr: [*]u8,\n    len: u64,\n    cap: u64,\n    allocator_id: u64,\n};\n\n");
    out.push_str("const InBorrowToken = extern struct {\n    arena_id: u64,\n    generation: u64,\n    start: u64,\n    len: u64,\n    flags: u64,\n};\n\n");
    out.push_str(
        "const InArenaHandle = extern struct {\n    id: u64,\n    generation: u64,\n};\n\n",
    );
}

fn emit_zig_layout(out: &mut String, layout: &BoundaryLayout) -> Result<(), String> {
    if layout.name.is_empty() {
        return Err("layout name is empty".to_string());
    }

    out.push_str(&format!("const {} = extern struct {{\n", layout.name));
    for field in &layout.fields {
        let zig_type = super::zig_type_name(&field.typ);
        out.push_str(&format!("    {}: {},\n", field.name, zig_type));
    }
    out.push_str("};\n\n");

    out.push_str("comptime {\n");
    out.push_str(&format!(
        "    if (@sizeOf({}) != {}) @compileError(\"{} size mismatch\");\n",
        layout.name, layout.size, layout.name
    ));
    out.push_str(&format!(
        "    if (@alignOf({}) != {}) @compileError(\"{} align mismatch\");\n",
        layout.name, layout.align, layout.name
    ));
    for field in &layout.fields {
        out.push_str(&format!(
            "    if (@offsetOf({}, .{}) != {}) @compileError(\"{} field {} offset mismatch\");\n",
            layout.name, field.name, field.offset, layout.name, field.name
        ));
    }
    out.push_str("}\n\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary_emit::sample_module;

    #[test]
    fn probes_cover_rust_and_zig() {
        let module = sample_module();
        let probes = emit_layout_probes(&module).expect("probes");
        assert!(probes.rust.contains("assert!(size_of::<Person>() == 24)"));
        assert!(probes.zig.contains("@sizeOf(Person) != 24"));
    }

    #[test]
    fn rejects_empty_layout_set() {
        let mut module = sample_module();
        module.layouts.clear();
        let err = emit_layout_probes(&module).expect_err("expected error");
        assert!(err.contains("no layouts"));
    }

    #[test]
    fn rejects_empty_module_id() {
        let mut module = sample_module();
        module.module.clear();
        let err = emit_layout_probes(&module).expect_err("expected error");
        assert!(err.contains("module id is empty"));
    }

    #[test]
    fn rejects_empty_layout_name() {
        let mut module = sample_module();
        module.layouts[0].name.clear();
        let err = emit_layout_probes(&module).expect_err("expected error");
        assert!(err.contains("layout name is empty"));
    }

    #[test]
    fn probes_emit_abi_types() {
        let module = sample_module();
        let probes = emit_layout_probes(&module).expect("probes");

        assert!(probes.rust.contains("InSliceU8"));
        assert!(probes.rust.contains("InBufU8"));

        assert!(probes.zig.contains("InSliceU8"));
        assert!(probes.zig.contains("InBufU8"));
    }

    #[test]
    fn test_emit_rust_layout_success() {
        let module = sample_module();
        let layout = &module.layouts[0];
        let mut out = String::new();
        emit_rust_layout(&mut out, layout).expect("emit_rust_layout");

        assert!(out.contains("#[repr(c)]\npub struct Person {"));
        assert!(out.contains("pub name: InSliceU8"));
        assert!(out.contains("pub age: u32"));
        assert!(out.contains("assert!(size_of::<Person>() == 24);"));
        assert!(out.contains("assert!(align_of::<Person>() == 8);"));
        assert!(out.contains("assert!(offset_of!(Person, name) == 0);"));
        assert!(out.contains("assert!(offset_of!(Person, age) == 16);"));
    }

    #[test]
    fn test_emit_rust_layout_empty_name() {
        let module = sample_module();
        let mut layout = module.layouts[0].clone();
        layout.name.clear();

        let mut out = String::new();
        let err = emit_rust_layout(&mut out, &layout).expect_err("expected error");
        assert!(err.contains("layout name is empty"));
    }
}
