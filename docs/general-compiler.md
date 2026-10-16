# General compiler (inauguration)

`in` is a **hybrid compiler driver**: many **source fronts** → shared **Core IR** → **MIR** → **native_emit / JIT**. Scope is compiler infrastructure, agent/graph reports, and owned backends—not UI (that stays in **crepuscularity**).

Status matrix: **`in languages`** / **`in languages --json`**. Roadmap: [universal-compiler-roadmap.md](universal-compiler-roadmap.md).

## Pipeline

```
  .in / .icore / Tree-sitter / rust_front / v_front
                    │
                    ▼
              UnifiedModule (Core IR)
                    │
                    ├─ hosted scalar JIT: typed MIR → machine MIR → native code
                    │                                               → jit_runtime
                    └─ other owned routes: native_emit → JIT / native artifacts
```

| Layer | Location | Notes |
|-------|----------|-------|
| Resolution | `parser_registry` | CLI, `IN_PARSER`, shebang, extension → `ParserId` |
| Parse | `in_lang_parse`, `icore`, `tree_front`, … | → `UnifiedModule` |
| Lower | `lower_core`, `mir_lower` | Bounded bodies → machine code |
| CLI | `in-cli` | `build`, `compile`, `execute`, `graph`, `test` |
| Docs hub | `docs-site/` | `crepus web build` / `web serve` (crepuscularity) |

Hosted JIT execution first tries verified scalar MIR for defined zero-argument
entries on AArch64 and x86_64. The current subset includes signed 64-bit Int,
Bool and Void, internal register-argument calls, branches, loops and returns.
Unsupported types, globals, runtime intrinsics, checked division/remainder and
other unrepresented operations retain native lowering. Reserved atomic and raw
runtime declarations cannot become ordinary MIR functions. Once machine MIR is
admitted, emission or entry-layout failures are reported rather than retried
through another backend. Existing loading, result decoding and runtime error
reporting are shared; successful MIR execution is identified in the report's
human-readable reason. Compile-only Rust modules and native artifacts retain
their existing routes. Textual analysis IR is separate from this execution path.

The component `Compiler` API emits executables through the same owned native
emitter used by the CLI. It reports parse errors and unsupported artifact kinds
instead of fabricating a successful placeholder. Host macOS AArch64 emission
produces linked machine code; cross-target executables inherit the existing
pure scalar exit-code subset; entries with runtime side effects are rejected. The analysis `IrModule` passes do not replace Core IR
native lowering.

Core IR constant folding covers global initializers as well as function bodies
in every emit profile. Integer expressions such as `-(2 + 3)` and `(1 << 5) | 3`
therefore reach native data emission as initialized scalar values. Calls,
unresolved names, division by zero, and overflowing negation remain expressions;
this pass does not execute runtime initializers.

For owned functions returning `Int`, integer division or remainder by zero
returns `1` immediately from that function. A caller continues with that result.
ARM64, x86 and Thumb preserve this scalar contract. Optimization retains the
evaluation and function boundary of division and remainder, including unused
bindings and arithmetic that otherwise discards their value. Checked array
indexing follows the same current-function failure contract for negative or
out-of-range indices; optimization preserves those checks and their function
boundaries too.

Shared type preparation records concrete inferred fixed-array element types for
native lowering, including Thumb `Int` and `Bool` array literals and
`array-repeat` initializers. Empty arrays still need an explicit element type;
Thumb array aliases and call-bearing store spills remain unsupported.

The bundled Nim grammar admits parenthesized expressions, including return
continuations and grouped call arguments. Groups preserve explicit precedence;
comments inside a group do not become its value. Consumer builds compile the
checked-in parser without a JavaScript runtime. This remains a bounded grammar,
not full Nim syntax: tuples and indirect calls remain unsupported, and malformed
grammar trees still refuse before execution.

Python scalar-name augmented assignments support `+=`, `-=`, `*=`, `&=`,
`|=` and `^=` through the existing typed native operations. They read the old
binding before evaluating the RHS once; supported conditional branches stay lazy.
Computed targets, Python-specific numeric operators and unsupported RHS syntax
refuse explicitly. Integer operands and intermediates must fit the existing
signed 64-bit native carrier: arbitrary-precision integers and overflow checking
are not implemented. Existing print formatting remains unchanged.

Rust assignments produce Unit in statement and value contexts. Tuple
destructuring evaluates the RHS once, stages its leaf values, then stores
assignees from left to right. Scalar compound assignments evaluate the RHS
before reading the destination; computed field and indexed places retain their
location for the final store. Rust integer widths remain available while these
operations expand, including unsigned division and right shifts. Inferred RHS
locals retain their contextual integer kinds; canonical native-call integer
results do not acquire an arbitrary Rust width.

Rust functions retain transient declaration-origin metadata when their syntax
otherwise lacks numeric metadata. This keeps Void helpers accessing global
nominal fields in Rust numeric normalization, including unsigned shifts using a
canonical builtin count. Normalization consumes the origin marker before
executable lowering and leaves unmarked mixed-language functions unchanged.

Array and struct destructuring, and dereference assignees, currently refuse
explicitly rather than silently discarding stores. Native output can still
contain documented degradation trap stubs for unsupported helper functions;
inspect reachable degradations before treating an artifact as executable
coverage. Parentheses and grouping do not consume the Rust expression-depth
budget; expressions exceeding the structural limit refuse instead of becoming
zero.

AArch64 lowering branches `break` and `continue` to the innermost loop's exit
and next iteration, including vector loops. `.in` range-loop expansion advances the
binding before `continue` on both native backends. AArch64 rejects loop jumps
outside a loop or across a `try` scope; error-flag unwind for those jumps remains
unsupported. Thumb `while` loops send `continue` to the innermost condition
and `break` to its exit; a `continue` outside a loop is refused.
Thumb call lowering honors defined functions before MMIO and invoke intrinsics.
Bodyless MMIO declarations retain intrinsic behavior; declared invoke symbols
use ordinary calls. Local callee bindings remain unsupported.
Selected Thumb and x86 MMIO intrinsics require integer addresses and values,
integer load results and void store results. Their bodyless declarations must
match those signatures. AArch64 declared MMIO names remain ordinary external
calls with their declared ABI.
C-family empty `void` definitions retain an explicit return in Core IR, so Thumb
and x86 emit owned functions rather than undefined external symbols.

AArch64 JIT and macOS executable lowering share writable scalar globals and
scalar struct fields across function calls. Both native backends use the same
global data layout; integer, boolean, and float literal initializers retain their
values, including `.in` declarations ending in semicolons. Locals and parameters
shadow globals. AArch64 boot images and standalone library writers still lack
global data relocations; they do not emit unresolved global addresses as code.
Runtime initializers and aggregate initializers are not executed by this layout.

x86_64 lowering uses caller-saved scratch registers for binary operands and
array addresses. Port-output operands are spilled across value evaluation,
so normal generated functions preserve the caller's `RBX` without extra saves.

## inlang + crepuscularity

- **inlang** (`.in`): orchestration, capabilities, polyglot driver, JIT programs.
- **crepuscularity** (`.crepus`): declarative UI, `docs-site/` web target (`crepus.toml`), WASM shell.

## icore (JSON Core IR)

- **v1**: declarations only; empty function bodies.
- **v2**: bounded statement/expression bodies for tools.
- **v3**: boundary ABI (`boundary_ir`) for layout/symbol export.

Samples: `apps/icore-sample/`.

## Orchestration surfaces (v0.4)

| Surface | CLI |
|---------|-----|
| Canonicalize | `in canonicalize` |
| Graph | `in graph` |
| Package report | `in package` |
| Backend facts | `in backend` |
| Static coverage | `in coverage` |
| Self-hosted tests | `in test` |

GPU, remote workers, and non-owned runtimes stay **status-only** until in-tree runtime + tests exist. See [orchestration-compiler.md](orchestration-compiler.md), [native-backend.md](native-backend.md).

## Static coverage and degradation codes

The native lowerer never fabricates a value for code it cannot compile. Anything
outside the native subset becomes a **trap** body that writes the reason to
stderr and exits with `inrt::INRT_TRAP_EXIT_CODE` (70), and is recorded on the
artifact and the compile report:

| Code | Meaning |
|------|---------|
| `IN3001` | The lowerer could not compile this function; its body is a trap |
| `IN3002` | A call names a function with no definition in the compiled unit |

`in coverage --path <file>` reports what compiled and what blocked the rest,
grouped by code, with `--json` for the full per-site list. It distinguishes a
lowerer limitation (`IN3001`) from an unresolved call (`IN3002`), because only the
former is a real coverage gap — a build that resolves packages or cargo crates can
satisfy the latter. Counts are withheld when analysis fails, so a rejected file
never shows a percentage. Granularity is per function, the unit the lowerer
degrades.

Degradations are marked `reachable` when the entry can follow calls out of
compiled code into the trap. Unreachable traps are dead code; reachable ones
abort the program when that path runs, which is why the JIT refuses to execute a
module with a reachable trap instead of guessing a result.

`IN_STRICT_LOWERING=1` turns any degradation into a build failure, for CI.

## Per-language landing

Tree-sitter fronts share scalar body conventions (`return`, `let`, `if`, `while`, calls). Extend each `ParserId` with tests in `in test` / polyglot corpora before raising maturity in `in languages`.

## See also

- [in-language.md](in-language.md)
- [multi-frontend-ir.md](multi-frontend-ir.md)
- [docs-site.md](docs-site.md)
