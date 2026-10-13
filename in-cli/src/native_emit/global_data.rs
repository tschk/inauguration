use crate::core_ir::{Decl, Expr, Typ, UnifiedModule};
use std::collections::HashMap;

/// Assign byte offsets in the writable data section, including scalar struct fields.
pub(crate) fn collect_globals(
    module: &UnifiedModule,
    structs: &HashMap<String, Vec<(String, Typ)>>,
) -> HashMap<String, u64> {
    let mut globals = HashMap::new();
    let mut offset = 0u64;
    for decl in &module.decls {
        if let Decl::Global {
            name,
            typ,
            constant_value: false,
            ..
        } = decl
        {
            globals.insert(name.clone(), offset);
            // Struct-typed globals reserve one word per known field so that
            // `{name}.{field}` sites lower to direct data-section slots.
            if let Typ::Named(struct_name) = typ {
                if let Some(fields) = structs.get(struct_name) {
                    for (idx, (field, _)) in fields.iter().enumerate() {
                        globals.insert(format!("{name}.{field}"), offset + idx as u64 * 8);
                    }
                    offset += (fields.len().max(1) as u64) * 8;
                    continue;
                }
            }
            offset += 8;
        }
    }
    globals
}

pub(crate) fn build_data_section(
    module: &UnifiedModule,
    globals: &HashMap<String, u64>,
) -> Result<Vec<u8>, String> {
    if globals.is_empty() {
        return Ok(Vec::new());
    }
    let mut data = Vec::new();
    for decl in &module.decls {
        if let Decl::Global {
            name,
            init,
            constant_value: false,
            ..
        } = decl
        {
            let offset = *globals.get(name).unwrap_or(&0);
            let val: i64 = match init.as_deref() {
                Some(Expr::IntLit(v)) => *v,
                Some(Expr::BoolLit(b)) => *b as i64,
                Some(Expr::FloatLit(crate::core_ir::FloatVal(v))) => v.to_bits() as i64,
                None => 0,
                // ponytail: runtime and aggregate initializers need an initialization pipeline, not fabricated zero values.
                _ => {
                    return Err(format!(
                        "unsupported native initializer for global `{name}`; expected an integer, float, or boolean constant"
                    ));
                }
            };
            while data.len() < offset as usize {
                data.push(0);
            }
            data.extend_from_slice(&val.to_le_bytes());
        }
    }
    // Struct-typed globals reserve extra field slots past their base word;
    // extend the section so every mapped slot exists in the image.
    let max_needed = globals.values().copied().max().unwrap_or(0) + 8;
    while data.len() < max_needed as usize {
        data.push(0);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_global_initializers_are_refused_by_both_backends() {
        let module = crate::in_lang_parse::parse_in_source("var count: Int = answer();\nfn answer() -> Int { return 42; }\nfn main() -> Int { return count; }").unwrap();
        assert!(
            crate::native_emit::lower::lower_module(
                &module,
                "main",
                crate::native_emit::NativeLinkage::Executable
            )
            .is_err()
        );
        assert!(crate::native_emit::x86_64_lower::lower_module(&module, "main").is_err());
    }

    #[test]
    fn optimized_global_field_projection_keeps_runtime_initializer_refusal() {
        for profile in [
            crate::emit_profile::EmitProfile::Default,
            crate::emit_profile::EmitProfile::Lean,
            crate::emit_profile::EmitProfile::Harden,
        ] {
            let mut module = crate::in_lang_parse::parse_in_source("struct Point { Int x; Int y }\nvar calls: Int = 0;\nfn effect() -> Int { calls += 1; return 0; }\nvar seed: Int = Point { x: 42, y: effect() }.x;\nfn main() -> Int { return seed + calls; }").unwrap();
            crate::core_opt::optimize_with_profile(&mut module.decls, Some("main"), profile);
            let globals = collect_globals(&module, &HashMap::new());
            assert!(
                build_data_section(&module, &globals)
                    .unwrap_err()
                    .contains("unsupported native initializer for global `seed`")
            );
            assert!(
                crate::native_emit::lower::lower_module(
                    &module,
                    "main",
                    crate::native_emit::NativeLinkage::Executable
                )
                .is_err()
            );
            assert!(crate::native_emit::x86_64_lower::lower_module(&module, "main").is_err());
        }
    }

    #[test]
    fn nested_global_struct_values_are_refused_by_both_backends() {
        for field in ["Inner inner", "[Int] items"] {
            for body in ["return outer;", "var copied: Outer = outer; return copied;"] {
                let source = format!(
                    "struct Inner {{ Int value }}\nstruct Outer {{ {field} }}\nvar outer: Outer = 0;\nfn main() -> Outer {{ {body} }}"
                );
                let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
                assert!(
                    crate::native_emit::lower::lower_module(
                        &module,
                        "main",
                        crate::native_emit::NativeLinkage::Executable
                    )
                    .is_err()
                );
                assert!(crate::native_emit::x86_64_lower::lower_module(&module, "main").is_err());
            }
        }
    }

    #[test]
    fn parsed_global_literals_initialize_shared_data() {
        let module = crate::in_lang_parse::parse_in_source("var count: Int = 3;\nvar value: Float = 1.5;\nconst enabled: Bool = true;\nfn main() -> Int { return count; }").expect("parse");
        let globals = collect_globals(&module, &HashMap::new());
        let data = build_data_section(&module, &globals).unwrap();
        let words = data
            .chunks_exact(8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(words, vec![3, 1.5_f64.to_bits(), 1]);
        let empty =
            crate::in_lang_parse::parse_in_source("fn main() -> Int { return 0; }").expect("parse");
        assert!(
            build_data_section(&empty, &collect_globals(&empty, &HashMap::new()))
                .unwrap()
                .is_empty()
        );
    }
}
