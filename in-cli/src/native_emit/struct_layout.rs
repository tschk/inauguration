//! Native word fields in declaration order for aggregate value transfer.
use crate::core_ir::Typ;
use std::collections::HashMap;

pub(crate) fn scalar_fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
) -> Result<Vec<(String, Typ)>, String> {
    fields(structs, name, false)
}

/// The x86 owned ABI stores vectors as one pointer to an owned array.
pub(crate) fn x86_fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
) -> Result<Vec<(String, Typ)>, String> {
    fields(structs, name, true)
}

fn fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
    vectors: bool,
) -> Result<Vec<(String, Typ)>, String> {
    fn walk(
        structs: &HashMap<String, Vec<(String, Typ)>>,
        name: &str,
        prefix: &str,
        visiting: &mut Vec<String>,
        out: &mut Vec<(String, Typ)>,
        vectors: bool,
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
                typ @ Typ::Vector(_) if vectors => out.push((field, typ)),
                Typ::Named(name) => walk(structs, &name, &field, visiting, out, vectors)?,
                _ => return Err(format!("unsupported scalar-field layout `{name}.{field}`")),
            }
        }
        visiting.pop();
        Ok(())
    }
    let mut fields = Vec::new();
    walk(structs, name, "", &mut Vec::new(), &mut fields, vectors)?;
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

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
