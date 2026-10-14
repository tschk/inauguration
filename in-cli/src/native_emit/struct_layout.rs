//! Scalar leaves in declaration order for aggregate value transfer.
use crate::core_ir::Typ;
use std::collections::HashMap;

pub(crate) fn scalar_fields(
    structs: &HashMap<String, Vec<(String, Typ)>>,
    name: &str,
) -> Result<Vec<(String, Typ)>, String> {
    fn walk(
        structs: &HashMap<String, Vec<(String, Typ)>>,
        name: &str,
        prefix: &str,
        visiting: &mut Vec<String>,
        out: &mut Vec<(String, Typ)>,
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
                Typ::Named(name) => walk(structs, &name, &field, visiting, out)?,
                _ => return Err(format!("unsupported scalar-field layout `{name}.{field}`")),
            }
        }
        visiting.pop();
        Ok(())
    }
    let mut fields = Vec::new();
    walk(structs, name, "", &mut Vec::new(), &mut fields)?;
    Ok(fields)
}
