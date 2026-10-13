# Multi-frontend IR (conceptual)

Multiple source languages lower to **Core IR** and share desugaring, verification, optimization, and native JIT/emission. Guarded matches use the same native pipeline as ordinary control flow.

## Historical textual SIL merge behavior (`hybrid_sil`)

`parse_textual_sil` keeps the legacy top-level **`function_id`** as whichever **`sil @name`** line appeared **last** in the input, and still exposes flattened block / instruction vectors for existing callers. It also records explicit per-function entries in **`SilArtifact::functions`**, each with its own blocks and instructions. **`extract_call_graph`** prefers those per-function records, then falls back to **`instruction_callers`**, then to the legacy **`function_id`** behavior for old artifacts. Emitters still typically place **`sil @main` last** so single-function views line up with `main`. For full detail see [in-language.md](in-language.md#hybrid_sil-and-merged-textual-sil).

## `UnifiedModule` (current schema)

Rust type: `in_cli::core_ir::UnifiedModule`.

| Field | Meaning |
|-------|--------|
| `decls: Vec<Decl>` | Top-level declarations in source order. |

`.in` also has a parser-side surface fact helper for `import`, `capability`, and `extern` declarations. Local relative `.in` imports merge imported declarations into the current `UnifiedModule`; `std.io` / `std.fs` / `std.http` / `std.json` / `std.process` / `std.cli` imports synthesize bounded extern-style declarations; imports and capabilities are exposed through `in agent` JSON, and extern `requires` contracts produce agent diagnostics when required capabilities are missing. Extern bindings lower into empty `Function` declarations today so call graph extraction can observe explicit `.in` calls without expanding the Core IR schema.

**`Decl` variants**

| Variant | Fields | Notes |
|---------|--------|-------|
| `Struct` | `name`, `fields: Vec<(String, Typ)>` | **`.in` v0.2** supports multiline field blocks; Tree-sitter and dedicated fronts fill fields where their current extractor supports them. |
| `Function` | `name`, `params`, `ret`, `body` | **`.in`**, `icoreVersion: 2`, dedicated Rust/Go/V fronts, and selected Tree-sitter fronts fill bounded statement lists; lowering follows `lower_core`. |

**`Typ`**: `Int`, `String`, `Bool`, `Void`, `Named(String)`.

## Compile-time values and stored globals

`Decl::Global.constant_value` distinguishes a value copied at use from runtime storage. Rust `const` items use this flag; `.in` globals keep their existing storage semantics. Shared lowering resolves pure constant expressions, including literal strings, arrays, and struct fields, with lexical shadowing and qualified module names. Unresolved constant uses are refused rather than initialized with fabricated data. Native and C emission omit compile-time declarations from runtime global storage.

Rust indexed assignment uses `index-assign-value-first(base, index, value)` in statement position. Shared operand spilling evaluates the assigned value before the place, evaluates each operand once, and skips place evaluation when the value throws. Primitive indexed compound assignments use `index-modify-value-first(base, index, value, operator)`; the old element is read after the right-hand operand has completed, and the same saved place receives the result. Numeric field compounds use `field-modify-value-first(base, field, value, operator)` and retain the original object storage rather than updating a copied struct. A throwing RHS skips the update.

## Array repetition and collection loops

Rust `[value; count]` lowers to the reserved Core IR operation `array-repeat(value, count)`. The shared pass evaluates an ordinary value expression once, including when the count is zero, and copies that value into an array. It preserves surrounding argument order, short-circuit branches, and repeated while-condition evaluation. Generated bindings avoid source names. Empty arrays retain the element type for checking unreachable loop bodies.

Counts currently support checked integer constant arithmetic and immutable integer globals. Expansion is bounded to 4096 items; backend frame and element-layout limits still apply. Unresolved, negative, and larger counts are refused. Inferred counts, const blocks, general const-function evaluation, and Rust `Copy` trait checking remain outside this subset.

`.in` collection loops use the same lowering as Rust collection loops, alongside existing bounded and open ranges. `conformance/control-flow/array-repeat-effects.in` covers single evaluation, zero length, short-circuit effects, and conditions reevaluated after `continue`.

## C-like local declarations

Every initialized identifier in a local C-like declaration lowers to an ordered `Stmt::Let`, using the declaration's common type. Later initializers can read earlier bindings, and unused bindings retain initializer side effects. The same extraction handles multiple bindings in `for` initializers while preserving their loop scope. Uninitialized, pointer, and aggregate declarators remain refused; they are not silently discarded when another declarator is supported.

## C-like integer literals

C-like integer literals retain hexadecimal digits and use their source radix: hexadecimal, binary, octal, or decimal. Traditional `U`/`L` suffix combinations are validated separately, and digit separators are accepted only between valid digits. The frontend still uses canonical signed 64-bit `Int`; suffixes do not yet preserve source width or unsigned arithmetic. Values outside that representation, unsupported suffixes, and malformed literal forms are refused instead of truncated or reinterpreted.

## Value-first variable updates

The reserved statement operation `modify-value-first(target, value, operator)` lowers through the shared value-spill pass. It evaluates `value` once into a hygienic temporary, then reads and updates the original identifier target. C-like compound assignments use this operation, including loop updates, so a right operand that modifies the target cannot leave a stale value in the calculation. Invalid operation shapes remain explicit refusals.

## C-like global storage

Top-level C and C++ variable definitions use `Decl::Global` and the existing shared native data section. Integer literal definitions, zero initialization, multiple identifier declarators, and static storage preserve reads and writes across functions. `const` definitions retain immutable storage. External declarations do not fabricate module-owned storage; unresolved external variable reads are refused. Pointer, array, and reference declarators are currently refused, and runtime initializers retain the native backend's existing refusal.

## C-like counted loops

C and C++ `for` statements lower to a scoped initializer and a Core IR loop. The condition is reevaluated before each iteration, and the update executes after the body or before a `continue` targeting that loop. Nested-loop continues retain their own target; `break` and `return` skip the update. Omitted conditions form an unbounded loop. Identifier increment, decrement, and compound assignment updates retain their stores. Unsupported initializer, condition, or update expressions produce an explicit unsupported operation instead of dropping the loop.

## Boolean bitwise operations

Core IR `&`, `|`, and `^` preserve the operand type: two `Bool` operands produce `Bool`, and two `Int` operands produce `Int`. Mixed types and floating operands are refused. Boolean bitwise operations evaluate both operands in order; `&&` and `||` retain conditional evaluation. Verification, native type inference, and constant folding share this distinction. C-like frontends insert explicit `bool-to-int` promotions where bitwise operands or integer call parameters require integer values.

## Guarded matches

`Stmt::Match` arms contain a pattern, an optional `guard: Expr`, and a body. The subject is evaluated once. A guard runs only when its pattern matches; a false guard continues to the next arm. The first successful arm runs its body, and later guards and bodies are skipped. Guard conditions must be boolean.

Rust preserves `pattern if condition => value`. `.in` accepts `pattern if condition { statements }`. Version 2/3 `.icore` accepts a `match` statement with `scrutinee` and `arms`; each arm has a string `pattern`, an optional expression `guard`, and a statement-array `body`. Expression operands use the parser's `lhs`/`rhs` fields. See [the JSON fixture](../conformance/control-flow/match-guard.icore).

Shared lowering saves the subject and uses one completion flag with one conditional gate per arm, avoiding duplication of the remaining arms. Generated names avoid user globals, parameters, bindings, and captures. Native and C emitters also prepare raw guarded Core IR through this lowering.

## Boundary IR (`icoreVersion: 3`)

Rust types: `in_cli::boundary_ir::BoundaryModule`, `CompileArtifact`.

| Field | Meaning |
|-------|--------|
| `abi_version` | Must match `IN_ABI_VERSION` in `shared/abi/in_abi.h`. |
| `module` | Stable boundary module id (for example `sample.person`). |
| `layouts` | Struct/enum layouts with C repr, size, align, stride, and per-field transfer mode. |
| `symbols` | Exported ABI symbols with ownership and calling convention metadata. |
| `allocators` | Allocator ids (`host_arena`, module heap) used by buffers and borrow tokens. |
| `layout_hash` | Canonical `blake3-…` hash over layouts + symbols; empty means compute on ingest. |

`compiler::icore::parse_icore_artifact` verifies boundary modules before lowering. Level-5 language fronts may attach the same `BoundaryModule` through dedicated extractors (Nim/Odin comment JSON today; Rust/Zig extractors planned) without expanding Core IR statement forms.

## `ParserId` (extensible)

| Id | Source | Entry |
|----|--------|--------|
| `In` | `.in` files (and `#!in parser=in`) | `in_lang_parse` → `UnifiedModule` → `compiler::driver` / `lower_core`; parser-side surface facts feed agent `effects` / `capabilities` |
| `Icore` | `.icore` files (and `#!in parser=icore`) | `compiler::icore` v1 declarations, v2 bounded body JSON, or **v3 Boundary IR** (`boundary` section + optional semantic decls) → `CompileArtifact` → same lowering |
| `swift`, `go`, `ocaml`, `haskell`, `c`, `cpp`, `java`, `python`, … | Known extensions or `#!in parser=<slug>` | **Tree-sitter polyglot** — [`compiler::tree_front`](../../in-cli/src/compiler/tree_front/mod.rs) grammar-backed AST → `UnifiedModule`. All 37 languages share the same pipeline. |

## Resolution order for `in build`

See **`in-cli/src/parser_registry.rs`** (`resolve_parser_id`) and [parser-surface.md](parser-surface.md). In short:

1. **`--parser in` / `--parser icore`** — force the `.in` or `.icore` front.
2. **Magic first line** — `#!in parser=in` | `auto` | `<slug>`.  
3. **`IN_PARSER=in` / `IN_PARSER=icore`**.
4. **Extension map** — `.in`, `.icore`, `.java`, `.cpp`, … → Core IR path (full parsers for `.in`/`.icore`; Tree-sitter for other wired extensions).  
5. Unknown extensions: fail closed with an `.icore` hint.

All 37 languages now route through Core IR. Swift was unified into the Tree-sitter pipeline (the old `swift_subset`/`native_swift_sil`/`sil_emit` modules have been deleted).

## Related

- [general-compiler.md](general-compiler.md) — multi-language driver, **icore**, roadmap.  
- [parser-surface.md](parser-surface.md) — extension + magic-line routing, Tree-sitter vs full fronts.  
- [in-language.md](in-language.md) — `.in` v0.2 grammar and `hybrid_sil` note.

- [README · Core Commands](../../README.md#core-commands) — CLI flags and sample commands (no duplicate install steps here).
