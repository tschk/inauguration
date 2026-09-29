use crate::boundary_ir::{BoundaryLayout, BoundaryModule, BoundaryRepr};

pub fn emit_c_header(module: &BoundaryModule) -> Result<String, String> {
    if module.module.is_empty() {
        return Err("module id is empty".to_string());
    }

    let guard = super::guard_token(&module.module);
    let mut out = String::new();
    out.push_str(&format!("#ifndef {guard}\n"));
    out.push_str(&format!("#define {guard}\n\n"));
    out.push_str("#include <stdint.h>\n");
    out.push_str("#include \"in_abi.h\"\n\n");
    out.push_str("#if defined(__GNUC__) || defined(__clang__)\n");
    out.push_str("#define IN_BOUNDARY_PACKED __attribute__((packed))\n");
    out.push_str("#else\n");
    out.push_str("#define IN_BOUNDARY_PACKED\n");
    out.push_str("#endif\n\n");

    for layout in &module.layouts {
        emit_layout(&mut out, layout)?;
    }

    out.push_str("\n#endif\n");
    Ok(out)
}

fn emit_layout(out: &mut String, layout: &BoundaryLayout) -> Result<(), String> {
    if layout.name.is_empty() {
        return Err("layout name is empty".to_string());
    }

    if layout.kind != "struct" && layout.kind != "enum" {
        return Err(format!(
            "layout `{}` unsupported kind `{}`",
            layout.name, layout.kind
        ));
    }

    let packed = matches!(layout.repr, Some(BoundaryRepr::Packed));
    if packed {
        out.push_str("typedef IN_BOUNDARY_PACKED struct ");
    } else {
        out.push_str("typedef struct ");
    }
    out.push_str(&layout.name);
    out.push_str(" {\n");

    if layout.fields.is_empty() {
        out.push_str(&format!("    uint8_t _reserved[{}];\n", layout.size));
    } else {
        for field in &layout.fields {
            let c_type = super::c_type_name(&field.typ);
            out.push_str(&format!("    {} {};\n", c_type, field.name));
        }
    }

    out.push_str(&format!("}} {};\n\n", layout.name));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary_emit::sample_module;
    use crate::boundary_ir::BoundaryRepr;

    #[test]
    fn header_guard_uses_module_token() {
        let module = sample_module();
        let header = emit_c_header(&module).expect("header");
        assert!(header.contains("#ifndef IN_BOUNDARY_SAMPLE_PERSON_H"));
        assert!(header.contains("#define IN_BOUNDARY_SAMPLE_PERSON_H"));
    }

    #[test]
    fn emit_empty_module_id() {
        let mut module = sample_module();
        module.module = "".to_string();
        let err = emit_c_header(&module).unwrap_err();
        assert_eq!(err, "module id is empty");
    }

    #[test]
    fn emit_empty_layout_name() {
        let mut module = sample_module();
        module.layouts[0].name = "".to_string();
        let err = emit_c_header(&module).unwrap_err();
        assert_eq!(err, "layout name is empty");
    }

    #[test]
    fn emit_unsupported_layout_kind() {
        let mut module = sample_module();
        module.layouts[0].kind = "union".to_string();
        let err = emit_c_header(&module).unwrap_err();
        assert_eq!(err, "layout `Person` unsupported kind `union`");
    }

    #[test]
    fn emit_packed_layout() {
        let mut module = sample_module();
        module.layouts[0].repr = Some(BoundaryRepr::Packed);
        let header = emit_c_header(&module).expect("header");
        assert!(header.contains("typedef IN_BOUNDARY_PACKED struct Person {"));
    }

    #[test]
    fn emit_empty_fields_reserved() {
        let mut module = sample_module();
        module.layouts[0].fields.clear();
        module.layouts[0].size = 42;
        let header = emit_c_header(&module).expect("header");
        assert!(header.contains("uint8_t _reserved[42];"));
    }

    #[test]
    fn c_header_snapshot_matches() {
        let module = sample_module();
        let header = emit_c_header(&module).expect("header");
        let expected = "#ifndef IN_BOUNDARY_SAMPLE_PERSON_H\n\
                        #define IN_BOUNDARY_SAMPLE_PERSON_H\n\n\
                        #include <stdint.h>\n\
                        #include \"in_abi.h\"\n\n\
                        #if defined(__GNUC__) || defined(__clang__)\n\
                        #define IN_BOUNDARY_PACKED __attribute__((packed))\n\
                        #else\n\
                        #define IN_BOUNDARY_PACKED\n\
                        #endif\n\n\
                        typedef struct Person {\n\
                        \x20   InSliceU8 name;\n\
                        \x20   uint32_t age;\n\
                        } Person;\n\n\n\
                        #endif\n";
        assert_eq!(header, expected);
    }
}
