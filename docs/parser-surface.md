# Parser surface (`in build`)

`in-cli/src/parser_registry.rs` resolves which **front** runs before the hybrid SIL pipeline. All 37 tracked languages go through the same unified path: Tree-sitter grammar (or dedicated front) → `UnifiedModule` → `family_typecheck` → Core IR → SIL → codegen.

Resolution order:
1. `--parser in` / `--parser icore` → force that Core IR front  
2. Magic first line `#!in parser=…` on a regular file  
3. `IN_PARSER=in` or `IN_PARSER=icore`  
4. Extension map below (case-insensitive)  
5. Unknown extensions fail closed with an `.icore` hint

## Magic line

- `#!in parser=in` — force `.in` grammar (even if the extension is not `.in`).  
- `#!in parser=auto` — defer to steps 3–5.  
- `#!in parser=<slug>` — force a tracked front when `<slug>` matches a [`ParserId`](../../in-cli/src/parser_registry.rs) (`java`, `cpp`, `objc`, …). Unrecognized values are ignored (fall through).

## Extension → front

| Family | Extensions | `ParserId` | Front |
|--------|------------|------------|-------|
| inauguration | `in` | `in` | dedicated (`in_lang_parse`) |
| Core IR JSON | `icore` | `icore` | dedicated (`compiler::icore`) |
| Swift | `swift` | `swift` | Tree-sitter |
| C / headers | `c`, `h` | `c` | Tree-sitter |
| C++ | `cc`, `cpp`, `cxx`, `hpp`, `hxx`, `hh`, `h++`, `ipp` | `cpp` | Tree-sitter |
| Objective-C | `m` | `objc` | Tree-sitter |
| Objective-C++ | `mm` | `objc++` | Tree-sitter |
| Java | `java` | `java` | Tree-sitter |
| Kotlin | `kt`, `kts` | `kotlin` | Tree-sitter |
| Scala | `scala`, `sc` | `scala` | Tree-sitter |
| C# | `cs` | `csharp` | Tree-sitter |
| F# | `fs`, `fsx`, `fsi` | `fsharp` | Tree-sitter |
| VB.NET | `vb` | `vb` | dedicated boundary |
| Python | `py`, `pyi`, `pyw` | `python` | Tree-sitter |
| Ruby | `rb`, `rake`, `gemspec` | `ruby` | Tree-sitter |
| PHP | `php`, `phtml` | `php` | Tree-sitter |
| Perl | `pl`, `pm` | `perl` | Tree-sitter |
| JavaScript | `js`, `mjs`, `cjs`, `jsx` | `javascript` | Tree-sitter |
| TypeScript | `ts`, `tsx`, `mts`, `cts` | `typescript` | Tree-sitter |
| Go | `go` | `go` | Tree-sitter |
| V | `v` | `v` | dedicated (`v_front`) |
| Rust | `rs` | `rust` | dedicated (`rust_front`) |
| Zig | `zig` | `zig` | Tree-sitter |
| Dart | `dart` | `dart` | Tree-sitter |
| Lua | `lua` | `lua` | Tree-sitter |
| Clojure | `clj`, `cljs`, `cljc` | `clojure` | dedicated boundary |
| Groovy | `groovy` | `groovy` | Tree-sitter |
| Elixir | `ex`, `exs` | `elixir` | Tree-sitter |
| Erlang | `erl`, `hrl` | `erlang` | Tree-sitter |
| Haskell | `hs`, `lhs` | `haskell` | Tree-sitter |
| Julia | `jl` | `julia` | Tree-sitter |
| R | `r` | `r` | Tree-sitter |
| Nim | `nim` | `nim` | dedicated boundary |
| OCaml | `ml`, `mli` | `ocaml` | Tree-sitter |
| Odin | `odin` | `odin` | dedicated boundary |
| Hare | `ha` | `hare` | dedicated boundary |
| HolyC | `hc`, `HC` | `holyc` | [tree-sitter-holyc](https://github.com/undivisible/tree-sitter-holyc) |
| D | `d` | `d` | dedicated boundary |
| Crystal | `cr` | `crystal` | dedicated boundary |

**`.h` headers** map to **`c`**; some Objective-C headers share `.h` — ambiguous paths stay **`c`** Tree-sitter.

The V Tree-sitter frontend is enabled by the `parse-extended` feature, which is
included by `extended`. Python and `.in` use frontends that are available in a
default build.

## Current compatibility ladder

All 37 languages run the same pipeline: grammar → `UnifiedModule` → `family_typecheck` → Core IR → SIL → codegen. Difference is extraction depth:

| Level | Meaning | Languages |
|-------|---------|-----------|
| 5 | Dedicated front or dedicated boundary front with verified extraction | `in`, `icore`, `V`, `JavaScript`, `TypeScript`, `Odin` |
| 4 | Tree-sitter body extraction + family typecheck + SIL artifact emission | All other 31 languages |

Run **`in languages --json`** for the full machine-readable split.

## `.in` binary operator precedence

Lowest precedence first. Operators on one row associate left to right; parentheses override grouping.

| Level | Operators |
| --- | --- |
| 1 | `\|\|` |
| 2 | `&&` |
| 3 | `\|` |
| 4 | `^` |
| 5 | `&` |
| 6 | `==`, `!=` |
| 7 | `<`, `>`, `<=`, `>=` |
| 8 | `<<`, `>>` |
| 9 | `+`, `-` |
| 10 | `*`, `/`, `%` |

For example, `8 == 16 >> 1` means `8 == (16 >> 1)`, and `true == 1 < 2` means `true == (1 < 2)`.
