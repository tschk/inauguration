use super::extract::{
    AstShape, ast_body, collect_kinds, extract_fn_nodes, first_named, last_named, named_descendant,
    node_txt, normalize_entry, simple_bounded_body, strict_simple_bounded_body,
};
use crate::boundary_ir::{
    BoundaryField, BoundaryLayout, BoundaryModule, BoundaryOwnership, BoundaryRepr, BoundarySymbol,
    BoundaryTransfer, IN_ABI_VERSION,
};
use crate::boundary_verify::boundary_ir_verify;
use crate::core_ir::{Decl, Stmt, Typ};
use std::collections::HashMap;
use std::hash::Hasher;
use tree_sitter::Node;

type ZigLayoutFields = Vec<(String, String)>;
type ZigLayoutSpec = (BoundaryRepr, ZigLayoutFields);
type ZigLayoutSpecs = HashMap<String, ZigLayoutSpec>;
const ZIG_AST: AstShape = AstShape {
    block_kinds: &["block"],
    return_kinds: &["return_expression"],
    expr_stmt_kinds: &["expression_statement", "statement"],
    local_decl_kinds: &["variable_declaration"],
    assignment_kinds: &["assign_expression", "assignment_expression"],
    if_kinds: &["if_expression", "if_statement"],
    while_kinds: &["while_expression", "while_statement"],
    call_kinds: &["call_expression"],
    arg_container_kinds: &[],
    arg_wrapper_kinds: &[],
    paren_kinds: &["grouped_expression", "parenthesized_expression"],
    binary_kinds: &["binary_expression"],
    unary_kinds: &["unary_expression"],
    int_kinds: &["integer"],
    string_kinds: &["string", "string_literal"],
    type_kinds: &["builtin_type", "type", "identifier"],
    local_decl_prefixes: &["var ", "const "],
    shell_first_kinds: &["block_expression"],
    shell_last_kinds: &["labeled_statement"],
    try_kinds: &[],
    catch_kinds: &[],
    match_kinds: &[],
    first_assignment_is_let: false,
    strict_args: false,
    binary_precedence: None,
    string_decoder: None,
};

#[derive(Clone)]
struct ZigAbiType {
    boundary_type: String,
    size: u64,
    align: u64,
    transfer: Option<BoundaryTransfer>,
}

fn zig_struct_repr(src: &[u8], n: Node<'_>) -> Option<BoundaryRepr> {
    let text = node_txt(src, n).trim_start();
    if text.starts_with("packed struct") {
        Some(BoundaryRepr::Packed)
    } else if text.starts_with("extern struct") {
        Some(BoundaryRepr::C)
    } else {
        None
    }
}

fn zig_boundary_type_name(src: &[u8], n: Node<'_>) -> String {
    node_txt(src, n).trim().to_string()
}

fn zig_boundary_container_fields(
    src: &[u8],
    struct_node: Node<'_>,
) -> Option<Vec<(String, String)>> {
    let mut fields = Vec::new();
    let mut field_nodes = Vec::new();
    collect_kinds(struct_node, &["container_field"], &mut field_nodes);
    for field in field_nodes {
        let name_n = field
            .child_by_field_name("name")
            .or_else(|| first_named(field, "identifier"))?;
        let name = node_txt(src, name_n).trim().to_string();
        let type_n = field.child_by_field_name("type").or_else(|| {
            let mut w = field.walk();
            let mut last = None;
            for ch in field.named_children(&mut w) {
                if ch != name_n {
                    last = Some(ch);
                }
            }
            last
        })?;
        fields.push((name, zig_boundary_type_name(src, type_n)));
    }
    if fields.is_empty() {
        None
    } else {
        Some(fields)
    }
}

fn zig_extern_struct_spec(
    src: &[u8],
    decl: Node<'_>,
) -> Option<(String, BoundaryRepr, ZigLayoutFields)> {
    let struct_node = first_named(decl, "struct_declaration")?;
    let repr = zig_struct_repr(src, struct_node)?;
    let name_n = first_named(decl, "identifier")?;
    let name = node_txt(src, name_n).trim().to_string();
    let fields = zig_boundary_container_fields(src, struct_node)?;
    Some((name, repr, fields))
}

fn zig_scalar_abi(boundary_type: &str, size: u64, align: u64) -> ZigAbiType {
    ZigAbiType {
        boundary_type: boundary_type.to_string(),
        size,
        align,
        transfer: Some(BoundaryTransfer::Copy),
    }
}

fn zig_align_up(offset: u64, align: u64) -> u64 {
    if align == 0 {
        return offset;
    }
    let mask = align - 1;
    (offset + mask) & !mask
}

fn zig_abi_type_for(
    type_name: &str,
    layout_specs: &ZigLayoutSpecs,
    packed: bool,
) -> Option<ZigAbiType> {
    if type_name.contains('*') || type_name.starts_with('[') {
        return Some(zig_scalar_abi("u64", 8, 8));
    }
    match type_name {
        "i8" => Some(zig_scalar_abi("i8", 1, 1)),
        "u8" => Some(zig_scalar_abi("u8", 1, 1)),
        "i16" => Some(zig_scalar_abi("i16", 2, 2)),
        "u16" => Some(zig_scalar_abi("u16", 2, 2)),
        "i32" => Some(zig_scalar_abi("i32", 4, 4)),
        "u32" => Some(zig_scalar_abi("u32", 4, 4)),
        "f16" => Some(zig_scalar_abi("float", 2, 2)),
        "f32" => Some(zig_scalar_abi("float", 4, 4)),
        "f64" => Some(zig_scalar_abi("f64", 8, 8)),
        "i64" => Some(zig_scalar_abi("i64", 8, 8)),
        "u64" => Some(zig_scalar_abi("u64", 8, 8)),
        "isize" => Some(zig_scalar_abi("i64", 8, 8)),
        "usize" => Some(zig_scalar_abi("u64", 8, 8)),
        "bool" => Some(zig_scalar_abi("bool", 1, 1)),
        "void" => Some(zig_scalar_abi("void", 0, 1)),
        "InSliceU8" => Some(ZigAbiType {
            boundary_type: "InSliceU8".to_string(),
            size: 16,
            align: 8,
            transfer: Some(BoundaryTransfer::Borrow),
        }),
        name => {
            if let Some((repr, fields)) = layout_specs.get(name) {
                let packed_layout = packed || matches!(repr, BoundaryRepr::Packed);
                let layout = zig_compute_struct_layout(name, repr.clone(), fields, layout_specs)?;
                Some(ZigAbiType {
                    boundary_type: name.to_string(),
                    size: layout.size,
                    align: if packed_layout { 1 } else { layout.align },
                    transfer: Some(BoundaryTransfer::Copy),
                })
            } else {
                None
            }
        }
    }
}

fn zig_compute_struct_layout(
    name: &str,
    repr: BoundaryRepr,
    fields: &[(String, String)],
    layout_specs: &ZigLayoutSpecs,
) -> Option<BoundaryLayout> {
    let packed = matches!(repr, BoundaryRepr::Packed);
    let mut offset = 0u64;
    let mut max_align = 1u64;
    let mut boundary_fields = Vec::new();

    for (field_name, field_ty) in fields {
        let abi = zig_abi_type_for(field_ty, layout_specs, packed)?;
        let field_align = if packed { 1 } else { abi.align };
        offset = zig_align_up(offset, field_align);
        boundary_fields.push(BoundaryField {
            name: field_name.clone(),
            offset,
            typ: abi.boundary_type.clone(),
            transfer: abi.transfer,
        });
        offset = offset.saturating_add(abi.size);
        max_align = max_align.max(field_align);
    }

    let struct_align = if packed { 1 } else { max_align };
    let size = if offset == 0 {
        struct_align
    } else {
        zig_align_up(offset, struct_align)
    };

    Some(BoundaryLayout {
        name: name.to_string(),
        kind: "struct".to_string(),
        repr: Some(repr),
        size,
        align: struct_align,
        stride: size,
        fields: boundary_fields,
    })
}

fn zig_fn_is_export(src: &[u8], fun: Node<'_>) -> bool {
    node_txt(src, fun).contains("export fn")
}

fn zig_fn_param_type_names(src: &[u8], fun: Node<'_>) -> Vec<String> {
    let Some(params) = named_descendant(fun, "parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut w = params.walk();
    for ch in params.named_children(&mut w) {
        if ch.kind() != "parameter" {
            continue;
        }
        let Some(id) = first_named(ch, "identifier") else {
            continue;
        };
        if let Some(ty) = last_named(ch).filter(|t| *t != id) {
            out.push(zig_boundary_type_name(src, ty));
        }
    }
    out
}

fn zig_fn_return_type_name(src: &[u8], fun: Node<'_>) -> String {
    let Some(params) = named_descendant(fun, "parameters") else {
        return "void".to_string();
    };
    let mut after_params = false;
    let mut w = fun.walk();
    for ch in fun.named_children(&mut w) {
        if ch == params {
            after_params = true;
            continue;
        }
        if after_params && ch.kind() != "block" {
            return zig_boundary_type_name(src, ch);
        }
        if ch.kind() == "block" {
            break;
        }
    }
    "void".to_string()
}

fn zig_boundary_symbol_from_fn(
    src: &[u8],
    fun: Node<'_>,
    layout_specs: &ZigLayoutSpecs,
) -> Option<BoundarySymbol> {
    if !zig_fn_is_export(src, fun) {
        return None;
    }
    let name_n = first_named(fun, "identifier")?;
    let name = node_txt(src, name_n).trim().to_string();
    let mut parts = vec![name.clone()];
    for ty in zig_fn_param_type_names(src, fun) {
        parts.push(
            zig_abi_type_for(&ty, layout_specs, false)
                .map(|abi| abi.boundary_type)
                .unwrap_or(ty),
        );
    }
    parts.push(
        zig_abi_type_for(&zig_fn_return_type_name(src, fun), layout_specs, false)
            .map(|abi| abi.boundary_type)
            .unwrap_or_else(|| zig_fn_return_type_name(src, fun)),
    );
    let canonical = parts.join(";");
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&canonical, &mut h);
    let hash = h.finish();
    Some(BoundarySymbol {
        name,
        signature_hash: format!("siphash-{:016x}", hash),
        ownership: BoundaryOwnership::ReturnsOwnedHandle,
        calling_convention: "c".to_string(),
    })
}

pub(super) fn extract_zig_boundary_module(
    src: &[u8],
    root: Node<'_>,
    module_id: &str,
) -> Option<BoundaryModule> {
    let mut layouts = Vec::new();
    let mut symbols = Vec::new();
    let mut layout_specs: ZigLayoutSpecs = HashMap::new();

    let mut var_decls = Vec::new();
    collect_kinds(root, &["variable_declaration"], &mut var_decls);
    for decl in var_decls {
        if let Some((name, repr, fields)) = zig_extern_struct_spec(src, decl) {
            layout_specs.insert(name, (repr, fields));
        }
    }

    for (name, (repr, fields)) in &layout_specs {
        if let Some(layout) = zig_compute_struct_layout(name, repr.clone(), fields, &layout_specs) {
            layouts.push(layout);
        }
    }

    let mut fun_nodes = Vec::new();
    collect_kinds(root, &["function_declaration"], &mut fun_nodes);
    for fun in fun_nodes {
        if let Some(symbol) = zig_boundary_symbol_from_fn(src, fun, &layout_specs) {
            symbols.push(symbol);
        }
    }

    if layouts.is_empty() && symbols.is_empty() {
        return None;
    }

    let boundary = BoundaryModule {
        abi_version: IN_ABI_VERSION,
        module: format!("zig.{module_id}"),
        layouts,
        symbols,
        allocators: vec![],
        layout_hash: String::new(),
    }
    .with_layout_hash();
    let report = boundary_ir_verify(&boundary);
    if !report.ok {
        return None;
    }
    Some(boundary)
}

pub(super) fn extract_zig(src: &[u8], root: Node<'_>) -> Result<Vec<Decl>, String> {
    extract_fn_nodes(src, root, &["function_declaration"], |src, n| {
        let name_n = n
            .child_by_field_name("name")
            .or_else(|| named_descendant(n, "identifier"))?;
        let name = normalize_entry(node_txt(src, name_n).trim());
        let params = zig_params(src, n);
        let ret = zig_return_type(src, n).unwrap_or(Typ::Void);
        let body = n
            .child_by_field_name("body")
            .or_else(|| first_named(n, "block"))
            .map(|b| zig_body(src, b))
            .unwrap_or_default();
        Some(Decl::Function {
            name,
            params,
            ret,
            body,
            type_params: vec![],
        })
    })
}

fn zig_params<'a>(src: &[u8], fun: Node<'a>) -> Vec<(String, Typ)> {
    let mut out = Vec::new();
    let Some(params) = named_descendant(fun, "parameters") else {
        return out;
    };
    let mut w = params.walk();
    for ch in params.named_children(&mut w) {
        if ch.kind() != "parameter" {
            continue;
        }
        let Some(id) = first_named(ch, "identifier") else {
            continue;
        };
        let ty = last_named(ch)
            .filter(|t| *t != id)
            .map(|t| Typ::Named(node_txt(src, t).trim().to_string()))
            .unwrap_or(Typ::Named("Any".into()));
        out.push((node_txt(src, id).trim().to_string(), ty));
    }
    out
}

fn zig_return_type(src: &[u8], fun: Node<'_>) -> Option<Typ> {
    let params = named_descendant(fun, "parameters")?;
    let mut after_params = false;
    let mut w = fun.walk();
    for ch in fun.named_children(&mut w) {
        if ch == params {
            after_params = true;
            continue;
        }
        if after_params && ch.kind() != "block" {
            return Some(Typ::Named(node_txt(src, ch).trim().to_string()));
        }
        if ch.kind() == "block" {
            break;
        }
    }
    None
}

fn zig_body(src: &[u8], body: Node<'_>) -> Vec<Stmt> {
    // ponytail: check text stripped of braces; AST child count unreliable for zig blocks
    let txt = node_txt(src, body).trim();
    if txt == "{}"
        || txt
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .is_some_and(|s| s.trim().is_empty())
    {
        return Vec::new();
    }
    // ponytail: skip strict_simple_bounded_body for block bodies
    if txt.starts_with('{') {
        let stmts = ast_body(src, body, ZIG_AST);
        if !stmts.is_empty() {
            return stmts;
        }
        return Vec::new();
    }
    if let Some(stmts) = strict_simple_bounded_body(txt, "=") {
        return stmts;
    }
    let stmts = ast_body(src, body, ZIG_AST);
    if !stmts.is_empty() {
        return stmts;
    }
    simple_bounded_body(node_txt(src, body), "=").unwrap_or_default()
}
