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
                    ▼
         compiler::driver → textual SIL / MIR
                    │
                    ▼
           native_emit (AArch64, x86_64) + jit_runtime
```

| Layer | Location | Notes |
|-------|----------|-------|
| Resolution | `parser_registry` | CLI, `IN_PARSER`, shebang, extension → `ParserId` |
| Parse | `in_lang_parse`, `icore`, `tree_front`, … | → `UnifiedModule` |
| Lower | `lower_core`, `mir_lower` | Bounded bodies → machine code |
| CLI | `in-cli` | `build`, `compile`, `execute`, `graph`, `test` |
| Docs hub | `docs-site/` | `crepus web build` / `web serve` (crepuscularity) |

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