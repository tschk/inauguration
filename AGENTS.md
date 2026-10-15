# AGENTS

Contributor guide for humans and coding agents working in `inauguration`.

## Mission

Ship an **ultrafast** compiler for **general object-oriented and C-type** languages: one **Core IR → MIR → native_emit/JIT** pipeline, many fronts (Tree-sitter polyglot, **`.in` / `.icore`**, Rust/Go/V lowers). JIT-primary: no LLVM, no SIL, no bytecode VM. Improve these layers together:

1. **`in-cli`**: **`lower_core`**, **`core_ir`**, **`compiler::tree_front`** (Tree-sitter polyglot), **`parser_registry`**, **`in_lang_parse`**, **`native_emit`** (JIT/AArch64/x86_64), **`mir`**/**`mir_lower`**/**`mir_emit`**, **`jit_runtime`**, **`native_link`**, **`inrt`** (builtins), **`in`** CLI, **`protocol-gen`**
2. **`compiler/rust-driver`**: pipeline, orchestration, batch path performance
3. **`crepuscularity`**: separate repo at `../crepuscularity` — GUI framework for `.in` programs, not a compiler frontend

## Working rules

- Work directly on `main`; create another branch only when explicitly requested.
- Prefer small, composable changes with tests in same commit.
- Keep terminal output useful: timings, reason codes, clear failures.
- Do not break `in test`.
- Preserve CLI ergonomics: sensible defaults first, flags second.
- When adding concurrency, keep behavior deterministic and observable.

## Required checks before push

Run all:

```bash
in test
```

Use an `in` binary built from this repo. From a checkout, run **`in update`** (alias **`in self-update`**) to reinstall via `cargo install --path in-cli --locked` (honours **`IN_INSTALL_DIR`** like `./install.sh`). Outside a checkout, `in update` falls back to remote `install.sh` from `https://raw.githubusercontent.com/${IN_REPO:-tschk/inauguration}/v${CARGO_PKG_VERSION}/install.sh` (the running CLI version tag) on Unix hosts. You can still use `./install.sh` or `cargo install --path in-cli --force` manually. A stale globally installed `in` (older than `in-cli` in your tree) can fail mid-suite with `No such file or directory` because `in test` must match the workspace layout.

If touching benchmarks or runtime timing, also run `in bench` (reads checked-in metrics NDJSON).

## Testing tiers

- **L0**: `cargo test` in `in-cli`
- **L1**: `scripts/run-conformance.sh` (skips listed in `conformance/skipped.txt`)
- **L2**: default `in test` integration scripts; `in test --list` prints groups; `--toolchain` / `--owned-native` / `--all` widen scope

## `protocol-gen`

- **`protocol-gen` (Rust, `in-cli`)**: canonical checked-in codegen for `PatchType` / Swift `GeneratedWirePatchType` from `shared/protocol/events.schema.json`. CI runs **`scripts/check-protocol-models.sh`** (Rust generator + `git diff`).

## Code ownership map

- `in-cli/src/in_lang_parse.rs`, `lower_core.rs`, `core_ir.rs`: **`.in`** and unified Core IR module
- `in-cli/src/compiler/*`: multi-front driver, **icore** JSON → Core IR, **tree_front** (Tree-sitter polyglot + dedicated fronts)
- `in-cli/src/parser_registry.rs`: extension and shebang resolution to parser ids
- `in-cli/src/native_emit/lower.rs`: Core IR → AArch64 JIT lowering
- `in-cli/src/native_emit/x86_64_lower.rs`: Core IR → x86_64 JIT lowering
- `in-cli/src/native_emit/native_link.rs`: dlsym-based native symbol resolver for JIT FFI
- `in-cli/src/native_emit/aarch64.rs`, `x86_64.rs`: instruction encoding helpers
- `in-cli/src/mir.rs`, `mir_lower.rs`, `mir_emit.rs`, `mir_emit_x86.rs`: MIR layer (offset-deferred assembly)
- `in-cli/src/jit_runtime.rs`: mmap dispatch, executable page management, error page
- `in-cli/src/inrt.rs`: JIT runtime builtins (str_eq, str_contains, etc.)
- `in-cli/src/owned_compile.rs`: `compile_jit()`, `compile_native()` dispatch, entry resolution
- `compiler/rust-driver/crates/pipeline`: stage model + artifact ingestion
- `in-cli` (remainder): **`main`**, plugins, **`in test`**, hotreload daemon, protocol-gen

## Plugin policy

- Built-in plugins live in `plugins/registry/*.sh`.
- Plugins must be safe on repeated runs (idempotent where practical).
- Plugins should improve one project profile clearly (e.g. aurorality, crepuscularity).

## Performance policy

- Prefer default-on improvements over extra toggles.
- Expose stage timing whenever behavior gets slower.
- Keep benchmark outputs checked in under `docs/benchmarks` when meaningful.

## Release & crates.io

- GitHub release binaries: bump `in-cli/Cargo.toml` version, commit, push `main`, tag `v*`, push tag — **`.github/workflows/release.yml`** builds tarballs and creates the GitHub release.
- crates.io: publish **locally** when ready (`cargo publish --manifest-path in-cli/Cargo.toml --locked`). Do **not** re-add crates publish to CI.
- Site hosting: **Cloudflare** (e.g. Pages) — build `docs-site/dist` with `./scripts/build-docs-site.sh`, deploy yourself; no GitHub Pages workflow.

## Commit style

- Use concise imperative subject.
- Body should describe why change matters for speed, correctness, or UX.


<claude-mem-context>
# Memory Context

# claude-mem status

This project has no memory yet. The current session will seed it; subsequent sessions will receive auto-injected context for relevant past work.

Memory injection starts on your second session in a project.

`/learn-codebase` is available if the user wants to front-load the entire repo into memory in a single pass (~5 minutes on a typical repo, optional). Otherwise memory builds passively as work happens.

Live activity: http://localhost:37701
How it works: `/how-it-works`

This message disappears once the first observation lands.
</claude-mem-context>