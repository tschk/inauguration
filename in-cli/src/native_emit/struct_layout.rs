//! Native word fields in declaration order for aggregate value transfer.
use crate::core_ir::Typ;
use std::collections::HashMap;

pub(crate) fn scalar_fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
) -> Result<Vec<(String, Typ)>, String> {
    fields(structs, name, FieldLayout::Scalar)
}

/// The x86 owned ABI stores vectors as one pointer to an owned array.
pub(crate) fn x86_fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
) -> Result<Vec<(String, Typ)>, String> {
    fields(structs, name, FieldLayout::X86)
}

/// ARM aggregates transfer scalar arrays as pointer/length words.
pub(crate) fn arm_fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
) -> Result<Vec<(String, Typ)>, String> {
    fields(structs, name, FieldLayout::Arm)
}

#[derive(Clone, Copy)]
enum FieldLayout {
    Scalar,
    Arm,
    X86,
}

fn fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
    layout: FieldLayout,
) -> Result<Vec<(String, Typ)>, String> {
    fn walk(
        structs: &HashMap<String, Vec<(String, Typ)>>,
        name: &str,
        prefix: &str,
        visiting: &mut Vec<String>,
        out: &mut Vec<(String, Typ)>,
        layout: FieldLayout,
    ) -> Result<(), String> {
        if visiting.iter().any(|item| item == name) || visiting.len() >= 40 {
            return Err(format!("recursive scalar-field layout `{name}`"));
        }
        let fields = structs
            .get(name)
            .ok_or_else(|| format!("unknown scalar-field layout `{name}`"))?;
        visiting.push(name.into());
        for (field, typ) in fields {
            let field = if prefix.is_empty() {
                field.clone()
            } else {
                format!("{prefix}.{field}")
            };
            match typ.canonical() {
                typ @ (Typ::Int | Typ::Bool | Typ::String | Typ::Float) => out.push((field, typ)),
                typ @ Typ::Vector(_) if matches!(layout, FieldLayout::X86) => {
                    out.push((field, typ))
                }
                Typ::Array(item)
                    if matches!(layout, FieldLayout::Arm)
                        && matches!(
                            item.canonical(),
                            Typ::Int | Typ::Bool | Typ::String | Typ::Float
                        ) =>
                {
                    out.push((format!("{field}.ptr"), Typ::Int));
                    out.push((format!("{field}.len"), Typ::Int));
                }
                Typ::Named(name) => walk(structs, &name, &field, visiting, out, layout)?,
                _ => return Err(format!("unsupported scalar-field layout `{name}.{field}`")),
            }
        }
        visiting.pop();
        Ok(())
    }
    let mut fields = Vec::new();
    walk(structs, name, "", &mut Vec::new(), &mut fields, layout)?;
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arm_scalar_array_fields_are_pointer_length_pairs_in_declaration_order() {
        for elem in [Typ::Int, Typ::Bool, Typ::Float, Typ::String] {
            let structs = HashMap::from([
                (
                    "Holder".into(),
                    vec![("items".into(), Typ::Array(Box::new(elem)))],
                ),
                (
                    "Outer".into(),
                    vec![
                        ("before".into(), Typ::Int),
                        ("holder".into(), Typ::Named("Holder".into())),
                        ("after".into(), Typ::Bool),
                    ],
                ),
            ]);
            assert_eq!(
                arm_fields(&structs, "Outer").unwrap(),
                vec![
                    ("before".into(), Typ::Int),
                    ("holder.items.ptr".into(), Typ::Int),
                    ("holder.items.len".into(), Typ::Int),
                    ("after".into(), Typ::Bool),
                ]
            );
            assert!(scalar_fields(&structs, "Outer").is_err());
            assert!(x86_fields(&structs, "Outer").is_err());
        }
    }

    #[test]
    fn arm_array_fields_reject_unsupported_element_layouts() {
        for elem in [
            Typ::Void,
            Typ::Named("Unknown".into()),
            Typ::Array(Box::new(Typ::Int)),
            Typ::Vector(Box::new(Typ::Int)),
            Typ::Generic("T".into()),
        ] {
            let structs = HashMap::from([(
                "Holder".into(),
                vec![("items".into(), Typ::Array(Box::new(elem)))],
            )]);
            assert!(arm_fields(&structs, "Holder").is_err());
        }
    }

    #[test]
    fn x86_vector_fields_are_pointers_in_nested_layouts() {
        let structs = HashMap::from([
            (
                "Bag".into(),
                vec![("items".into(), Typ::Vector(Box::new(Typ::Int)))],
            ),
            (
                "Outer".into(),
                vec![
                    ("id".into(), Typ::Int),
                    ("bag".into(), Typ::Named("Bag".into())),
                ],
            ),
        ]);
        assert_eq!(
            x86_fields(&structs, "Outer").unwrap(),
            vec![
                ("id".into(), Typ::Int),
                ("bag.items".into(), Typ::Vector(Box::new(Typ::Int))),
            ]
        );
        assert!(scalar_fields(&structs, "Outer").is_err());
    }
}
