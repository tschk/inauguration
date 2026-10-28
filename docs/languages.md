# Language fronts

**39** parser fronts route into one **Core IR** pipeline. Run `in languages` or `in languages --json` to inspect this live from the CLI.

| Language | Parser | Capabilities | Front | Runtime / Compilation Status |
| :--- | :--- | :--- | :--- | :--- |
| **in** | `in` | `parse, lower, typecheck, boundary` | `in_lang_parse` | self-hosted Core IR to textual SIL and in-memory JIT |
| **icore** | `icore` | `parse, lower, typecheck, boundary` | `compiler::icore` | self-hosted Core IR to textual SIL and in-memory JIT |
| **Swift** | `swift` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR, textual SIL, Boundary IR; Swift runtime is not bundled |
| **Rust** | `rust` | `parse, lower, typecheck, boundary` | `compiler::rust_front` | Core IR and textual SIL; rustc is validation only |
| **Go** | `go` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL |
| **V** | `v` | `parse, lower, typecheck, boundary` | `compiler::tree_front` | Core IR and textual SIL |
| **C** | `c` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; libc/runtime ABI is not bundled |
| **C++** | `cpp` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; standard library/runtime ABI is not bundled |
| **Objective-C** | `objc` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; Objective-C runtime is not bundled |
| **Objective-C++** | `objc++` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Objective-C++ runtime/ABI is not bundled |
| **Java** | `java` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; JVM runtime is not bundled |
| **Groovy** | `groovy` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; JVM runtime is not bundled |
| **JavaScript** | `javascript` | `parse, lower, typecheck, boundary` | `compiler::tree_front` | Core IR, Boundary IR, textual SIL, and in-memory JIT; JS runtime is not bundled |
| **TypeScript** | `typescript` | `parse, lower, typecheck, boundary` | `compiler::tree_front` | Core IR, Boundary IR, textual SIL, and in-memory JIT; TS checker/runtime is not bundled |
| **Kotlin** | `kotlin` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; JVM runtime is not bundled |
| **Scala** | `scala` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; JVM runtime is not bundled |
| **C#** | `csharp` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; CLR runtime is not bundled |
| **F#** | `fsharp` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; CLR runtime is not bundled |
| **VB.NET** | `vb` | `parse, lower, typecheck, boundary` | `compiler::vb_boundary` | Core IR and textual SIL; CLR runtime is not bundled |
| **Python** | `python` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Python runtime is not bundled |
| **Ruby** | `ruby` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Ruby runtime is not bundled |
| **PHP** | `php` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; PHP runtime is not bundled |
| **Perl** | `perl` | `parse, lower` | `compiler::tree_front` | Core IR and textual SIL; Perl runtime is not bundled |
| **Zig** | `zig` | `parse, lower, typecheck, boundary` | `compiler::tree_front` | Core IR and textual SIL; Zig runtime/ABI is not bundled |
| **Dart** | `dart` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Dart runtime is not bundled |
| **Lua** | `lua` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Lua runtime is not bundled |
| **Clojure** | `clojure` | `parse, lower, typecheck, boundary` | `compiler::clojure_boundary` | Core IR and textual SIL; JVM runtime is not bundled |
| **Elixir** | `elixir` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; BEAM runtime is not bundled |
| **Erlang** | `erlang` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; BEAM runtime is not bundled |
| **Haskell** | `haskell` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; Haskell runtime is not bundled |
| **Nim** | `nim` | `parse, lower, typecheck, boundary` | `compiler::nim_boundary` | Core IR and textual SIL; Nim runtime is not bundled |
| **OCaml** | `ocaml` | `parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; OCaml runtime is not bundled |
| **Julia** | `julia` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; Julia runtime is not bundled |
| **R** | `r` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; R runtime is not bundled |
| **D** | `d` | `parse, lower, typecheck, boundary` | `compiler::d_boundary` | Core IR and textual SIL; D runtime is not bundled |
| **Crystal** | `crystal` | `parse, lower, typecheck, boundary` | `compiler::crystal_boundary` | Core IR and textual SIL; Crystal runtime is not bundled |
| **Odin** | `odin` | `parse, lower, typecheck, boundary` | `compiler::odin_boundary` | Core IR and textual SIL; Odin runtime is not bundled |
| **Hare** | `hare` | `parse, lower, typecheck, boundary` | `compiler::hare_boundary` | Core IR and textual SIL; Hare runtime is not bundled |
| **HolyC** | `holyc` | `parse, lower` | `compiler::tree_front (tree-sitter-holyc)` | Core IR and textual SIL; TempleOS runtime is not bundled |

## Resolution

For details on how language/file extensions map to specific parsers, using the `IN_PARSER` environment variable or shebang resolution directives (e.g. `#!in parser=...`), see [Parser Surface](parser-surface.md).

## Runtime boundary

Owned path is **JIT / native_emit** on AArch64 and x86_64. External toolchains (`rustc`, `swiftc`, …) are optional parity checks in `in test --external-parity`, not the default execution engine.