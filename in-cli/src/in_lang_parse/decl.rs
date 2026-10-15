use super::stmt::parse_function_body;
use super::surface::InExternBinding;
use super::types::{parse_fn_header, parse_in_type};
use super::util::*;
use crate::core_ir::{
    ComponentCapability, ComponentExport, ComponentImport, Decl, MethodSig, Stmt, Typ, Visibility,
};

pub(crate) fn extract_struct_method_blocks(inner: &str) -> (String, Vec<String>) {
    let mut fields = String::new();
    let mut methods = Vec::new();
    let mut pos = 0usize;
    while pos < inner.len() {
        let rest = &inner[pos..];
        let Some(rel) = rest.find("fn ") else {
            fields.push_str(rest);
            break;
        };
        let start = pos + rel;
        let before = &inner[pos..start];
        let boundary = start == 0
            || inner[..start]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_whitespace() || ch == ';');
        if !boundary {
            fields.push_str(&inner[pos..start + 3]);
            pos = start + 3;
            continue;
        }
        fields.push_str(before);
        let Some(open_rel) = inner[start..].find('{') else {
            fields.push_str(&inner[start..]);
            break;
        };
        let open = start + open_rel;
        if let Some((_, close)) = brace_content_bounds_after_open(inner, open) {
            methods.push(inner[start..=close].to_string());
            pos = close + 1;
        } else {
            fields.push_str(&inner[start..]);
            break;
        }
    }
    (fields, methods)
}

pub(crate) fn parse_struct_fields_inner(inner: &str) -> Result<Vec<(String, Typ)>, String> {
    let mut fields = Vec::new();
    for raw_seg in split_struct_field_segments(inner) {
        let seg = strip_line_comment_outside_strings(raw_seg);
        let seg = trim(seg);
        if seg.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = seg.split_whitespace().collect();
        if tokens.len() < 2 {
            return Err(format!(".in: invalid struct field `{seg}`"));
        }
        let field_name = tokens[tokens.len() - 1].to_string();
        let ty_str = tokens[..tokens.len() - 1].join(" ");
        fields.push((field_name, parse_in_type(&ty_str)));
    }
    Ok(fields)
}

pub(crate) type StructBlock = (String, Vec<(String, Typ)>, Vec<Decl>);

pub(crate) fn parse_struct_block(block: &str) -> Result<StructBlock, String> {
    let t = trim(block);
    let rest = t
        .strip_prefix("struct ")
        .ok_or_else(|| ".in: expected `struct`".to_string())?;
    let open = rest
        .find('{')
        .ok_or_else(|| ".in: struct must contain `{`".to_string())?;
    let name = trim(&rest[..open]).to_string();
    let inner = brace_content_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `struct { ... }`".to_string())?;
    let (field_inner, method_blocks) = extract_struct_method_blocks(inner);
    let fields = parse_struct_fields_inner(&field_inner)?;
    let mut methods = Vec::new();
    for method in method_blocks {
        let (method_name, params, ret, body) = parse_fn_block(&method, 0)?;
        let mut lowered_params = vec![("self".to_string(), Typ::Named(name.clone()))];
        lowered_params.extend(params);
        methods.push(Decl::Function {
            name: format!("{name}_{method_name}"),
            params: lowered_params,
            ret,
            body,
            type_params: vec![],
        });
    }
    Ok((name, fields, methods))
}

pub(crate) fn parse_class_header(
    header: &str,
) -> Result<(String, Option<String>, Vec<String>), String> {
    let header = trim(header);
    let mut tokens = header.split_whitespace();
    let name = tokens.next().ok_or(".in: class name missing")?.to_string();
    let mut extends = None;
    let mut implements = Vec::new();
    while let Some(token) = tokens.next() {
        match token {
            "extends" => {
                if extends.is_some() {
                    return Err(".in: duplicate `extends` in class header".into());
                }
                let parent = tokens
                    .next()
                    .ok_or(".in: `extends` needs a parent class name")?;
                extends = Some(parent.to_string());
            }
            "implements" => {
                for iface in tokens.by_ref() {
                    let clean = iface.trim_end_matches(',');
                    if !clean.is_empty() {
                        implements.push(clean.to_string());
                    }
                }
                break;
            }
            _ => return Err(format!(".in: unexpected token `{token}` in class header")),
        }
    }
    Ok((name, extends, implements))
}

pub(crate) fn parse_class_fields_inner(inner: &str) -> Result<Vec<(String, Typ)>, String> {
    let mut fields = Vec::new();
    for raw_seg in split_struct_field_segments(inner) {
        let seg = strip_line_comment_outside_strings(raw_seg);
        let seg = trim(seg);
        if seg.is_empty() {
            continue;
        }
        let (name, ty_str) = seg
            .split_once(':')
            .ok_or_else(|| format!(".in: invalid class field `{seg}`"))?;
        let name = trim(name).to_string();
        let ty = parse_in_type(trim(ty_str));
        if name.is_empty() {
            return Err(format!(".in: missing field name in `{seg}`"));
        }
        fields.push((name, ty));
    }
    Ok(fields)
}

pub(crate) fn parse_class_block(block: &str) -> Result<Decl, String> {
    let t = trim(block);
    let rest = t
        .strip_prefix("class ")
        .ok_or_else(|| ".in: expected `class`".to_string())?;
    let open = rest
        .find('{')
        .ok_or_else(|| ".in: class must contain `{`".to_string())?;
    let header = trim(&rest[..open]);
    let (name, extends, implements) = parse_class_header(header)?;
    let inner = brace_content_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `class { ... }`".to_string())?;
    let (field_inner, method_blocks) = extract_struct_method_blocks(inner);
    let fields = parse_class_fields_inner(&field_inner)?;
    let mut methods = Vec::new();
    for method in method_blocks {
        let (method_name, params, ret, body) = parse_fn_block(&method, 0)?;
        methods.push(Decl::Function {
            name: method_name,
            params,
            ret,
            body,
            type_params: vec![],
        });
    }
    Ok(Decl::Class {
        name,
        fields,
        methods,
        visibility: Visibility::Pub,
        extends,
        implements,
        type_params: vec![],
    })
}

pub(crate) fn parse_component_block(block: &str) -> Result<Decl, String> {
    let t = trim(block);
    let rest = t
        .strip_prefix("component ")
        .ok_or_else(|| ".in: expected `component`".to_string())?;
    let open = rest
        .find('{')
        .ok_or_else(|| ".in: component must contain `{`".to_string())?;
    let name = trim(&rest[..open]).to_string();
    if name.is_empty() {
        return Err(".in: component name missing".into());
    }
    let inner = brace_content_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `component { ... }`".to_string())?;

    let mut target = String::new();
    let mut deterministic = false;
    let mut checkpoint = String::new();
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let mut capabilities = Vec::new();

    for raw_line in inner.lines() {
        let line = trim(strip_line_comment_outside_strings(raw_line));
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(val) = line.strip_prefix("target ") {
            let val = trim(val).trim_end_matches(';').trim();
            target = if val.starts_with('"') {
                crate::core_ir::decode_string_literal(val)
                    .ok_or_else(|| ".in: invalid component target string".to_string())?
            } else {
                val.trim_matches('\'').trim().to_string()
            };
            if target.is_empty() {
                return Err(".in: component target missing".into());
            }
        } else if let Some(val) = line.strip_prefix("deterministic ") {
            deterministic = match trim(val).trim_end_matches(';').trim() {
                "true" => true,
                "false" => false,
                other => {
                    return Err(format!(
                        ".in: component deterministic must be true/false, got `{other}`"
                    ));
                }
            };
        } else if let Some(val) = line.strip_prefix("checkpoint ") {
            checkpoint = trim(val).trim_end_matches(';').trim().to_string();
            if checkpoint.is_empty() {
                return Err(".in: component checkpoint policy missing".into());
            }
        } else if let Some(val) = line.strip_prefix("import ") {
            let val = trim(val).trim_end_matches(';').trim();
            let (name, interface) = val.split_once(':').ok_or_else(|| {
                format!(".in: component import must be `name: Interface`, got `{val}`")
            })?;
            imports.push(ComponentImport {
                name: trim(name).to_string(),
                interface: trim(interface).to_string(),
            });
        } else if let Some(val) = line.strip_prefix("export ") {
            let val = trim(val).trim_end_matches(';').trim();
            let (name, interface) = val.split_once(':').ok_or_else(|| {
                format!(".in: component export must be `name: Interface`, got `{val}`")
            })?;
            exports.push(ComponentExport {
                name: trim(name).to_string(),
                interface: trim(interface).to_string(),
            });
        } else if let Some(val) = line.strip_prefix("capability ") {
            let val = trim(val).trim_end_matches(';').trim();
            let (name_and_type, args_part) = val.split_once('(').unwrap_or((val, ""));
            let args = if args_part.is_empty() {
                Vec::new()
            } else {
                let args_str = args_part.trim_end_matches(')').trim();
                args_str
                    .split(',')
                    .map(|a| trim(a).to_string())
                    .filter(|a| !a.is_empty())
                    .collect()
            };
            let (name, capability_type) = name_and_type.split_once(':').ok_or_else(|| {
                format!(".in: component capability must be `name: Type(args)`, got `{val}`")
            })?;
            capabilities.push(ComponentCapability {
                name: trim(name).to_string(),
                capability_type: trim(capability_type).to_string(),
                args,
            });
        } else {
            return Err(format!(".in: unknown component field `{line}`"));
        }
    }

    Ok(Decl::Component {
        name,
        target,
        deterministic,
        checkpoint,
        imports,
        exports,
        capabilities,
    })
}

pub(crate) fn parse_interface_method_sigs(inner: &str) -> Result<Vec<MethodSig>, String> {
    let mut sigs = Vec::new();
    for line in inner.lines() {
        let line = trim(line).trim_end_matches(';');
        let line = strip_line_comment_outside_strings(line);
        let line = trim(line);
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let rest = trim(line.strip_prefix("fn ").ok_or_else(|| {
            format!(".in: interface body may only contain method signatures, got `{line}`")
        })?);
        let (name, params, ret) = parse_fn_header(rest)?;
        if name.is_empty() {
            return Err(format!(".in: interface method name missing in `{line}`"));
        }
        sigs.push(MethodSig { name, params, ret });
    }
    Ok(sigs)
}

pub(crate) fn parse_interface_block(block: &str) -> Result<Decl, String> {
    let t = trim(block);
    let rest = t
        .strip_prefix("interface ")
        .ok_or_else(|| ".in: expected `interface`".to_string())?;
    let open = rest
        .find('{')
        .ok_or_else(|| ".in: interface must contain `{`".to_string())?;
    let name = trim(&rest[..open]).to_string();
    if name.is_empty() {
        return Err(".in: interface name missing".into());
    }
    let inner = brace_content_after_open(rest, open)
        .ok_or_else(|| ".in: unclosed `interface { ... }`".to_string())?;
    let methods = parse_interface_method_sigs(inner)?;
    Ok(Decl::Interface {
        name,
        methods,
        visibility: Visibility::Pub,
        type_params: vec![],
    })
}

#[allow(clippy::type_complexity)]
pub(crate) fn parse_fn_block(
    block: &str,
    fn_line: u32,
) -> Result<(String, Vec<(String, Typ)>, Typ, Vec<Stmt>), String> {
    let t = trim(block);
    let rest = t
        .strip_prefix("fn ")
        .ok_or_else(|| format!(".in at line {fn_line}: expected `fn`"))?;
    if let Some(brace_idx) = find_fn_body_open_brace(rest) {
        let header = trim(&rest[..brace_idx]);
        let (name, params, ret) = parse_fn_header(header)?;
        let body_inner = brace_content_after_open(rest, brace_idx)
            .ok_or_else(|| format!(".in at line {fn_line}: unclosed `{{` in function body"))?;
        let body =
            parse_function_body(body_inner).map_err(|e| format!(".in at line {fn_line}: {e}"))?;
        Ok((name, params, ret, body))
    } else {
        let (name, params, ret) = parse_fn_header(rest)?;
        Ok((name, params, ret, Vec::new()))
    }
}

pub(crate) fn parse_extern_fn_block(block: &str) -> Result<InExternBinding, String> {
    let t = trim(block).trim_end_matches(';').trim();
    if t.contains('{') || t.contains('}') {
        return Err(".in: `extern` bindings cannot contain bodies".into());
    }
    let rest = t
        .strip_prefix("extern ")
        .ok_or_else(|| ".in: expected `extern`".to_string())?;
    let Some((language, header)) = rest.split_once(" fn ") else {
        return Err(".in: expected `extern <language> fn name(...)`".into());
    };
    let language = trim(language);
    if language.is_empty() || language.contains(char::is_whitespace) {
        return Err(".in: invalid extern language".into());
    }
    let (header, required_capabilities) =
        if let Some((left, right)) = header.split_once(" requires ") {
            let caps = split_and_trim(',', right);
            if caps.is_empty() {
                return Err(".in: extern requires at least one capability".into());
            }
            (left, caps)
        } else {
            (header, Vec::new())
        };
    let (name, params, ret) = parse_fn_header(header)?;
    if name.is_empty() {
        return Err(".in: extern function name missing".into());
    }
    Ok(InExternBinding {
        language: language.to_string(),
        name,
        params,
        required_capabilities,
        ret: Some(ret),
    })
}
