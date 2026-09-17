use crate::compile::{JitExecution, cmd_execute, compile_and_run_jit_source_path};
use crate::util::{extract_cargo_bin_path, resolve_invocation_path};
use crate::{InError, Result};
use inauguration::parser_registry::{self, ParserCli};
use std::path::Path;
use std::process;

pub(crate) fn cmd_eval_dispatch(
    cwd: &Path,
    code: &str,
    parser: Option<&str>,
    verbose: bool,
    debug: bool,
) -> Result<()> {
    if parser.is_none() && !has_polyglot_fences(code) {
        let blocks = split_auto_blocks(code);
        if blocks.len() > 1 {
            return cmd_auto_polyglot_eval(cwd, code, verbose, debug);
        }
    }
    cmd_eval(cwd, code, parser, verbose, debug)
}

pub(crate) fn cmd_eval_source_or_path(
    invocation_cwd: &Path,
    source: Option<String>,
    parser: Option<String>,
    verbose: bool,
    debug: bool,
) -> Result<()> {
    let code = match source {
        Some(ref s) => {
            let resolved = resolve_invocation_path(invocation_cwd, s);
            if resolved.is_dir() {
                let cargo_toml = resolved.join("Cargo.toml");
                if cargo_toml.exists() {
                    let contents = std::fs::read_to_string(&cargo_toml)
                        .map_err(|e| InError::Message(format!("read Cargo.toml: {e}")))?;
                    let bin_path = extract_cargo_bin_path(&contents, &resolved)?;
                    let module_id = bin_path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let bin_str = bin_path.to_string_lossy().to_string();
                    return cmd_execute(invocation_cwd, &bin_str, &module_id, verbose, debug);
                }
            }
            if resolved.exists() {
                let ext = resolved.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext == "in"
                    || ext == "rs"
                    || ext == "zig"
                    || ext == "go"
                    || ext == "v"
                    || ext == "swift"
                {
                    let module_id = resolved
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    return cmd_execute(invocation_cwd, s, &module_id, verbose, debug);
                }
                std::fs::read_to_string(&resolved)
                    .map_err(|e| InError::Message(format!("read {}: {e}", resolved.display())))?
            } else {
                s.clone()
            }
        }
        None => {
            return Err(InError::Message("eval requires code or file path".into()));
        }
    };
    cmd_eval_dispatch(invocation_cwd, &code, parser.as_deref(), verbose, debug)
}

pub(crate) fn cmd_eval(
    cwd: &Path,
    code: &str,
    parser: Option<&str>,
    verbose: bool,
    debug: bool,
) -> Result<()> {
    if parser.is_none() && has_polyglot_fences(code) {
        return cmd_polyglot_eval(cwd, code, verbose, debug);
    }

    let parser_id = parse_eval_parser(parser, code)?;

    #[cfg(unix)]
    if inauguration::daemon_client::daemon_is_running() {
        return cmd_eval_daemon(parser_id, code, parser, verbose, debug);
    }

    let dir = std::env::temp_dir().join(format!(
        "inaug-eval-{}-{}",
        process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| InError::Message(format!("create eval temp dir {}: {e}", dir.display())))?;
    let path = dir.join(format!("eval.{}", parser_id.default_extension()));
    let source_path = resolve_invocation_path(cwd, &path.to_string_lossy());
    let module_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("App")
        .to_string();
    let mut last_err = None;
    let mut execution = None;
    let mut print_result = false;

    for plan in eval_plans(parser_id, code) {
        std::fs::write(&path, &plan.wrapped)
            .map_err(|e| InError::Message(format!("write eval temp: {e}")))?;
        match compile_and_run_jit_source_path(&source_path, &module_id, ParserCli::Auto, debug) {
            Ok(run) => {
                print_result = plan.print_result;
                execution = Some(run);
                break;
            }
            Err(err) => last_err = Some(err),
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    let execution = match execution {
        Some(run) => run,
        None => return Err(last_err.unwrap_or_else(|| InError::Message("eval failed".to_string()))),
    };
    if verbose {
        match &execution {
            JitExecution::Int(result) => eprintln!("> {}", result),
            JitExecution::Bool(result) => eprintln!("> {}", result),
            JitExecution::Float(result) => eprintln!("> {}", result.0),
            JitExecution::String(result) => eprintln!("> {}", result),
        }
    } else if print_result {
        match execution {
            JitExecution::Int(result) => println!("{}", result),
            JitExecution::Bool(result) => println!("{}", result),
            JitExecution::Float(result) => println!("{}", result.0),
            JitExecution::String(result) => println!("{}", result),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn cmd_eval_daemon(
    parser_id: parser_registry::ParserId,
    code: &str,
    parser: Option<&str>,
    verbose: bool,
    _debug: bool,
) -> Result<()> {
    let mut last_err = None;
    for plan in eval_plans(parser_id, code) {
        let response =
            inauguration::daemon_client::daemon_eval_code(&plan.wrapped, parser, verbose)
                .map_err(|e| InError::Message(format!("daemon eval: {e}")))?;
        if response.success {
            if verbose {
                eprintln!("> {:?}", response.result);
            } else if plan.print_result
                && let Some(result) = response.result
            {
                println!("{}", result);
            }
            return Ok(());
        }
        if let Some(err) = response.error {
            last_err = Some(err);
        }
    }
    Err(InError::Message(
        last_err.unwrap_or_else(|| "daemon eval failed".to_string()),
    ))
}

pub(crate) fn cmd_polyglot_eval(cwd: &Path, code: &str, verbose: bool, debug: bool) -> Result<()> {
    let blocks = split_polyglot_blocks(code);
    if blocks.is_empty() {
        return cmd_eval(cwd, code, None, verbose, debug);
    }
    for (lang, block) in &blocks {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            continue;
        }
        eprint!("[{}] ", lang);
        match cmd_eval(cwd, trimmed, Some(lang), verbose, debug) {
            Ok(()) => {}
            Err(e) => eprintln!("failed: {e}"),
        }
    }
    Ok(())
}

pub(crate) fn cmd_auto_polyglot_eval(
    cwd: &Path,
    code: &str,
    verbose: bool,
    debug: bool,
) -> Result<()> {
    let blocks = split_auto_blocks(code);
    if blocks.len() <= 1 {
        return cmd_eval(cwd, code, None, verbose, debug);
    }
    for block in &blocks {
        let trimmed = block.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lang = match infer_eval_parser(trimmed) {
            parser_registry::ParserId::In => "in",
            parser_registry::ParserId::Rust => "rust",
            parser_registry::ParserId::JavaScript => "javascript",
            parser_registry::ParserId::TypeScript => "typescript",
            parser_registry::ParserId::Python => "python",
            parser_registry::ParserId::Zig => "zig",
            parser_registry::ParserId::Cpp => "cpp",
            _ => "in",
        };
        eprint!("[{}] ", lang);
        match cmd_eval(cwd, trimmed, Some(lang), verbose, debug) {
            Ok(()) => {}
            Err(e) => eprintln!("failed: {e}"),
        }
    }
    Ok(())
}

pub(crate) fn has_polyglot_fences(code: &str) -> bool {
    code.lines().any(|l| l.starts_with("## ") && l.len() > 3)
}

pub(crate) fn split_polyglot_blocks(code: &str) -> Vec<(&str, String)> {
    let mut blocks: Vec<(&str, String)> = Vec::new();
    let mut current_lang: Option<&str> = None;
    let mut current_block = String::new();

    for line in code.lines() {
        if let Some(lang) = line.strip_prefix("## ") {
            if let Some(lang) = current_lang.take()
                && !current_block.trim().is_empty()
            {
                blocks.push((lang, std::mem::take(&mut current_block)));
            }
            let lang_word = lang.split_whitespace().next().unwrap_or("");
            let lang = match lang_word.to_lowercase().as_str() {
                "python" | "py" => "python",
                "rust" | "rs" => "rust",
                "javascript" | "js" => "javascript",
                "typescript" | "ts" => "typescript",
                "zig" => "zig",
                "go" | "golang" => "go",
                "java" => "java",
                "kotlin" | "kt" => "kotlin",
                "scala" => "scala",
                "c" => "c",
                "cpp" | "c++" => "cpp",
                "ruby" | "rb" => "ruby",
                "php" => "php",
                "perl" | "pl" => "perl",
                "lua" => "lua",
                "csharp" | "c#" | "cs" => "csharp",
                "fsharp" | "f#" | "fs" => "fsharp",
                "swift" => "swift",
                "dart" => "dart",
                "haskell" | "hs" => "haskell",
                "ocaml" | "ml" => "ocaml",
                "elixir" | "ex" => "elixir",
                "erlang" | "erl" => "erlang",
                "julia" | "jl" => "julia",
                "r" => "r",
                "nim" => "nim",
                "d" => "d",
                "crystal" | "cr" => "crystal",
                "odin" => "odin",
                "hare" => "hare",
                "holyc" => "holyc",
                "groovy" => "groovy",
                "clojure" | "clj" => "clojure",
                "vb" | "vbnet" | "vb.net" => "vb",
                "in" | "inlang" | ".in" => "in",
                _ => lang,
            };
            current_lang = Some(lang);
        } else if current_lang.is_some() {
            current_block.push_str(line);
            current_block.push('\n');
        }
    }
    if let Some(lang) = current_lang
        && !current_block.trim().is_empty()
    {
        blocks.push((lang, current_block));
    }
    blocks
}

pub(crate) fn split_auto_blocks(code: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    for line in code.lines() {
        if line.trim().is_empty() {
            if !current.trim().is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.trim().is_empty() {
        blocks.push(current);
    }
    blocks
}

pub(crate) fn normalize_in_eval_code(code: &str) -> String {
    fn normalize_human_in_print_arg(rest: &str) -> String {
        let rest = rest.trim();
        let smart_quoted = [
            ('\'', '\''),
            ('"', '"'),
            ('\u{2018}', '\u{2019}'),
            ('\u{201C}', '\u{201D}'),
        ];
        for (open, close) in smart_quoted {
            if rest.starts_with(open)
                && rest.ends_with(close)
                && rest.len() >= open.len_utf8() + close.len_utf8()
            {
                let inner = &rest[open.len_utf8()..rest.len() - close.len_utf8()];
                let inner = if inner.ends_with("\\n") && open == '"' {
                    &inner[..inner.len() - 2]
                } else {
                    inner
                };
                return format!("\"{inner}\"");
            }
        }
        rest.to_string()
    }

    let trimmed = code.trim();
    if let Some(rest) = trimmed.strip_prefix("std.io.print ") {
        return format!("print({})", normalize_human_in_print_arg(rest));
    }
    if let Some(rest) = trimmed
        .strip_prefix("std.io.print(")
        .and_then(|r| r.strip_suffix(")"))
    {
        return format!("print({})", normalize_human_in_print_arg(rest));
    }
    if let Some(rest) = trimmed.strip_prefix("print ") {
        return format!("print({})", normalize_human_in_print_arg(rest));
    }
    if let Some(rest) = trimmed
        .strip_prefix("print(")
        .and_then(|r| r.strip_suffix(")"))
    {
        let arg = normalize_human_in_print_arg(rest);
        return format!("print({})", arg);
    }
    if let Some(expr) = trimmed
        .strip_prefix("std::cout <<")
        .and_then(|rest| rest.trim().strip_suffix(';'))
    {
        let parts: Vec<&str> = expr
            .split("<<")
            .map(str::trim)
            .filter(|part| !part.is_empty() && *part != "std::endl")
            .collect();
        if !parts.is_empty() {
            let part_count = parts.len();
            let parts: Vec<String> = parts
                .into_iter()
                .enumerate()
                .map(|(idx, part)| {
                    if idx + 1 == part_count
                        && part.starts_with('"')
                        && part.ends_with('"')
                        && part.contains("\\n")
                    {
                        part.replacen("\\n\"", "\"", 1)
                    } else {
                        part.to_string()
                    }
                })
                .collect();
            return format!("print({})", parts.join(" + "));
        }
    }
    code.replace("println(", "print(")
}

pub(crate) fn normalize_eval_php_code(code: &str) -> String {
    let code = code.replace("echo ", "print(");
    let trimmed = code.trim();
    if trimmed.starts_with("print(") && !trimmed.ends_with(')') && !trimmed.contains(';') {
        return format!("print({})\");", &code[6..code.len() - 1]);
    }
    if trimmed.starts_with("print(") && !trimmed.ends_with(';') {
        return format!("{code};");
    }
    code
}

pub(crate) fn normalize_eval_code(parser_id: parser_registry::ParserId, code: &str) -> String {
    match parser_id {
        parser_registry::ParserId::In => normalize_in_eval_code(code),
        parser_registry::ParserId::JavaScript | parser_registry::ParserId::TypeScript => code
            .replace("console.log(", "print(")
            .replace("println(", "print("),
        parser_registry::ParserId::Rust => code.replace("println!(", "print("),
        parser_registry::ParserId::Java => code.replace("System.out.println(", "print("),
        parser_registry::ParserId::Kotlin
        | parser_registry::ParserId::Scala
        | parser_registry::ParserId::Groovy => code.replace("println(", "print("),
        parser_registry::ParserId::Cpp
        | parser_registry::ParserId::ObjC
        | parser_registry::ParserId::ObjCpp => normalize_in_eval_code(code),
        parser_registry::ParserId::HolyC => code.to_string(),
        parser_registry::ParserId::Php => normalize_eval_php_code(code),
        _ => code.to_string(),
    }
}

pub(crate) fn guess_eval_type(s: &str) -> &'static str {
    let s = s.trim();
    if s == "true" || s == "false" {
        "Bool"
    } else if s.starts_with('"') {
        "String"
    } else {
        "Int"
    }
}

pub(crate) fn infer_eval_parser(code: &str) -> parser_registry::ParserId {
    let trimmed = code.trim();
    if trimmed.contains("#import ")
        || trimmed.contains("@interface")
        || trimmed.contains("@implementation")
        || trimmed.contains("@end")
    {
        return parser_registry::ParserId::ObjC;
    }
    if trimmed.contains("std::cout") || trimmed.contains("#include") || trimmed.contains("::") {
        return parser_registry::ParserId::Cpp;
    }
    if trimmed.contains("println!(") || trimmed.contains("let mut ") {
        return parser_registry::ParserId::Rust;
    }
    if trimmed.starts_with("fn main() -> void") {
        return parser_registry::ParserId::In;
    }
    if trimmed.starts_with("fn main") || trimmed.starts_with("fn ") {
        let rest =
            trimmed.trim_start_matches(|c: char| c.is_alphanumeric() || c == ' ' || c == '!');
        if let Some(rest) = rest.strip_prefix('(')
            && (rest.contains("println!") || rest.contains("let mut "))
        {
            return parser_registry::ParserId::Rust;
        }
        if trimmed.contains("println!(") {
            return parser_registry::ParserId::Rust;
        }
        if trimmed.starts_with("fn main") {
            return parser_registry::ParserId::Rust;
        }
        if trimmed.contains("-> void")
            || trimmed.contains("-> String")
            || trimmed.contains("-> Int")
        {
            return parser_registry::ParserId::In;
        }
        return parser_registry::ParserId::Rust;
    }
    if trimmed.contains("console.log(") || trimmed.contains("function ") || trimmed.contains("=>") {
        return if trimmed.contains(": number")
            || trimmed.contains(": string")
            || trimmed.contains(": boolean")
            || trimmed.contains("): ")
        {
            parser_registry::ParserId::TypeScript
        } else {
            parser_registry::ParserId::JavaScript
        };
    }
    if trimmed.starts_with("def ") || trimmed.contains("\ndef ") {
        return parser_registry::ParserId::Python;
    }
    if trimmed.contains("@import(")
        || trimmed.contains("@cImport(")
        || trimmed.contains("@TypeOf(")
        || trimmed.starts_with("_ = try ")
        || (trimmed.contains("std.fs.") && trimmed.contains('('))
        || (trimmed.contains("std.mem.") && trimmed.contains('('))
    {
        return parser_registry::ParserId::Zig;
    }
    parser_registry::ParserId::In
}

pub(crate) fn parse_eval_parser(
    parser: Option<&str>,
    code: &str,
) -> Result<parser_registry::ParserId> {
    match parser.map(str::trim).filter(|s| !s.is_empty()) {
        Some(value) if value.eq_ignore_ascii_case("auto") => Ok(infer_eval_parser(code)),
        Some(value) => parser_registry::parser_id_from_cli_token(value)
            .ok_or_else(|| InError::Message(format!("unknown eval parser `{value}`"))),
        None => Ok(infer_eval_parser(code)),
    }
}

pub(crate) fn eval_return_type(parser_id: parser_registry::ParserId, ret: &str) -> String {
    match parser_id {
        parser_registry::ParserId::In => ret.to_string(),
        parser_registry::ParserId::JavaScript => String::new(),
        parser_registry::ParserId::TypeScript => match ret {
            "Bool" => "boolean".to_string(),
            "String" => "string".to_string(),
            _ => "number".to_string(),
        },
        parser_registry::ParserId::Rust => match ret {
            "Bool" => "bool".to_string(),
            "String" => "String".to_string(),
            _ => "i64".to_string(),
        },
        parser_registry::ParserId::Python => match ret {
            "Bool" => "bool".to_string(),
            "String" => "str".to_string(),
            _ => "int".to_string(),
        },
        parser_registry::ParserId::Swift => match ret {
            "Bool" => "Bool".to_string(),
            "String" => "String".to_string(),
            _ => "Int".to_string(),
        },
        parser_registry::ParserId::Go
        | parser_registry::ParserId::V
        | parser_registry::ParserId::Nim
        | parser_registry::ParserId::D => match ret {
            "Bool" => "bool".to_string(),
            "String" => "string".to_string(),
            _ => "int".to_string(),
        },
        parser_registry::ParserId::Zig => match ret {
            "Bool" => "bool".to_string(),
            "String" => "[]const u8".to_string(),
            _ => "i32".to_string(),
        },
        parser_registry::ParserId::Dart => match ret {
            "Bool" => "bool".to_string(),
            "String" => "String".to_string(),
            _ => "int".to_string(),
        },
        parser_registry::ParserId::Scala => match ret {
            "Bool" => "Boolean".to_string(),
            "String" => "String".to_string(),
            _ => "Int".to_string(),
        },
        parser_registry::ParserId::Kotlin => match ret {
            "Bool" => "Boolean".to_string(),
            "String" => "String".to_string(),
            _ => "Int".to_string(),
        },
        parser_registry::ParserId::Crystal => match ret {
            "Bool" => "Bool".to_string(),
            "String" => "String".to_string(),
            _ => "Int32".to_string(),
        },
        parser_registry::ParserId::Hare => match ret {
            "Bool" => "bool".to_string(),
            "String" => "str".to_string(),
            _ => "int".to_string(),
        },
        parser_registry::ParserId::HolyC => match ret {
            "Bool" => "Bool".to_string(),
            "String" => "U8 *".to_string(),
            _ => "I64".to_string(),
        },
        parser_registry::ParserId::C
        | parser_registry::ParserId::Cpp
        | parser_registry::ParserId::ObjC
        | parser_registry::ParserId::ObjCpp => match ret {
            "Bool" => "bool".to_string(),
            _ => "int".to_string(),
        },
        parser_registry::ParserId::Java | parser_registry::ParserId::Groovy => match ret {
            "Bool" => "boolean".to_string(),
            "String" => "String".to_string(),
            _ => "int".to_string(),
        },
        parser_registry::ParserId::CSharp => match ret {
            "Bool" => "bool".to_string(),
            "String" => "string".to_string(),
            _ => "int".to_string(),
        },
        parser_registry::ParserId::FSharp => match ret {
            "Bool" => "bool".to_string(),
            "String" => "string".to_string(),
            _ => "int".to_string(),
        },
        parser_registry::ParserId::VbNet => match ret {
            "Bool" => "Boolean".to_string(),
            "String" => "String".to_string(),
            _ => "Integer".to_string(),
        },
        parser_registry::ParserId::OCaml => match ret {
            "Bool" => "bool".to_string(),
            "String" => "string".to_string(),
            _ => "int".to_string(),
        },
        _ => String::new(),
    }
}

pub(crate) fn wrap_eval_expression(
    parser_id: parser_registry::ParserId,
    code: &str,
    ret: &str,
) -> Option<String> {
    let ret = eval_return_type(parser_id, ret);
    match parser_id {
        parser_registry::ParserId::In => Some(format!("fn main() -> {ret} {{ return {code} }}")),
        parser_registry::ParserId::JavaScript => {
            Some(format!("function main() {{ return {code}; }}"))
        }
        parser_registry::ParserId::TypeScript => {
            Some(format!("function main(): {ret} {{ return {code}; }}"))
        }
        parser_registry::ParserId::Rust => Some(format!("fn main() -> {ret} {{ {code} }}")),
        parser_registry::ParserId::Python => {
            Some(format!("def main() -> {ret}:\n    return {code}"))
        }
        parser_registry::ParserId::Swift => {
            Some(format!("func main() -> {ret} {{\n  return {code}\n}}"))
        }
        parser_registry::ParserId::Go => Some(format!(
            "package main\n\nfunc main() {ret} {{\n\treturn {code}\n}}"
        )),
        parser_registry::ParserId::V => Some(format!(
            "module main\n\nfn main() {ret} {{\n\treturn {code}\n}}"
        )),
        parser_registry::ParserId::Zig => {
            Some(format!("pub fn main() {ret} {{\n    return {code};\n}}"))
        }
        parser_registry::ParserId::Dart => Some(format!("{ret} main() {{\n  return {code};\n}}")),
        parser_registry::ParserId::Scala => Some(format!("def main(): {ret} = {{\n  {code}\n}}")),
        parser_registry::ParserId::Haskell => {
            Some(format!("main = {}", render_haskell_eval_expr(code)))
        }
        parser_registry::ParserId::Nim => Some(format!("proc main(): {ret} =\n  {code}")),
        parser_registry::ParserId::FSharp => Some(format!("let main _ : {ret} = {code}")),
        parser_registry::ParserId::Odin => Some(format!(
            "package main\n\nmain :: proc() -> {ret} {{\n\treturn {code}\n}}\n"
        )),
        parser_registry::ParserId::D => Some(format!("{ret} main() {{ return {code}; }}")),
        parser_registry::ParserId::Crystal => Some(format!("def main : {ret}\n  {code}\nend")),
        parser_registry::ParserId::Julia => {
            Some(format!("function main()\n    return {code}\nend\n"))
        }
        parser_registry::ParserId::R => Some(format!("main <- function() {{\n    {code}\n}}\n")),
        parser_registry::ParserId::Ruby => Some(format!("def main\n  {code}\nend")),
        parser_registry::ParserId::Lua => Some(format!("function main()\n  return {code}\nend")),
        parser_registry::ParserId::Perl => Some(format!("sub main {{\n    return {code};\n}}\n")),
        parser_registry::ParserId::Php => Some(format!(
            "<?php\nfunction main() {{\n    return {code};\n}}\n"
        )),
        parser_registry::ParserId::Elixir => Some(format!("def main do\n  {code}\nend\n")),
        parser_registry::ParserId::Erlang => Some(format!(
            "-module(app).\n-export([main/0]).\n\nmain() ->\n    {code}.\n"
        )),
        parser_registry::ParserId::CSharp => Some(format!(
            "class App {{\n    static {ret} Main() {{\n        return {code};\n    }}\n}}"
        )),
        parser_registry::ParserId::Java => Some(format!(
            "class App {{\n  public static {ret} main(String[] args) {{\n    return {code};\n  }}\n}}"
        )),
        parser_registry::ParserId::Groovy => Some(format!(
            "class App {{\n  static {ret} main(String[] args) {{\n    return {code}\n  }}\n}}"
        )),
        parser_registry::ParserId::Kotlin => {
            Some(format!("fun main(): {ret} {{\n    return {code}\n}}"))
        }
        parser_registry::ParserId::Clojure => Some(format!("(defn main [] {code})\n")),
        parser_registry::ParserId::VbNet => Some(format!(
            "Function main() As {ret}\n    main = {code}\nEnd Function\n"
        )),
        parser_registry::ParserId::OCaml => Some(format!("let main =\n  {code}")),
        parser_registry::ParserId::Hare => Some(format!(
            "export fn main() {ret} = {{\n\treturn {code};\n}};"
        )),
        parser_registry::ParserId::HolyC => {
            Some(format!("{ret} Main()\n{{\n  return {code};\n}}\nMain;"))
        }
        parser_registry::ParserId::C
        | parser_registry::ParserId::Cpp
        | parser_registry::ParserId::ObjC
        | parser_registry::ParserId::ObjCpp => {
            if ret == "int" || ret == "bool" {
                Some(format!("{ret} main() {{ return {code}; }}"))
            } else {
                None
            }
        }
        parser_registry::ParserId::Fortran => Some(format!(
            "program main\n  implicit none\n  print *, {code}\nend program main"
        )),
        parser_registry::ParserId::Cobol => Some(format!(
            "IDENTIFICATION DIVISION.\nPROGRAM-ID. MAIN.\nPROCEDURE DIVISION.\n    DISPLAY {code}.\n    STOP RUN."
        )),
        parser_registry::ParserId::Lolcode => Some(format!(
            "HAI 1.2\nI HAS A RESULT ITZ {code}\nVISIBLE RESULT\nKTHXBYE"
        )),
        _ => None,
    }
}

pub(crate) fn wrap_eval_statement(
    parser_id: parser_registry::ParserId,
    code: &str,
) -> Option<String> {
    match parser_id {
        parser_registry::ParserId::In => {
            let trimmed = code.trim();
            if trimmed.starts_with("import ")
                || trimmed.starts_with("needs ")
                || trimmed.starts_with("capability ")
                || trimmed.starts_with("enable ")
                || trimmed.starts_with("parallel:")
                || trimmed.starts_with("main:")
                || trimmed.contains('\n')
            {
                Some(code.to_string())
            } else {
                Some(format!("main:\n  {trimmed}"))
            }
        }
        parser_registry::ParserId::JavaScript => Some(format!("function main() {{ {code} }}")),
        parser_registry::ParserId::TypeScript => {
            Some(format!("function main(): void {{ {code} }}"))
        }
        parser_registry::ParserId::Rust => Some(format!("fn main() -> i64 {{ {code};\n0\n}}")),
        parser_registry::ParserId::Python => Some(format!("def main() -> None:\n    {code}")),
        parser_registry::ParserId::Swift => Some(format!("func main() -> Void {{\n  {code}\n}}")),
        parser_registry::ParserId::Go => {
            Some(format!("package main\n\nfunc main() {{\n\t{code}\n}}"))
        }
        parser_registry::ParserId::V => Some(format!("module main\n\nfn main() {{\n\t{code}\n}}")),
        parser_registry::ParserId::Zig => Some(format!("pub fn main() void {{\n    {code};\n}}")),
        parser_registry::ParserId::Dart => Some(format!("void main() {{\n  {code};\n}}")),
        parser_registry::ParserId::Scala => Some(format!("def main(): Unit = {{\n  {code}\n}}")),
        parser_registry::ParserId::Haskell => {
            Some(format!("main = {}", render_haskell_eval_expr(code)))
        }
        parser_registry::ParserId::Nim => Some(format!("proc main() =\n  {code}")),
        parser_registry::ParserId::FSharp => {
            Some(format!("let main _ =\n    let value = {code}\n    value"))
        }
        parser_registry::ParserId::Odin => {
            Some(format!("package main\n\nmain :: proc() {{\n\t{code}\n}}\n"))
        }
        parser_registry::ParserId::D => Some(format!("void main() {{ {code}; }}")),
        parser_registry::ParserId::Crystal => Some(format!("def main\n  {code}\nend")),
        parser_registry::ParserId::Julia => {
            Some(format!("function main()\n    return {code}\nend\n"))
        }
        parser_registry::ParserId::R => Some(format!("main <- function() {{\n    {code}\n}}\n")),
        parser_registry::ParserId::Ruby => Some(format!("def main\n  {code}\nend")),
        parser_registry::ParserId::Lua => Some(format!("function main()\n  {code}\nend")),
        parser_registry::ParserId::Perl => Some(format!("sub main {{\n    {code};\n}}\n")),
        parser_registry::ParserId::Php => {
            let trimmed = code.trim_end().trim_end_matches(';');
            Some(format!("<?php\nfunction main() {{\n    {trimmed};\n}}\n"))
        }
        parser_registry::ParserId::Elixir => Some(format!("def main do\n  {code}\nend\n")),
        parser_registry::ParserId::Erlang => Some(format!(
            "-module(app).\n-export([main/0]).\n\nmain() ->\n    {code}.\n"
        )),
        parser_registry::ParserId::Java => Some(format!(
            "class App {{\n  public static void main(String[] args) {{\n    {code};\n  }}\n}}"
        )),
        parser_registry::ParserId::CSharp => Some(format!(
            "class App {{\n    static void Main() {{\n        {code};\n    }}\n}}"
        )),
        parser_registry::ParserId::Groovy => Some(format!(
            "class App {{\n  static void main(String[] args) {{\n    {code}\n  }}\n}}"
        )),
        parser_registry::ParserId::Kotlin => Some(format!("fun main() {{\n    {code}\n}}")),
        parser_registry::ParserId::Clojure => Some(format!("(defn main [] {code})\n")),
        parser_registry::ParserId::VbNet => Some(format!("Sub main()\n    {code}\nEnd Sub\n")),
        parser_registry::ParserId::OCaml => Some(format!("let main =\n  {code}")),
        parser_registry::ParserId::Hare => {
            Some(format!("export fn main() void = {{\n\t{code};\n}};"))
        }
        parser_registry::ParserId::HolyC => Some(format!("U0 Main()\n{{\n  {code};\n}}\nMain;")),
        parser_registry::ParserId::C | parser_registry::ParserId::ObjC => {
            Some(format!("int main() {{ {code}; return 0; }}"))
        }
        parser_registry::ParserId::Cpp | parser_registry::ParserId::ObjCpp => {
            Some(format!("int main() {{ {code}; return 0; }}"))
        }
        parser_registry::ParserId::Fortran => Some(format!(
            "program main\n  implicit none\n  {code}\nend program main"
        )),
        parser_registry::ParserId::Cobol => Some(format!(
            "IDENTIFICATION DIVISION.\nPROGRAM-ID. MAIN.\nPROCEDURE DIVISION.\n    {code}.\n    STOP RUN."
        )),
        parser_registry::ParserId::Lolcode => Some(format!("HAI 1.2\n{code}\nKTHXBYE")),
        _ => None,
    }
}

pub(crate) fn render_haskell_eval_expr(code: &str) -> String {
    let trimmed = code.trim();
    if let Some(inner) = trimmed
        .strip_prefix("print(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return format!("print {}", inner.trim());
    }
    trimmed.to_string()
}

pub(crate) struct EvalPlan {
    pub(crate) wrapped: String,
    pub(crate) print_result: bool,
}

pub(crate) fn eval_plans(parser_id: parser_registry::ParserId, code: &str) -> Vec<EvalPlan> {
    let normalized = normalize_eval_code(parser_id, code);
    let trimmed = normalized.trim();
    let starts_decl = trimmed.starts_with("fn ")
        || trimmed.starts_with("function ")
        || trimmed.starts_with("def ")
        || trimmed.starts_with("func ")
        || trimmed.starts_with("proc ")
        || trimmed.starts_with("fun ")
        || trimmed.starts_with("pub fn ")
        || trimmed.starts_with("export fn ")
        || trimmed.starts_with("let ")
        || trimmed.starts_with("int main")
        || trimmed.starts_with("void main")
        || trimmed.starts_with("module ")
        || trimmed.starts_with("package ")
        || trimmed.starts_with("class ")
        || trimmed.starts_with("trait ")
        || trimmed.starts_with("sub ")
        || trimmed.starts_with("<?php")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("needs ")
        || trimmed.starts_with("capability ")
        || trimmed.starts_with("enable ")
        || trimmed.starts_with("parallel:")
        || trimmed.starts_with("interrupt fn ")
        || trimmed.starts_with("struct ")
        || trimmed.starts_with("const ")
        || trimmed.starts_with("var ");
    if starts_decl
        || trimmed.contains("\nfn ")
        || trimmed.contains("\nmain:")
        || trimmed.contains("\nneeds ")
        || trimmed.contains("\nparallel:")
    {
        return vec![EvalPlan {
            wrapped: normalized,
            print_result: false,
        }];
    }

    if trimmed.starts_with("print(") {
        return wrap_eval_statement(parser_id, &normalized)
            .into_iter()
            .map(|wrapped| EvalPlan {
                wrapped,
                print_result: false,
            })
            .collect();
    }

    let ret = guess_eval_type(trimmed);
    let mut plans = Vec::new();
    if let Some(wrapped) = wrap_eval_expression(parser_id, &normalized, ret) {
        plans.push(EvalPlan {
            wrapped,
            print_result: true,
        });
    }
    if let Some(wrapped) = wrap_eval_statement(parser_id, &normalized) {
        plans.push(EvalPlan {
            wrapped,
            print_result: false,
        });
    }
    plans
}
