# Multi-frontend IR (conceptual)

Multiple source languages lower to **Core IR** and share desugaring, verification, optimization, and native JIT/emission. Guarded matches use the same native pipeline as ordinary control flow.

## Conditional and block values

Core IR represents value control flow explicitly. `Conditional { cond, then_expr, else_expr }` evaluates a Bool condition once and executes only the selected branch. `Block { body, result }` has a local scope and an optional trailing value; its `return`, `break`, and `continue` retain their enclosing function or loop targets. Branch values must have compatible types, while a branch that exits does not need to produce a value.

Rust and brace-form `.in` lower these expressions through the shared verifier and owned AArch64/x86_64 backends, including scalar, array, tuple, record, and scalar-vector values. Canonical `.in` output preserves block statements and the distinction between a trailing result and a discarded expression. Go source projection, Thumb, and INISA explicitly refuse structured value forms they cannot preserve.

For `.icore` version 2 or 3, expression objects use `kind: "conditional"` with `cond`, `then`, and `else`, or `kind: "block"` with `body` and optional `result`. For example, this expression produces 42:

```json
{
  "kind": "conditional",
  "cond": true,
  "then": {
    "kind": "block",
    "body": [{"kind": "let", "name": "base", "type": "Int", "value": 20}],
    "result": {"kind": "binary", "op": "+", "lhs": {"kind": "ident", "name": "base"}, "rhs": 22}
  },
  "else": 0
}
```

## Match values

`MatchValue { scrutinee, arms }` evaluates its scrutinee once, tries patterns and guards in source order, and yields the first selected arm's value. Each arm has its own pattern bindings. Bindings are captured before its guard executes; a failed guard continues to the next arm without evaluating the arm value. Native dispatch uses one branch sequence without duplicating later arms.

Value matches require compatible arm types and proven exhaustiveness. An unguarded wildcard, binding, or irrefutable typed aggregate pattern establishes coverage; both unguarded Bool literals also cover Bool values. Guards alone do not prove coverage. Fixed-length array patterns require a fallback because Core IR arrays do not retain a static length. Block arms retain enclosing returns and loop jumps.

Rust expression `match` and value-position `if let`, brace-form `.in` matches, and `.icore` v2/v3 `kind: "match-value"` use this representation. The JSON form contains `scrutinee` and `arms`; each arm has `pattern`, optional `guard`, and `value`. C/Go source projection, Thumb, and INISA refuse match values until those emitters preserve the same semantics.

## Rust inline constants

Rust `const { ... }` lowers to the reserved `#rust-inline-const` call with one scoped `Block` argument. It must be evaluated within the bounded constant evaluator before native lowering; it is neither a closure nor a runtime block. Its environment contains constant globals and its own locals, never captured runtime bindings. Unsupported evaluation remains an explicit refusal.

Inline constants have their own control-flow boundary: `return` is invalid, and `break` or `continue` cannot target an enclosing runtime loop. A loop declared inside the constant block has its own jump targets, though executing such loops still requires constant-evaluator support. General Rust const-function evaluation remains outside this subset.

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

Counts currently support checked integer constant arithmetic and immutable integer globals. Expansion is bounded to 4096 items; backend frame and element-layout limits still apply. Unresolved, negative, and larger counts are refused. Inline constants must first resolve under the bounded constant evaluator described above. Inferred counts, general const-function evaluation, and Rust `Copy` trait checking remain outside this subset.

`.in` collection loops use the same lowering as Rust collection loops, alongside existing bounded and open ranges. `conformance/control-flow/array-repeat-effects.in` covers single evaluation, zero length, short-circuit effects, and conditions reevaluated after `continue`.

## Fields of produced struct values

x86 global scalar struct fields use the same writable data-section slots for reads and writes. Local bindings shadow global field paths. Both native backends copy, return, and pass complete global structs with scalar fields using the existing internal struct ABI. Whole-global assignments stage the complete source before committing fields, including constructors, local or global values, function results, and zero reset. Assignment respects local and parameter shadowing.

Constant folding projects a constructor field only when every initializer is a scalar literal. Calls, reads, and possible failures in unselected fields remain in the IR; unsupported runtime global initializers stay refused.

Field reads from calls and struct constructors materialize their base once through shared operand spills. This evaluates every constructor field, preserves surrounding operand order, and keeps producers inside short-circuit branches. Native backends read the resulting local struct through their existing field paths. x86 hosted 64-bit results with scalar fields use owned heap storage and an internal data-pointer ABI; bindings and assignments copy all fields into their local layout. JIT storage lives in the runtime arena, and native storage uses the existing process-lifetime allocation policy.

## Call argument values and order

x86 stages arguments in source order before loading ABI registers and outgoing stack slots. Known scalar-field struct value parameters capture their fields before later arguments run; receiver parameters retain their existing pointer path. Incoming stack arguments account for Harden's extra saved register. AArch64 evaluates producers of empty struct arguments even when the internal ABI assigns them no words. Nested constructor fields use the existing flattened slots recursively, including calls returning a nested scalar-field value.

## Receiver addresses and argument positions

AArch64 named `self` parameters occupy one pointer word regardless of field count; the callee saves that pointer separately from its local field copies. Register and incoming stack arguments are saved before reading receiver fields, and null/failure sentinels return failure before reads. Supported global receiver layouts use their data-section addresses. x86 empty locals reserve stable address storage separately from value fields, so their receiver addresses do not add an ABI value word or trigger a fake field copy. Scalar receiver field reads and writes, forwarding, and value copies use the saved object pointer; ordinary value parameters remain independent copies. Whole receiver replacements stage the source before committing fields. AArch64 also resolves nested receiver addresses and materializes nested struct field values; known nested aggregate replacements stage every field before stores. Shared lowering materializes call and constructor results once for known value-layout receivers, preserving source argument order and conditional execution. Addressable receiver and field-assignment bases retain their identity when later operands spill. `.in` chained calls keep the terminal call. Scalar vectors support construction, growth, extension, cloning, capacity queries, and clearing; borrowed headers and aggregate elements still require additional ownership and drop semantics.

## Writable array literal results

AArch64 materializes array literal results in writable storage instead of returning pointers into read-only code. Elements can be dynamic and retain their evaluation order; a saved cursor survives element calls. JIT storage belongs to the runtime arena, while native hosted artifacts use the existing process-lifetime heap policy. The internal AArch64 data-pointer/length array ABI remains unchanged. Array-returning functions initialize local literal arrays in owned storage, so returns through aliases and other calls survive frame release. Inline arrays materialized elsewhere copy their current elements into owned storage. Direct indexing of literals evaluates elements before the index and uses the same pointer/length bounds checks as call results. Direct literal and call-result array arguments use this same materializer; argument staging preserves earlier values across later calls.

## Array bounds failures

AArch64 and x86 native array reads and writes return 1 from the current function on a negative or out-of-range index. They preserve effects evaluated before the bounds check and skip later statements. Raw integer-pointer indexing remains unchecked. This is the existing native failure-return convention; callers receive the result and can continue.

## Indexed compound update order

Assignment scans share the character-aware operator scanner with expressions, respecting nested delimiters, escaped strings, and Unicode identifiers. Comparisons inside an assignment RHS retain their expression meaning.

`.in` supports `&=`, `|=`, and `^=` for Int and Bool values through the existing bitwise IR operations. Boolean updates evaluate their RHS eagerly.

`.in` indexed compound assignments share the indexed-store spill machinery. The array/base, index, and old element are saved once before RHS effects, then the update stores through the saved place. This retains `.in` read-before-RHS order while eliminating duplicate index evaluation and unsupported call operands. Rust's value-before-place order remains distinct in the same lowering path.

## Harden dispatcher scopes

Control-flow dispatcher states keep a local declaration with its last read or write, preserving lexical scope without moving initialization effects. A single scan computes the use boundaries, and the synthetic program counter avoids existing names. Hardened programs retain valid bindings across assignments and returns.

## Signed shifts

Canonical Int64 `>>` and `>>=` preserve the sign bit at runtime, matching shared constant folding and C projection. `.in` shift compound assignments use the existing assignment desugaring, retaining the complete multi-character operator. Explicit logical-shift instruction encoders remain available for bit-oriented runtime helpers. Source unsignedness and narrower integer widths still require separate representation work.

## Floating width casts

Rust `as f32` and `as f64` preserve explicit precision conversion for canonical floating values. `f32` conversion rounds to IEEE single precision, then widens into the internal Float64 word; `f64` conversion preserves that word. Both native CPUs and C projection use this representation. Rust `f64` annotations normalize to the canonical floating type, and explicit `f32`/`f64` literal suffixes retain their precision, including decimal literals without a point. Overflowing source literals remain refused; floating-width conversion itself can produce infinity or subnormal values.

Integer-to-floating casts, unsigned source semantics, and full typed `f32` binding/arithmetic semantics remain pending. The shared width operations require canonical Float operands rather than guessing the representation of named types.

## Saturating floating-to-integer casts

Rust numeric casts retain their target until typed normalization can resolve the source, including fields, indexed vector values, and function results. Canonical floating operands cast to signed or unsigned 8-, 16-, 32-, or 64-bit integers use `float-saturate-*` unary IR operations. They truncate toward zero, clamp to the destination range, and map NaN to zero. Unsigned 64-bit results retain their bits in the existing Int64 word. The operand is evaluated once. Constant folding, C projection, and the AArch64/x86_64 native emitters share these semantics; 32-bit x86 emission remains refused.

Raw integer-width operations still require integral operands. C's existing `float-to-int` operation retains its finite, in-range contract. Rust integer-to-floating casts and full unsigned source arithmetic remain pending.

## Rust integer casts

Explicit Rust casts to signed and unsigned 8-, 16-, 32-, and 64-bit integers truncate low bits and then sign- or zero-extend into the canonical Int64 word. Boolean operands become zero or one. `isize`/`usize` use the supported 64-bit target word. Shared bit operations implement these conversions for both native backends and C projection, with one evaluation of the operand. General source-width arithmetic and floating-point cast semantics remain separate coverage gaps.

## C-style numeric casts

C/C++ `(type)value` casts to primitive numeric types lower through the same typed conversions as implicit value contexts. Nested casts, boolean truth conversion, and qualifiers preserve operand evaluation once. Pointer, reference, object, typedef, void, and extended-precision casts remain refused. Numeric casts use canonical Int64/Float64, with the source-width and unsigned limitations described below.

## C-like numeric conversions

Typed C/C++ initializers, assignments, returns, and call arguments convert between canonical signed Int64 and Float64 through shared unary IR operations. Mixed floating arithmetic and comparisons promote integer and boolean operands to Float64. Floating-to-integer conversion truncates toward zero; only finite values whose truncated result fits Int64 have defined cross-backend behavior. Source integer widths, unsigned conversion, and Float32 arithmetic remain outside this canonical subset.

## C-like post-test loops

`do` loops lower through the shared loop representation with a hygienic first-iteration flag. The flag skips the initial condition check; later loop headers evaluate the condition after the preceding body, including after `continue`. The body retains its own scope so local shadowing cannot change which binding the condition reads. Breaks and returns skip the next condition check, and nested-loop continues keep their original target.

## C++ while declaration conditions

A `while` declaration condition lowers to a declaration at the start of every loop iteration, followed by the converted condition and a break on false. Its binding remains scoped to the body, and `continue` reevaluates the initializer. The existing shared loop and boolean lowering handle returns and breaks. Declaration conditions in `for` headers remain unavailable in the installed C++ grammar.

## C-like decimal floating literals

Decimal floating literals, exponent forms, and valid digit separators lower to canonical `Float`. An `f` suffix rounds the literal as an `f32` value before widening into the existing `f64` representation. Shared constant folding handles literal floating comparisons with NaN and signed-zero semantics, including static boolean initializers. Hexadecimal floating literals, long-double suffixes, non-finite overflow literals, and full source-width arithmetic remain unsupported. Ordinary literals borrow their digit text; only separators require a copy.

## AArch64 argument staging

Known native calls save every ABI argument word in the reserved caller frame before evaluating later arguments. Register arguments are reloaded only after evaluation completes, and stack arguments are copied from those saved words into a 16-byte-aligned outgoing area. Scalar arguments and flattened struct words share this staging, including the 32-word call limit. Nested calls reserve disjoint portions of the existing 64-word temporary pool and refuse exhausted capacity rather than reusing live argument slots.

## C-like scalar boolean conversion

C-like boolean declarations retain `Bool` in Core IR. Typed normalization converts numeric conditions and logical operands with `value != 0`, preserving single evaluation and the conditional right operand of `&&` and `||`. The same conversion handles numeric values stored or passed as booleans; boolean arithmetic and integer destinations use explicit `bool-to-int` promotion. Function returns, arguments, locals, globals, and field or index stores use their expected types. Global types participate in normal primitive normalization. C++ `if` initializers and declaration conditions preserve their evaluation and scoped bindings before conversion. C type keywords are matched as keywords, preserving nominal names. Unsupported named boolean conversions and reference values are refused. Other frontends retain strict boolean operands; pointer, object, and general numeric casts remain outside this conversion.

AArch64 assignments reuse scalar parameter slots instead of growing the frame after reservation. Calls align outgoing stack arguments to 16 bytes, including odd numbers of argument words.

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
