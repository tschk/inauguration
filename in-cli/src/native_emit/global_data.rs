use crate::core_ir::{Decl, Expr, Typ, UnifiedModule};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub(crate) struct DataSection {
    pub(crate) bytes: Vec<u8>,
    pub(crate) pointer_fixups: Vec<(u64, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringLayout {
    Instring,
    Cstring,
}

/// Append an aligned immutable string payload and return its data-relative address.
pub(crate) fn append_string_payload(
    bytes: &mut Vec<u8>,
    value: &str,
    layout: StringLayout,
) -> Result<u64, String> {
    let start = bytes
        .len()
        .checked_add(7)
        .map(|size| size & !7)
        .ok_or("native string payload size overflow")?;
    let prefix = if layout == StringLayout::Instring {
        8
    } else {
        0
    };
    let suffix = if layout == StringLayout::Cstring {
        1
    } else {
        0
    };
    let end = start
        .checked_add(prefix)
        .and_then(|size| size.checked_add(value.len()))
        .and_then(|size| size.checked_add(suffix))
        .and_then(|size| size.checked_add(7))
        .map(|size| size & !7)
        .ok_or("native string payload size overflow")?;
    bytes
        .try_reserve(end - bytes.len())
        .map_err(|error| format!("native string payload allocation failed: {error}"))?;
    bytes.resize(end, 0);
    if prefix != 0 {
        bytes[start..start + 8].copy_from_slice(&(value.len() as u64).to_le_bytes());
    }
    bytes[start + prefix..start + prefix + value.len()].copy_from_slice(value.as_bytes());
    Ok(start as u64)
}

/// Rebase data pointers; callers select their target's maximum pointer address.
pub(crate) fn apply_data_relocations(
    bytes: &mut [u8],
    fixups: &[(u64, u64)],
    data_base: u64,
    address_limit: u64,
) -> Result<(), String> {
    let len = bytes.len() as u64;
    if len != 0
        && data_base
            .checked_add(len - 1)
            .is_none_or(|last| last > address_limit)
    {
        return Err("native data section exceeds target pointer address space".into());
    }
    let mut slots = std::collections::HashSet::with_capacity(fixups.len());
    for &(slot, target) in fixups {
        if slot % 8 != 0 || slot.checked_add(8).is_none_or(|end| end > len) {
            return Err(format!(
                "native data relocation slot {slot} is unaligned or out of bounds"
            ));
        }
        if !slots.insert(slot) {
            return Err(format!("native data relocation slot {slot} is duplicated"));
        }
        if target >= len {
            return Err(format!(
                "native data relocation target {target} is out of bounds"
            ));
        }
        let address = data_base
            .checked_add(target)
            .ok_or("native data relocation address overflow")?;
        if address > address_limit {
            return Err("native data relocation exceeds target pointer width".into());
        }
    }
    for &(slot, target) in fixups {
        bytes[slot as usize..slot as usize + 8]
            .copy_from_slice(&(data_base + target).to_le_bytes());
    }
    Ok(())
}

fn array_storage_size(init: Option<&Expr>) -> usize {
    let Some(Expr::ArrayLit(items)) = init else {
        return 16;
    };
    (items.len() + 2) * 8
        + items
            .iter()
            .filter(|item| matches!(item, Expr::ArrayLit(_)))
            .map(|item| array_storage_size(Some(item)))
            .sum::<usize>()
}

fn append_array<'a>(
    data: &mut Vec<u8>,
    elem: &Typ,
    init: Option<&'a Expr>,
    name: &str,
    strings: &mut Vec<(u64, &'a str)>,
) -> Result<(), String> {
    let items = match init {
        Some(Expr::ArrayLit(items)) => items.as_slice(),
        None => &[],
        _ => {
            return Err(format!(
                "unsupported native initializer for global `{name}`; expected an array literal"
            ));
        }
    };
    let elem = elem.canonical();
    if !matches!(
        elem,
        Typ::Int | Typ::Float | Typ::Bool | Typ::String | Typ::Array(_)
    ) {
        return Err(format!(
            "unsupported native initializer for global `{name}`; expected scalar string, numeric, boolean, or nested array elements"
        ));
    }
    let header = data.len();
    data.extend_from_slice(&(items.len() as u64).to_le_bytes());
    data.extend_from_slice(&(items.len() as u64).to_le_bytes());
    data.resize(data.len() + items.len() * 8, 0);
    for (index, item) in items.iter().enumerate() {
        let word = match (&elem, item) {
            (Typ::Int, Expr::IntLit(value)) => *value,
            (Typ::Float, Expr::FloatLit(value)) => value.0.to_bits() as i64,
            (Typ::Bool, Expr::BoolLit(value)) => i64::from(*value),
            (Typ::String, Expr::StringLit(value)) => {
                // Array slots carry absolute payload pointers, rebased like String globals.
                strings.push(((header + 16 + index * 8) as u64, value));
                0
            }
            (Typ::Array(child), Expr::ArrayLit(_)) => {
                // Relative child headers keep static data relocatable.
                let offset = (data.len() - header) as i64;
                append_array(data, child, Some(item), name, strings)?;
                offset
            }
            _ => {
                return Err(format!(
                    "unsupported native initializer for global `{name}`; expected scalar array elements or nested array literals"
                ));
            }
        };
        let slot = header + 16 + index * 8;
        data[slot..slot + 8].copy_from_slice(&word.to_le_bytes());
    }
    Ok(())
}

pub(crate) fn indexed_root(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Ident(name) => Some(name),
        Expr::Index { base, .. } => indexed_root(base),
        _ => None,
    }
}

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
            init,
            constant_value: false,
            ..
        } = decl
        {
            globals.insert(name.clone(), offset);
            if matches!(typ.canonical(), Typ::Array(_)) {
                offset += array_storage_size(init.as_deref()) as u64;
                continue;
            }
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

/// Initial values are snapshots, independent of the mutable runtime slots.
#[derive(Clone, Copy)]
enum InitialValue<'a> {
    Scalar(i64),
    String(&'a str),
}

fn eval_initial_value<'a>(
    typ: &Typ,
    init: Option<&'a Expr>,
    name: &str,
    earlier: &HashMap<&str, (Typ, InitialValue<'a>)>,
) -> Result<InitialValue<'a>, String> {
    let typ = typ.canonical();
    if let Some(Expr::Ident(source)) = init {
        let Some((source_type, value)) = earlier.get(source.as_str()) else {
            return Err(format!(
                "unsupported native initializer for global `{name}`; reference `{source}` requires an earlier initialized scalar or string global"
            ));
        };
        if source_type != &typ {
            return Err(format!(
                "unsupported native initializer for global `{name}`; initial-value reference `{source}` has type {source_type:?}, expected {typ:?}"
            ));
        }
        return Ok(*value);
    }
    match (&typ, init) {
        (Typ::String, Some(Expr::StringLit(value))) => Ok(InitialValue::String(value)),
        (Typ::String, None) => Ok(InitialValue::String("")),
        (Typ::Int, Some(Expr::IntLit(value))) => Ok(InitialValue::Scalar(*value)),
        (Typ::Float, Some(Expr::FloatLit(value))) => {
            Ok(InitialValue::Scalar(value.0.to_bits() as i64))
        }
        (Typ::Bool, Some(Expr::BoolLit(value))) => Ok(InitialValue::Scalar(i64::from(*value))),
        (Typ::Int | Typ::Float | Typ::Bool, None) => Ok(InitialValue::Scalar(0)),
        (Typ::String, _) => Err(format!(
            "unsupported native initializer for global `{name}`; expected a string literal or an earlier compatible global"
        )),
        _ => Err(format!(
            "unsupported native initializer for global `{name}`; expected an integer, float, or boolean constant or an earlier compatible global"
        )),
    }
}

pub(crate) fn build_data_section(
    module: &UnifiedModule,
    globals: &HashMap<String, u64>,
    layout: StringLayout,
) -> Result<DataSection, String> {
    if globals.is_empty() {
        return Ok(DataSection::default());
    }
    let mut data = Vec::new();
    let mut strings = Vec::new();
    let mut initial_values = HashMap::new();
    for decl in &module.decls {
        if let Decl::Global {
            name,
            init,
            typ,
            constant_value: false,
            ..
        } = decl
        {
            let offset = *globals.get(name).unwrap_or(&0);
            if matches!(
                typ.canonical(),
                Typ::Int | Typ::Float | Typ::Bool | Typ::String
            ) {
                let value = eval_initial_value(typ, init.as_deref(), name, &initial_values)?;
                match value {
                    InitialValue::String(value) => {
                        data.resize(offset as usize + 8, 0);
                        strings.push((offset, value));
                    }
                    InitialValue::Scalar(value) => {
                        data.resize(offset as usize, 0);
                        data.extend_from_slice(&value.to_le_bytes());
                    }
                }
                initial_values.insert(name.as_str(), (typ.canonical(), value));
                continue;
            }
            if let Typ::Array(elem) = typ.canonical() {
                data.resize(offset as usize, 0);
                append_array(&mut data, &elem, init.as_deref(), name, &mut strings)?;
                continue;
            }
            if let Some(kind) = crate::core_atomic::Kind::from_type(typ)
                && !module
                    .decls
                    .iter()
                    .any(|decl| matches!(decl,Decl::Struct{name,..} if name==kind.type_name()))
            {
                if module.decls.iter().any(
                    |decl| matches!(decl,Decl::Function{name,..} if name==&kind.intrinsic("new")),
                ) {
                    return Err(format!(
                        "unsupported native initializer for global `{name}`; atomic constructor is shadowed"
                    ));
                }
                let value = match init.as_deref() {
                    Some(Expr::Call { callee, args })
                        if matches!(callee.as_ref(),Expr::Ident(name) if name == &kind.intrinsic("new"))
                            && args.len() == 1 =>
                    {
                        match args[0] {
                            Expr::IntLit(value) => value,
                            _ => {
                                return Err(format!(
                                    "unsupported native initializer for global `{name}`; atomic value must be a constant integer"
                                ));
                            }
                        }
                    }
                    _ => {
                        return Err(format!(
                            "unsupported native initializer for global `{name}`; expected an atomic constructor"
                        ));
                    }
                };
                if !kind.wide() && !(0..=u32::MAX as i64).contains(&value) {
                    return Err(format!(
                        "unsupported native initializer for global `{name}`; atomic u32 value is out of range"
                    ));
                }
                data.resize(offset as usize, 0);
                data.extend_from_slice(&value.to_le_bytes());
                continue;
            }
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
    // All global slots and array storage precede payloads, regardless of declaration order.
    let mut pointer_fixups = Vec::with_capacity(strings.len());
    for (slot, value) in strings {
        let target = append_string_payload(&mut data, value, layout)?;
        pointer_fixups.push((slot, target));
    }
    Ok(DataSection {
        bytes: data,
        pointer_fixups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global(name: &str, typ: Typ, init: Option<Expr>) -> Decl {
        Decl::Global {
            name: name.into(),
            typ,
            init: init.map(Box::new),
            mutable: true,
            constant_value: false,
        }
    }

    #[test]
    fn string_arrays_keep_relative_children_and_relocatable_payload_slots() {
        let text = "λ\0雪";
        let module = UnifiedModule::new(vec![
            global("before", Typ::Int, Some(Expr::IntLit(20))),
            global(
                "words",
                Typ::Array(Box::new(Typ::String)),
                Some(Expr::ArrayLit(vec![
                    Expr::StringLit(text.into()),
                    Expr::StringLit(String::new()),
                ])),
            ),
            global(
                "nested",
                Typ::Array(Box::new(Typ::Array(Box::new(Typ::String)))),
                Some(Expr::ArrayLit(vec![Expr::ArrayLit(vec![Expr::StringLit(
                    "tail".into(),
                )])])),
            ),
            global("after", Typ::Int, Some(Expr::IntLit(22))),
        ]);
        let globals = collect_globals(&module, &HashMap::new());
        for layout in [StringLayout::Instring, StringLayout::Cstring] {
            let mut section = build_data_section(&module, &globals, layout).unwrap();
            let word = |data: &[u8], offset: u64| {
                u64::from_le_bytes(
                    data[offset as usize..offset as usize + 8]
                        .try_into()
                        .unwrap(),
                )
            };
            assert_eq!(word(&section.bytes, globals["before"]), 20);
            assert_eq!(word(&section.bytes, globals["after"]), 22);
            assert_eq!(word(&section.bytes, globals["words"]), 2);
            let nested = globals["nested"];
            let child = nested + word(&section.bytes, nested + 16);
            assert_eq!(word(&section.bytes, child), 1);
            assert_eq!(section.pointer_fixups.len(), 3);
            let slots = [globals["words"] + 16, globals["words"] + 24, child + 16];
            for (slot, expected) in slots.into_iter().zip([text, "", "tail"]) {
                let target = section
                    .pointer_fixups
                    .iter()
                    .find(|(at, _)| *at == slot)
                    .unwrap()
                    .1;
                assert!(target >= globals["after"] + 8);
                let payload = target as usize
                    + if layout == StringLayout::Instring {
                        8
                    } else {
                        0
                    };
                assert_eq!(
                    &section.bytes[payload..payload + expected.len()],
                    expected.as_bytes()
                );
                if layout == StringLayout::Instring {
                    assert_eq!(word(&section.bytes, target), expected.len() as u64);
                } else {
                    assert_eq!(section.bytes[payload + expected.len()], 0);
                }
            }
            apply_data_relocations(
                &mut section.bytes,
                &section.pointer_fixups,
                0x1000,
                u64::MAX,
            )
            .unwrap();
            for (slot, target) in &section.pointer_fixups {
                assert_eq!(word(&section.bytes, *slot), 0x1000 + target);
            }
        }
    }

    #[test]
    fn string_array_initializers_still_refuse_calls_and_wrong_element_types() {
        for value in [
            Expr::IntLit(42),
            Expr::Call {
                callee: Box::new(Expr::Ident("make".into())),
                args: vec![],
            },
        ] {
            let module = UnifiedModule::new(vec![global(
                "words",
                Typ::Array(Box::new(Typ::String)),
                Some(Expr::ArrayLit(vec![value])),
            )]);
            let globals = collect_globals(&module, &HashMap::new());
            assert!(build_data_section(&module, &globals, StringLayout::Instring).is_err());
        }
    }

    #[test]
    fn string_payload_fixups_preserve_interleaved_global_storage() {
        let text = "λ\0\n";
        let structs = HashMap::from([(
            "Pair".into(),
            vec![("left".into(), Typ::Int), ("right".into(), Typ::Int)],
        )]);
        let module = UnifiedModule::new(vec![
            global("first", Typ::String, Some(Expr::StringLit(text.into()))),
            global("count", Typ::Int, Some(Expr::IntLit(42))),
            global("pair", Typ::Named("Pair".into()), Some(Expr::IntLit(0))),
            global(
                "numbers",
                Typ::Array(Box::new(Typ::Int)),
                Some(Expr::ArrayLit(vec![Expr::IntLit(20), Expr::IntLit(22)])),
            ),
            global("last", Typ::String, Some(Expr::StringLit(String::new()))),
            global("sentinel", Typ::Int, Some(Expr::IntLit(7))),
        ]);
        let globals = collect_globals(&module, &structs);
        for layout in [StringLayout::Instring, StringLayout::Cstring] {
            let mut section = build_data_section(&module, &globals, layout).unwrap();
            assert_eq!(section.pointer_fixups.len(), 2);
            let word = |bytes: &[u8], offset: u64| {
                u64::from_le_bytes(
                    bytes[offset as usize..offset as usize + 8]
                        .try_into()
                        .unwrap(),
                )
            };
            assert_eq!(word(&section.bytes, globals["count"]), 42);
            assert_eq!(word(&section.bytes, globals["sentinel"]), 7);
            assert_eq!(word(&section.bytes, globals["pair.right"]), 0);
            assert_eq!(word(&section.bytes, globals["numbers"]), 2);
            assert_eq!(word(&section.bytes, globals["numbers"] + 16), 20);
            assert_eq!(word(&section.bytes, globals["numbers"] + 24), 22);
            let start = section.pointer_fixups[0].1 as usize;
            let empty = section.pointer_fixups[1].1 as usize;
            assert!(start >= globals["sentinel"] as usize + 8);
            assert_eq!(start % 8, 0);
            assert_eq!(empty % 8, 0);
            let prefix = if layout == StringLayout::Instring {
                8
            } else {
                0
            };
            if layout == StringLayout::Instring {
                assert_eq!(word(&section.bytes, start as u64), text.len() as u64);
                assert_eq!(word(&section.bytes, empty as u64), 0);
            } else {
                assert_eq!(section.bytes[start + text.len()], 0);
                assert_eq!(section.bytes[empty], 0);
            }
            assert_eq!(
                &section.bytes[start + prefix..start + prefix + text.len()],
                text.as_bytes()
            );
            apply_data_relocations(
                &mut section.bytes,
                &section.pointer_fixups,
                0x1000,
                u64::MAX,
            )
            .unwrap();
            assert_eq!(
                word(&section.bytes, globals["first"]),
                0x1000 + start as u64
            );
            assert_eq!(word(&section.bytes, globals["last"]), 0x1000 + empty as u64);
        }
    }

    #[test]
    fn default_empty_string_has_a_real_payload_and_calls_stay_unsupported() {
        let empty = UnifiedModule::new(vec![global("empty", Typ::String, None)]);
        let offsets = collect_globals(&empty, &HashMap::new());
        for layout in [StringLayout::Instring, StringLayout::Cstring] {
            let data = build_data_section(&empty, &offsets, layout).unwrap();
            assert_eq!(data.pointer_fixups, [(0, 8)]);
            assert!(data.bytes[8..].iter().all(|byte| *byte == 0));
        }
        for init in [
            Expr::IntLit(0),
            Expr::Call {
                callee: Box::new(Expr::Ident("make".into())),
                args: vec![],
            },
        ] {
            let module = UnifiedModule::new(vec![global("text", Typ::String, Some(init))]);
            let offsets = collect_globals(&module, &HashMap::new());
            assert!(
                build_data_section(&module, &offsets, StringLayout::Instring)
                    .unwrap_err()
                    .contains("expected a string literal")
            );
        }
    }

    #[test]
    fn data_pointer_relocation_validation_is_atomic_and_checks_target_width() {
        for (fixups, base, limit) in [
            (vec![(1, 16)], 0, u64::MAX),
            (vec![(24, 16)], 0, u64::MAX),
            (vec![(u64::MAX, 16)], 0, u64::MAX),
            (vec![(0, 16), (0, 16)], 0, u64::MAX),
            (vec![(0, 24)], 0, u64::MAX),
            (vec![(0, u64::MAX)], 0, u64::MAX),
            (vec![(0, 16)], u64::MAX - 8, u64::MAX),
            (vec![(0, 16)], u32::MAX as u64, u32::MAX as u64),
            (vec![(0, 16)], u32::MAX as u64 - 16, u32::MAX as u64),
            (vec![(0, 16), (8, 24)], 0x1000, u64::MAX),
        ] {
            let mut bytes = vec![0xAA; 24];
            assert!(apply_data_relocations(&mut bytes, &fixups, base, limit).is_err());
            assert_eq!(
                bytes, [0xAA; 24],
                "invalid metadata must not partially patch data"
            );
        }
        let mut bytes = vec![0; 24];
        apply_data_relocations(
            &mut bytes,
            &[(0, 16)],
            u32::MAX as u64 - 23,
            u32::MAX as u64,
        )
        .unwrap();
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            u32::MAX as u64 - 7
        );
        // Target offsets are byte addresses; only pointer slots require word alignment.
        apply_data_relocations(&mut bytes, &[(8, 17)], 0x1000, u64::MAX).unwrap();
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 0x1011);
    }

    #[test]
    fn nested_headers_use_relative_offsets_and_preserve_neighbors() {
        let module = crate::in_lang_parse::parse_in_source(
            "var before: Int = 7;\nvar items: [[Int]] = [[20,22],[]];\nvar after: Int = 9;\nfn main() -> Int { return 42; }",
        )
        .unwrap();
        let globals = collect_globals(&module, &HashMap::new());
        assert_eq!(globals["items"], 8);
        assert_eq!(globals["after"], 88);
        let data = build_data_section(&module, &globals, StringLayout::Instring)
            .unwrap()
            .bytes;
        let words = data
            .chunks_exact(8)
            .map(|word| i64::from_le_bytes(word.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(words, [7, 2, 2, 32, 64, 2, 2, 20, 22, 0, 0, 9]);
    }

    #[test]
    fn literal_array_headers_do_not_overlap_neighboring_globals() {
        let module = crate::in_lang_parse::parse_in_source("var before: Int = 7;\nvar items: [Int] = [20,22];\nvar after: Int = 9;\nfn main() -> Int { return 42; }").unwrap();
        let globals = collect_globals(&module, &HashMap::new());
        assert_eq!(globals["before"], 0);
        assert_eq!(globals["items"], 8);
        assert_eq!(globals["after"], 40);
        let data = build_data_section(&module, &globals, StringLayout::Instring)
            .unwrap()
            .bytes;
        let words: Vec<_> = data
            .chunks_exact(8)
            .map(|bytes| i64::from_le_bytes(bytes.try_into().unwrap()))
            .collect();
        assert_eq!(words, vec![7, 2, 2, 20, 22, 9]);
    }

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
                build_data_section(&module, &globals, StringLayout::Instring)
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
    fn unsupported_global_struct_assignments_are_refused_by_both_backends() {
        for field in ["Inner inner", "[Int] items"] {
            let source = format!(
                "struct Inner {{ Int value }}\nstruct Outer {{ {field} }}\nvar outer: Outer = 0;\nfn main() -> Int {{ outer = 0; return 42; }}"
            );
            let module = crate::in_lang_parse::parse_in_source(&source).unwrap();
            assert!(
                crate::native_emit::lower::lower_module(
                    &module,
                    "main",
                    crate::native_emit::NativeLinkage::Executable
                )
                .err()
                .unwrap()
                .contains("global struct values require a known scalar-field layout")
            );
            assert!(
                crate::native_emit::x86_64_lower::lower_module(&module, "main")
                    .err()
                    .unwrap()
                    .contains("global struct assignment needs a scalar-field 64-bit layout")
            );
        }
    }

    #[test]
    fn malformed_global_struct_sources_are_refused_by_both_backends() {
        for malformed in [
            Expr::Ident("value".into()),
            Expr::StructInit {
                name: "Point".into(),
                fields: vec![("x".into(), Expr::IntLit(20))],
            },
            Expr::StructInit {
                name: "Point".into(),
                fields: vec![
                    ("x".into(), Expr::IntLit(20)),
                    ("x".into(), Expr::IntLit(22)),
                ],
            },
        ] {
            let mut module = crate::in_lang_parse::parse_in_source("struct Point { Int x; Int y }\nvar point: Point = 0;\nfn main() -> Int { var value: Int = 7; point = Point { x: 20, y: 22 }; return point.x + point.y; }").unwrap();
            for decl in &mut module.decls {
                if let Decl::Function { body, .. } = decl {
                    for stmt in body {
                        if let crate::core_ir::Stmt::Assign(name, expr) = stmt
                            && name == "point"
                        {
                            *expr = malformed.clone();
                        }
                    }
                }
            }
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
    fn nested_global_receiver_layouts_are_refused_by_both_backends() {
        let mut module = crate::in_lang_parse::parse_in_source("struct Inner { Int value }\nclass Wrapper { inner: Inner; fn answer() -> Int { return inner.value; } }\nvar wrapper: Wrapper = 0;\nfn main() -> Int { return wrapper.answer(); }").unwrap();
        crate::lower_core::desugar_module(&mut module);
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
    fn parsed_global_literals_initialize_shared_data() {
        let module = crate::in_lang_parse::parse_in_source("var count: Int = 3;\nvar value: Float = 1.5;\nconst enabled: Bool = true;\nfn main() -> Int { return count; }").expect("parse");
        let globals = collect_globals(&module, &HashMap::new());
        let data = build_data_section(&module, &globals, StringLayout::Instring)
            .unwrap()
            .bytes;
        let words = data
            .chunks_exact(8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(words, vec![3, 1.5_f64.to_bits(), 1]);
        let empty =
            crate::in_lang_parse::parse_in_source("fn main() -> Int { return 0; }").expect("parse");
        assert!(
            build_data_section(
                &empty,
                &collect_globals(&empty, &HashMap::new()),
                StringLayout::Instring
            )
            .unwrap()
            .bytes
            .is_empty()
        );
    }
}

#[cfg(test)]
mod initial_reference_tests {
    use super::*;

    fn global(name: &str, typ: Typ, init: Option<Expr>) -> Decl {
        Decl::Global {
            name: name.into(),
            typ,
            init: init.map(Box::new),
            mutable: true,
            constant_value: false,
        }
    }

    fn word(bytes: &[u8], offset: u64) -> u64 {
        u64::from_le_bytes(
            bytes[offset as usize..offset as usize + 8]
                .try_into()
                .unwrap(),
        )
    }

    #[test]
    fn earlier_scalar_and_string_initializers_snapshot_values_without_rewriting_ir() {
        let text = "λ\0雪";
        let float_bits = 0x8000_0000_0000_0000;
        let module = UnifiedModule::new(vec![
            global("word", Typ::String, Some(Expr::StringLit(text.into()))),
            global("sentinel", Typ::Int, Some(Expr::IntLit(42))),
            global("selected", Typ::String, Some(Expr::Ident("word".into()))),
            global("first", Typ::Int, Some(Expr::IntLit(20))),
            global("second", Typ::Int, Some(Expr::Ident("first".into()))),
            global(
                "third",
                Typ::Named("i64".into()),
                Some(Expr::Ident("second".into())),
            ),
            global(
                "float",
                Typ::Float,
                Some(Expr::FloatLit(crate::core_ir::FloatVal(f64::from_bits(
                    float_bits,
                )))),
            ),
            global("float_copy", Typ::Float, Some(Expr::Ident("float".into()))),
            global("flag", Typ::Bool, Some(Expr::BoolLit(true))),
            global("flag_copy", Typ::Bool, Some(Expr::Ident("flag".into()))),
        ]);
        let original = module.clone();
        let offsets = collect_globals(&module, &HashMap::new());
        for layout in [StringLayout::Instring, StringLayout::Cstring] {
            let mut data = build_data_section(&module, &offsets, layout).unwrap();
            assert_eq!(word(&data.bytes, offsets["sentinel"]), 42);
            for name in ["first", "second", "third"] {
                assert_eq!(word(&data.bytes, offsets[name]), 20);
            }
            for name in ["float", "float_copy"] {
                assert_eq!(word(&data.bytes, offsets[name]), float_bits);
            }
            for name in ["flag", "flag_copy"] {
                assert_eq!(word(&data.bytes, offsets[name]), 1);
            }
            assert_eq!(data.pointer_fixups.len(), 2);
            let targets: Vec<_> = data
                .pointer_fixups
                .iter()
                .map(|(_, target)| *target)
                .collect();
            assert_ne!(targets[0], targets[1]);
            for target in &targets {
                let prefix = if layout == StringLayout::Instring {
                    8
                } else {
                    0
                };
                if prefix != 0 {
                    assert_eq!(word(&data.bytes, *target), text.len() as u64);
                }
                let start = *target as usize + prefix;
                assert_eq!(&data.bytes[start..start + text.len()], text.as_bytes());
                if layout == StringLayout::Cstring {
                    assert_eq!(data.bytes[start + text.len()], 0);
                }
            }
            apply_data_relocations(&mut data.bytes, &data.pointer_fixups, 0x1000, u64::MAX)
                .unwrap();
            let selected = word(&data.bytes, offsets["selected"]);
            // Mutating the source's runtime slot cannot change the copied slot.
            let source = offsets["word"] as usize;
            data.bytes[source..source + 8].copy_from_slice(&0u64.to_le_bytes());
            assert_eq!(word(&data.bytes, offsets["selected"]), selected);
            assert_eq!(word(&data.bytes, offsets["sentinel"]), 42);
        }
        assert_eq!(module, original);
    }

    #[test]
    fn earlier_default_initializers_keep_their_typed_zero_or_empty_value() {
        let mut decls = Vec::new();
        for (name, typ) in [
            ("int", Typ::Int),
            ("float", Typ::Float),
            ("bool", Typ::Bool),
            ("string", Typ::String),
        ] {
            decls.push(global(name, typ.clone(), None));
            decls.push(global(
                &format!("{name}_copy"),
                typ,
                Some(Expr::Ident(name.into())),
            ));
        }
        let module = UnifiedModule::new(decls);
        let offsets = collect_globals(&module, &HashMap::new());
        for layout in [StringLayout::Instring, StringLayout::Cstring] {
            let data = build_data_section(&module, &offsets, layout).unwrap();
            for name in [
                "int",
                "int_copy",
                "float",
                "float_copy",
                "bool",
                "bool_copy",
            ] {
                assert_eq!(word(&data.bytes, offsets[name]), 0);
            }
            assert_eq!(data.pointer_fixups.len(), 2);
            for (_, target) in data.pointer_fixups {
                assert_eq!(data.bytes[target as usize], 0);
            }
        }
    }

    #[test]
    fn unsupported_initial_value_references_and_calls_remain_refused() {
        let call = || Expr::Call {
            callee: Box::new(Expr::Ident("effect".into())),
            args: vec![],
        };
        let cases = [
            {
                let mut source = global("constant", Typ::Int, Some(Expr::IntLit(42)));
                if let Decl::Global { constant_value, .. } = &mut source {
                    *constant_value = true;
                }
                vec![
                    source,
                    global("copy", Typ::Int, Some(Expr::Ident("constant".into()))),
                ]
            },
            vec![
                global("copy", Typ::Int, Some(Expr::Ident("later".into()))),
                global("later", Typ::Int, Some(Expr::IntLit(42))),
            ],
            vec![global(
                "copy",
                Typ::Int,
                Some(Expr::Ident("missing".into())),
            )],
            vec![
                global("a", Typ::Int, Some(Expr::Ident("b".into()))),
                global("b", Typ::Int, Some(Expr::Ident("a".into()))),
            ],
            vec![
                global("source", Typ::Bool, Some(Expr::BoolLit(true))),
                global("copy", Typ::Int, Some(Expr::Ident("source".into()))),
            ],
            vec![
                global("source", Typ::String, Some(Expr::StringLit("text".into()))),
                global("copy", Typ::Int, Some(Expr::Ident("source".into()))),
            ],
            vec![
                global("copy", Typ::Int, Some(Expr::Ident("function".into()))),
                Decl::Function {
                    name: "function".into(),
                    params: vec![],
                    ret: Typ::Int,
                    body: vec![],
                    type_params: vec![],
                },
            ],
            vec![global("copy", Typ::Int, Some(call()))],
            vec![global(
                "copy",
                Typ::Int,
                Some(Expr::Conditional {
                    cond: Box::new(Expr::BoolLit(true)),
                    then_expr: Box::new(Expr::IntLit(42)),
                    else_expr: Box::new(call()),
                }),
            )],
            vec![
                global(
                    "source",
                    Typ::Array(Box::new(Typ::Int)),
                    Some(Expr::ArrayLit(vec![Expr::IntLit(42)])),
                ),
                global(
                    "copy",
                    Typ::Array(Box::new(Typ::Int)),
                    Some(Expr::Ident("source".into())),
                ),
            ],
            vec![
                global("source", Typ::Named("Point".into()), Some(Expr::IntLit(0))),
                global(
                    "copy",
                    Typ::Named("Point".into()),
                    Some(Expr::Ident("source".into())),
                ),
            ],
        ];
        for decls in cases {
            let module = UnifiedModule::new(decls);
            let offsets = collect_globals(&module, &HashMap::new());
            for layout in [StringLayout::Instring, StringLayout::Cstring] {
                let error = build_data_section(&module, &offsets, layout).unwrap_err();
                assert!(
                    error.contains("unsupported native initializer"),
                    "{module:?}: {error}"
                );
            }
        }
    }
}
