# Language fronts

Parser fronts route into one **Core IR → MIR → native_emit/JIT** pipeline. Run `in languages` or `in languages --json` for the live matrix. A `jit` capability means the front emits function bodies the in-process JIT can run for the scalar subset (ints, bools, direct calls, `if`/`while`). Declaration-only fronts stay off that list. Host language runtimes (JVM, CPython, libc) are still not bundled.

| Language | Parser | Capabilities | Front | Runtime / Compilation Status |
| :--- | :--- | :--- | :--- | :--- |
| **in** | `in` | `jit, parse, lower, typecheck, boundary` | `in_lang_parse` | self-hosted Core IR to textual SIL and in-memory JIT |
| **icore** | `icore` | `jit, parse, lower, typecheck, boundary` | `compiler::icore` | self-hosted Core IR to textual SIL and in-memory JIT |
| **Swift** | `swift` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR, textual SIL, Boundary IR; Swift runtime is not bundled |
| **Rust** | `rust` | `jit, parse, lower, typecheck, boundary` | `compiler::rust_front` | in-memory JIT executes the scalar subset; rustc is validation only |
| **Go** | `go` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL |
| **V** | `v` | `jit, parse, lower, typecheck, boundary` | `compiler::tree_front` | Core IR and textual SIL |
| **C** | `c` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; libc/runtime ABI is not bundled |
| **C++** | `cpp` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; standard library/runtime ABI is not bundled |
| **Objective-C** | `objc` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; Objective-C runtime is not bundled |
| **Objective-C++** | `objc++` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Objective-C++ runtime/ABI is not bundled |
| **Java** | `java` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; JVM runtime is not bundled |
| **Groovy** | `groovy` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; JVM runtime is not bundled |
| **JavaScript** | `javascript` | `jit, parse, lower, typecheck, boundary` | `compiler::tree_front` | Core IR, Boundary IR, textual SIL, and in-memory JIT; JS runtime is not bundled |
| **TypeScript** | `typescript` | `jit, parse, lower, typecheck, boundary` | `compiler::tree_front` | Core IR, Boundary IR, textual SIL, and in-memory JIT; TS checker/runtime is not bundled |
| **Kotlin** | `kotlin` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; JVM runtime is not bundled |
| **Scala** | `scala` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; JVM runtime is not bundled |
| **C#** | `csharp` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; CLR runtime is not bundled |
| **F#** | `fsharp` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; CLR runtime is not bundled |
| **VB.NET** | `vb` | `parse, lower, typecheck, boundary` | `compiler::vb_boundary` | Core IR and textual SIL; CLR runtime is not bundled |
| **Python** | `python` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Python runtime is not bundled |
| **Ruby** | `ruby` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Ruby runtime is not bundled |
| **PHP** | `php` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; PHP runtime is not bundled |
| **Perl** | `perl` | `jit, parse, lower` | `compiler::tree_front` | Core IR and textual SIL; Perl runtime is not bundled |
| **Zig** | `zig` | `jit, parse, lower, typecheck, boundary` | `compiler::tree_front` | Core IR and textual SIL; Zig runtime/ABI is not bundled |
| **Dart** | `dart` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; Dart runtime is not bundled |
| **Lua** | `lua` | `jit, parse, lower, typecheck` | `compiler::tree_front` | in-memory JIT for free functions and int returns; `function t:m()` methods are skipped |
| **Clojure** | `clojure` | `parse, lower, typecheck, boundary` | `compiler::clojure_boundary` | Core IR and textual SIL; JVM runtime is not bundled |
| **Elixir** | `elixir` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; BEAM runtime is not bundled |
| **Erlang** | `erlang` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; BEAM runtime is not bundled |
| **Haskell** | `haskell` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; Haskell runtime is not bundled |
| **Nim** | `nim` | `parse, lower, typecheck, boundary` | `compiler::nim_boundary` | Core IR and textual SIL; Nim runtime is not bundled |
| **OCaml** | `ocaml` | `jit, parse, lower, typecheck` | `compiler::tree_front` | Core IR and textual SIL; OCaml runtime is not bundled |
| **Julia** | `julia` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; Julia runtime is not bundled |
| **R** | `r` | `parse, lower` | `compiler::tree_front` | Core IR declarations only; R runtime is not bundled |
| **D** | `d` | `parse, lower, typecheck, boundary` | `compiler::d_boundary` | Core IR and textual SIL; D runtime is not bundled |
| **Crystal** | `crystal` | `parse, lower, typecheck, boundary` | `compiler::crystal_boundary` | Core IR and textual SIL; Crystal runtime is not bundled |
| **Odin** | `odin` | `parse, lower, typecheck, boundary` | `compiler::odin_boundary` | Core IR and textual SIL; Odin runtime is not bundled |
| **Hare** | `hare` | `parse, lower, typecheck, boundary` | `compiler::hare_boundary` | Core IR and textual SIL; Hare runtime is not bundled |
| **HolyC** | `holyc` | `jit, parse, lower` | `compiler::tree_front (tree-sitter-holyc)` | Core IR and textual SIL; TempleOS runtime is not bundled |

## Resolution

For details on how language/file extensions map to specific parsers, using the `IN_PARSER` environment variable or shebang resolution directives (e.g. `#!in parser=...`), see [Parser Surface](parser-surface.md).

## Runtime boundary

Owned path is **JIT / native_emit** on AArch64 and x86_64. External toolchains (`rustc`, `swiftc`, …) are optional parity checks in `in test --external-parity`, not the default execution engine.

## What we took from Valen

[Valen](https://github.com/valen-lang/Valen) is a single-language LLVM compiler. Two of its ideas fit this pipeline; the LLVM backend does not.

- **Borrow regions.** Valen tags every value with a region (`mut` or shared) and records `param_index_to_noalias` for LLVM. Core IR now carries the same fact on `UnifiedModule.regions`. The AArch64 and x86_64 prologues compare those registers and trap when two `mut` pointers are the same object. Fronts that have not assigned a region emit no check.
- **Foreign staging.** Valen's `Valen.toml` orchestrator copies foreign sources into a workspace a native backend can see. `inauguration.foreign.toml` does the same for fronts this compiler already lowers: list the sources, `foreign_stage` parses each with its own front, and the merged module is one JIT unit. A later source replaces an earlier function of the same name.

```
[project]
name = "staged"
entry = "answer"

[[sources]]
path = "answer.c"
parser = "c"
```
