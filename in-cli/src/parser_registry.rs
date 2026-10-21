//! Parser selection for `in build`: CLI, env, magic first line, and file extension.
//!
//! ## Precedence (narrower wins first)
//!
//! 1. **`--parser in`** / **`--parser icore`**: force the `.in` or JSON **icore** front.
//! 2. **`--parser auto`** and the path is an **existing regular file**: read the first line.
//!    If it starts with `#!in parser=` (after trimming):
//!    - `parser=in` → `.in` front (overrides extension and `IN_PARSER`).
//!    - `parser=auto` → continue with normal `auto` rules below.
//!    - `parser=<slug>` for any other known slug → that front (Tree-sitter polyglot or icore-only);
//!      see [`ParserId::as_str`] and [parser-surface.md](../../docs/parser-surface.md).
//! 3. **`IN_PARSER=in`** or **`IN_PARSER=icore`** (case-insensitive): force that Core IR front.
//! 4. **Known extension** (`.in`, `.icore`, `.java`, …): [`ResolvedBuildParser::CoreIr`]
//!    with the matching [`ParserId`] (full: [`ParserId::In`], [`ParserId::Icore`]; others: Tree-sitter).
//! 5. Otherwise: Swift SIL emit path (in-tree subset by default; optional toolchain fallback via env).

use crate::core_ir::UnifiedModule;
use crate::in_lang_parse;
use clap::ValueEnum;
use std::fmt;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// In-tree source front identifier. Full parsers: [`ParserId::In`], [`ParserId::Icore`].
/// Other [`ParserId`] values use [`crate::compiler::tree_front`] (Tree-sitter → signature
/// [`UnifiedModule`]) where a grammar is wired; otherwise an error directs callers to `.icore`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParserId {
    /// `.in` v0 — implemented.
    In,
    /// JSON **icore** interchange — implemented ([`crate::compiler::icore`]).
    Icore,
    // C family
    C,
    Cpp,
    ObjC,
    ObjCpp,
    // JVM / CLR
    Java,
    Kotlin,
    Scala,
    CSharp,
    FSharp,
    VbNet,
    // Dynamic OO / scripting
    Python,
    Ruby,
    Php,
    Perl,
    // Web / JS-shaped
    JavaScript,
    TypeScript,
    // Systems / other curly-brace
    Go,
    V,
    Rust,
    Swift,
    Zig,
    Dart,
    Lua,
    // More OO / FP fronts (stubs)
    Clojure,
    Groovy,
    Elixir,
    Erlang,
    Haskell,
    OCaml,
    Julia,
    R,
    Nim,
    D,
    Crystal,
    Odin,
    Hare,
    HolyC,
    Cobol,
    Fortran,
    Lolcode,
}

impl ParserId {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ParserId::In => "in",
            ParserId::Icore => "icore",
            ParserId::C => "c",
            ParserId::Cpp => "cpp",
            ParserId::ObjC => "objc",
            ParserId::ObjCpp => "objc++",
            ParserId::Java => "java",
            ParserId::Kotlin => "kotlin",
            ParserId::Scala => "scala",
            ParserId::CSharp => "csharp",
            ParserId::FSharp => "fsharp",
            ParserId::VbNet => "vb",
            ParserId::Python => "python",
            ParserId::Ruby => "ruby",
            ParserId::Php => "php",
            ParserId::Perl => "perl",
            ParserId::JavaScript => "javascript",
            ParserId::TypeScript => "typescript",
            ParserId::Go => "go",
            ParserId::V => "v",
            ParserId::Rust => "rust",
            ParserId::Swift => "swift",
            ParserId::Zig => "zig",
            ParserId::Dart => "dart",
            ParserId::Lua => "lua",
            ParserId::Clojure => "clojure",
            ParserId::Groovy => "groovy",
            ParserId::Elixir => "elixir",
            ParserId::Erlang => "erlang",
            ParserId::Haskell => "haskell",
            ParserId::OCaml => "ocaml",
            ParserId::Julia => "julia",
            ParserId::R => "r",
            ParserId::Nim => "nim",
            ParserId::D => "d",
            ParserId::Crystal => "crystal",
            ParserId::Odin => "odin",
            ParserId::Hare => "hare",
            ParserId::HolyC => "holyc",
            ParserId::Cobol => "cobol",
            ParserId::Fortran => "fortran",
            ParserId::Lolcode => "lolcode",
        }
    }

    #[must_use]
    pub const fn family_label(self) -> &'static str {
        match self {
            ParserId::In => "inauguration .in",
            ParserId::Icore => "JSON Core IR (icore)",
            ParserId::C | ParserId::Cpp | ParserId::ObjC | ParserId::ObjCpp => "C-like",
            ParserId::Java | ParserId::Kotlin | ParserId::Scala => "JVM / class-based",
            ParserId::CSharp | ParserId::FSharp | ParserId::VbNet => ".NET",
            ParserId::Python | ParserId::Ruby | ParserId::Php | ParserId::Perl => {
                "dynamic OO / scripting"
            }
            ParserId::JavaScript | ParserId::TypeScript => "ECMAScript-shaped",
            ParserId::Go | ParserId::V | ParserId::Rust | ParserId::Swift | ParserId::Zig => {
                "systems / curly-brace"
            }
            ParserId::Dart | ParserId::Lua => "OO / embeddable",
            ParserId::Clojure
            | ParserId::Elixir
            | ParserId::Erlang
            | ParserId::Haskell
            | ParserId::OCaml => "functional",
            ParserId::Groovy => "JVM scripting",
            ParserId::Julia | ParserId::R | ParserId::Fortran => "numeric / scientific",
            ParserId::Nim | ParserId::D | ParserId::Crystal => "ALGOL-descended",
            ParserId::Odin | ParserId::Hare | ParserId::HolyC => "systems / native",
            ParserId::Cobol => "legacy / business",
            ParserId::Lolcode => "esoteric / dynamic",
        }
    }

    #[must_use]
    pub const fn default_extension(self) -> &'static str {
        match self {
            ParserId::In => "in",
            ParserId::Icore => "icore",
            ParserId::C => "c",
            ParserId::Cpp => "cpp",
            ParserId::ObjC => "m",
            ParserId::ObjCpp => "mm",
            ParserId::Java => "java",
            ParserId::Kotlin => "kt",
            ParserId::Scala => "scala",
            ParserId::CSharp => "cs",
            ParserId::FSharp => "fs",
            ParserId::VbNet => "vb",
            ParserId::Python => "py",
            ParserId::Ruby => "rb",
            ParserId::Php => "php",
            ParserId::Perl => "pl",
            ParserId::JavaScript => "js",
            ParserId::TypeScript => "ts",
            ParserId::Go => "go",
            ParserId::V => "v",
            ParserId::Rust => "rs",
            ParserId::Swift => "swift",
            ParserId::Zig => "zig",
            ParserId::Dart => "dart",
            ParserId::Lua => "lua",
            ParserId::Clojure => "clj",
            ParserId::Groovy => "groovy",
            ParserId::Elixir => "ex",
            ParserId::Erlang => "erl",
            ParserId::Haskell => "hs",
            ParserId::OCaml => "ml",
            ParserId::Julia => "jl",
            ParserId::R => "r",
            ParserId::Nim => "nim",
            ParserId::D => "d",
            ParserId::Crystal => "cr",
            ParserId::Odin => "odin",
            ParserId::Hare => "ha",
            ParserId::HolyC => "hc",
            ParserId::Cobol => "cob",
            ParserId::Fortran => "f90",
            ParserId::Lolcode => "lol",
        }
    }
}

/// Map a **lowercase** extension (no leading dot) to a tracked front.
#[must_use]
pub fn parser_id_from_extension(ext: &str) -> Option<ParserId> {
    match ext {
        "swift" => Some(ParserId::Swift),
        "in" => Some(ParserId::In),
        "icore" => Some(ParserId::Icore),
        "c" | "h" => Some(ParserId::C),
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" | "h++" | "ipp" => Some(ParserId::Cpp),
        "m" => Some(ParserId::ObjC),
        "mm" => Some(ParserId::ObjCpp),
        "java" => Some(ParserId::Java),
        "kt" | "kts" => Some(ParserId::Kotlin),
        "scala" | "sc" => Some(ParserId::Scala),
        "cs" => Some(ParserId::CSharp),
        "fs" | "fsx" | "fsi" => Some(ParserId::FSharp),
        "vb" => Some(ParserId::VbNet),
        "py" | "pyi" | "pyw" => Some(ParserId::Python),
        "rb" | "rake" | "gemspec" => Some(ParserId::Ruby),
        "php" | "phtml" => Some(ParserId::Php),
        "pl" | "pm" => Some(ParserId::Perl),
        "js" | "mjs" | "cjs" | "jsx" => Some(ParserId::JavaScript),
        "ts" | "tsx" | "mts" | "cts" => Some(ParserId::TypeScript),
        "go" => Some(ParserId::Go),
        "v" => Some(ParserId::V),
        "rs" => Some(ParserId::Rust),
        "zig" => Some(ParserId::Zig),
        "dart" => Some(ParserId::Dart),
        "lua" => Some(ParserId::Lua),
        "clj" | "cljs" | "cljc" => Some(ParserId::Clojure),
        "groovy" => Some(ParserId::Groovy),
        "ex" | "exs" => Some(ParserId::Elixir),
        "erl" | "hrl" => Some(ParserId::Erlang),
        "hs" | "lhs" => Some(ParserId::Haskell),
        "ml" | "mli" => Some(ParserId::OCaml),
        "jl" => Some(ParserId::Julia),
        "r" => Some(ParserId::R),
        "nim" => Some(ParserId::Nim),
        "d" => Some(ParserId::D),
        "cr" => Some(ParserId::Crystal),
        "odin" => Some(ParserId::Odin),
        "ha" => Some(ParserId::Hare),
        "hc" => Some(ParserId::HolyC),
        "cob" | "cbl" => Some(ParserId::Cobol),
        "f90" | "f" | "for" | "f95" => Some(ParserId::Fortran),
        "lol" | "lolcode" => Some(ParserId::Lolcode),
        _ => None,
    }
}

/// Map a magic-line token (already trimmed) after `parser=`. Does not handle `in` / `auto`
/// (those are handled before this is called).
#[must_use]
pub fn parser_id_from_magic_token(token: &str) -> Option<ParserId> {
    let t = token.trim();
    parser_id_from_extension(&t.to_ascii_lowercase()).or_else(|| {
        match t.to_ascii_lowercase().as_str() {
            "objc" | "objective-c" => Some(ParserId::ObjC),
            "objc++" | "objcpp" => Some(ParserId::ObjCpp),
            "csharp" => Some(ParserId::CSharp),
            "fsharp" => Some(ParserId::FSharp),
            "kotlin" => Some(ParserId::Kotlin),
            "typescript" => Some(ParserId::TypeScript),
            "javascript" => Some(ParserId::JavaScript),
            "vlang" => Some(ParserId::V),
            "cplusplus" | "c++" => Some(ParserId::Cpp),
            "ocaml" => Some(ParserId::OCaml),
            "icore" => Some(ParserId::Icore),
            "holyc" | "holy-c" => Some(ParserId::HolyC),
            "cobol" => Some(ParserId::Cobol),
            "fortran" => Some(ParserId::Fortran),
            "lolcode" | "lol" => Some(ParserId::Lolcode),
            _ => None,
        }
    })
}

#[must_use]
pub fn parser_id_from_cli_token(token: &str) -> Option<ParserId> {
    let token = token.trim();
    if token == "in" || token == "In" || token == "IN" {
        Some(ParserId::In)
    } else if token == "icore" || token == "Icore" || token == "ICORE" {
        Some(ParserId::Icore)
    } else {
        let lower = token.to_ascii_lowercase();
        parser_id_from_magic_token(token).or(match lower.as_str() {
            "c" => Some(ParserId::C),
            "cpp" | "c++" => Some(ParserId::Cpp),
            "objc" | "objective-c" => Some(ParserId::ObjC),
            "objc++" | "objective-c++" | "objcpp" => Some(ParserId::ObjCpp),
            "java" => Some(ParserId::Java),
            "kotlin" => Some(ParserId::Kotlin),
            "scala" => Some(ParserId::Scala),
            "csharp" | "c#" => Some(ParserId::CSharp),
            "fsharp" | "f#" => Some(ParserId::FSharp),
            "vb" | "vbnet" => Some(ParserId::VbNet),
            "python" => Some(ParserId::Python),
            "ruby" => Some(ParserId::Ruby),
            "php" => Some(ParserId::Php),
            "perl" => Some(ParserId::Perl),
            "javascript" | "js" => Some(ParserId::JavaScript),
            "typescript" | "ts" => Some(ParserId::TypeScript),
            "go" => Some(ParserId::Go),
            "v" | "vlang" => Some(ParserId::V),
            "rust" | "rs" => Some(ParserId::Rust),
            "swift" => Some(ParserId::Swift),
            "zig" => Some(ParserId::Zig),
            "dart" => Some(ParserId::Dart),
            "lua" => Some(ParserId::Lua),
            "clojure" | "clj" => Some(ParserId::Clojure),
            "groovy" => Some(ParserId::Groovy),
            "elixir" => Some(ParserId::Elixir),
            "erlang" => Some(ParserId::Erlang),
            "haskell" => Some(ParserId::Haskell),
            "ocaml" => Some(ParserId::OCaml),
            "julia" => Some(ParserId::Julia),
            "r" => Some(ParserId::R),
            "nim" => Some(ParserId::Nim),
            "d" => Some(ParserId::D),
            "crystal" => Some(ParserId::Crystal),
            "odin" => Some(ParserId::Odin),
            "hare" => Some(ParserId::Hare),
            "holyc" | "holy-c" => Some(ParserId::HolyC),
            "cobol" | "cob" => Some(ParserId::Cobol),
            "fortran" | "f90" => Some(ParserId::Fortran),
            "lolcode" | "lol" => Some(ParserId::Lolcode),
            _ => None,
        })
    }
}

/// CLI `--parser`: `auto` (default) or `in` (see `IN_PARSER=in` env override).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ParserCli {
    #[default]
    Auto,
    In,
    /// JSON [`ParserId::Icore`] interchange (`.icore` files).
    Icore,
}

/// Resolved frontend for `in build`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedBuildParser {
    /// Core IR path (implemented: [`ParserId::In`], [`ParserId::Icore`]; stubs: other [`ParserId`]).
    CoreIr(ParserId),
    /// Deprecated: Swift now routes through Tree-sitter Core IR like all other languages.
    Swift,
}

/// `IN_PARSER=in` or `IN_PARSER=icore` forces the matching Core IR front (any file path).
fn env_core_ir_parser_override(in_parser_env: Option<&str>) -> Option<ParserId> {
    let s = in_parser_env?;
    if s == "in" || s == "In" || s == "IN" {
        Some(ParserId::In)
    } else if s == "icore" || s == "Icore" || s == "ICORE" {
        Some(ParserId::Icore)
    } else {
        None
    }
}

const MAGIC_SHEBANG_PREFIX: &str = "#!in ";
const MAGIC_PARSER_KEY: &str = "parser=";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MagicParserDirective {
    ForceIn,
    /// Hand control back to normal `auto` resolution (extension + `IN_PARSER`).
    DeferAuto,
    UseParser(ParserId),
}

/// Reads the first line of `path` when it is a regular file; returns `None` for
/// directories, symlinks to dirs, missing paths, or I/O errors (fail-open to legacy behavior).
fn read_magic_parser_directive(path: &Path) -> Option<MagicParserDirective> {
    if !path.is_file() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    parse_magic_parser_first_line(&line)
}

fn parse_magic_parser_first_line(line: &str) -> Option<MagicParserDirective> {
    let s = line.trim_end_matches(['\r', '\n']);
    let rest = s.strip_prefix(MAGIC_SHEBANG_PREFIX)?;
    let value = rest.strip_prefix(MAGIC_PARSER_KEY)?;
    if value == "in" {
        return Some(MagicParserDirective::ForceIn);
    }
    if value == "auto" {
        return Some(MagicParserDirective::DeferAuto);
    }
    let id = parser_id_from_magic_token(value)?;
    Some(MagicParserDirective::UseParser(id))
}

pub fn resolve_parser_id(path: &Path, cli: ParserCli) -> ResolvedBuildParser {
    resolve_parser_id_with_env(
        path,
        cli,
        crate::config::env_config().parser_override.as_deref(),
    )
}

pub fn resolve_parser_id_with_env(
    path: &Path,
    cli: ParserCli,
    in_parser_env: Option<&str>,
) -> ResolvedBuildParser {
    match cli {
        ParserCli::In => ResolvedBuildParser::CoreIr(ParserId::In),
        ParserCli::Icore => ResolvedBuildParser::CoreIr(ParserId::Icore),
        ParserCli::Auto => {
            if let Some(m) = read_magic_parser_directive(path) {
                match m {
                    MagicParserDirective::ForceIn => {
                        return ResolvedBuildParser::CoreIr(ParserId::In);
                    }
                    MagicParserDirective::UseParser(ParserId::In) => {
                        return ResolvedBuildParser::CoreIr(ParserId::In);
                    }
                    MagicParserDirective::UseParser(id) => {
                        return ResolvedBuildParser::CoreIr(id);
                    }
                    MagicParserDirective::DeferAuto => {}
                }
            }
            if let Some(id) = env_core_ir_parser_override(in_parser_env) {
                return ResolvedBuildParser::CoreIr(id);
            }
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                let el = ext.to_ascii_lowercase();
                if let Some(id) = parser_id_from_extension(&el) {
                    return ResolvedBuildParser::CoreIr(id);
                }
            }
            ResolvedBuildParser::CoreIr(ParserId::Icore)
        }
    }
}

#[derive(Debug)]
pub enum ParserRegistryError {
    Msg(String),
}

impl fmt::Display for ParserRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParserRegistryError::Msg(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for ParserRegistryError {}

/// Preserve frontend-owned metadata for compilation/optimization consumers.
/// Semantic-only callers retain `parse_with_resolved` and its existing behavior.
pub fn parse_artifact_with_resolved(
    resolved: ResolvedBuildParser,
    path: &Path,
) -> Result<Option<crate::boundary_ir::CompileArtifact>, ParserRegistryError> {
    if matches!(resolved, ResolvedBuildParser::CoreIr(ParserId::Rust)) {
        return crate::compiler::rust_front::parse_rust_artifact(path)
            .map(Some)
            .map_err(ParserRegistryError::Msg);
    }
    parse_with_resolved(resolved, path)
        .map(|module| module.map(crate::boundary_ir::CompileArtifact::from_semantic))
}

/// Dispatch by [`ResolvedBuildParser`].
pub fn parse_with_resolved(
    resolved: ResolvedBuildParser,
    path: &Path,
) -> Result<Option<UnifiedModule>, ParserRegistryError> {
    match resolved {
        ResolvedBuildParser::Swift => Ok(None),
        ResolvedBuildParser::CoreIr(ParserId::In) => in_lang_parse::parse_in_file(path)
            .map_err(ParserRegistryError::Msg)
            .map(Some),
        ResolvedBuildParser::CoreIr(ParserId::Icore) => {
            crate::compiler::icore::parse_icore_file(path)
                .map_err(ParserRegistryError::Msg)
                .map(Some)
        }

        ResolvedBuildParser::CoreIr(ParserId::Rust) => {
            crate::compiler::rust_front::parse_rust_file(path)
                .map_err(ParserRegistryError::Msg)
                .map(Some)
        }

        ResolvedBuildParser::CoreIr(id) => {
            crate::compiler::tree_front::parse_polyglot_file(id, path)
                .map_err(ParserRegistryError::Msg)
                .map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_file_path(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "inauguration-parser-registry-{}-{}-{suffix}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn parse_magic_first_line_recognizes_in_and_auto() {
        assert_eq!(
            parse_magic_parser_first_line("#!in parser=in\n"),
            Some(MagicParserDirective::ForceIn)
        );
        assert_eq!(
            parse_magic_parser_first_line("#!in parser=auto\r\n"),
            Some(MagicParserDirective::DeferAuto)
        );
        assert_eq!(
            parse_magic_parser_first_line("#!in parser=java\n"),
            Some(MagicParserDirective::UseParser(ParserId::Java))
        );
        assert_eq!(
            parse_magic_parser_first_line("#!in parser=icore\n"),
            Some(MagicParserDirective::UseParser(ParserId::Icore))
        );
        assert_eq!(parse_magic_parser_first_line("#!in parser=nope\n"), None);
        assert_eq!(parse_magic_parser_first_line("#!/usr/bin/env in\n"), None);
    }

    #[test]
    fn auto_resolves_in_extension() {
        assert!(matches!(
            resolve_parser_id(Path::new("hello.in"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::In)
        ));
    }

    #[test]
    fn auto_resolves_java_extension_to_core_ir() {
        assert!(matches!(
            resolve_parser_id(Path::new("Foo.java"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::Java)
        ));
    }

    #[test]
    fn auto_resolves_v_extension_to_core_ir() {
        assert!(matches!(
            resolve_parser_id(Path::new("main.v"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::V)
        ));
    }

    #[test]
    fn auto_resolves_go_extension_to_core_ir() {
        assert!(matches!(
            resolve_parser_id(Path::new("main.go"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::Go)
        ));
    }

    #[test]
    fn auto_resolves_icore_extension() {
        assert!(matches!(
            resolve_parser_id(Path::new("module.icore"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::Icore)
        ));
    }

    #[test]
    fn parser_cli_icore_forces_icore() {
        assert!(matches!(
            resolve_parser_id(Path::new("anything.swift"), ParserCli::Icore),
            ResolvedBuildParser::CoreIr(ParserId::Icore)
        ));
    }

    #[test]
    fn auto_swift_for_swift() {
        assert!(matches!(
            resolve_parser_id(Path::new("App.swift"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::Swift)
        ));
    }

    #[test]
    fn auto_cpp_for_cc() {
        assert!(matches!(
            resolve_parser_id(Path::new("lib.cc"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::Cpp)
        ));
    }

    #[test]
    fn auto_resolves_ocaml_odin_and_hare_extensions() {
        assert!(matches!(
            resolve_parser_id(Path::new("main.ml"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::OCaml)
        ));
        assert!(matches!(
            resolve_parser_id(Path::new("main.odin"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::Odin)
        ));
        assert!(matches!(
            resolve_parser_id(Path::new("main.ha"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::Hare)
        ));
        assert!(matches!(
            resolve_parser_id(Path::new("main.hc"), ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::HolyC)
        ));
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn ocaml_front_parses_polyglot_sample_shape() {
        let path = temp_file_path("sample.ml");
        std::fs::write(
            &path,
            "let helper value = value\n\nlet main () = ignore (helper 1)\n",
        )
        .expect("write temp");
        let m = parse_with_resolved(ResolvedBuildParser::CoreIr(ParserId::OCaml), &path)
            .expect("parse")
            .expect("module");
        let _ = std::fs::remove_file(&path);
        assert!(
            m.decls.iter().any(
                |d| matches!(d, crate::core_ir::Decl::Function { name, .. } if name == "helper")
            )
        );
        assert!(
            m.decls.iter().any(
                |d| matches!(d, crate::core_ir::Decl::Function { name, .. } if name == "main")
            )
        );
    }

    #[test]
    fn magic_parser_in_overrides_non_in_extension() {
        let path = temp_file_path("magic.swift");
        std::fs::write(&path, "#!in parser=in\nfn main() -> void\n").expect("write temp");
        assert!(matches!(
            resolve_parser_id(&path, ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::In)
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn magic_parser_auto_defers_to_in_parser_env() {
        let path = temp_file_path("defer.swift");
        std::fs::write(&path, "#!in parser=auto\nfn main() -> void\n").expect("write temp");
        let resolved = resolve_parser_id_with_env(&path, ParserCli::Auto, Some("in"));
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            resolved,
            ResolvedBuildParser::CoreIr(ParserId::In)
        ));
    }

    #[test]
    fn magic_parser_auto_defers_to_dot_in_extension() {
        let path = temp_file_path("defer.in");
        std::fs::write(&path, "#!in parser=auto\nfn main() -> void\n").expect("write temp");
        assert!(matches!(
            resolve_parser_id(&path, ParserCli::Auto),
            ResolvedBuildParser::CoreIr(ParserId::In)
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_magic_parser_value_falls_through_to_extension() {
        let path = temp_file_path("unknown.xyz");
        std::fs::write(&path, "#!in parser=nope\n").expect("write temp");
        assert!(matches!(
            resolve_parser_id_with_env(&path, ParserCli::Auto, None),
            ResolvedBuildParser::CoreIr(ParserId::Icore)
        ));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn polyglot_java_class_without_methods_errors() {
        let path = temp_file_path("empty.java");
        std::fs::write(&path, "class X {}\n").expect("write temp");
        let m = parse_with_resolved(ResolvedBuildParser::CoreIr(ParserId::Java), &path)
            .expect("parse")
            .expect("module");
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, crate::core_ir::Decl::Class { name, .. } if name == "X"))
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn polyglot_java_static_void_main_ok() {
        let path = temp_file_path("entry.java");
        std::fs::write(
            &path,
            "class X { public static void main(String[] a) { } }\n",
        )
        .expect("write temp");
        let m = parse_with_resolved(ResolvedBuildParser::CoreIr(ParserId::Java), &path)
            .expect("parse")
            .expect("module");
        let _ = std::fs::remove_file(&path);
        assert!(
            m.decls
                .iter()
                .any(|d| matches!(d, crate::core_ir::Decl::Class { name, .. } if name == "X")),
            "expected class X, got {:?}",
            m.decls
        );
        let class_x = m
            .decls
            .iter()
            .find_map(|d| match d {
                crate::core_ir::Decl::Class { methods, .. } => Some(methods),
                _ => None,
            })
            .expect("class X methods");
        assert!(
            class_x.iter().any(
                |d| matches!(d, crate::core_ir::Decl::Function { name, .. } if name == "main")
            ),
            "expected main method in class"
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn v_front_parses_main_function() {
        let path = temp_file_path("main.v");
        std::fs::write(
            &path,
            "module main\nfn main() {\n    x := 1\n    return\n}\n",
        )
        .expect("write temp");
        let m = parse_with_resolved(ResolvedBuildParser::CoreIr(ParserId::V), &path)
            .expect("parse")
            .expect("module");
        let _ = std::fs::remove_file(&path);
        assert!(
            m.decls.iter().any(
                |d| matches!(d, crate::core_ir::Decl::Function { name, .. } if name == "main")
            )
        );
    }

    #[cfg(feature = "parse-extended")]
    #[test]
    fn v_front_parses_polyglot_sample_functions() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root")
            .join("apps/polyglot-sample/sample.v");
        let m = parse_with_resolved(ResolvedBuildParser::CoreIr(ParserId::V), &path)
            .expect("parse")
            .expect("module");
        let names: Vec<_> = m
            .decls
            .iter()
            .filter_map(|decl| match decl {
                crate::core_ir::Decl::Function { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["answer", "main"]);
    }

    #[test]
    fn go_front_parses_main_function() {
        let path = temp_file_path("main.go");
        std::fs::write(
            &path,
            "package main\nfunc main() {\n    x := 1\n    return\n}\n",
        )
        .expect("write temp");
        let m = parse_with_resolved(ResolvedBuildParser::CoreIr(ParserId::Go), &path)
            .expect("parse")
            .expect("module");
        let _ = std::fs::remove_file(&path);
        assert!(
            m.decls.iter().any(
                |d| matches!(d, crate::core_ir::Decl::Function { name, .. } if name == "main")
            )
        );
    }

    #[test]
    fn parse_with_resolved_reads_minimal_in_file() {
        let path = temp_file_path("hello.in");
        std::fs::write(&path, "fn main() -> void\n").expect("write temp .in");
        let m = parse_with_resolved(ResolvedBuildParser::CoreIr(ParserId::In), &path)
            .expect("parse")
            .expect("module");
        let _ = std::fs::remove_file(&path);
        assert!(!m.decls.is_empty());
    }
}

#[cfg(test)]
mod artifact_route_tests {
    use super::*;

    #[test]
    fn rust_artifact_and_semantic_file_routes_preserve_external_module_directory() {
        let dir = std::env::temp_dir().join(format!(
            "inauguration-artifact-parent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("root.rs");
        std::fs::write(&root, "mod child;fn main()->i64{child::value()}").unwrap();
        std::fs::write(dir.join("child.rs"), "pub fn value()->i64{42}").unwrap();
        let resolved = ResolvedBuildParser::CoreIr(ParserId::Rust);
        let artifact = parse_artifact_with_resolved(resolved, &root)
            .unwrap()
            .unwrap();
        let semantic = parse_with_resolved(resolved, &root).unwrap().unwrap();
        assert_eq!(artifact.semantic, semantic);
        assert!(artifact.semantic.decls.iter().any(|decl| matches!(decl,
            crate::core_ir::Decl::Function { name, .. } if name == "child::value")));
        let direct = crate::compiler::rust_front::parse_rust_artifact(&root).unwrap();
        assert_eq!(direct.semantic, artifact.semantic);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn other_front_artifacts_default_empty_and_swift_remains_none() {
        let path = std::env::temp_dir().join(format!(
            "inauguration-artifact-empty-{}.in",
            std::process::id()
        ));
        std::fs::write(&path, "fn main()->Int{return 42;}").unwrap();
        let artifact =
            parse_artifact_with_resolved(ResolvedBuildParser::CoreIr(ParserId::In), &path)
                .unwrap()
                .unwrap();
        assert!(artifact.unary_methods.is_empty());
        assert!(
            parse_artifact_with_resolved(ResolvedBuildParser::Swift, &path)
                .unwrap()
                .is_none()
        );
        std::fs::remove_file(path).unwrap();
    }
}
