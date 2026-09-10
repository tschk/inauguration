use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LanguageSupport {
    pub language: &'static str,
    pub parser_id: Option<&'static str>,
    pub extensions: &'static [&'static str],
    /// Capabilities this frontend supports: parse, lower, typecheck, boundary, jit
    pub capabilities: &'static [&'static str],
    pub front: &'static str,
    pub runtime_boundary: &'static str,
    pub example: &'static str,
    pub next_step: &'static str,
}

impl LanguageSupport {
    pub fn can_parse(&self) -> bool {
        self.capabilities.contains(&"parse")
    }
    pub fn can_lower(&self) -> bool {
        self.capabilities.contains(&"lower")
    }
    pub fn can_typecheck(&self) -> bool {
        self.capabilities.contains(&"typecheck")
    }
    pub fn can_boundary(&self) -> bool {
        self.capabilities.contains(&"boundary")
    }
    pub fn can_jit(&self) -> bool {
        self.capabilities.contains(&"jit")
    }
}

pub const LANGUAGE_SUPPORT: &[LanguageSupport] = &[
    LanguageSupport {
        language: "in",
        parser_id: Some("in"),
        extensions: &["in"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "in_lang_parse",
        runtime_boundary: "self-hosted Core IR to textual SIL and in-memory JIT",
        example: "apps/polyglot-sample/sample.in",
        next_step: "Deepen closure runtime semantics, package binding execution boundaries, and richer class/interface lowering",
    },
    LanguageSupport {
        language: "icore",
        parser_id: Some("icore"),
        extensions: &["icore"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "compiler::icore",
        runtime_boundary: "self-hosted Core IR to textual SIL and in-memory JIT",
        example: "apps/polyglot-sample/sample.icore",
        next_step: "Keep schema stable and add conformance fixtures from external emitters",
    },
    LanguageSupport {
        language: "Swift",
        parser_id: Some("swift"),
        extensions: &["swift"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR, textual SIL, Boundary IR; Swift runtime is not bundled",
        example: "apps/polyglot-sample/sample.swift",
        next_step: "Deepen method extraction, enum lowering, and Swift runtime strategy",
    },
    LanguageSupport {
        language: "Rust",
        parser_id: Some("rust"),
        extensions: &["rs"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "compiler::rust_front",
        runtime_boundary: "Core IR and textual SIL; rustc is validation only",
        example: "apps/polyglot-sample/sample_class.rs",
        next_step: "Deepen trait support, generics, and remove validation from default success paths",
    },
    LanguageSupport {
        language: "Go",
        parser_id: Some("go"),
        extensions: &["go"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL",
        example: "apps/polyglot-sample/sample_class.go",
        next_step: "Deepen declarations, packages, and control flow; add full method body support in class lowering",
    },
    LanguageSupport {
        language: "V",
        parser_id: Some("v"),
        extensions: &["v"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL",
        example: "apps/polyglot-sample/sample.v",
        next_step: "Deepen module syntax, structs, and control flow",
    },
    LanguageSupport {
        language: "C",
        parser_id: Some("c"),
        extensions: &["c", "h"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR, textual SIL, and Boundary IR; libc/runtime ABI is not bundled",
        example: "apps/polyglot-sample/sample.c",
        next_step: "Add pointer types, declarator metadata, and full ABI boundary verification",
    },
    LanguageSupport {
        language: "C++",
        parser_id: Some("cpp"),
        extensions: &["cc", "cpp", "cxx", "hpp", "hxx", "hh", "h++", "ipp"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR, textual SIL, and Boundary IR; standard library/runtime ABI is not bundled",
        example: "apps/polyglot-sample/sample_class.cpp",
        next_step: "Add class_specifier lowering to Decl::Class, methods, templates-as-metadata, and full ABI boundary verification",
    },
    LanguageSupport {
        language: "Objective-C",
        parser_id: Some("objc"),
        extensions: &["m"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Objective-C runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add Objective-C method metadata, bounded bodies, and runtime boundary docs",
    },
    LanguageSupport {
        language: "Objective-C++",
        parser_id: Some("objc++"),
        extensions: &["mm"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; Objective-C++ runtime/ABI is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add method metadata, C++ interop boundaries, and ABI docs",
    },
    LanguageSupport {
        language: "Java",
        parser_id: Some("java"),
        extensions: &["java"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; JVM runtime is not bundled",
        example: "apps/polyglot-sample/Sample.java",
        next_step: "Add constructors, full field type resolution, and JVM runtime strategy",
    },
    LanguageSupport {
        language: "Groovy",
        parser_id: Some("groovy"),
        extensions: &["groovy"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; JVM runtime is not bundled",
        example: "apps/polyglot-sample/Sample.groovy",
        next_step: "Share JVM-family lowering with Java and Kotlin",
    },
    LanguageSupport {
        language: "JavaScript",
        parser_id: Some("javascript"),
        extensions: &["js", "mjs", "cjs", "jsx"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR, Boundary IR, textual SIL, and in-memory JIT; JS runtime is not bundled",
        example: "apps/polyglot-sample/sample.js",
        next_step: "Expand class/closure/arrow semantics beyond the bounded entrypoint subset and define JS runtime strategy",
    },
    LanguageSupport {
        language: "TypeScript",
        parser_id: Some("typescript"),
        extensions: &["ts", "tsx", "mts", "cts"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR, Boundary IR, textual SIL, and in-memory JIT; TS checker/runtime is not bundled",
        example: "apps/polyglot-sample/sample.ts",
        next_step: "Expand typed class/arrow semantics beyond the bounded entrypoint subset and define TS checker/runtime strategy",
    },
    LanguageSupport {
        language: "Kotlin",
        parser_id: Some("kotlin"),
        extensions: &["kt", "kts"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; JVM runtime is not bundled",
        example: "apps/polyglot-sample/Sample.kt",
        next_step: "Share JVM-family class metadata, constructors, and runtime strategy",
    },
    LanguageSupport {
        language: "Scala",
        parser_id: Some("scala"),
        extensions: &["scala", "sc"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; JVM runtime is not bundled",
        example: "apps/polyglot-sample/sample.scala",
        next_step: "Add parameters, return types, bounded bodies, and JVM runtime strategy",
    },
    LanguageSupport {
        language: "C#",
        parser_id: Some("csharp"),
        extensions: &["cs"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; CLR runtime is not bundled",
        example: "apps/polyglot-sample/Program.cs",
        next_step: "Add properties, generics metadata, and CLR runtime strategy",
    },
    LanguageSupport {
        language: "F#",
        parser_id: Some("fsharp"),
        extensions: &["fs", "fsx", "fsi"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; CLR runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add parameters, return types, bounded bodies, and CLR runtime strategy",
    },
    LanguageSupport {
        language: "Python",
        parser_id: Some("python"),
        extensions: &["py", "pyi", "pyw"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; Python runtime is not bundled",
        example: "apps/polyglot-sample/sample.py",
        next_step: "Extract classes, lambdas, and richer bodies from Tree-sitter CST to Core IR",
    },
    LanguageSupport {
        language: "Ruby",
        parser_id: Some("ruby"),
        extensions: &["rb", "rake", "gemspec"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; Ruby runtime is not bundled",
        example: "apps/polyglot-sample/sample.rb",
        next_step: "Add blocks, classes, richer calls, and runtime strategy",
    },
    LanguageSupport {
        language: "PHP",
        parser_id: Some("php"),
        extensions: &["php", "phtml"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; PHP runtime is not bundled",
        example: "apps/polyglot-sample/sample.php",
        next_step: "Add traits, namespaces, and richer class metadata",
    },
    LanguageSupport {
        language: "Perl",
        parser_id: Some("perl"),
        extensions: &["pl", "pm"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; Perl runtime is not bundled",
        example: "apps/polyglot-sample/sample.pl",
        next_step: "Add parameters, bounded bodies, and runtime strategy (tree-sitter grammar mismatch: variable_declaration, bless, ++= need new node types)",
    },
    LanguageSupport {
        language: "Zig",
        parser_id: Some("zig"),
        extensions: &["zig"],
        capabilities: &["parse", "lower", "typecheck", "boundary"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; Zig runtime/ABI is not bundled",
        example: "apps/polyglot-sample/sample.zig",
        next_step: "Add comptime-aware boundaries and ABI metadata",
    },
    LanguageSupport {
        language: "Dart",
        parser_id: Some("dart"),
        extensions: &["dart"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; Dart runtime is not bundled",
        example: "apps/polyglot-sample/sample.dart",
        next_step: "Add class metadata, async forms, and runtime policy",
    },
    LanguageSupport {
        language: "Lua",
        parser_id: Some("lua"),
        extensions: &["lua"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; Lua runtime is not bundled",
        example: "apps/polyglot-sample/sample.lua",
        next_step: "Add metatables, richer calls, and runtime strategy",
    },
    LanguageSupport {
        language: "Elixir",
        parser_id: Some("elixir"),
        extensions: &["ex", "exs"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; BEAM runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add arity metadata, bounded bodies, and BEAM runtime strategy",
    },
    LanguageSupport {
        language: "Erlang",
        parser_id: Some("erlang"),
        extensions: &["erl", "hrl"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; BEAM runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add arity metadata, bounded bodies, and BEAM runtime strategy",
    },
    LanguageSupport {
        language: "Haskell",
        parser_id: Some("haskell"),
        extensions: &["hs", "lhs"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Haskell runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add parameters, bounded bodies, and runtime strategy",
    },
    LanguageSupport {
        language: "OCaml",
        parser_id: Some("ocaml"),
        extensions: &["ml", "mli"],
        capabilities: &["parse", "lower", "typecheck"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR and textual SIL; OCaml runtime is not bundled",
        example: "apps/polyglot-sample/sample.ml",
        next_step: "Deepen let syntax, pattern matching, modules, and OCaml runtime strategy",
    },
    LanguageSupport {
        language: "Julia",
        parser_id: Some("julia"),
        extensions: &["jl"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Julia runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add parameters, bounded bodies, and runtime strategy",
    },
    LanguageSupport {
        language: "R",
        parser_id: Some("r"),
        extensions: &["r"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; R runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add parameters, bounded bodies, and runtime strategy",
    },
    LanguageSupport {
        language: "HolyC",
        parser_id: Some("holyc"),
        extensions: &["hc", "HC"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front (tree-sitter-holyc)",
        runtime_boundary: "Core IR and textual SIL; TempleOS runtime is not bundled",
        example: "apps/polyglot-sample/sample.hc",
        next_step: "Declarators, classes, asm strings, deeper body lowering",
    },
    LanguageSupport {
        language: "COBOL",
        parser_id: Some("cobol"),
        extensions: &["cob", "cbl"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; COBOL runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add program-id extraction, paragraph lowering, and runtime strategy",
    },
    LanguageSupport {
        language: "Fortran",
        parser_id: Some("fortran"),
        extensions: &["f90", "f", "for", "f95"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Fortran runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add subroutines, modules, arrays, and runtime strategy",
    },
    LanguageSupport {
        language: "LOLCODE",
        parser_id: Some("lolcode"),
        extensions: &["lol"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; LOLCODE runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Add HAI/KTHXBYE top-level extraction and visible statement lowering",
    },
    LanguageSupport {
        language: "Crystal",
        parser_id: Some("crystal"),
        extensions: &["cr"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Crystal runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Named statement rules for body-level lowering",
    },
    LanguageSupport {
        language: "Nim",
        parser_id: Some("nim"),
        extensions: &["nim"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Nim runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Named statement rules for body-level lowering",
    },
    LanguageSupport {
        language: "VbNet",
        parser_id: Some("vb"),
        extensions: &["vb"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; VB.NET runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Real tree-sitter grammar; Sub/Function lines only today",
    },
    LanguageSupport {
        language: "Clojure",
        parser_id: Some("clojure"),
        extensions: &["clj"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Clojure runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Real tree-sitter grammar; (defn ...) lines only today",
    },
    LanguageSupport {
        language: "D",
        parser_id: Some("d"),
        extensions: &["d"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; D runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Real tree-sitter grammar; heuristic decl lines only today",
    },
    LanguageSupport {
        language: "Odin",
        parser_id: Some("odin"),
        extensions: &["odin"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Odin runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Real tree-sitter grammar; `:: proc(` lines only today",
    },
    LanguageSupport {
        language: "Hare",
        parser_id: Some("hare"),
        extensions: &["ha"],
        capabilities: &["parse", "lower"],
        front: "compiler::tree_front",
        runtime_boundary: "Core IR declarations only; Hare runtime is not bundled",
        example: "docs/parser-surface.md",
        next_step: "Real tree-sitter grammar; fn lines only today",
    },
];

#[must_use]
pub fn all_language_support() -> &'static [LanguageSupport] {
    LANGUAGE_SUPPORT
}

#[must_use]
pub fn language_support_for_parser(parser_id: &str) -> Option<&'static LanguageSupport> {
    all_language_support()
        .iter()
        .find(|lang| lang.parser_id == Some(parser_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser_registry::ParserId;

    #[test]
    fn matrix_tracks_every_parser_id() {
        for parser_id in [
            ParserId::In,
            ParserId::Icore,
            ParserId::C,
            ParserId::Cpp,
            ParserId::ObjC,
            ParserId::ObjCpp,
            ParserId::Java,
            ParserId::Kotlin,
            ParserId::Scala,
            ParserId::CSharp,
            ParserId::FSharp,
            ParserId::Python,
            ParserId::Ruby,
            ParserId::Php,
            ParserId::Perl,
            ParserId::JavaScript,
            ParserId::TypeScript,
            ParserId::Go,
            ParserId::V,
            ParserId::Rust,
            ParserId::Zig,
            ParserId::Dart,
            ParserId::Lua,
            ParserId::Groovy,
            ParserId::Elixir,
            ParserId::Erlang,
            ParserId::Haskell,
            ParserId::OCaml,
            ParserId::Julia,
            ParserId::R,
            ParserId::HolyC,
            ParserId::Cobol,
            ParserId::Fortran,
            ParserId::Lolcode,
            ParserId::Crystal,
            ParserId::Nim,
            ParserId::VbNet,
            ParserId::Clojure,
            ParserId::D,
            ParserId::Odin,
            ParserId::Hare,
        ] {
            assert!(
                LANGUAGE_SUPPORT
                    .iter()
                    .any(|entry| entry.parser_id == Some(parser_id.as_str())),
                "missing parser id {}",
                parser_id.as_str()
            );
        }
    }

    #[test]
    fn ruby_can_lower_and_typecheck() {
        let entry = language_support_for_parser(ParserId::Ruby.as_str()).expect("ruby");
        assert!(entry.can_parse());
        assert!(entry.can_lower());
        assert!(entry.can_typecheck());
        assert!(entry.runtime_boundary.contains("Core IR"));
    }

    #[test]
    fn dedicated_fronts_have_max_capabilities() {
        for language in ["in", "icore", "C", "C++", "V", "JavaScript", "TypeScript"] {
            let entry = LANGUAGE_SUPPORT
                .iter()
                .find(|entry| entry.language == language)
                .expect(language);
            assert!(entry.can_boundary(), "{} should support boundary", language);
        }
    }

    #[test]
    fn all_languages_can_parse_and_lower() {
        for entry in LANGUAGE_SUPPORT.iter() {
            assert!(entry.can_parse(), "{} should support parse", entry.language);
            assert!(entry.can_lower(), "{} should support lower", entry.language);
        }
    }

    #[test]
    fn javascript_and_typescript_have_full_capabilities() {
        for language in ["JavaScript", "TypeScript"] {
            let entry = LANGUAGE_SUPPORT
                .iter()
                .find(|entry| entry.language == language)
                .expect(language);
            assert!(entry.can_boundary(), "{language} should support boundary");
            assert!(entry.runtime_boundary.contains("Boundary IR"), "{language}");
        }
    }

    #[test]
    fn routed_languages_have_examples() {
        for entry in LANGUAGE_SUPPORT.iter().filter(|entry| {
            entry.parser_id.is_some() && entry.language != "in" && entry.language != "icore"
        }) {
            assert!(
                !entry.example.is_empty(),
                "{} should report an example or documentation surface, got {}",
                entry.language,
                entry.example
            );
        }
    }

    #[test]
    fn test_all_language_support() {
        let all = all_language_support();
        assert!(!all.is_empty());
        assert_eq!(all.len(), 42);
    }

    #[test]
    fn test_language_support_for_parser() {
        let in_support = language_support_for_parser("in").expect("should find in");
        assert_eq!(in_support.language, "in");

        let rust_support = language_support_for_parser("rust").expect("should find rust");
        assert_eq!(rust_support.language, "Rust");

        // Exhaustive test: Verify every registered parser_id can be looked up.
        for lang in all_language_support() {
            if let Some(parser_id) = lang.parser_id {
                let support = language_support_for_parser(parser_id)
                    .unwrap_or_else(|| panic!("should find language for parser_id '{}'", parser_id));
                assert_eq!(support.language, lang.language);
            }
        }

        // Edge Cases: Validating error handling for irregular strings.
        for edge_case in [
            "fake",  // simple non-existent
            "",      // empty string
            "Rust",  // case sensitivity (should be "rust")
            "In",    // case sensitivity (should be "in")
            " ",     // spaces
            "\0",    // null terminator
            "$$$",   // special characters
            "🦀",    // non-ascii / unicode
        ] {
            assert!(
                language_support_for_parser(edge_case).is_none(),
                "Expected None for edge case '{}'",
                edge_case
            );
        }
    }
}
