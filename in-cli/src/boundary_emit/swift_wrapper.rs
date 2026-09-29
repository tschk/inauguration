use crate::boundary_ir::{BoundaryLayout, BoundaryModule, BoundaryOwnership};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwiftBoundaryEmit {
    pub wrapper: String,
    pub layout_probes: String,
}

pub fn emit_swift_boundary(module: &BoundaryModule) -> Result<SwiftBoundaryEmit, String> {
    if module.module.is_empty() {
        return Err("module id is empty".to_string());
    }

    Ok(SwiftBoundaryEmit {
        wrapper: emit_swift_wrapper(module)?,
        layout_probes: emit_swift_layout_probes(module)?,
    })
}

pub fn emit_swift_wrapper(module: &BoundaryModule) -> Result<String, String> {
    if module.module.is_empty() {
        return Err("module id is empty".to_string());
    }

    let import_name = super::swift_c_import_name(&module.module);
    let namespace = super::swift_namespace_name(&module.module);
    let mut out = String::new();
    out.push_str("import Foundation\n");
    out.push_str(&format!("import {import_name}\n\n"));
    out.push_str(&format!("public enum {namespace} {{\n"));
    out.push_str(&format!(
        "    public typealias SliceU8 = {import_name}.InSliceU8\n"
    ));
    out.push_str(&format!(
        "    public typealias BufU8 = {import_name}.InBufU8\n"
    ));
    out.push_str(&format!(
        "    public typealias BorrowToken = {import_name}.InBorrowToken\n"
    ));
    out.push_str(&format!(
        "    public typealias ArenaHandle = {import_name}.InArenaHandle\n"
    ));

    for layout in &module.layouts {
        if layout.name.is_empty() {
            return Err("layout name is empty".to_string());
        }
        out.push_str(&format!(
            "    public typealias {} = {import_name}.{}\n",
            layout.name, layout.name
        ));
    }

    if !module.symbols.is_empty() {
        out.push('\n');
        for symbol in &module.symbols {
            if symbol.name.is_empty() {
                return Err("symbol name is empty".to_string());
            }
            let swift_name = super::swift_symbol_name(&symbol.name);
            let return_type = swift_symbol_return_type(&symbol.ownership);
            out.push_str(&format!("    @_silgen_name(\"{}\")\n", symbol.name));
            out.push_str(&format!(
                "    public static func {swift_name}() -> {return_type}\n"
            ));
        }
    }

    out.push_str("}\n");
    Ok(out)
}

pub fn emit_swift_layout_probes(module: &BoundaryModule) -> Result<String, String> {
    if module.module.is_empty() {
        return Err("module id is empty".to_string());
    }
    if module.layouts.is_empty() {
        return Err("no layouts to probe".to_string());
    }

    let class_name = super::swift_probe_class_name(&module.module);
    let mut out = String::new();
    out.push_str("import XCTest\n\n");
    emit_swift_abi_types(&mut out);

    for layout in &module.layouts {
        emit_swift_probe_layout(&mut out, layout)?;
    }

    out.push_str(&format!("final class {class_name}: XCTestCase {{\n"));
    for layout in &module.layouts {
        emit_swift_probe_tests(&mut out, layout)?;
    }
    out.push_str("}\n");
    Ok(out)
}

fn swift_symbol_return_type(ownership: &BoundaryOwnership) -> &'static str {
    match ownership {
        BoundaryOwnership::ReturnsOwnedHandle => "UInt64",
        BoundaryOwnership::OwnedBuffer => "InBufU8",
        BoundaryOwnership::Borrowed => "InSliceU8",
    }
}

fn emit_swift_abi_types(out: &mut String) {
    out.push_str("@frozen\n");
    out.push_str("struct InSliceU8 {\n");
    out.push_str("    var ptr: UnsafePointer<UInt8>?\n");
    out.push_str("    var len: UInt64\n");
    out.push_str("}\n\n");
    out.push_str("@frozen\n");
    out.push_str("struct InBufU8 {\n");
    out.push_str("    var ptr: UnsafeMutablePointer<UInt8>?\n");
    out.push_str("    var len: UInt64\n");
    out.push_str("    var cap: UInt64\n");
    out.push_str("    var allocator_id: UInt64\n");
    out.push_str("}\n\n");
    out.push_str("@frozen\n");
    out.push_str("struct InBorrowToken {\n");
    out.push_str("    var arena_id: UInt64\n");
    out.push_str("    var generation: UInt64\n");
    out.push_str("    var start: UInt64\n");
    out.push_str("    var len: UInt64\n");
    out.push_str("    var flags: UInt64\n");
    out.push_str("}\n\n");
    out.push_str("@frozen\n");
    out.push_str("struct InArenaHandle {\n");
    out.push_str("    var id: UInt64\n");
    out.push_str("    var generation: UInt64\n");
    out.push_str("}\n\n");
}

fn emit_swift_probe_layout(out: &mut String, layout: &BoundaryLayout) -> Result<(), String> {
    if layout.name.is_empty() {
        return Err("layout name is empty".to_string());
    }

    out.push_str("@frozen\n");
    out.push_str(&format!("struct {} {{\n", layout.name));
    for field in &layout.fields {
        let swift_type = super::swift_type_name(&field.typ);
        out.push_str(&format!("    var {}: {}\n", field.name, swift_type));
    }
    out.push_str("}\n\n");
    Ok(())
}

fn emit_swift_probe_tests(out: &mut String, layout: &BoundaryLayout) -> Result<(), String> {
    if layout.name.is_empty() {
        return Err("layout name is empty".to_string());
    }

    let prefix = super::swift_probe_method_prefix(&layout.name);
    out.push_str(&format!("    func test{prefix}Size() {{\n"));
    out.push_str(&format!(
        "        XCTAssertEqual(MemoryLayout<{}>.size, {})\n",
        layout.name, layout.size
    ));
    out.push_str("    }\n\n");
    out.push_str(&format!("    func test{prefix}Alignment() {{\n"));
    out.push_str(&format!(
        "        XCTAssertEqual(MemoryLayout<{}>.alignment, {})\n",
        layout.name, layout.align
    ));
    out.push_str("    }\n\n");
    if !layout.fields.is_empty() {
        out.push_str(&format!("    func test{prefix}FieldOffsets() {{\n"));
        for field in &layout.fields {
            out.push_str(&format!(
                "        XCTAssertEqual(MemoryLayout<{}>.offset(of: \\.{}), {})\n",
                layout.name, field.name, field.offset
            ));
        }
        out.push_str("    }\n\n");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary_emit::sample_module;

    #[test]
    fn wrapper_imports_c_module_and_exposes_symbols() {
        let module = sample_module();
        let wrapper = emit_swift_wrapper(&module).expect("wrapper");
        assert!(wrapper.contains("import InBoundarySamplePerson"));
        assert!(wrapper.contains("public enum SamplePerson"));
        assert!(wrapper.contains("public typealias Person = InBoundarySamplePerson.Person"));
        assert!(wrapper.contains("@_silgen_name(\"person_new\")"));
        assert!(wrapper.contains("public static func personNew() -> UInt64"));
    }

    #[test]
    fn wrapper_rejects_empty_module_id() {
        let mut module = sample_module();
        module.module.clear();
        let err = emit_swift_wrapper(&module).unwrap_err();
        assert_eq!(err, "module id is empty");
    }

    #[test]
    fn wrapper_rejects_empty_layout_name() {
        let mut module = sample_module();
        module.layouts[0].name.clear();
        let err = emit_swift_wrapper(&module).unwrap_err();
        assert_eq!(err, "layout name is empty");
    }

    #[test]
    fn wrapper_rejects_empty_symbol_name() {
        let mut module = sample_module();
        module.symbols[0].name.clear();
        let err = emit_swift_wrapper(&module).unwrap_err();
        assert_eq!(err, "symbol name is empty");
    }

    #[test]
    fn layout_probes_emit_memory_layout_xctest() {
        let module = sample_module();
        let probes = emit_swift_layout_probes(&module).expect("probes");
        assert!(probes.contains("import XCTest"));
        assert!(probes.contains("final class InBoundarySamplePersonLayoutProbes: XCTestCase"));
        assert!(probes.contains("XCTAssertEqual(MemoryLayout<Person>.size, 24)"));
        assert!(probes.contains("XCTAssertEqual(MemoryLayout<Person>.alignment, 8)"));
        assert!(probes.contains("XCTAssertEqual(MemoryLayout<Person>.offset(of: \\.age), 16)"));
    }

    #[test]
    fn rejects_empty_layout_set_for_probes() {
        let mut module = sample_module();
        module.layouts.clear();
        let err = emit_swift_layout_probes(&module).expect_err("expected error");
        assert!(err.contains("no layouts"));
    }

    #[test]
    fn combined_emit_returns_wrapper_and_probes() {
        let module = sample_module();
        let emit = emit_swift_boundary(&module).expect("emit");
        assert!(emit.wrapper.contains("import InBoundarySamplePerson"));
        assert!(
            emit.layout_probes
                .contains("XCTAssertEqual(MemoryLayout<Person>.size, 24)")
        );
    }
}
