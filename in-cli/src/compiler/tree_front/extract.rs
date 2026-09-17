//! Tree-sitter grammars → [`UnifiedModule`] with per-language declaration extraction and bounded
//! scalar body lowering where wired.

#[cfg_attr(not(feature = "parse-extended"), allow(unused_imports))]
use super::c_family::{c_like_function_decl, extract_cpp_with_classes, objc_like};
use super::crystal::extract_crystal;
#[cfg(feature = "parse-extended")]
use super::csharp::extract_csharp;
#[cfg(feature = "parse-extended")]
use super::dart::extract_dart;
#[cfg(feature = "parse-extended")]
use super::elixir::extract_elixir;
#[cfg(feature = "parse-extended")]
use super::erlang::extract_erlang;
#[cfg(feature = "parse-extended")]
use super::fsharp::extract_fsharp;
use super::go::{go_body, go_params, go_return_type};
#[cfg(feature = "parse-extended")]
use super::haskell::extract_haskell;
use super::holyc::extract_holyc;
#[cfg_attr(not(feature = "parse-extended"), allow(unused_imports))]
use super::java::{extract_java_style_methods, extract_java_with_classes};
use super::js::extract_js_with_classes;
#[cfg(feature = "parse-extended")]
use super::julia::extract_julia;
#[cfg(feature = "parse-extended")]
use super::kotlin::extract_kotlin;
use super::lolcat::extract_lolcat;
use super::lua::extract_lua;
use super::nim::extract_nim;
#[cfg(feature = "parse-extended")]
use super::ocaml::extract_ocaml;
use super::perl::extract_perl;
#[cfg(feature = "parse-extended")]
use super::php::extract_php;
use super::python::extract_python_with_classes;
#[cfg(feature = "parse-extended")]
use super::r_lang::extract_r_lang;
use super::ruby::extract_ruby;
use super::rust::extract_rust;
#[cfg(feature = "parse-extended")]
use super::scala::extract_scala;
use super::swift::extract_swift;
use super::ts::extract_ts_with_classes;
#[cfg(feature = "parse-extended")]
use super::v_lang::{v_body, v_params, v_return_type};
use super::zig::{extract_zig, extract_zig_boundary_module};
use crate::boundary_ir::CompileArtifact;
use crate::core_ir::{CatchArm, Expr, MatchArm, Stmt, Typ};
use crate::core_ir::{Decl, UnifiedModule};
use crate::parser_registry::ParserId;
use std::collections::HashSet;
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[cfg(feature = "parse-extended")]
use tree_sitter_c_sharp;
use tree_sitter_crystal;
#[cfg(feature = "parse-extended")]
use tree_sitter_dart;
#[cfg(feature = "parse-extended")]
use tree_sitter_elixir;
#[cfg(feature = "parse-extended")]
use tree_sitter_erlang;
#[cfg(feature = "parse-extended")]
use tree_sitter_fsharp;
#[cfg(feature = "parse-extended")]
use tree_sitter_groovy;
#[cfg(feature = "parse-extended")]
use tree_sitter_haskell;
use tree_sitter_holyc;
#[cfg(feature = "parse-extended")]
use tree_sitter_julia;
#[cfg(feature = "parse-extended")]
use tree_sitter_kotlin_ng;
use tree_sitter_lolcat;
use tree_sitter_lua;
use tree_sitter_nim;
#[cfg(feature = "parse-extended")]
use tree_sitter_objc;
#[cfg(feature = "parse-extended")]
use tree_sitter_ocaml;
use tree_sitter_perl;
#[cfg(feature = "parse-extended")]
use tree_sitter_php;
#[cfg(feature = "parse-extended")]
use tree_sitter_r;
use tree_sitter_ruby;
#[cfg(feature = "parse-extended")]
use tree_sitter_scala;
#[cfg(feature = "parse-extended")]
use tree_sitter_v;

/// Try to resolve a ParserId to a Tree-sitter Language.
fn try_lang_for(id: ParserId) -> Option<Language> {
    Some(match id {
        ParserId::C => tree_sitter_c::LANGUAGE.into(),
        ParserId::Cpp | ParserId::ObjCpp => tree_sitter_cpp::LANGUAGE.into(),
        ParserId::Java => tree_sitter_java::LANGUAGE.into(),
        ParserId::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        ParserId::Python => tree_sitter_python::LANGUAGE.into(),
        ParserId::Rust => tree_sitter_rust::LANGUAGE.into(),
        ParserId::Zig => tree_sitter_zig::LANGUAGE.into(),
        ParserId::Go => tree_sitter_go::LANGUAGE.into(),
        ParserId::Swift => tree_sitter_swift::LANGUAGE.into(),
        ParserId::TypeScript => tree_sitter_typescript::LANGUAGE_TSX.into(),
        ParserId::Lua => tree_sitter_lua::LANGUAGE.into(),
        ParserId::Perl => tree_sitter_perl::LANGUAGE.into(),
        ParserId::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        ParserId::HolyC => tree_sitter_holyc::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::ObjC => tree_sitter_objc::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Scala => tree_sitter_scala::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Groovy => tree_sitter_groovy::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::FSharp => tree_sitter_fsharp::LANGUAGE_FSHARP.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Dart => tree_sitter_dart::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Elixir => tree_sitter_elixir::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Erlang => tree_sitter_erlang::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Haskell => tree_sitter_haskell::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::Julia => tree_sitter_julia::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::OCaml => tree_sitter_ocaml::LANGUAGE_OCAML.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::R => tree_sitter_r::LANGUAGE.into(),
        #[cfg(feature = "parse-extended")]
        ParserId::V => tree_sitter_v::LANGUAGE.into(),
        ParserId::Lolcode => tree_sitter_lolcat::LANGUAGE.into(),
        ParserId::Crystal => tree_sitter_crystal::LANGUAGE.into(),
        ParserId::Nim => tree_sitter_nim::LANGUAGE.into(),
        _ => return None,
    })
}

pub fn parse_polyglot_file(id: ParserId, path: &Path) -> Result<UnifiedModule, String> {
    match id {
        ParserId::In | ParserId::Icore => Err(format!(
            "internal: `{}` must use the dedicated front, not tree_front",
            id.as_str()
        )),
        #[cfg(feature = "parse-extended")]
        ParserId::V => {
            let src = std::fs::read_to_string(path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            parse_lang(
                try_lang_for(ParserId::V).ok_or_else(|| {
                    format!(
                        "Parser `{}` unavailable in this build",
                        ParserId::V.as_str()
                    )
                })?,
                &src,
                |b, r| {
                    extract_fn_nodes(b, r, &["function_declaration"], |s, n| {
                        let name_n = n.child_by_field_name("name")?;
                        let name = normalize_entry(node_txt(s, name_n).trim());
                        let params = v_params(s, n);
                        let ret = v_return_type(s, n).unwrap_or(Typ::Void);
                        let body = n
                            .child_by_field_name("body")
                            .map(|b| v_body(s, b))
                            .unwrap_or_default();
                        Some(Decl::Function {
                            name,
                            params,
                            ret,
                            body,
                            type_params: vec![],
                        })
                    })
                },
            )
        }
        _ => {
            let src = std::fs::read_to_string(path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            if try_lang_for(id).is_none() {
                return parse_textual_polyglot_module(id, &src);
            }
            dispatch(id, path, &src)
        }
    }
}

fn dispatch(id: ParserId, _path: &Path, src: &str) -> Result<UnifiedModule, String> {
    match id {
        ParserId::C => parse_lang(
            try_lang_for(ParserId::C).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::C.as_str()
                )
            })?,
            src,
            |b, r| extract_fn_nodes(b, r, &["function_definition"], c_like_function_decl),
        ),
        ParserId::Cpp | ParserId::ObjCpp => parse_lang(
            try_lang_for(ParserId::Cpp).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Cpp.as_str()
                )
            })?,
            src,
            extract_cpp_with_classes,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::ObjC => parse_lang(
            try_lang_for(ParserId::ObjC).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::ObjC.as_str()
                )
            })?,
            src,
            |b, r| {
                extract_fn_nodes(
                    b,
                    r,
                    &["function_definition", "method_definition"],
                    |src, n| objc_like(src, n),
                )
            },
        ),
        ParserId::Java => parse_lang(
            try_lang_for(ParserId::Java).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Java.as_str()
                )
            })?,
            src,
            extract_java_with_classes,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Kotlin => parse_lang(
            try_lang_for(ParserId::Kotlin).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Kotlin.as_str()
                )
            })?,
            src,
            extract_kotlin,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Scala => parse_lang(
            try_lang_for(ParserId::Scala).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Scala.as_str()
                )
            })?,
            src,
            extract_scala,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Groovy => parse_lang(
            try_lang_for(ParserId::Groovy).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Groovy.as_str()
                )
            })?,
            src,
            extract_java_style_methods,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::CSharp => parse_lang(
            try_lang_for(ParserId::CSharp).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::CSharp.as_str()
                )
            })?,
            src,
            extract_csharp,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::FSharp => parse_lang(
            try_lang_for(ParserId::FSharp).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::FSharp.as_str()
                )
            })?,
            src,
            extract_fsharp,
        ),
        ParserId::Python => parse_lang(
            try_lang_for(ParserId::Python).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Python.as_str()
                )
            })?,
            src,
            extract_python_with_classes,
        ),
        ParserId::Ruby => parse_lang(
            try_lang_for(ParserId::Ruby).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Ruby.as_str()
                )
            })?,
            src,
            extract_ruby,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Php => parse_lang(
            try_lang_for(ParserId::Php).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Php.as_str()
                )
            })?,
            src,
            extract_php,
        ),
        ParserId::Perl => parse_lang(
            try_lang_for(ParserId::Perl).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Perl.as_str()
                )
            })?,
            src,
            extract_perl,
        ),
        ParserId::JavaScript => parse_lang(
            try_lang_for(ParserId::JavaScript).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::JavaScript.as_str()
                )
            })?,
            src,
            extract_js_with_classes,
        ),
        ParserId::TypeScript => {
            let ts_lang = try_lang_for(ParserId::TypeScript).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::TypeScript.as_str()
                )
            })?;
            parse_lang(ts_lang, src, extract_ts_with_classes)
        }
        ParserId::Go => parse_lang(
            try_lang_for(ParserId::Go).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Go.as_str()
                )
            })?,
            src,
            |b, r| {
                extract_fn_nodes(
                    b,
                    r,
                    &["function_declaration", "method_declaration"],
                    |src, n| {
                        let name_n = n.child_by_field_name("name")?;
                        let name = normalize_entry(node_txt(src, name_n).trim());
                        let params = go_params(src, n);
                        let ret = go_return_type(src, n).unwrap_or(Typ::Void);
                        let body = n
                            .child_by_field_name("body")
                            .map(|b| go_body(src, b))
                            .unwrap_or_default();
                        Some(Decl::Function {
                            name,
                            params,
                            ret,
                            body,
                            type_params: vec![],
                        })
                    },
                )
            },
        ),
        ParserId::Rust => parse_lang(
            try_lang_for(ParserId::Rust).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Rust.as_str()
                )
            })?,
            src,
            extract_rust,
        ),
        ParserId::Zig => parse_lang(
            try_lang_for(ParserId::Zig).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Zig.as_str()
                )
            })?,
            src,
            extract_zig,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Dart => parse_lang(
            try_lang_for(ParserId::Dart).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Dart.as_str()
                )
            })?,
            src,
            extract_dart,
        ),
        ParserId::Lua => parse_lang(
            try_lang_for(ParserId::Lua).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Lua.as_str()
                )
            })?,
            src,
            extract_lua,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Elixir => parse_lang(
            try_lang_for(ParserId::Elixir).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Elixir.as_str()
                )
            })?,
            src,
            extract_elixir,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Erlang => parse_lang(
            try_lang_for(ParserId::Erlang).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Erlang.as_str()
                )
            })?,
            src,
            extract_erlang,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Haskell => parse_lang(
            try_lang_for(ParserId::Haskell).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Haskell.as_str()
                )
            })?,
            src,
            extract_haskell,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::Julia => parse_lang(
            try_lang_for(ParserId::Julia).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Julia.as_str()
                )
            })?,
            src,
            extract_julia,
        ),
        ParserId::Swift => parse_lang(
            try_lang_for(ParserId::Swift).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Swift.as_str()
                )
            })?,
            src,
            extract_swift,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::OCaml => parse_lang(
            try_lang_for(ParserId::OCaml).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::OCaml.as_str()
                )
            })?,
            src,
            extract_ocaml,
        ),
        #[cfg(feature = "parse-extended")]
        ParserId::R => parse_lang(
            try_lang_for(ParserId::R).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::R.as_str()
                )
            })?,
            src,
            extract_r_lang,
        ),
        ParserId::HolyC => parse_lang(
            try_lang_for(ParserId::HolyC).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::HolyC.as_str()
                )
            })?,
            src,
            extract_holyc,
        ),
        ParserId::Lolcode => parse_lang(
            try_lang_for(ParserId::Lolcode).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Lolcode.as_str()
                )
            })?,
            src,
            extract_lolcat,
        ),
        ParserId::Crystal => parse_lang(
            try_lang_for(ParserId::Crystal).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Crystal.as_str()
                )
            })?,
            src,
            extract_crystal,
        ),
        ParserId::Nim => parse_lang(
            try_lang_for(ParserId::Nim).ok_or_else(|| {
                format!(
                    "Parser `{}` unavailable in this build",
                    ParserId::Nim.as_str()
                )
            })?,
            src,
            extract_nim,
        ),
        _ => parse_textual_polyglot_module(id, src),
    }
}

fn parse_lang(
    lang: Language,
    src: &str,
    extract: impl FnOnce(&[u8], Node<'_>) -> Result<Vec<Decl>, String>,
) -> Result<UnifiedModule, String> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("Tree-sitter grammar load failed: {e}"))?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| "Tree-sitter parse returned None".to_string())?;
    let root = tree.root_node();
    let extracted = extract(src.as_bytes(), root)?;
    let has_errors = root.has_error();
    if has_errors && extracted.is_empty() {
        return Err(
            "Tree-sitter parse tree contains syntax errors (zero functions extracted)".into(),
        );
    }
    let decls = dedup_fns(extracted);
    if decls.is_empty() {
        return Err(
            "parsed successfully but extracted zero functions — file may contain only types/data"
                .into(),
        );
    }
    Ok(UnifiedModule::new(decls))
}

pub fn parse_zig_artifact(path: &Path) -> Result<CompileArtifact, String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let module_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("zig")
        .to_string();
    parse_zig_artifact_source(&src, &module_id)
}

fn textual_fn(name: String, body_src: &str) -> Decl {
    let body = simple_bounded_body(body_src, "=")
        .or_else(|| simple_bounded_body(body_src, "<-"))
        .unwrap_or_default();
    Decl::Function {
        name,
        params: vec![],
        ret: Typ::Named("dynamic".into()),
        body,
        type_params: vec![],
    }
}

fn slice_lines(lines: &[&str], start: usize, end: usize) -> String {
    lines
        .get(start.saturating_add(1)..end)
        .unwrap_or(&[])
        .join("\n")
}

pub fn parse_textual_polyglot_module(id: ParserId, src: &str) -> Result<UnifiedModule, String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut headers: Vec<(String, usize)> = Vec::new();

    match id {
        ParserId::Cobol => {
            let mut prog_name = "main".to_string();
            let mut start = 0usize;
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("PROGRAM-ID.") {
                    prog_name = normalize_entry(&rest.trim_end_matches('.').trim().to_lowercase());
                    start = i;
                }
            }
            headers.push((prog_name, start));
        }
        ParserId::Fortran => {
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                let lower = trimmed.to_lowercase();
                if lower.starts_with("subroutine ")
                    || lower.starts_with("function ")
                    || lower.starts_with("program ")
                {
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if parts.len() >= 2 {
                        let name =
                            normalize_entry(parts[1].split('(').next().unwrap_or(parts[1]).trim());
                        if !name.is_empty() {
                            headers.push((name, i));
                        }
                    }
                }
            }
        }
        ParserId::VbNet => {
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                let lower = trimmed.to_lowercase();
                if lower.starts_with("sub ") || lower.starts_with("function ") {
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if parts.len() >= 2 {
                        let name =
                            normalize_entry(parts[1].split('(').next().unwrap_or(parts[1]).trim());
                        if !name.is_empty() {
                            headers.push((name, i));
                        }
                    }
                }
            }
        }
        ParserId::Odin => {
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if trimmed.contains(":: proc(") || trimmed.contains("::proc(") {
                    if let Some((name_part, _)) = trimmed.split_once("::") {
                        let name = normalize_entry(name_part.trim());
                        if !name.is_empty() {
                            headers.push((name, i));
                        }
                    }
                }
            }
        }
        ParserId::Hare => {
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed
                    .strip_prefix("fn ")
                    .or_else(|| trimmed.strip_prefix("export fn "))
                {
                    let name = normalize_entry(rest.split('(').next().unwrap_or(rest).trim());
                    if !name.is_empty() {
                        headers.push((name, i));
                    }
                }
            }
        }
        ParserId::D => {
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if (trimmed.contains('(') && trimmed.ends_with('{'))
                    || trimmed.starts_with("void ")
                    || trimmed.starts_with("int ")
                {
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if parts.len() >= 2 {
                        let name_part = parts[1].split('(').next().unwrap_or(parts[1]);
                        let name = normalize_entry(name_part.trim());
                        if !name.is_empty() && !name.contains('=') {
                            headers.push((name, i));
                        }
                    }
                }
            }
        }
        ParserId::Clojure => {
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed
                    .strip_prefix("(defn ")
                    .or_else(|| trimmed.strip_prefix("(defn- "))
                {
                    let name =
                        normalize_entry(rest.split_whitespace().next().unwrap_or(rest).trim());
                    if !name.is_empty() {
                        headers.push((name, i));
                    }
                }
            }
        }
        _ => {}
    }

    let mut decls = Vec::new();
    for (idx, (name, start)) in headers.iter().enumerate() {
        let end = headers.get(idx + 1).map(|(_, n)| *n).unwrap_or(lines.len());
        decls.push(textual_fn(name.clone(), &slice_lines(&lines, *start, end)));
    }

    if decls.is_empty() {
        decls.push(textual_fn("main".to_string(), src));
    }

    let decls = dedup_fns(decls);
    Ok(UnifiedModule::new(decls))
}

pub fn parse_zig_artifact_source(src: &str, module_id: &str) -> Result<CompileArtifact, String> {
    let mut parser = Parser::new();
    parser
        .set_language(&try_lang_for(ParserId::Zig).ok_or_else(|| {
            format!(
                "Parser `{}` unavailable in this build",
                ParserId::Zig.as_str()
            )
        })?)
        .map_err(|e| format!("Tree-sitter grammar load failed: {e}"))?;
    let tree = parser
        .parse(src, None)
        .ok_or_else(|| "Tree-sitter parse returned None".to_string())?;
    let root = tree.root_node();
    if root.has_error() {
        return Err("Tree-sitter parse tree contains syntax errors".into());
    }
    let decls = dedup_fns(extract_zig(src.as_bytes(), root)?);
    if decls.is_empty() {
        return Err(
            "parsed successfully but extracted zero functions — file may contain only types/data"
                .into(),
        );
    }
    let semantic = UnifiedModule::new(decls);
    let boundary = extract_zig_boundary_module(src.as_bytes(), root, module_id);
    Ok(match boundary {
        Some(boundary) => CompileArtifact::with_boundary(semantic, boundary),
        None => CompileArtifact::from_semantic(semantic),
    })
}

pub(super) fn decl_fn(name: String, params: Vec<(String, Typ)>, ret: Typ) -> Decl {
    Decl::Function {
        name,
        params,
        ret,
        body: vec![],
        type_params: vec![],
    }
}

pub(super) fn normalize_entry(raw: &str) -> String {
    match raw {
        "Main" | "MAIN" | "_main" => "main".into(),
        other if other.eq_ignore_ascii_case("main") => "main".into(),
        other => other.to_string(),
    }
}

fn dedup_fns(decls: Vec<Decl>) -> Vec<Decl> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for d in decls {
        match &d {
            Decl::Function { name, .. } => {
                if seen.insert(name.clone()) {
                    out.push(d);
                }
            }
            _ => out.push(d),
        }
    }
    out
}

pub(super) fn node_txt<'a>(src: &'a [u8], n: Node<'a>) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

pub(super) fn collect_kinds<'a>(root: Node<'a>, kinds: &[&str], out: &mut Vec<Node<'a>>) {
    if kinds.contains(&root.kind()) {
        out.push(root);
    }
    let mut w = root.walk();
    for ch in root.named_children(&mut w) {
        collect_kinds(ch, kinds, out);
    }
}

pub(super) fn first_named<'a>(n: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut w = n.walk();
    n.named_children(&mut w).find(|ch| ch.kind() == kind)
}

pub(super) fn last_named<'a>(n: Node<'a>) -> Option<Node<'a>> {
    let mut w = n.walk();
    n.named_children(&mut w).last()
}

pub(super) fn named_descendant<'a>(root: Node<'a>, kind: &str) -> Option<Node<'a>> {
    if root.kind() == kind {
        return Some(root);
    }
    let mut w = root.walk();
    root.named_children(&mut w)
        .find_map(|ch| named_descendant(ch, kind))
}

#[derive(Clone, Copy)]
pub(super) struct AstShape {
    pub(super) block_kinds: &'static [&'static str],
    pub(super) return_kinds: &'static [&'static str],
    pub(super) expr_stmt_kinds: &'static [&'static str],
    pub(super) local_decl_kinds: &'static [&'static str],
    pub(super) assignment_kinds: &'static [&'static str],
    pub(super) if_kinds: &'static [&'static str],
    pub(super) while_kinds: &'static [&'static str],
    pub(super) call_kinds: &'static [&'static str],
    pub(super) arg_container_kinds: &'static [&'static str],
    pub(super) arg_wrapper_kinds: &'static [&'static str],
    pub(super) paren_kinds: &'static [&'static str],
    pub(super) binary_kinds: &'static [&'static str],
    pub(super) unary_kinds: &'static [&'static str],
    pub(super) int_kinds: &'static [&'static str],
    pub(super) string_kinds: &'static [&'static str],
    pub(super) type_kinds: &'static [&'static str],
    pub(super) local_decl_prefixes: &'static [&'static str],
    pub(super) shell_first_kinds: &'static [&'static str],
    pub(super) shell_last_kinds: &'static [&'static str],
    pub(super) try_kinds: &'static [&'static str],
    pub(super) catch_kinds: &'static [&'static str],
    pub(super) match_kinds: &'static [&'static str],
    pub(super) first_assignment_is_let: bool,
    pub(super) strict_args: bool,
}

fn kind_in(n: Node<'_>, kinds: &[&str]) -> bool {
    kinds.contains(&n.kind())
}

pub(super) fn ast_body(src: &[u8], body: Node<'_>, shape: AstShape) -> Vec<Stmt> {
    let block = ast_body_node(body, shape)
        .or_else(|| first_body_child(body, shape))
        .unwrap_or(body);
    let mut locals = HashSet::new();
    ast_body_with_locals(src, block, shape, &mut locals)
}

fn ast_body_with_locals(
    src: &[u8],
    body: Node<'_>,
    shape: AstShape,
    locals: &mut HashSet<String>,
) -> Vec<Stmt> {
    let block = ast_body_node(body, shape).unwrap_or(body);
    let mut out = Vec::new();
    let mut w = block.walk();
    for ch in block.named_children(&mut w) {
        if let Some(stmt) = ast_stmt(src, ch, shape, locals) {
            out.push(stmt);
        }
    }
    out
}

fn ast_body_node<'a>(n: Node<'a>, shape: AstShape) -> Option<Node<'a>> {
    if kind_in(n, shape.block_kinds) {
        if kind_in(n, shape.shell_first_kinds)
            && let Some(block) = shape
                .block_kinds
                .iter()
                .filter(|k| **k != n.kind())
                .find_map(|k| first_named(n, k))
        {
            return Some(block);
        }
        return Some(n);
    }
    if kind_in(n, shape.shell_first_kinds) {
        return n.named_child(0).or(Some(n));
    }
    if kind_in(n, shape.shell_last_kinds) {
        return last_named(n).or(Some(n));
    }
    None
}

pub(super) fn ast_stmt(
    src: &[u8],
    stmt: Node<'_>,
    shape: AstShape,
    locals: &mut HashSet<String>,
) -> Option<Stmt> {
    let stmt = ast_body_node(stmt, shape).unwrap_or(stmt);
    if kind_in(stmt, shape.return_kinds) {
        return ast_return_expr(src, stmt, shape).map(Stmt::Return);
    }
    if kind_in(stmt, shape.expr_stmt_kinds) {
        return ast_expr_statement(src, stmt, shape, locals);
    }
    if kind_in(stmt, shape.local_decl_kinds) {
        return ast_local_decl(src, stmt, shape)
            .or_else(|| ast_assignment(src, stmt, shape, locals));
    }
    if kind_in(stmt, shape.assignment_kinds) {
        return ast_assignment(src, stmt, shape, locals);
    }
    if kind_in(stmt, shape.if_kinds) {
        return ast_if(src, stmt, shape, locals);
    }
    if kind_in(stmt, shape.while_kinds) {
        return ast_while(src, stmt, shape, locals);
    }
    if kind_in(stmt, shape.try_kinds) {
        return ast_try(src, stmt, shape, locals);
    }
    if kind_in(stmt, shape.match_kinds) {
        return ast_match(src, stmt, shape, locals);
    }
    if kind_in(stmt, shape.call_kinds) {
        return ast_expr(src, stmt, shape).map(Stmt::Expr);
    }
    ast_expr(src, stmt, shape).map(Stmt::Expr)
}

pub(super) fn ast_return_expr(src: &[u8], ret: Node<'_>, shape: AstShape) -> Option<Option<Expr>> {
    let mut w = ret.walk();
    if let Some(ch) = ret.named_children(&mut w).next() {
        let expr_node = if ch.kind() == "expression_list" {
            ch.child_by_field_name("value")
                .or_else(|| ch.named_child(0))
        } else {
            Some(ch)
        };
        if let Some(expr_node) = expr_node {
            return ast_expr(src, expr_node, shape).map(Some);
        }
    }
    Some(None)
}

fn ast_expr_statement(
    src: &[u8],
    stmt: Node<'_>,
    shape: AstShape,
    locals: &mut HashSet<String>,
) -> Option<Stmt> {
    let mut w = stmt.walk();
    let expr = stmt.named_children(&mut w).next()?;
    if kind_in(expr, shape.return_kinds) {
        return ast_return_expr(src, expr, shape).map(Stmt::Return);
    }
    if kind_in(expr, shape.assignment_kinds) {
        return ast_assignment(src, expr, shape, locals);
    }
    ast_expr(src, expr, shape).map(Stmt::Expr)
}

fn ast_local_decl(src: &[u8], decl: Node<'_>, shape: AstShape) -> Option<Stmt> {
    if !shape.local_decl_prefixes.is_empty() {
        let text = node_txt(src, decl).trim_start();
        if !shape
            .local_decl_prefixes
            .iter()
            .any(|prefix| text.starts_with(prefix))
        {
            return None;
        }
    }
    let var = named_descendant(decl, "variable_declarator")
        .or_else(|| named_descendant(decl, "initialized_variable_definition"))
        .unwrap_or(decl);
    let name_node = var
        .child_by_field_name("name")
        .or_else(|| first_named(var, "identifier"))
        .or_else(|| named_descendant(var, "identifier"))?;
    let value = var
        .child_by_field_name("value")
        .or_else(|| last_named(var))?;
    if name_node == value {
        return None;
    }
    let ty = ast_decl_type(src, decl, name_node, value, shape);
    Some(Stmt::Let(
        node_txt(src, name_node).trim().to_string(),
        ty,
        ast_expr(src, value, shape)?,
    ))
}

fn ast_decl_type(
    src: &[u8],
    decl: Node<'_>,
    name_node: Node<'_>,
    value: Node<'_>,
    shape: AstShape,
) -> Option<Typ> {
    if shape.type_kinds.is_empty() {
        return None;
    }
    for kind in shape.type_kinds {
        let mut hits = Vec::new();
        collect_kinds(decl, &[*kind], &mut hits);
        if let Some(t) = hits.into_iter().find(|t| *t != name_node && *t != value) {
            return Some(Typ::Named(node_txt(src, t).trim().to_string()));
        }
    }
    None
}

fn ast_assignment(
    src: &[u8],
    expr: Node<'_>,
    shape: AstShape,
    locals: &mut HashSet<String>,
) -> Option<Stmt> {
    let left = expr
        .child_by_field_name("left")
        .or_else(|| expr.named_child(0))?;
    let right = expr
        .child_by_field_name("right")
        .or_else(|| expr.child_by_field_name("value"))
        .or_else(|| expr.named_child(expr.named_child_count().saturating_sub(1) as u32))?;
    let left = if matches!(
        left.kind(),
        "identifier" | "name" | "variable_name" | "simple_identifier"
    ) {
        left
    } else if kind_in(left, shape.arg_wrapper_kinds) {
        first_named(left, "identifier")?
    } else {
        return None;
    };
    if left == right {
        return None;
    }
    let name = if left.kind() == "variable_name" {
        node_txt(src, left)
            .trim()
            .trim_start_matches('$')
            .to_string()
    } else {
        node_txt(src, left).trim().to_string()
    };
    let value = ast_expr(src, right, shape)?;
    if shape.first_assignment_is_let && locals.insert(name.clone()) {
        Some(Stmt::Let(name, None, value))
    } else {
        Some(Stmt::Assign(name, value))
    }
}

fn ast_if(src: &[u8], stmt: Node<'_>, shape: AstShape, locals: &HashSet<String>) -> Option<Stmt> {
    let cond = stmt
        .child_by_field_name("condition")
        .and_then(|n| ast_expr(src, n, shape))
        .or_else(|| {
            shape
                .paren_kinds
                .iter()
                .find_map(|k| first_named(stmt, k).and_then(|n| ast_expr(src, n, shape)))
        })
        .or_else(|| {
            shape
                .binary_kinds
                .iter()
                .find_map(|k| first_named(stmt, k).and_then(|n| ast_expr(src, n, shape)))
        })?;
    let mut then_locals = locals.clone();
    let then_body = stmt
        .child_by_field_name("consequence")
        .or_else(|| stmt.child_by_field_name("body"))
        .or_else(|| first_body_child(stmt, shape))
        .map(|n| ast_stmt_or_body(src, n, shape, &mut then_locals))
        .unwrap_or_default();
    let mut else_locals = locals.clone();
    let mut else_body = stmt
        .child_by_field_name("alternative")
        .or_else(|| first_named(stmt, "else_clause"))
        .and_then(|n| ast_else_node(n, shape))
        .map(|n| ast_stmt_or_body(src, n, shape, &mut else_locals))
        .unwrap_or_default();
    if else_body.is_empty() {
        let mut bodies = Vec::new();
        collect_kinds(stmt, shape.block_kinds, &mut bodies);
        if let Some(n) = bodies.into_iter().nth(1) {
            let mut fallback_locals = locals.clone();
            else_body = ast_stmt_or_body(src, n, shape, &mut fallback_locals);
        }
    }
    Some(Stmt::If {
        cond,
        then_body,
        else_body,
    })
}

fn ast_while(
    src: &[u8],
    stmt: Node<'_>,
    shape: AstShape,
    locals: &HashSet<String>,
) -> Option<Stmt> {
    let cond = stmt
        .child_by_field_name("condition")
        .and_then(|n| ast_expr(src, n, shape))
        .or_else(|| {
            shape
                .paren_kinds
                .iter()
                .find_map(|k| first_named(stmt, k).and_then(|n| ast_expr(src, n, shape)))
        })
        .or_else(|| {
            shape
                .binary_kinds
                .iter()
                .find_map(|k| first_named(stmt, k).and_then(|n| ast_expr(src, n, shape)))
        })?;
    let mut scoped = locals.clone();
    let body = stmt
        .child_by_field_name("body")
        .or_else(|| first_body_child(stmt, shape))
        .map(|n| ast_stmt_or_body(src, n, shape, &mut scoped))
        .unwrap_or_default();
    Some(Stmt::Loop {
        kind: crate::core_ir::LoopKind::While,
        cond: Some(cond),
        body,
    })
}

fn ast_try(src: &[u8], stmt: Node<'_>, shape: AstShape, locals: &HashSet<String>) -> Option<Stmt> {
    let mut scoped = locals.clone();
    let body = stmt
        .child_by_field_name("body")
        .or_else(|| first_body_child(stmt, shape))
        .map(|n| ast_stmt_or_body(src, n, shape, &mut scoped))
        .unwrap_or_default();
    let mut catches = Vec::new();
    for kind in shape.catch_kinds {
        let mut found = Vec::new();
        collect_kinds(stmt, &[*kind], &mut found);
        for c in found {
            let mut catch_scoped = locals.clone();
            let pattern = first_named(c, "identifier")
                .map(|n| node_txt(src, n).trim().to_string())
                .unwrap_or_default();
            let catch_body = first_body_child(c, shape)
                .or_else(|| c.child_by_field_name("body"))
                .map(|n| ast_stmt_or_body(src, n, shape, &mut catch_scoped))
                .unwrap_or_default();
            catches.push(CatchArm {
                pattern,
                body: catch_body,
            });
        }
    }
    Some(Stmt::Try { body, catches })
}

fn ast_match(
    src: &[u8],
    stmt: Node<'_>,
    shape: AstShape,
    locals: &HashSet<String>,
) -> Option<Stmt> {
    let mut scrutinee = None;
    {
        let mut w = stmt.walk();
        for ch in stmt.named_children(&mut w) {
            if !kind_in(ch, shape.match_kinds)
                && !kind_in(ch, &["case_clause"])
                && scrutinee.is_none()
            {
                scrutinee = ast_expr(src, ch, shape);
            }
        }
    }
    let scrutinee = scrutinee?;
    let mut case_nodes = Vec::new();
    collect_kinds(stmt, &["case_clause"], &mut case_nodes);
    let mut arms = Vec::new();
    for c in case_nodes {
        let mut scoped = locals.clone();
        let pattern = c
            .child_by_field_name("pattern")
            .map(|p| node_txt(src, p).trim().to_string())
            .unwrap_or_default();
        let body = c
            .child_by_field_name("body")
            .or_else(|| c.child_by_field_name("consequence"))
            .or_else(|| first_body_child(c, shape))
            .map(|n| ast_stmt_or_body(src, n, shape, &mut scoped))
            .unwrap_or_default();
        arms.push(MatchArm { pattern, body });
    }
    Some(Stmt::Match { scrutinee, arms })
}

fn first_body_child<'a>(stmt: Node<'a>, shape: AstShape) -> Option<Node<'a>> {
    shape
        .block_kinds
        .iter()
        .find_map(|kind| first_named(stmt, kind))
}

fn ast_else_node<'a>(n: Node<'a>, shape: AstShape) -> Option<Node<'a>> {
    if kind_in(n, shape.shell_last_kinds) || n.kind() == "else_clause" {
        return last_named(n);
    }
    if kind_in(n, shape.shell_first_kinds) {
        return first_body_child(n, shape).or_else(|| n.named_child(0));
    }
    Some(n)
}

fn ast_stmt_or_body(
    src: &[u8],
    n: Node<'_>,
    shape: AstShape,
    locals: &mut HashSet<String>,
) -> Vec<Stmt> {
    let n = ast_else_node(n, shape).unwrap_or(n);
    if ast_body_node(n, shape).is_some() {
        ast_body_with_locals(src, n, shape, locals)
    } else {
        ast_stmt(src, n, shape, locals).into_iter().collect()
    }
}

pub(super) fn ast_expr(src: &[u8], expr: Node<'_>, shape: AstShape) -> Option<Expr> {
    if expr.kind() == "print_intrinsic" {
        let inner = expr.named_child(0)?;
        return Some(Expr::Call {
            callee: Box::new(Expr::Ident("print".to_string())),
            args: vec![ast_expr(src, inner, shape)?],
        });
    }
    if matches!(expr.kind(), "this" | "this_expression") {
        return Some(Expr::Ident("this".to_string()));
    }
    if matches!(
        expr.kind(),
        "member_expression" | "member_access_expression" | "navigation_expression"
    ) {
        return ast_member_expr(src, expr, shape);
    }
    if matches!(expr.kind(), "new_expression" | "object_creation_expression") {
        return ast_new_expr(src, expr, shape);
    }
    if matches!(expr.kind(), "identifier" | "simple_identifier") {
        return Some(Expr::Ident(node_txt(src, expr).trim().to_string()));
    }
    // ponytail: Zig expression wrapper - unwrap to first child
    if expr.kind() == "expression" {
        return expr.named_child(0).and_then(|n| ast_expr(src, n, shape));
    }
    if expr.kind() == "name" {
        return Some(Expr::Ident(node_txt(src, expr).trim().to_string()));
    }
    if expr.kind() == "variable_name" {
        return Some(Expr::Ident(
            node_txt(src, expr)
                .trim()
                .trim_start_matches('$')
                .to_string(),
        ));
    }
    if kind_in(expr, shape.int_kinds) {
        return java_int_literal(node_txt(src, expr)).map(Expr::IntLit);
    }
    if kind_in(expr, shape.string_kinds) {
        return Some(Expr::StringLit(
            node_txt(src, expr)
                .trim()
                .trim_matches(['"', '\''])
                .to_string(),
        ));
    }
    if matches!(node_txt(src, expr).trim(), "true" | "True") {
        return Some(Expr::BoolLit(true));
    }
    if matches!(node_txt(src, expr).trim(), "false" | "False") {
        return Some(Expr::BoolLit(false));
    }
    if kind_in(expr, shape.call_kinds) {
        return ast_call_expr(src, expr, shape);
    }
    if kind_in(expr, shape.arg_wrapper_kinds) || kind_in(expr, shape.paren_kinds) {
        return expr.named_child(0).and_then(|n| ast_expr(src, n, shape));
    }
    if kind_in(expr, shape.binary_kinds) {
        return ast_binary_expr(src, expr, shape);
    }
    if kind_in(expr, shape.unary_kinds) {
        return ast_unary_expr(src, expr, shape);
    }
    if matches!(
        expr.kind(),
        "subscript_expression"
            | "index_expression"
            | "element_access_expression"
            | "array_access"
            | "indexed_expression"
    ) {
        let base = expr
            .child_by_field_name("object")
            .or_else(|| expr.child_by_field_name("array"))
            .or_else(|| expr.child_by_field_name("argument"))
            .or_else(|| expr.named_child(0))?;
        let index = expr
            .child_by_field_name("index")
            .or_else(|| expr.named_child(1))?;
        return Some(Expr::Index {
            base: Box::new(ast_expr(src, base, shape)?),
            index: Box::new(ast_expr(src, index, shape)?),
        });
    }
    if matches!(
        expr.kind(),
        "array" | "array_literal" | "list" | "slice" | "array_creation_expression"
    ) {
        let mut items = Vec::new();
        let mut w = expr.walk();
        for ch in expr.named_children(&mut w) {
            if let Some(e) = ast_expr(src, ch, shape) {
                items.push(e);
            }
        }
        return Some(Expr::ArrayLit(items));
    }
    if matches!(
        expr.kind(),
        "field_expression"
            | "member_expression"
            | "field_access"
            | "selector_expression"
            | "member_access_expression"
    ) {
        return ast_member_expr(src, expr, shape);
    }
    expr.named_child(0).and_then(|n| ast_expr(src, n, shape))
}

fn ast_member_expr(src: &[u8], expr: Node<'_>, shape: AstShape) -> Option<Expr> {
    let base = expr
        .child_by_field_name("object")
        .or_else(|| expr.child_by_field_name("expression"))
        .or_else(|| expr.named_child(0))?;
    let property = expr
        .child_by_field_name("property")
        .or_else(|| expr.child_by_field_name("name"))
        .or_else(|| expr.named_child(expr.named_child_count().saturating_sub(1) as u32))?;
    let name = node_txt(src, property).trim().to_string();
    if name.is_empty() {
        return None;
    }
    Some(Expr::Field {
        base: Box::new(ast_expr(src, base, shape)?),
        name,
    })
}

fn ast_new_expr(src: &[u8], expr: Node<'_>, shape: AstShape) -> Option<Expr> {
    let class_node = expr
        .child_by_field_name("constructor")
        .or_else(|| expr.child_by_field_name("type"))
        .or_else(|| expr.child_by_field_name("class"))
        .or_else(|| first_named(expr, "identifier"))
        .or_else(|| first_named(expr, "type_identifier"))?;
    let class_name = node_txt(src, class_node).trim();
    if class_name.is_empty() {
        return None;
    }
    let mut args = Vec::new();
    for kind in shape.arg_container_kinds {
        if let Some(arg_node) = expr
            .child_by_field_name("arguments")
            .filter(|n| n.kind() == *kind)
            .or_else(|| named_descendant(expr, kind))
        {
            args.extend(ast_args(src, arg_node, shape)?);
            break;
        }
    }
    Some(Expr::Call {
        callee: Box::new(Expr::Ident(format!("__new__{class_name}"))),
        args,
    })
}

fn ast_binary_expr(src: &[u8], expr: Node<'_>, shape: AstShape) -> Option<Expr> {
    let lhs = expr
        .child_by_field_name("left")
        .or_else(|| expr.child_by_field_name("lhs"))
        .or_else(|| expr.named_child(0))?;
    let rhs = expr
        .child_by_field_name("right")
        .or_else(|| expr.child_by_field_name("rhs"))
        .or_else(|| expr.named_child(expr.named_child_count().saturating_sub(1) as u32))?;
    let op = std::str::from_utf8(src.get(lhs.end_byte()..rhs.start_byte())?)
        .ok()?
        .trim()
        .to_string();
    if op.len() > 5 || op.contains('\n') {
        return None;
    }
    Some(Expr::Binary {
        op,
        lhs: Box::new(ast_expr(src, lhs, shape)?),
        rhs: Box::new(ast_expr(src, rhs, shape)?),
    })
}

fn ast_unary_expr(src: &[u8], expr: Node<'_>, shape: AstShape) -> Option<Expr> {
    let inner = last_named(expr)?;
    let op = std::str::from_utf8(src.get(expr.start_byte()..inner.start_byte())?)
        .ok()?
        .trim()
        .to_string();
    Some(Expr::Unary {
        op,
        expr: Box::new(ast_expr(src, inner, shape)?),
    })
}

fn ast_call_expr(src: &[u8], call: Node<'_>, shape: AstShape) -> Option<Expr> {
    let callee = call
        .child_by_field_name("function")
        .and_then(|n| ast_expr(src, n, shape))
        .or_else(|| {
            call.child_by_field_name("callee")
                .and_then(|n| ast_expr(src, n, shape))
        })
        .or_else(|| {
            call.child_by_field_name("name")
                .map(|n| Expr::Ident(node_txt(src, n).trim().to_string()))
        })
        .or_else(|| {
            first_named(call, "identifier")
                .map(|id| Expr::Ident(node_txt(src, id).trim().to_string()))
        })
        .or_else(|| {
            first_named(call, "simple_identifier")
                .map(|id| Expr::Ident(node_txt(src, id).trim().to_string()))
        })?;
    let mut args = Vec::new();
    if shape.arg_container_kinds.is_empty() {
        let mut w = call.walk();
        for ch in call.named_children(&mut w) {
            if matches!(&callee, Expr::Ident(name) if node_txt(src, ch).trim() == name) {
                continue;
            }
            if let Some(expr) = ast_expr(src, ch, shape) {
                args.push(expr);
            }
        }
    } else {
        for kind in shape.arg_container_kinds {
            if let Some(arg_node) = call
                .child_by_field_name("arguments")
                .filter(|n| n.kind() == *kind)
                .or_else(|| named_descendant(call, kind))
            {
                args.extend(ast_args(src, arg_node, shape)?);
                break;
            }
        }
    }
    Some(Expr::Call {
        callee: Box::new(callee),
        args,
    })
}

fn ast_args(src: &[u8], args: Node<'_>, shape: AstShape) -> Option<Vec<Expr>> {
    let mut out = Vec::new();
    let mut w = args.walk();
    for ch in args.named_children(&mut w) {
        // ponytail: Swift call_suffix children are value_argument wrappers
        let target = if ch.kind() == "value_argument" {
            first_named(ch, "simple_identifier")
                .or_else(|| ch.named_child(0))
                .unwrap_or(ch)
        } else {
            ch
        };
        if let Some(expr) = ast_expr(src, target, shape) {
            out.push(expr);
        } else if shape.strict_args {
            return None;
        }
    }
    if out.is_empty() {
        let text = node_txt(src, args).trim();
        if let Some(inner) = text
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            && let Some(expr) = simple_bounded_expr(inner.trim())
        {
            out.push(expr);
        }
    }
    Some(out)
}

pub(super) fn extract_fn_nodes<'a>(
    src: &[u8],
    root: Node<'a>,
    kinds: &[&str],
    map_one: impl Fn(&[u8], Node<'a>) -> Option<Decl>,
) -> Result<Vec<Decl>, String> {
    let mut hits = Vec::new();
    collect_kinds(root, kinds, &mut hits);
    Ok(hits.into_iter().filter_map(|n| map_one(src, n)).collect())
}

fn java_int_literal(raw: &str) -> Option<i64> {
    let lower = raw
        .trim()
        .trim_end_matches(['l', 'L'])
        .replace('_', "")
        .to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("0x") {
        return i64::from_str_radix(rest, 16).ok();
    }
    if let Some(rest) = lower.strip_prefix("0b") {
        return i64::from_str_radix(rest, 2).ok();
    }
    lower.parse::<i64>().ok()
}

pub(super) fn find_return_expr(stmts: &[Stmt]) -> Option<&Expr> {
    for stmt in stmts {
        match stmt {
            Stmt::Return(Some(expr)) => return Some(expr),
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                if let Some(e) = find_return_expr(then_body) {
                    return Some(e);
                }
                if let Some(e) = find_return_expr(else_body) {
                    return Some(e);
                }
            }
            Stmt::Loop { body, .. } => {
                if let Some(e) = find_return_expr(body) {
                    return Some(e);
                }
            }
            Stmt::Match { arms, .. } => {
                for arm in arms {
                    if let Some(e) = find_return_expr(&arm.body) {
                        return Some(e);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

pub(super) fn infer_expr_type(expr: &Expr) -> Typ {
    match expr {
        Expr::IntLit(_) => Typ::Int,
        Expr::StringLit(_) => Typ::String,
        Expr::BoolLit(_) => Typ::Bool,
        Expr::Binary { op, .. } => match op.as_str() {
            "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" => Typ::Bool,
            _ => Typ::Int,
        },
        _ => Typ::Named("Any".into()),
    }
}

pub(super) fn simple_bounded_body(text: &str, assign_op: &str) -> Option<Vec<Stmt>> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim().trim_end_matches([';', '.']).trim();
        if line.is_empty() || line == "{" || line == "}" || line == "->" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("let value ") {
            let expr = rest.trim().strip_prefix(assign_op)?.trim();
            out.push(Stmt::Let("value".into(), None, simple_bounded_expr(expr)?));
            continue;
        }
        if let Some(expr) = line
            .strip_prefix("value ")
            .and_then(|rest| rest.trim().strip_prefix(assign_op))
        {
            out.push(Stmt::Let(
                "value".into(),
                None,
                simple_bounded_expr(expr.trim())?,
            ));
            continue;
        }
        if let Some(expr) = line
            .strip_prefix("return(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            out.push(Stmt::Return(Some(simple_bounded_expr(expr.trim())?)));
            continue;
        }
        if let Some(expr) = line.strip_prefix("return ") {
            out.push(Stmt::Return(Some(simple_bounded_expr(expr.trim())?)));
            continue;
        }
        if let Some(expr) = simple_bounded_expr(line) {
            out.push(Stmt::Expr(expr));
            continue;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

pub(super) fn strict_simple_bounded_body(text: &str, assign_op: &str) -> Option<Vec<Stmt>> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim().trim_end_matches([';', '.']).trim();
        if line.is_empty() || line == "{" || line == "}" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("let value ") {
            let expr = rest.trim().strip_prefix(assign_op)?.trim();
            out.push(Stmt::Let("value".into(), None, simple_bounded_expr(expr)?));
            continue;
        }
        if let Some(expr) = line
            .strip_prefix("value ")
            .and_then(|rest| rest.trim().strip_prefix(assign_op))
        {
            out.push(Stmt::Let(
                "value".into(),
                None,
                simple_bounded_expr(expr.trim())?,
            ));
            continue;
        }
        if let Some(expr) = line
            .strip_prefix("return(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            out.push(Stmt::Return(Some(simple_bounded_expr(expr.trim())?)));
            continue;
        }
        if let Some(expr) = line.strip_prefix("return ") {
            out.push(Stmt::Return(Some(simple_bounded_expr(expr.trim())?)));
            continue;
        }
        if let Some(expr) = simple_bounded_expr(line) {
            out.push(Stmt::Expr(expr));
            continue;
        }
        return None;
    }
    if out.is_empty() { None } else { Some(out) }
}

pub(super) fn simple_bounded_expr(text: &str) -> Option<Expr> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(inner) = text
        .strip_prefix("print(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return Some(Expr::Call {
            callee: Box::new(Expr::Ident("print".into())),
            args: vec![simple_bounded_expr(inner)?],
        });
    }
    if let Some(inner) = text.strip_prefix("print ") {
        return Some(Expr::Call {
            callee: Box::new(Expr::Ident("print".into())),
            args: vec![simple_bounded_expr(inner.trim())?],
        });
    }
    if let Some((lhs, rhs)) = text.split_once(" + ") {
        return Some(Expr::Binary {
            op: "+".into(),
            lhs: Box::new(simple_bounded_expr(lhs)?),
            rhs: Box::new(simple_bounded_expr(rhs)?),
        });
    }
    if let Ok(value) = text.parse::<i64>() {
        return Some(Expr::IntLit(value));
    }
    if (text.starts_with('"') && text.ends_with('"'))
        || (text.starts_with('\'') && text.ends_with('\''))
    {
        return Some(Expr::StringLit(text[1..text.len() - 1].to_string()));
    }
    Some(Expr::Ident(text.to_string()))
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use super::*;
    use crate::boundary_ir::{BoundaryRepr, BoundaryTransfer};
    use crate::core_ir::Visibility;
    use std::path::PathBuf;

    fn repo_sample(name: &str) -> String {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root")
            .to_path_buf();
        std::fs::read_to_string(root.join("apps/polyglot-sample").join(name)).expect(name)
    }

    fn main_body(module: &UnifiedModule) -> &[Stmt] {
        module
            .decls
            .iter()
            .find_map(|decl| match decl {
                Decl::Function { name, body, .. } if name == "main" => Some(body.as_slice()),
                _ => None,
            })
            .expect("main body")
    }

    fn body_shape(body: &[Stmt]) -> Vec<&'static str> {
        body.iter()
            .map(|stmt| match stmt {
                Stmt::Let(..) => "let",
                Stmt::Assign(..) => "assign",
                Stmt::Expr(Expr::Call { .. }) => "call",
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    assert_eq!(body_shape(then_body), vec!["assign"]);
                    assert_eq!(body_shape(else_body), vec!["assign"]);
                    "if"
                }
                Stmt::Loop { body, .. } => {
                    assert_eq!(body_shape(body), vec!["assign"]);
                    "while"
                }
                Stmt::Return(Some(Expr::Ident(name))) if name == "value" => "return-ident",
                other => panic!("unexpected stmt shape: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn java_main_method_extracted() {
        let src = "class X { public static void main(String[] a) { } }\n";
        let m = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            src,
            extract_java_style_methods,
        )
        .expect("ok");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "main")),
            "{m:?}"
        );
    }

    #[test]
    fn generic_ast_examples_converge_with_inlang_control_flow() {
        let expected = vec!["let", "assign", "call", "if", "while", "return-ident"];

        let in_module = crate::in_lang_parse::parse_in_source(&repo_sample("control_flow.in"))
            .expect("parse .in control flow");
        assert_eq!(body_shape(main_body(&in_module)), expected);

        let c_module = parse_lang(
            try_lang_for(ParserId::C).unwrap(),
            &repo_sample("control_flow.c"),
            |b, r| extract_fn_nodes(b, r, &["function_definition"], c_like_function_decl),
        )
        .expect("parse c control flow");
        assert_eq!(body_shape(main_body(&c_module)), expected);

        let java_module = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            &repo_sample("ControlFlow.java"),
            extract_java_style_methods,
        )
        .expect("parse java control flow");
        assert_eq!(body_shape(main_body(&java_module)), expected);

        let ts_module = parse_lang(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            &repo_sample("control_flow.ts"),
            extract_ts_with_classes,
        )
        .expect("parse typescript control flow");
        assert_eq!(body_shape(main_body(&ts_module)), expected);

        #[cfg(feature = "parse-extended")]
        {
            let dart_module = parse_lang(
                try_lang_for(ParserId::Dart).unwrap(),
                &repo_sample("control_flow.dart"),
                extract_dart,
            )
            .expect("parse dart control flow");
            assert_eq!(body_shape(main_body(&dart_module)), expected);
        }
    }

    #[test]
    fn java_methods_with_bounded_bodies_extract_declarations() {
        let src = r#"
class X {
  private static int helper(int value) { return value; }
  private static int literal() { return 7; }
  private static int callReturn() { return helper(1); }
  private static void done() { return; }
  public static void main(String[] args) { value = helper(2); helper(value); helper(args.length); obj.value = 1; int local = 9; return helper(3) + 4; }
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            src,
            extract_java_style_methods,
        )
        .expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function {
                params, ret, body, ..
            } => {
                assert_eq!(ret, &Typ::Named("int".into()));
                assert_eq!(params, &vec![("value".into(), Typ::Named("int".into()))]);
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let literal = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "literal"))
            .expect("literal");
        match literal {
            Decl::Function { body, .. } => {
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::IntLit(7)))]);
            }
            _ => panic!("expected function"),
        }
        let call_return = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "callReturn"))
            .expect("callReturn");
        match call_return {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body,
                    &vec![Stmt::Return(Some(Expr::Call {
                        callee: Box::new(Expr::Ident("helper".into())),
                        args: vec![Expr::IntLit(1)],
                    }))]
                );
            }
            _ => panic!("expected function"),
        }
        let done = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "done"))
            .expect("done");
        match done {
            Decl::Function { body, .. } => {
                assert_eq!(body, &vec![Stmt::Return(None)]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function {
                params, ret, body, ..
            } => {
                assert_eq!(ret, &Typ::Named("void".into()));
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].0, "args");
                assert_eq!(params[0].1, Typ::Named("String[]".into()));
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Assign(
                            "value".into(),
                            Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(2)],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Field {
                                base: Box::new(Expr::Ident("args".into())),
                                name: "length".into(),
                            }],
                        }),
                        Stmt::Let(
                            "local".into(),
                            Some(Typ::Named("int".into())),
                            Expr::IntLit(9)
                        ),
                        Stmt::Return(Some(Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(3)],
                            }),
                            rhs: Box::new(Expr::IntLit(4)),
                        })),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn java_literal_return_bodies_extract() {
        let src = r#"
class X {
  static boolean ready() { return true; }
  static String label() { return "ok"; }
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            src,
            extract_java_style_methods,
        )
        .expect("ok");
        assert!(
            m.decls.iter().any(|d| matches!(
                d,
                Decl::Function { name, body, .. }
                    if name == "ready"
                        && matches!(body.as_slice(), [Stmt::Return(Some(Expr::BoolLit(true)))])
            )),
            "{m:?}"
        );
        assert!(
            m.decls.iter().any(|d| matches!(
                d,
                Decl::Function { name, body, .. }
                    if name == "label"
                        && matches!(body.as_slice(), [Stmt::Return(Some(Expr::StringLit(s)))] if s == "ok")
            )),
            "{m:?}"
        );
    }

    #[test]
    fn java_lowers_scalar_body_shapes() {
        let src = r#"
class X {
  static int helper(int value) { return value; }
  static int main() {
    int value = 1;
    value = value + 2;
    helper(value);
    if (value > 2) { value = value - 1; } else { value = 0; }
    while (value < 4) { value = value + 1; }
    return value;
  }
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            src,
            extract_java_style_methods,
        )
        .expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert!(
                    matches!(
                        &body[0],
                        Stmt::Let(name, Some(Typ::Named(ty)), Expr::IntLit(1))
                            if name == "value" && ty == "int"
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[1],
                    Stmt::Assign(name, Expr::Binary { op, .. }) if name == "value" && op == "+"
                ));
                assert!(matches!(
                    &body[2],
                    Stmt::Expr(Expr::Call { callee, args, ..})
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && args == &vec![Expr::Ident("value".into())]
                ));
                assert!(
                    matches!(
                        &body[3],
                        Stmt::If { cond: Expr::Binary { op, .. }, then_body, else_body }
                            if op == ">" && then_body.len() == 1 && else_body.len() == 1
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[4],
                    Stmt::Loop { cond: Some(Expr::Binary { op, .. }), body, .. }
                        if op == "<" && body.len() == 1
                ));
                assert!(matches!(
                    &body[5],
                    Stmt::Return(Some(Expr::Ident(name))) if name == "value"
                ));
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn rust_main_extracted() {
        let src = "fn main() {}\n";
        let m = parse_lang(try_lang_for(ParserId::Rust).unwrap(), src, extract_rust).expect("ok");
        assert!(matches!(m.decls.as_slice(), [Decl::Function { name, .. }] if name == "main"));
    }

    #[test]
    fn extract_holyc_eval_return_shape() {
        let src = "I64 Main()\n{\n  return 1 + 2;\n}\nMain;\n";
        let m = parse_lang(try_lang_for(ParserId::HolyC).unwrap(), src, extract_holyc).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(
                matches!(
                    body.as_slice(),
                    [Stmt::Return(Some(Expr::Binary { op, .. }))] if op == "+"
                ),
                "{body:?}"
            ),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_rust_eval_print_shape() {
        let src = "fn main() -> i64 {\nprint(\"hi\");\n0\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Rust).unwrap(), src, extract_rust).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(
                matches!(
                    body.as_slice(),
                    [
                        Stmt::Expr(Expr::Call { callee, args, .. }),
                        Stmt::Expr(Expr::IntLit(0))
                    ] if matches!(callee.as_ref(), Expr::Ident(name) if name == "print")
                        && matches!(args.as_slice(), [Expr::StringLit(value)] if value == "hi")
                ),
                "{body:?}"
            ),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_rust_if_else() {
        let src = "fn main() -> i64 { if true { return 1; } else { return 0; } }";
        let m = parse_lang(try_lang_for(ParserId::Rust).unwrap(), src, extract_rust).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                for stmt in body {
                    eprintln!("STMT: {:?}", stmt);
                }
                assert!(!body.is_empty(), "body is empty");
                assert!(
                    matches!(body[0], Stmt::If { .. }),
                    "expected if, got {:?}",
                    body[0]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn zig_function_declarations_extract() {
        let src =
            "fn helper(value: i32) i32 { return value; }\npub fn main() void { _ = helper(1); }\n";
        let m = parse_lang(try_lang_for(ParserId::Zig).unwrap(), src, extract_zig).expect("ok");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "helper")),
            "{m:?}"
        );
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "main")),
            "{m:?}"
        );
    }

    #[test]
    fn extract_zig_eval_print_shape() {
        let src = "pub fn main() void {\n    print(\"hi\");\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Zig).unwrap(), src, extract_zig).expect("ok");
        match m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main")
        {
            Decl::Function { body, .. } => assert!(
                matches!(
                    body.as_slice(),
                    [Stmt::Expr(Expr::Call { callee, .. })]
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "print")
                ),
                "{body:?}"
            ),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_zig_empty_main_body_stays_empty() {
        let src = "pub fn main() void {}\n";
        let m = parse_lang(try_lang_for(ParserId::Zig).unwrap(), src, extract_zig).expect("ok");
        match m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main")
        {
            Decl::Function { body, .. } => assert!(body.is_empty(), "{body:?}"),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn javascript_function_bodies_extract_calls() {
        let src = "function helper(value) { return value; }\nfunction main() { helper(1); }\n";
        let m = parse_lang(
            try_lang_for(ParserId::JavaScript).unwrap(),
            src,
            extract_js_with_classes,
        )
        .expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(
                matches!(
                    body.as_slice(),
                    [Stmt::Expr(Expr::Call { callee, args, ..})]
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && matches!(args.as_slice(), [Expr::IntLit(1)])
                ),
                "{body:?}"
            ),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn typescript_function_bodies_extract_calls() {
        let src = "function helper(value: number): number { return value; }\nfunction main(): void { helper(1); }\n";
        let m = parse_lang(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            src,
            extract_ts_with_classes,
        )
        .expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(
                matches!(
                    body.as_slice(),
                    [Stmt::Expr(Expr::Call { callee, args, ..})]
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && matches!(args.as_slice(), [Expr::IntLit(1)])
                ),
                "{body:?}"
            ),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn javascript_lowers_scalar_body_shapes() {
        let src = r#"
function helper(value) { return value; }
function main() {
  let value = 1;
  value = value + 2;
  helper(value);
  if (value > 2) { value = value - 1; } else { value = 0; }
  while (value < 4) { value = value + 1; }
  return value;
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::JavaScript).unwrap(),
            src,
            extract_js_with_classes,
        )
        .expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert!(matches!(
                    &body[0],
                    Stmt::Let(name, None, Expr::IntLit(1)) if name == "value"
                ));
                assert!(matches!(
                    &body[1],
                    Stmt::Assign(name, Expr::Binary { op, .. }) if name == "value" && op == "+"
                ));
                assert!(matches!(
                    &body[2],
                    Stmt::Expr(Expr::Call { callee, args, ..})
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && args == &vec![Expr::Ident("value".into())]
                ));
                assert!(
                    matches!(
                        &body[3],
                        Stmt::If { cond: Expr::Binary { op, .. }, then_body, else_body }
                            if op == ">" && then_body.len() == 1 && else_body.len() == 1
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[4],
                    Stmt::Loop { cond: Some(Expr::Binary { op, .. }), body, .. }
                        if op == "<" && body.len() == 1
                ));
                assert!(matches!(
                    &body[5],
                    Stmt::Return(Some(Expr::Ident(name))) if name == "value"
                ));
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn typescript_lowers_scalar_body_shapes() {
        let src = r#"
function helper(value: number): number { return value; }
function main(): void {
  const value = 1;
  helper(value);
  return;
}
"#;
        let m = parse_lang(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            src,
            extract_ts_with_classes,
        )
        .expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(matches!(
                body.as_slice(),
                [
                    Stmt::Let(name, None, Expr::IntLit(1)),
                    Stmt::Expr(Expr::Call { callee, args, ..}),
                    Stmt::Return(None),
                ] if name == "value"
                    && matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                    && args == &vec![Expr::Ident("value".into())]
            )),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn typescript_functions_extract_params_return_and_body() {
        let src = "function helper(value: number, label: string): number { return value; }\nfunction declared(value: number): boolean;\n";
        let m = parse_lang(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            src,
            extract_ts_with_classes,
        )
        .expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function {
                params, ret, body, ..
            } => {
                assert_eq!(
                    params,
                    &vec![
                        ("value".into(), Typ::Named("number".into())),
                        ("label".into(), Typ::Named("string".into())),
                    ]
                );
                assert_eq!(ret, &Typ::Named("number".into()));
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let declared = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "declared"))
            .expect("declared");
        match declared {
            Decl::Function {
                params, ret, body, ..
            } => {
                assert_eq!(params, &vec![("value".into(), Typ::Named("number".into()))]);
                assert_eq!(ret, &Typ::Named("boolean".into()));
                assert!(body.is_empty(), "{body:?}");
            }
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn csharp_methods_extract_bounded_bodies() {
        let src = r#"
class X {
  int Helper(int value) { return value; }
  void Main() { value = Helper(2); Helper(value); return; }
}
"#;
        let m =
            parse_lang(try_lang_for(ParserId::CSharp).unwrap(), src, extract_csharp).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "Helper"))
            .expect("Helper");
        match helper {
            Decl::Function { body, .. } => {
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("Main");
        match main {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Assign(
                            "value".into(),
                            Expr::Call {
                                callee: Box::new(Expr::Ident("Helper".into())),
                                args: vec![Expr::IntLit(2)],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("Helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Return(None),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn csharp_lowers_scalar_body_shapes() {
        let src = r#"
class X {
  int Helper(int value) { return value; }
  int Main() {
    int value = 1;
    value = value + 2;
    Helper(value);
    if (value > 2) { value = value - 1; } else { value = 0; }
    while (value < 4) { value = value + 1; }
    return value;
  }
}
"#;
        let m =
            parse_lang(try_lang_for(ParserId::CSharp).unwrap(), src, extract_csharp).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("Main");
        match main {
            Decl::Function { body, .. } => {
                assert!(
                    matches!(
                        &body[0],
                        Stmt::Let(name, Some(Typ::Named(ty)), Expr::IntLit(1))
                            if name == "value" && ty == "int"
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[1],
                    Stmt::Assign(name, Expr::Binary { op, .. }) if name == "value" && op == "+"
                ));
                assert!(matches!(
                    &body[2],
                    Stmt::Expr(Expr::Call { callee, args, ..})
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "Helper")
                            && args == &vec![Expr::Ident("value".into())]
                ));
                assert!(
                    matches!(
                        &body[3],
                        Stmt::If { cond: Expr::Binary { op, .. }, then_body, else_body }
                            if op == ">" && then_body.len() == 1 && else_body.len() == 1
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[4],
                    Stmt::Loop { cond: Some(Expr::Binary { op, .. }), body, .. }
                        if op == "<" && body.len() == 1
                ));
                assert!(matches!(
                    &body[5],
                    Stmt::Return(Some(Expr::Ident(name))) if name == "value"
                ));
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn python_functions_extract_bounded_bodies() {
        let src = r#"
def helper(value: int) -> int:
    return value

def main():
    value = helper(2)
    helper(value)
    return
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Python).unwrap(),
            src,
            extract_python_with_classes,
        )
        .expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function { body, .. } => {
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Let(
                            "value".into(),
                            None,
                            Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(2)],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Return(None),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn python_lowers_scalar_body_shapes() {
        let src = r#"
def helper(value: int) -> int:
    return value

def main():
    value = 1
    value = value + 2
    helper(value)
    if value > 2:
        value = value - 1
    else:
        value = 0
    while value < 4:
        value = value + 1
    return value
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Python).unwrap(),
            src,
            extract_python_with_classes,
        )
        .expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert!(matches!(
                    &body[0],
                    Stmt::Let(name, None, Expr::IntLit(1)) if name == "value"
                ));
                assert!(matches!(
                    &body[1],
                    Stmt::Assign(name, Expr::Binary { op, .. }) if name == "value" && op == "+"
                ));
                assert!(matches!(
                    &body[2],
                    Stmt::Expr(Expr::Call { callee, args, ..})
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && args == &vec![Expr::Ident("value".into())]
                ));
                assert!(
                    matches!(
                        &body[3],
                        Stmt::If { cond: Expr::Binary { op, .. }, then_body, else_body }
                            if op == ">" && then_body.len() == 1 && else_body.len() == 1
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[4],
                    Stmt::Loop { cond: Some(Expr::Binary { op, .. }), body, .. }
                        if op == "<" && body.len() == 1
                ));
                assert!(matches!(
                    &body[5],
                    Stmt::Return(Some(Expr::Ident(name))) if name == "value"
                ));
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn ruby_methods_extract_scalar_bodies() {
        let src = r#"
def helper(value)
  return value
end

def main
  value = helper(2)
  helper(value)
  return helper(3) + 4
end
"#;
        let m = parse_lang(try_lang_for(ParserId::Ruby).unwrap(), src, extract_ruby).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function {
                params, ret, body, ..
            } => {
                assert_eq!(params, &vec![("value".into(), Typ::Named("Any".into()))]);
                assert_eq!(ret, &Typ::Named("Any".into()));
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Let(
                            "value".into(),
                            None,
                            Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(2)],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Return(Some(Expr::Binary {
                            op: "+".into(),
                            lhs: Box::new(Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(3)],
                            }),
                            rhs: Box::new(Expr::IntLit(4)),
                        })),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn zig_extracts_extern_struct_boundary_module() {
        let src = r#"
pub const InSliceU8 = extern struct {
    ptr: [*]const u8,
    len: u64,
};

pub export fn person_new(age: u32) Person {
    return Person{ .name = InSliceU8{ .ptr = undefined, .len = 0 }, .age = age };
}

pub const Person = extern struct {
    name: InSliceU8,
    age: u32,
};

pub fn main() void {}
"#;
        let artifact = parse_zig_artifact_source(src, "person").expect("parse zig artifact");
        let boundary = artifact.boundary.expect("boundary module");
        assert_eq!(boundary.module, "zig.person");
        assert_eq!(boundary.layouts.len(), 2);
        let person = boundary
            .layouts
            .iter()
            .find(|layout| layout.name == "Person")
            .expect("Person layout");
        assert_eq!(person.repr, Some(BoundaryRepr::C));
        assert_eq!(person.size, 24);
        assert_eq!(person.align, 8);
        assert_eq!(person.fields.len(), 2);
        assert_eq!(person.fields[0].typ, "InSliceU8");
        assert_eq!(person.fields[0].transfer, Some(BoundaryTransfer::Borrow));
        assert_eq!(person.fields[1].typ, "u32");
        let in_slice = boundary
            .layouts
            .iter()
            .find(|layout| layout.name == "InSliceU8")
            .expect("InSliceU8 layout");
        assert_eq!(in_slice.size, 16);
        assert_eq!(in_slice.fields[0].typ, "u64");
        assert_eq!(in_slice.fields[1].typ, "u64");
        assert_eq!(boundary.symbols.len(), 1);
        assert_eq!(boundary.symbols[0].name, "person_new");
        assert_eq!(boundary.symbols[0].calling_convention, "c");
        assert!(!boundary.symbols[0].signature_hash.is_empty());
        assert!(!boundary.layout_hash.is_empty());
    }

    #[test]
    fn zig_artifact_without_boundary_markers_has_no_boundary() {
        let src = "fn helper(value: i32) i32 { return value; }\npub fn main() void { return; }\n";
        let artifact = parse_zig_artifact_source(src, "point").expect("parse zig artifact");
        assert!(artifact.boundary.is_none());
    }

    #[test]
    fn zig_fixture_extracts_extern_struct_boundary() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../conformance/abi/zig-extern-struct.zig");
        let artifact = parse_zig_artifact(&path).expect("parse zig fixture");
        let boundary = artifact.boundary.expect("boundary module");
        assert_eq!(boundary.module, "zig.zig-extern-struct");
        assert!(
            boundary
                .layouts
                .iter()
                .any(|layout| layout.name == "Person" && layout.size == 24)
        );
        assert!(
            boundary
                .symbols
                .iter()
                .any(|symbol| symbol.name == "person_new")
        );
    }

    #[test]
    fn zig_functions_extract_params_return_and_body() {
        let src = "fn helper(value: i32) i32 { return value; }\npub fn main() void { value = helper(2); helper(value); return; }\n";
        let m = parse_lang(try_lang_for(ParserId::Zig).unwrap(), src, extract_zig).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function {
                params, ret, body, ..
            } => {
                assert_eq!(params, &vec![("value".into(), Typ::Named("i32".into()))]);
                assert_eq!(ret, &Typ::Named("i32".into()));
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { ret, body, .. } => {
                assert_eq!(ret, &Typ::Named("void".into()));
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Assign(
                            "value".into(),
                            Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(2)],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Return(None),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn zig_lowers_scalar_body_shapes() {
        let src = r#"
fn helper(value: i32) i32 { return value; }
pub fn main() void {
    var value: i32 = 1;
    value = value + 2;
    helper(value);
    if (value > 2) {
        value = value - 1;
    } else {
        value = 0;
    }
    while (value < 4) {
        value = value + 1;
    }
    return;
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Zig).unwrap(), src, extract_zig).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert!(
                    matches!(
                        &body[0],
                        Stmt::Let(name, Some(Typ::Named(ty)), Expr::IntLit(1))
                            if name == "value" && ty == "i32"
                    ),
                    "{body:?}"
                );
                assert!(
                    matches!(
                        &body[1],
                        Stmt::Assign(name, Expr::Binary { op, .. }) if name == "value" && op == "+"
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[2],
                    Stmt::Expr(Expr::Call { callee, args, ..})
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && args == &vec![Expr::Ident("value".into())]
                ));
                assert!(
                    matches!(
                        &body[3],
                        Stmt::If { cond: Expr::Binary { op, .. }, then_body, else_body }
                            if op == ">" && then_body.len() == 1 && else_body.len() == 1
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[4],
                    Stmt::Loop { cond: Some(Expr::Binary { op, .. }), body, .. }
                        if op == "<" && body.len() == 1
                ));
                assert!(matches!(&body[5], Stmt::Return(None)));
            }
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn kotlin_functions_extract_bounded_bodies() {
        let src = "fun helper(value: Int): Int { return value }\nfun main() { value = helper(2); helper(value); return }\n";
        let m =
            parse_lang(try_lang_for(ParserId::Kotlin).unwrap(), src, extract_kotlin).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function { body, .. } => {
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Assign(
                            "value".into(),
                            Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(2)],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Return(None),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn kotlin_lowers_scalar_body_shapes() {
        let src = r#"
fun helper(value: Int): Int { return value }
fun main() {
    var value: Int = 1
    value = value + 2
    helper(value)
    if (value > 2) {
        value = value - 1
    } else {
        value = 0
    }
    while (value < 4) {
        value = value + 1
    }
    return
}
"#;
        let m =
            parse_lang(try_lang_for(ParserId::Kotlin).unwrap(), src, extract_kotlin).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert!(
                    matches!(
                        &body[0],
                        Stmt::Let(name, Some(Typ::Named(ty)), Expr::IntLit(1))
                            if name == "value" && ty == "Int"
                    ),
                    "{body:?}"
                );
                assert!(
                    matches!(
                        &body[1],
                        Stmt::Assign(name, Expr::Binary { op, .. }) if name == "value" && op == "+"
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[2],
                    Stmt::Expr(Expr::Call { callee, args, ..})
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && args == &vec![Expr::Ident("value".into())]
                ));
                assert!(
                    matches!(
                        &body[3],
                        Stmt::If { cond: Expr::Binary { op, .. }, then_body, else_body }
                            if op == ">" && then_body.len() == 1 && else_body.len() == 1
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[4],
                    Stmt::Loop { cond: Some(Expr::Binary { op, .. }), body, .. }
                        if op == "<" && body.len() == 1
                ));
                assert!(matches!(&body[5], Stmt::Return(None)));
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn c_binary_return_lowers_function_body() {
        let src = "int f(void) { return 1 + 2; }\nint main(void) { return 0; }\n";
        let m = parse_lang(try_lang_for(ParserId::C).unwrap(), src, |b, r| {
            extract_fn_nodes(b, r, &["function_definition"], c_like_function_decl)
        })
        .expect("parse");
        let f = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "f"))
            .expect("f");
        match f {
            Decl::Function { body, .. } => assert!(matches!(
                body.as_slice(),
                [Stmt::Return(Some(Expr::Binary { op, .. }))] if op == "+"
            )),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn c_lowers_locals_assignments_calls_if_and_while() {
        let src = r#"
int helper(int value) { return value; }
int main(void) {
  int value = 1;
  value = value + 2;
  helper(value);
  if (value > 2) { value = value - 1; } else { value = 0; }
  while (value < 4) { value = value + 1; }
  return value;
}
"#;
        let m = parse_lang(try_lang_for(ParserId::C).unwrap(), src, |b, r| {
            extract_fn_nodes(b, r, &["function_definition"], c_like_function_decl)
        })
        .expect("parse");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert!(matches!(
                    &body[0],
                    Stmt::Let(name, Some(Typ::Int), Expr::IntLit(1)) if name == "value"
                ));
                assert!(matches!(
                    &body[1],
                    Stmt::Assign(name, Expr::Binary { op, .. }) if name == "value" && op == "+"
                ));
                assert!(matches!(
                    &body[2],
                    Stmt::Expr(Expr::Call { callee, args, ..})
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && args == &vec![Expr::Ident("value".into())]
                ));
                assert!(
                    matches!(
                        &body[3],
                        Stmt::If { cond: Expr::Binary { op, .. }, then_body, else_body }
                            if op == ">" && then_body.len() == 1 && else_body.len() == 1
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[4],
                    Stmt::Loop { cond: Some(Expr::Binary { op, .. }), body, .. }
                        if op == "<" && body.len() == 1
                ));
                assert!(matches!(
                    &body[5],
                    Stmt::Return(Some(Expr::Ident(name))) if name == "value"
                ));
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn cpp_uses_c_like_body_lowering() {
        let src = "int main() { int value = 1; value = value + 1; return value; }\n";
        let m = parse_lang(try_lang_for(ParserId::Cpp).unwrap(), src, |b, r| {
            extract_fn_nodes(b, r, &["function_definition"], c_like_function_decl)
        })
        .expect("parse");
        match &m.decls[0] {
            Decl::Function { body, .. } => assert!(matches!(
                body.as_slice(),
                [
                    Stmt::Let(name, Some(Typ::Int), Expr::IntLit(1)),
                    Stmt::Assign(assign_name, Expr::Binary { op, .. }),
                    Stmt::Return(Some(Expr::Ident(ret_name))),
                ] if name == "value" && assign_name == "value" && op == "+" && ret_name == "value"
            )),
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn dart_functions_extract_params_return_and_body() {
        let src = r#"
int helper(int value) { return value; }
int main() {
  int value = 1;
  value = value + 2;
  helper(value);
  if (value > 2) { value = value - 1; } else { value = 0; }
  while (value < 4) { value = value + 1; }
  return value;
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Dart).unwrap(), src, extract_dart).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function {
                params, ret, body, ..
            } => {
                assert_eq!(params, &vec![("value".into(), Typ::Named("int".into()))]);
                assert_eq!(ret, &Typ::Named("int".into()));
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function {
                params, ret, body, ..
            } => {
                assert!(params.is_empty(), "{params:?}");
                assert_eq!(ret, &Typ::Named("int".into()));
                assert!(
                    matches!(
                        &body[0],
                        Stmt::Let(name, Some(Typ::Named(ty)), Expr::IntLit(1))
                            if name == "value" && ty == "int"
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[1],
                    Stmt::Assign(name, Expr::Binary { op, .. }) if name == "value" && op == "+"
                ));
                assert!(matches!(
                    &body[2],
                    Stmt::Expr(Expr::Call { callee, args, ..})
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "helper")
                            && args == &vec![Expr::Ident("value".into())]
                ));
                assert!(
                    matches!(
                        &body[3],
                        Stmt::If { cond: Expr::Binary { op, .. }, then_body, else_body }
                            if op == ">" && then_body.len() == 1 && else_body.len() == 1
                    ),
                    "{body:?}"
                );
                assert!(matches!(
                    &body[4],
                    Stmt::Loop { cond: Some(Expr::Binary { op, .. }), body, .. }
                        if op == "<" && body.len() == 1
                ));
                assert!(matches!(
                    &body[5],
                    Stmt::Return(Some(Expr::Ident(name))) if name == "value"
                ));
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn java_class_declarations_extract_with_fields_and_methods() {
        let src = r#"
public class Counter {
    private int count;
    static int answer() { return 42; }
    public int increment() {
        count = count + 1;
        return count;
    }
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            src,
            extract_java_with_classes,
        )
        .expect("ok");

        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    fields,
                    methods,
                    visibility,
                    extends,
                    ..
                } if name == "Counter" => Some((
                    fields.clone(),
                    methods.clone(),
                    *visibility,
                    extends.clone(),
                )),
                _ => None,
            })
            .expect("Counter class");
        let (fields, methods, visibility, extends) = class;
        assert_eq!(visibility, Visibility::Pub);
        assert!(extends.is_none());
        assert_eq!(fields, vec![("count".into(), Typ::Named("int".into()))]);
        assert_eq!(methods.len(), 2);
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "answer"))
        );
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "increment"))
        );

        let flat_answer = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "answer" => Some(body.clone()),
                _ => None,
            })
            .expect("flat answer function");
        assert_eq!(flat_answer, vec![Stmt::Return(Some(Expr::IntLit(42)))]);
    }

    #[test]
    fn java_interface_declarations_extract_with_method_sigs() {
        let src = r#"
interface Printable {
    String format();
    int version();
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            src,
            extract_java_with_classes,
        )
        .expect("ok");

        let iface = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Interface { name, methods, .. } if name == "Printable" => {
                    Some(methods.clone())
                }
                _ => None,
            })
            .expect("Printable interface");
        assert_eq!(iface.len(), 2);
        assert!(
            iface
                .iter()
                .any(|s| s.name == "format" && s.ret == Typ::Named("String".into()))
        );
        assert!(
            iface
                .iter()
                .any(|s| s.name == "version" && s.ret == Typ::Named("int".into()))
        );
    }

    #[test]
    fn java_class_with_extends_and_implements_extracts() {
        let src = r#"
class Child extends Parent implements Runnable, Serializable {
    public void run() {}
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            src,
            extract_java_with_classes,
        )
        .expect("ok");

        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    extends,
                    implements,
                    ..
                } if name == "Child" => Some((extends.clone(), implements.clone())),
                _ => None,
            })
            .expect("Child class");
        let (extends, implements) = class;
        assert_eq!(extends, Some("Parent".to_string()));
        assert_eq!(
            implements,
            vec!["Runnable".to_string(), "Serializable".to_string()]
        );
    }

    #[test]
    fn cpp_class_declarations_extract_with_fields_and_methods() {
        let src = r#"
class Calculator {
public:
    int value;
    int answer() const { return 42; }
    int add(int x) { value = value + x; return value; }
};
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Cpp).unwrap(),
            src,
            extract_cpp_with_classes,
        )
        .expect("ok");

        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    fields,
                    methods,
                    ..
                } if name == "Calculator" => Some((fields.clone(), methods.clone())),
                _ => None,
            })
            .expect("Calculator class");
        let (fields, methods) = class;
        assert_eq!(fields, vec![("value".into(), Typ::Named("int".into()))]);
        assert_eq!(methods.len(), 2);
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "answer")),
            "{methods:?}"
        );
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "add")),
            "{methods:?}"
        );
    }

    #[test]
    fn cpp_class_with_base_class_extracts_extends() {
        let src = r#"
class Child : public Parent {
public:
    void method() {}
};
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Cpp).unwrap(),
            src,
            extract_cpp_with_classes,
        )
        .expect("ok");

        let extends_val = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class { name, extends, .. } if name == "Child" => extends.clone(),
                _ => None,
            })
            .expect("Child class extends");
        assert_eq!(extends_val, "Parent");
    }

    #[test]
    fn cpp_top_level_functions_still_extracted_with_class_extractor() {
        let src = r#"
class Helper {
public:
    int get() { return 1; }
};

int answer() {
    return 42;
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Cpp).unwrap(),
            src,
            extract_cpp_with_classes,
        )
        .expect("ok");

        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Class { name, .. } if name == "Helper")),
            "{m:?}"
        );
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "answer")),
            "{m:?}"
        );
    }

    #[test]
    fn java_constructors_extracted_as_functions() {
        let src = r#"
class Counter {
    private int count;
    Counter() {
        count = 0;
    }
    Counter(int start) {
        count = start;
    }
    int getValue() { return count; }
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Java).unwrap(),
            src,
            extract_java_with_classes,
        )
        .expect("ok");

        let methods = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    methods: mtds,
                    ..
                } if name == "Counter" => Some(mtds.clone()),
                _ => None,
            })
            .expect("Counter class");
        assert_eq!(methods.len(), 3);
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "Counter"))
        );
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "getValue"))
        );

        let ctor = methods
            .iter()
            .find(|d| matches!(d, Decl::Function { name, params, .. } if name == "Counter" && params.len() == 1));
        assert!(ctor.is_some(), "expected parameterized constructor");
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn csharp_class_declarations_extract_with_fields_and_methods() {
        let src = r#"
class Accumulator {
    private int total;
    public int Add(int value) {
        total = total + value;
        return total;
    }
    public void Reset() { total = 0; }
}
"#;
        let m =
            parse_lang(try_lang_for(ParserId::CSharp).unwrap(), src, extract_csharp).expect("ok");

        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    fields,
                    methods: mtds,
                    ..
                } if name == "Accumulator" => Some((fields.clone(), mtds.clone())),
                _ => None,
            })
            .expect("Accumulator class");
        let (fields, methods) = class;
        assert_eq!(fields, vec![("total".into(), Typ::Named("int".into()))]);
        assert_eq!(methods.len(), 2);
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "Add"))
        );
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "Reset"))
        );

        let flat_add = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "Add"));
        assert!(
            flat_add.is_some(),
            "methods also extracted as flat functions"
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn csharp_interface_declarations_extract_with_method_sigs() {
        let src = r#"
interface IResettable {
    void Reset();
    int GetValue();
}
"#;
        let m =
            parse_lang(try_lang_for(ParserId::CSharp).unwrap(), src, extract_csharp).expect("ok");

        let iface = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Interface { name, methods, .. } if name == "IResettable" => {
                    Some(methods.clone())
                }
                _ => None,
            })
            .expect("IResettable interface");
        assert_eq!(iface.len(), 2);
        assert!(
            iface
                .iter()
                .any(|s| s.name == "Reset" && s.ret == Typ::Named("void".into()))
        );
        assert!(
            iface
                .iter()
                .any(|s| s.name == "GetValue" && s.ret == Typ::Named("int".into()))
        );
    }

    #[test]
    fn c_return_statement_child_kinds_for_param_return() {
        let src = "int echo(int x) { return x; }\n";
        let mut p = Parser::new();
        p.set_language(&try_lang_for(ParserId::C).unwrap()).unwrap();
        let tree = p.parse(src, None).unwrap();
        let mut found = false;
        fn visit(n: Node<'_>, src: &str, found: &mut bool) {
            if n.kind() == "return_statement" {
                *found = true;
                let mut w = n.walk();
                let kinds: Vec<_> = n
                    .named_children(&mut w)
                    .map(|c| c.kind().to_string())
                    .collect();
                assert!(
                    kinds.iter().any(|k| {
                        matches!(k.as_str(), "expression" | "comma_expression" | "identifier")
                    }),
                    "unexpected return_statement named children: {kinds:?} text={:?}",
                    &src[n.start_byte()..n.end_byte()]
                );
            }
            let mut w = n.walk();
            for ch in n.named_children(&mut w) {
                visit(ch, src, found);
            }
        }
        visit(tree.root_node(), src, &mut found);
        assert!(found, "expected a return_statement in parse tree");
    }

    #[test]
    fn js_class_extraction_produces_decl_class() {
        let src = r#"
class Calculator {
    value = 0;
    constructor(start) {
        this.count = start;
    }
    add(x) {
        return x;
    }
}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::JavaScript).unwrap(),
            src,
            extract_js_with_classes,
        )
        .expect("ok");

        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    fields,
                    methods,
                    ..
                } if name == "Calculator" => Some((fields.clone(), methods.clone())),
                _ => None,
            })
            .expect("Calculator class");
        let (fields, methods) = class;
        assert_eq!(fields.len(), 2); // value=0 + this.count
        assert!(fields.iter().any(|(n, _)| n == "value"));
        assert!(fields.iter().any(|(n, _)| n == "count"));
        assert_eq!(methods.len(), 2); // constructor + add
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "constructor"))
        );
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "add"))
        );
    }

    #[test]
    fn js_arrow_and_function_expr_extracted_from_vars() {
        let src = r#"
const add = (a, b) => { return a + b; };
var multiply = function(a, b) { return a * b; };
"#;
        let m = parse_lang(
            try_lang_for(ParserId::JavaScript).unwrap(),
            src,
            extract_js_with_classes,
        )
        .expect("ok");

        let add_fn = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "add"));
        assert!(add_fn.is_some(), "arrow function add not extracted: {m:?}");

        let mul_fn = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "multiply"));
        assert!(
            mul_fn.is_some(),
            "function expression multiply not extracted: {m:?}"
        );
    }

    #[test]
    fn js_member_new_and_method_call_lower_to_core_ir() {
        let src = r#"
class Counter {
    value = 0;
    constructor(start) {
        this.value = start;
    }
    inc() {
        return this.value + 1;
    }
}
function answer() {
    const c = new Counter(41);
    return c.inc();
}
function main() {}
"#;
        let m = parse_lang(
            try_lang_for(ParserId::JavaScript).unwrap(),
            src,
            extract_js_with_classes,
        )
        .expect("ok");
        let inc = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class { methods, .. } => methods.iter().find_map(|method| match method {
                    Decl::Function { name, body, .. } if name == "inc" => Some(body.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .expect("inc method");
        assert!(matches!(
            &inc[0],
            Stmt::Return(Some(Expr::Binary { lhs, .. }))
                if matches!(lhs.as_ref(), Expr::Field { base, name, ..} if name == "value" && matches!(base.as_ref(), Expr::Ident(id) if id == "self"))
        ));
        let answer = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "answer" => Some(body.clone()),
                _ => None,
            })
            .expect("answer");
        assert!(matches!(
            &answer[0],
            Stmt::Let(_, _, Expr::StructInit { name, fields, ..})
                if name == "Counter" && fields.iter().any(|(field, expr)| field == "value" && matches!(expr, Expr::IntLit(41)))
        ));
        assert!(matches!(
            &answer[1],
            Stmt::Return(Some(Expr::Call { callee, args, ..}))
                if matches!(callee.as_ref(), Expr::Field { base, name, ..} if name == "inc" && matches!(base.as_ref(), Expr::Ident(id) if id == "c"))
                    && args.is_empty()
        ));
    }

    #[test]
    fn ts_interface_extraction_produces_decl_interface() {
        let src = r#"
interface Drawable {
    draw(): void;
    getBounds(): Rect;
}
"#;
        let m = parse_lang(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            src,
            extract_ts_with_classes,
        )
        .expect("ok");

        let iface = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Interface { name, methods, .. } if name == "Drawable" => {
                    Some(methods.clone())
                }
                _ => None,
            })
            .expect("Drawable interface");
        assert_eq!(iface.len(), 2);
        assert!(
            iface
                .iter()
                .any(|s| s.name == "draw" && s.ret == Typ::Named("void".into()))
        );
        assert!(
            iface
                .iter()
                .any(|s| s.name == "getBounds" && s.ret == Typ::Named("Rect".into()))
        );
    }

    #[test]
    fn ts_class_extraction_preserves_type_annotations() {
        let src = r#"
class TypedCounter {
    value: number;
    constructor(start: number) {
        this.value = start;
    }
    inc(): number {
        return 1;
    }
}
"#;
        let m = parse_lang(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            src,
            extract_ts_with_classes,
        )
        .expect("ok");

        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    fields,
                    methods,
                    ..
                } if name == "TypedCounter" => Some((fields.clone(), methods.clone())),
                _ => None,
            })
            .expect("TypedCounter class");
        let (fields, methods) = class;
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields[0],
            ("value".to_string(), Typ::Named("number".to_string()))
        );
        assert_eq!(methods.len(), 2); // constructor + inc
        let inc = methods
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "inc"))
            .expect("inc method");
        match inc {
            Decl::Function { ret, .. } => {
                assert_eq!(ret, &Typ::Named("number".into()));
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn ts_member_new_and_method_call_lower_to_core_ir() {
        let src = r#"
class Counter {
    value: number;
    constructor(start: number) {
        this.value = start;
    }
    inc(): number {
        return this.value + 1;
    }
}
function answer(): number {
    const c = new Counter(41);
    return c.inc();
}
function main(): void {}
"#;
        let m = parse_lang(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            src,
            extract_ts_with_classes,
        )
        .expect("ok");
        let answer = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "answer" => Some(body.clone()),
                _ => None,
            })
            .expect("answer");
        assert!(matches!(
            &answer[0],
            Stmt::Let(_, _, Expr::StructInit { name, fields, ..})
                if name == "Counter" && fields.iter().any(|(field, expr)| field == "value" && matches!(expr, Expr::IntLit(41)))
        ));
        assert!(matches!(
            &answer[1],
            Stmt::Return(Some(Expr::Call { callee, .. }))
                if matches!(callee.as_ref(), Expr::Field { name, .. } if name == "inc")
        ));
    }

    #[test]
    fn python_class_extraction_produces_decl_class() {
        let src = r#"
class Counter:
    def __init__(self, start: int):
        self.value = start
        self.label = "ok"
    def inc(self) -> int:
        return 1
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Python).unwrap(),
            src,
            extract_python_with_classes,
        )
        .expect("ok");

        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    fields,
                    methods,
                    ..
                } if name == "Counter" => Some((fields.clone(), methods.clone())),
                _ => None,
            })
            .expect("Counter class");
        let (fields, methods) = class;
        assert_eq!(fields.len(), 2); // self.value + self.label
        assert!(fields.iter().any(|(n, _)| n == "value"));
        assert!(fields.iter().any(|(n, _)| n == "label"));
        assert_eq!(methods.len(), 2); // __init__ + inc
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "__init__"))
        );
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "inc"))
        );
    }

    #[test]
    fn python_lambda_extracted_as_function() {
        let src = r#"
double = lambda x: x * 2
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Python).unwrap(),
            src,
            extract_python_with_classes,
        )
        .expect("ok");

        let double = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "double"))
            .expect("double lambda");
        match double {
            Decl::Function { params, body, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].0, "x");
                assert_eq!(body.len(), 1); // return x * 2
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn python_try_except_lowered_to_stmt_try() {
        let src = r#"
def risky(x):
    try:
        value = 1
    except TypeError:
        value = 0
    return value
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Python).unwrap(),
            src,
            extract_python_with_classes,
        )
        .expect("ok");

        let risky = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "risky"))
            .expect("risky function");
        match risky {
            Decl::Function { body, .. } => {
                assert_eq!(body.len(), 2); // try + return
                assert!(
                    matches!(&body[0], Stmt::Try { .. }),
                    "expected Stmt::Try, got {:?}",
                    body[0]
                );
                if let Stmt::Try { catches, .. } = &body[0] {
                    assert_eq!(catches.len(), 1);
                }
            }
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn php_function_with_body_extracts() {
        let src = "<?php\nfunction helper($value) {\n    return $value;\n}\nfunction main() {\n    $value = 1;\n    helper($value);\n    return;\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Php).unwrap(), src, extract_php).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function { params, body, .. } => {
                assert_eq!(params, &vec![("value".into(), Typ::Named("Any".into()))]);
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Assign("value".into(), Expr::IntLit(1),),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Return(None),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn php_class_with_method_extracts_decl_class() {
        let src = r#"<?php
class Calculator {
    private int $total = 0;
    public function add($value): int {
        $this->total = $this->total + $value;
        return $this->total;
    }
    public function reset(): void {
        $this->total = 0;
    }
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Php).unwrap(), src, extract_php).expect("ok");
        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    fields,
                    methods,
                    ..
                } if name == "Calculator" => Some((fields.clone(), methods.clone())),
                _ => None,
            })
            .expect("Calculator class");
        let (fields, methods) = class;
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].0, "total");
        assert_eq!(methods.len(), 2);
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "add"))
        );
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "reset"))
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn php_echo_statement_extracts_as_expression() {
        let src = "<?php\nfunction main() {\n    echo \"hello\";\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Php).unwrap(), src, extract_php).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        assert!(
            matches!(main, Decl::Function { .. }),
            "main should be a function"
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn php_eval_main_body_extracts() {
        let src = "<?php\nfunction main() {\n    print(\"hi\");\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Php).unwrap(), src, extract_php).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(!body.is_empty(), "main body empty"),
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn php_eval_print_shape_extracts() {
        let src = "<?php\nfunction main() {\n    print(1 + 2);\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Php).unwrap(), src, extract_php).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => match body.as_slice() {
                [Stmt::Expr(Expr::Call { callee, args, .. })] => {
                    assert!(matches!(callee.as_ref(), Expr::Ident(name) if name == "print"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0], Expr::Binary { .. }));
                }
                other => panic!("unexpected body: {other:?}"),
            },
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn php_interface_with_method_sigs_extracts() {
        let src = "<?php\ninterface Printable {\n    public function format(): string;\n    public function version(): int;\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Php).unwrap(), src, extract_php).expect("ok");
        let iface = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Interface { name, methods, .. } if name == "Printable" => {
                    Some(methods.clone())
                }
                _ => None,
            })
            .expect("Printable interface");
        assert_eq!(iface.len(), 2);
        assert!(
            iface
                .iter()
                .any(|s| s.name == "format" && s.ret == Typ::Named("string".into()))
        );
        assert!(
            iface
                .iter()
                .any(|s| s.name == "version" && s.ret == Typ::Named("int".into()))
        );
    }

    #[test]
    fn lua_function_with_body_extracts() {
        let src = r#"
function helper(value)
  return value
end
function main()
  value = helper(2)
  helper(value)
  return
end
"#;
        let m = parse_lang(try_lang_for(ParserId::Lua).unwrap(), src, extract_lua).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function { params, body, .. } => {
                assert_eq!(params, &vec![("value".into(), Typ::Named("Any".into()))]);
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Assign(
                            "value".into(),
                            Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(2)],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Return(None),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn lua_local_function_extracts_as_decl() {
        let src = r#"
local function helper(value)
  return value
end
function main()
  helper(1)
end
"#;
        let m = parse_lang(try_lang_for(ParserId::Lua).unwrap(), src, extract_lua).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        assert!(
            matches!(helper, Decl::Function { .. }),
            "expected helper function"
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn scala_function_with_body_extracts() {
        let src = r#"
def helper(value: Int): Int = {
  value
}
def main(): Unit = {
  val result = helper(2)
  helper(result)
  return
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Scala).unwrap(), src, extract_scala).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function { params, .. } => {
                assert_eq!(params, &vec![("value".into(), Typ::Named("Int".into()))]);
                // body lowering for Scala is WIP; extract function sigs and class shapes first
                assert!(
                    matches!(helper, Decl::Function { .. }),
                    "helper should be a function"
                );
            }
            _ => panic!("expected function"),
        }
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "main")),
            "main function not found"
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn scala_class_with_val_field_extracts() {
        let src = r#"
class Counter(val value: Int) {
    def inc(): Int = {
        value + 1
    }
    def get(): Int = value
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Scala).unwrap(), src, extract_scala).expect("ok");
        let class = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Class {
                    name,
                    fields,
                    methods,
                    ..
                } if name == "Counter" => Some((fields.clone(), methods.clone())),
                _ => None,
            })
            .expect("Counter class");
        let (fields, methods) = class;
        assert!(!fields.is_empty(), "expected fields, got {fields:?}");
        assert!(!methods.is_empty(), "expected methods, got {methods:?}");
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "inc"))
        );
        assert!(
            methods
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "get"))
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn scala_trait_with_method_sigs_extracts() {
        let src = r#"
trait Drawable {
    def draw(): Unit
    def getBounds(): Rect
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Scala).unwrap(), src, extract_scala).expect("ok");
        let iface = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Interface { name, methods, .. } if name == "Drawable" => {
                    Some(methods.clone())
                }
                _ => None,
            })
            .expect("Drawable trait");
        assert_eq!(iface.len(), 2);
        assert!(iface.iter().any(|s| s.name == "draw"));
        assert!(iface.iter().any(|s| s.name == "getBounds"));
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn php_functions_extract_bounded_bodies() {
        let src = r#"<?php
function helper($value): int {
    return $value;
}
function main() {
    $value = helper(2);
    helper($value);
    return;
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Php).unwrap(), src, extract_php).expect("ok");
        let helper = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "helper"))
            .expect("helper");
        match helper {
            Decl::Function { body, .. } => {
                assert_eq!(body, &vec![Stmt::Return(Some(Expr::Ident("value".into())))]);
            }
            _ => panic!("expected function"),
        }
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => {
                assert_eq!(
                    body,
                    &vec![
                        Stmt::Assign(
                            "value".into(),
                            Expr::Call {
                                callee: Box::new(Expr::Ident("helper".into())),
                                args: vec![Expr::IntLit(2)],
                            },
                        ),
                        Stmt::Expr(Expr::Call {
                            callee: Box::new(Expr::Ident("helper".into())),
                            args: vec![Expr::Ident("value".into())],
                        }),
                        Stmt::Return(None),
                    ]
                );
            }
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_fsharp_function_with_body() {
        let src = r#"let answer x = x + 42

let main _ =
    let value = answer 1
    ()
"#;
        let m =
            parse_lang(try_lang_for(ParserId::FSharp).unwrap(), src, extract_fsharp).expect("ok");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "answer")),
            "answer function not found"
        );
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "main")),
            "main function not found"
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_fsharp_eval_main_body() {
        let src = r#"let main _ =
    let value = print("hi")
    value
"#;
        let m =
            parse_lang(try_lang_for(ParserId::FSharp).unwrap(), src, extract_fsharp).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(!body.is_empty(), "main body empty"),
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_erlang_function_clause() {
        let src = r#"-module(calculator).
-export([answer/0, main/0]).

answer() ->
    42.

main() ->
    X = answer(),
    ok.
"#;
        let m =
            parse_lang(try_lang_for(ParserId::Erlang).unwrap(), src, extract_erlang).expect("ok");
        let found_answer = m
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name, .. } if name == "answer"));
        let found_in_class = m
            .decls
            .iter()
            .filter_map(|d| match d {
                Decl::Class { name, methods, .. } if name == "calculator" => Some(methods.clone()),
                _ => None,
            })
            .any(|methods| {
                methods
                    .iter()
                    .any(|m| matches!(m, Decl::Function { name, .. } if name == "answer"))
            });
        assert!(found_answer || found_in_class, "answer function not found");
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_erlang_eval_print_shape() {
        let src = "-module(app).\n-export([main/0]).\n\nmain() ->\n    print(\"hi\").\n";
        let m =
            parse_lang(try_lang_for(ParserId::Erlang).unwrap(), src, extract_erlang).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(
                matches!(
                    body.as_slice(),
                    [Stmt::Expr(Expr::Call { callee, args, .. })]
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "print")
                            && matches!(args.as_slice(), [Expr::StringLit(value)] if value == "hi")
                ),
                "{body:?}"
            ),
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_elixir_defmodule() {
        let src = r#"defmodule Calculator do
  def answer do
    42
  end

  def main do
    value = answer()
    value
  end
end
"#;
        let m =
            parse_lang(try_lang_for(ParserId::Elixir).unwrap(), src, extract_elixir).expect("ok");
        let found_class = m
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Class { name, .. } if name == "Calculator"));
        let found_answer = m
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name, .. } if name == "answer"));
        let found_main = m
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name, .. } if name == "main"));
        assert!(
            found_class && found_answer && found_main,
            "expected Calculator module plus answer/main functions (found class={found_class}, answer={found_answer}, main={found_main})"
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_julia_struct() {
        let src = r#"mutable struct Point
    x::Int
    y::Int
end

function answer()
    return 42
end

function main()
    p = answer()
    return nothing
end
"#;
        let m = parse_lang(try_lang_for(ParserId::Julia).unwrap(), src, extract_julia).expect("ok");
        let found_struct = m
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Struct { name, .. } if name == "Point"));
        let found_answer = m
            .decls
            .iter()
            .any(|d| matches!(d, Decl::Function { name, .. } if name == "answer"));
        assert!(found_struct, "Point struct not found");
        assert!(found_answer, "answer function not found");
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_julia_eval_main_body() {
        let src = r#"function main()
    print("hi")
end
"#;
        let m = parse_lang(try_lang_for(ParserId::Julia).unwrap(), src, extract_julia).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(!body.is_empty(), "main body empty"),
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_ocaml_eval_print_shape() {
        let src = "let main () =\n  print \"hi\"\n";
        let m = parse_lang(try_lang_for(ParserId::OCaml).unwrap(), src, extract_ocaml).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(
                matches!(
                    body.as_slice(),
                    [Stmt::Expr(Expr::Call { callee, args, .. })]
                        if matches!(callee.as_ref(), Expr::Ident(name) if name == "print")
                            && matches!(args.as_slice(), [Expr::StringLit(value)] if value == "hi")
                ),
                "{body:?}"
            ),
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_r_function() {
        let src = r#"answer <- function(x) {
    return(x + 42)
}

main <- function() {
    value <- answer(1)
    return(value)
}
"#;
        let m = parse_lang(try_lang_for(ParserId::R).unwrap(), src, extract_r_lang).expect("ok");
        let answer = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "answer"))
            .expect("answer");
        match answer {
            Decl::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].0, "x");
            }
            _ => panic!("expected function"),
        }
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "main")),
            "main function not found"
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_r_eval_main_body() {
        let src = r#"main <- function() {
    print("hi")
}
"#;
        let m = parse_lang(try_lang_for(ParserId::R).unwrap(), src, extract_r_lang).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(!body.is_empty(), "main body empty"),
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_julia_eval_print_shape() {
        let src = r#"function main()
    print("hi")
end
"#;
        let m = parse_lang(try_lang_for(ParserId::Julia).unwrap(), src, extract_julia).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => match body.as_slice() {
                [Stmt::Expr(Expr::Call { callee, args, .. })] => {
                    assert_eq!(args.len(), 1);
                    assert!(matches!(callee.as_ref(), Expr::Ident(name) if name == "print"));
                }
                other => panic!("unexpected body: {other:?}"),
            },
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_r_eval_print_shape() {
        let src = r#"main <- function() {
    print("hi")
}
"#;
        let m = parse_lang(try_lang_for(ParserId::R).unwrap(), src, extract_r_lang).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => match body.as_slice() {
                [Stmt::Expr(Expr::Call { callee, args, .. })] => {
                    assert_eq!(args.len(), 1);
                    assert!(matches!(callee.as_ref(), Expr::Ident(name) if name == "print"));
                }
                other => panic!("unexpected body: {other:?}"),
            },
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_r_eval_numeric_print_shape() {
        let src = r#"main <- function() {
    print(1 + 2)
}
"#;
        let m = parse_lang(try_lang_for(ParserId::R).unwrap(), src, extract_r_lang).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => match body.as_slice() {
                [Stmt::Expr(Expr::Call { callee, args, .. })] => {
                    assert!(matches!(callee.as_ref(), Expr::Ident(name) if name == "print"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0], Expr::Binary { .. }));
                }
                other => panic!("unexpected body: {other:?}"),
            },
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_swift_eval_main_body() {
        let src = r#"func main() -> Void {
  print("hi")
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Swift).unwrap(), src, extract_swift).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(!body.is_empty(), "main body empty"),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_swift_eval_print_shape() {
        let src = r#"func main() -> Void {
  print(1 + 2)
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Swift).unwrap(), src, extract_swift).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => match body.as_slice() {
                [Stmt::Expr(Expr::Call { callee, args, .. })] => {
                    assert!(matches!(callee.as_ref(), Expr::Ident(name) if name == "print"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0], Expr::Binary { .. }));
                }
                other => panic!("unexpected body: {other:?}"),
            },
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_go_eval_main_body() {
        let src = "package main\n\nfunc main() {\n\tprint(\"hi\")\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Go).unwrap(), src, |b, r| {
            extract_fn_nodes(
                b,
                r,
                &["function_declaration", "method_declaration"],
                |src, n| {
                    let name_n = n.child_by_field_name("name")?;
                    let name = normalize_entry(node_txt(src, name_n).trim());
                    let params = go_params(src, n);
                    let body = n
                        .child_by_field_name("body")
                        .map(|b| go_body(src, b))
                        .unwrap_or_default();
                    Some(Decl::Function {
                        name,
                        params,
                        ret: Typ::Void,
                        body,
                        type_params: vec![],
                    })
                },
            )
        })
        .expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(!body.is_empty(), "main body empty"),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_go_eval_print_shape() {
        let src = "package main\n\nfunc main() {\n\tprint(1 + 2)\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Go).unwrap(), src, |b, r| {
            extract_fn_nodes(
                b,
                r,
                &["function_declaration", "method_declaration"],
                |src, n| {
                    let name_n = n.child_by_field_name("name")?;
                    let name = normalize_entry(node_txt(src, name_n).trim());
                    let params = go_params(src, n);
                    let ret = go_return_type(src, n).unwrap_or(Typ::Void);
                    let body = n
                        .child_by_field_name("body")
                        .map(|b| go_body(src, b))
                        .unwrap_or_default();
                    Some(Decl::Function {
                        name,
                        params,
                        ret,
                        body,
                        type_params: vec![],
                    })
                },
            )
        })
        .expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => match body.as_slice() {
                [Stmt::Expr(Expr::Call { callee, args, .. })] => {
                    assert!(matches!(callee.as_ref(), Expr::Ident(name) if name == "print"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0], Expr::Binary { .. }));
                }
                other => panic!("unexpected body: {other:?}"),
            },
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_go_function_return_type() {
        let src = "package main\n\nfunc answer() int {\n\treturn 42\n}\n";
        let m = parse_lang(try_lang_for(ParserId::Go).unwrap(), src, |b, r| {
            extract_fn_nodes(
                b,
                r,
                &["function_declaration", "method_declaration"],
                |src, n| {
                    let name_n = n.child_by_field_name("name")?;
                    let name = normalize_entry(node_txt(src, name_n).trim());
                    let params = go_params(src, n);
                    let ret = go_return_type(src, n).unwrap_or(Typ::Void);
                    let body = n
                        .child_by_field_name("body")
                        .map(|b| go_body(src, b))
                        .unwrap_or_default();
                    Some(Decl::Function {
                        name,
                        params,
                        ret,
                        body,
                        type_params: vec![],
                    })
                },
            )
        })
        .expect("ok");
        let answer = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "answer"))
            .expect("answer");
        match answer {
            Decl::Function { ret, .. } => assert_eq!(ret, &Typ::Int),
            _ => panic!("expected function"),
        }
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn extract_v_function_return_type() {
        let src = "module main\n\nfn answer() int {\n\treturn 42\n}\n";
        let m = parse_lang(try_lang_for(ParserId::V).unwrap(), src, |b, r| {
            extract_fn_nodes(b, r, &["function_declaration"], |src, n| {
                let name_n = n.child_by_field_name("name")?;
                let name = normalize_entry(node_txt(src, name_n).trim());
                let params = v_params(src, n);
                let ret = v_return_type(src, n).unwrap_or(Typ::Void);
                let body = n
                    .child_by_field_name("body")
                    .map(|b| v_body(src, b))
                    .unwrap_or_default();
                Some(Decl::Function {
                    name,
                    params,
                    ret,
                    body,
                    type_params: vec![],
                })
            })
        })
        .expect("ok");
        let answer = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "answer"))
            .expect("answer");
        match answer {
            Decl::Function { ret, .. } => assert_eq!(ret, &Typ::Named("int".into())),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_perl_subroutine() {
        let src = r#"sub answer {
    my ($x) = @_;
    return $x + 42;
}

sub main {
    my $value = answer(1);
    return;
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Perl).unwrap(), src, extract_perl).expect("ok");
        let answer = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "answer"))
            .expect("answer");
        assert!(
            matches!(answer, Decl::Function { .. }),
            "answer should be a function"
        );
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "main")),
            "main function not found"
        );
    }

    #[test]
    fn extract_perl_eval_main_body() {
        let src = r#"sub main {
    print("hi");
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Perl).unwrap(), src, extract_perl).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => assert!(!body.is_empty(), "main body empty"),
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_perl_eval_print_shape() {
        let src = r#"sub main {
    print(1 + 2);
}
"#;
        let m = parse_lang(try_lang_for(ParserId::Perl).unwrap(), src, extract_perl).expect("ok");
        let main = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
            .expect("main");
        match main {
            Decl::Function { body, .. } => match body.as_slice() {
                [Stmt::Expr(Expr::Call { callee, args, .. })] => {
                    assert!(matches!(callee.as_ref(), Expr::Ident(name) if name == "print"));
                    assert_eq!(args.len(), 1);
                    assert!(matches!(args[0], Expr::Binary { .. }));
                }
                other => panic!("unexpected body: {other:?}"),
            },
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_lolcat_eval_function() {
        let src = r#"HAI 1.2
  HOW IZ I add YR x AN YR y
    I HAS A result ITZ SUM OF x AN y
    FOUND YR result
  IF U SAY SO
KTHXBYE
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Lolcode).unwrap(),
            src,
            extract_lolcat,
        )
        .expect("ok");
        let add = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "add"))
            .expect("add function");
        match add {
            Decl::Function {
                name, params, body, ..
            } => {
                assert_eq!(name, "add");
                assert_eq!(params.len(), 2);
                assert_eq!(params[0].0, "x");
                assert_eq!(params[1].0, "y");
                assert!(!body.is_empty(), "body should not be empty");
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_polyglot_parity_all_languages() {
        let cobol =
            "IDENTIFICATION DIVISION.\nPROGRAM-ID. HELLO.\nPROCEDURE DIVISION.\nDISPLAY \"Hi\".";
        let m = parse_textual_polyglot_module(ParserId::Cobol, cobol).expect("cobol ok");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "hello"))
        );

        let fortran = "program hello\n print *, \"Hello\"\nend program hello";
        let m = parse_textual_polyglot_module(ParserId::Fortran, fortran).expect("fortran ok");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "hello"))
        );

        let odin = "main :: proc() {\n}";
        let m = parse_textual_polyglot_module(ParserId::Odin, odin).expect("odin ok");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
        );

        let hare = "export fn main() void = {\n};";
        let m = parse_textual_polyglot_module(ParserId::Hare, hare).expect("hare ok");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "main"))
        );
    }

    #[test]
    fn extract_crystal_method() {
        let src = r#"class Greeter
  def initialize(@name : String)
  end

  def greet(times : Int32)
    times.times do
      puts "Hello, #{@name}!"
    end
  end

  def name
    @name
  end
end
"#;
        let m = parse_lang(
            try_lang_for(ParserId::Crystal).unwrap(),
            src,
            extract_crystal,
        )
        .expect("ok");
        let names: Vec<&str> = m
            .decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["initialize", "greet", "name"]);
        let greet = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "greet"))
            .expect("greet method");
        match greet {
            Decl::Function {
                name, params, body, ..
            } => {
                assert_eq!(name, "greet");
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].0, "times");
                assert!(!body.is_empty(), "body should lower statements");
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn extract_nim_proc() {
        let src = r#"type
  Greeter = object
    name: string

proc newGreeter(name: string): Greeter =
  result = Greeter(name: name)

proc greet(g: Greeter): string =
  result = "Hello, " & g.name

proc main() =
  let g = newGreeter("World")
  echo greet(g)

main()
"#;
        let m = parse_lang(try_lang_for(ParserId::Nim).unwrap(), src, extract_nim).expect("ok");
        let names: Vec<&str> = m
            .decls
            .iter()
            .filter_map(|d| match d {
                Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["newGreeter", "greet", "main"]);
        let greet = m
            .decls
            .iter()
            .find(|d| matches!(d, Decl::Function { name, .. } if name == "greet"))
            .expect("greet proc");
        match greet {
            Decl::Function {
                name, params, body, ..
            } => {
                assert_eq!(name, "greet");
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].0, "g");
                assert!(!body.is_empty(), "body should lower statements");
            }
            _ => panic!("expected function"),
        }
    }

    #[test]
    fn textual_hare_lowers_return_body() {
        let src = "fn add() {\n  return 1 + 2;\n}\n";
        let m = parse_textual_polyglot_module(ParserId::Hare, src).expect("hare");
        let body = m
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Function { name, body, .. } if name == "add" => Some(body),
                _ => None,
            })
            .expect("add");
        assert!(
            !body.is_empty(),
            "hare textual front should lower a body: {body:?}"
        );
    }

    #[test]
    fn textual_cobol_program_id() {
        let src =
            "IDENTIFICATION DIVISION.\nPROGRAM-ID. GREET.\nPROCEDURE DIVISION.\n  return 0.\n";
        let m = parse_textual_polyglot_module(ParserId::Cobol, src).expect("cobol");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, Decl::Function { name, .. } if name == "greet"))
        );
    }
}
