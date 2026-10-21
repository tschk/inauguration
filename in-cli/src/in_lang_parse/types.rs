use super::util::{split_and_trim, trim};
use crate::core_ir::Typ;

pub(crate) fn parse_in_type(s: &str) -> Typ {
    let s = trim(s);
    if s == "void" || s == "Void" || s == "VOID" || s == "()" {
        return Typ::Void;
    }
    if s.starts_with('[') && s.ends_with(']') {
        return Typ::Array(Box::new(parse_in_type(&s[1..s.len() - 1])));
    }
    match s {
        "Int" => Typ::Int,
        "Float" => Typ::Float,
        "String" => Typ::String,
        "Bool" => Typ::Bool,
        "Void" => Typ::Void,
        other => Typ::Named(other.to_string()),
    }
}

pub(crate) fn parse_param(token: &str) -> Result<(String, Typ), String> {
    match split_and_trim(':', token).as_slice() {
        [name, ty] if !trim(name).is_empty() && !trim(ty).is_empty() => {
            Ok((trim(name).to_string(), parse_in_type(ty)))
        }
        _ => Err(format!("parameter must be `name: Type`, got `{token}`")),
    }
}

pub(crate) fn parse_fn_header(
    after_fn_keyword: &str,
) -> Result<(String, Vec<(String, Typ)>, Typ), String> {
    let after = trim(after_fn_keyword).trim_end_matches(';').trim();
    let open_idx = after.find('(');
    let close_idx = after.rfind(')');
    if let (Some(i), Some(j)) = (open_idx, close_idx)
        && j > i
    {
        let name = trim(&after[..i]).to_string();
        let param_blob = trim(&after[i + 1..j]);
        let params = if param_blob.is_empty() {
            Vec::new()
        } else {
            let mut out = Vec::new();
            for t in split_and_trim(',', param_blob) {
                out.push(parse_param(&t)?);
            }
            out
        };
        let tail = after.get(j + 1..).unwrap_or("");
        let ret = match tail.split('>').collect::<Vec<_>>().as_slice() {
            [left, right] if trim(left).ends_with('-') => parse_in_type(right),
            _ => Typ::Void,
        };
        Ok((name, params, ret))
    } else {
        Ok((trim(after).to_string(), Vec::new(), Typ::Void))
    }
}
