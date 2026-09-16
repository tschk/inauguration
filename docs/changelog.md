# Changelog

Release notes for **inauguration** — the `in` CLI, the crates.io crate `inauguration`, and GitHub release binaries.

**Install**

```bash
cargo install inauguration
# or, from a checkout:
./install.sh
```

Prebuilt tarballs (Linux x86_64, macOS aarch64) and SHA-256 sidecars: [GitHub Releases](https://github.com/tschk/inauguration/releases).

This page is written for humans and coding agents. Each version lists *why it mattered*, not every commit. Git history remains authoritative for drive-by tests and refactors.

---

## 0.9.8 — 2026-09-16

**Current crates.io and GitHub release.** Tag `v0.9.8`.

This is the first public ship after a long stretch of compiler work on `master` that never made it into 0.9.6 (July). Treat 0.9.8 as the real 0.9-line feature drop.

### Compiler correctness (why v0.9.7 never shipped)

Dead-function elimination (`remove_dead_functions`) kept only `kernel_entry` / `kernel-entry` when `--entry` was omitted. On **AArch64 hosts** (macOS CI, native JIT), a module whose only function was `fn main` was emptied before lowering:

```
jit-lowering-failed: native-lower: module has no functions
```

Linux x86_64 CI does not take that AArch64 JIT path, so the bug hid until the GitHub Release *prerelease gate* on `macos-latest`. **0.9.8 keeps `main` as an implicit DCE root.** Related earlier fix: unused `let` bindings whose initializer *calls* a function are no longer DCE'd (side-effecting calls must run).

### Packages and integrity

- **npm `shasum`:** `ArtifactChecksum::Sha1Hex` hashed the archive with **SHA-256**. Registry `shasum` is SHA-1, so verification could never succeed. Now uses `sha1::Sha1`.
- **Go modules:** zip checksums are verified with the Go `h1:` dirhash (SHA-256 of sorted file hashes), not a whole-archive digest.
- **Invoke allowlist:** package export adapters may spawn `echo`, `node`, `python3`, `cargo`, `go`, `true` — **not `sh`**. The `go:fiber` sample adapter is `go run ./inauguration-invoke` (argv, no shell).
- **Archives:** tar extraction rejects `..`, absolute paths, symlinks, and hardlinks. Zip extraction uses the `zip` crate's path sanitizer (zip-slip).
- **Plugins:** `in plugin run` passes the script path as an argument to `bash --` (no `bash -c` interpolation). Plugin names and workspace targets are path-contained.

### Emit profiles and dual-emit

Three orthogonal profiles:

| Profile | Intent |
| --- | --- |
| `default` | Conventional optimize + emit |
| `harden` | Anti-decomp: opaque predicates, junk pads, CFG dispatch-lite, MBA-ish pads. For *samples*, not the runtime artifact |
| `lean` | Aggressive inlining / shortest internal calls |

`--dual-emit` / `--harden-out` writes a **runtime** artifact (`default` or `lean`) plus a separate **harden** sample. Combining `--harden` with dual-emit is rejected so the runnable binary is never the anti-decomp shape.

### Polyglot fronts

- **42 registered languages** on the language-support matrix (`in languages --json`).
- First-party Tree-sitter grammars for **Crystal**, **Nim**, and **LOLCAT**; COBOL / Fortran / LOLCODE eval wrappers.
- Speculative fronts (C#, Dart, Elixir, …) stay behind `--features extended` / `parse-extended`.
- Tree-sitter extract guards malformed operators instead of panicking.

### JIT / native

- x86_64 JIT resolves stdlib externs and string returns; snake_case stdlib calls (`process_run`, etc.) normalize to `in_*` wrappers.
- `in eval` / `in execute` propagate **nonzero JIT exit status**.
- Cached script executions are **re-run** (the compile cache must not skip side-effecting eval).
- Static libraries keep **all** functions (not only the entry) so unused helpers remain linkable.
- Thumb-2: branch/div/shift/function-address encodings and scratch-slot binary ops.

### Process spawn

`.in` `process_run` is argv-style (`Command::new` + args via `external_guard`). No `/bin/sh -c`. Preview daemon sockets use shorter names.

### Docs site

Moonshine (Bun + Crepus IR) landing page at [inauguration.tsc.hk](https://inauguration.tsc.hk). `docs-gen` turns repo `docs/*.md` into `/docs/*.html`. Cloudflare Pages deploy of `docs-site/dist`.

### CI

Pull-request CI is `permissions: contents: read`. Checkouts use `persist-credentials: false`. Amp orb `.agents/setup` / `.agents/resume` install rustup + fetch `in-cli` lockfile.

### Artifacts

- `in-linux-x86_64.tar.gz` + `.sha256`
- `in-macos-aarch64.tar.gz` + `.sha256`

Rollback: previous published crate is **0.9.6**. Yank 0.9.8 with `cargo yank inauguration --vers 0.9.8` if needed.

---

## 0.9.7 — skipped (2026-09-16)

Tag `v0.9.7` was pushed. The Release workflow **failed** the macOS prerelease `cargo test` gate (see 0.9.8 DCE / `main`). **No GitHub assets, no crates.io 0.9.7.** Do not install this tag.

---

## 0.9.6 — 2026-07-30

Last published crate before 0.9.8.

- ELF `symbol_index` avoids redundant string allocations.
- `docs-gen` parallelizes recursive markdown reads (`rayon`).

Small patch; most compiler work after this landed only in 0.9.8.

---

## 0.9.5 — 2026-07-29

Lockfile / version bump on the 0.9 line. No user-facing compiler change vs 0.9.4.

---

## 0.9.4 — 2026-07-29

- **Release checksums:** GitHub `.sha256` asset names match the tarball names (`in-linux-x86_64.tar.gz.sha256`, …) so `install.sh` can verify.
- **CI:** `cargo fmt --check` and `--locked` on test/clippy.
- **i386:** never save/use `r8`–`r15` in protected mode (those registers do not exist).
- **Security:** `in_process_run` no longer shells; daemon shutdown does not trust a PID file blindly.
- **docs-gen:** cache parsed markdown HTML.

---

## 0.9.3 — 2026-07-23

`tree-sitter-holyc` **0.1.2** and lockfile refresh. HolyC remains an active (always-compiled) front.

---

## 0.9.2 — 2026-07-23

**`.in` nested index expressions.** Bracket-depth tracking in `in_lang_parse` so `xs[i][j]` is one index chain, not a truncated parse.

---

## 0.9.1 — 2026-07-23

Thumb lowerer decomposition and encoding fixes (follow-up to 0.9.0's Thumb feature set). Prefer 0.9.2+ if you emit `thumbv8m`.

---

## 0.9.0 — 2026-07-23

First **0.9** crate. Freestanding **Thumb-2** (`.in` → `thumbv8m.main-none-eabi`) grew from a stub into a real lowerer:

- `extern` calls with `R_ARM_THM_CALL` relocations in static libs
- structs and field assignment
- fixed-size array locals
- more than four AAPCS arguments
- short-circuit `&&` / `||` and `break`
- MMIO load/store builtins
- Thumb branch offsets use **PC+4** for T1 and T2

Also in the 0.8 → 0.9 window (published as 0.9.0):

- **Core IR → C** source sink restored and widened (Vec layout, multi-catch, capturing closures)
- **Core IR → Go / Rust** source sinks
- Compile-cache rebuilds when the report exists but the artifact file is missing

Docs: [`freestanding-thumbv8m.md`](freestanding-thumbv8m.md), [`in-language.md`](in-language.md).

---

## 0.8.0 — 2026-07-18

Self-host / lowering completeness wave. The Rust-on-Core-IR path moved from “parse only” toward **emitting 0 for remaining unsupported patterns** so self-host compile could proceed instead of hard-failing:

- ~100% lowering coverage on several Rust sample projects; library-mode modules (no `main`)
- struct field limit raised (eventually 32 ABI slots); nested field access; `Self` resolution
- `Vec` layout: `with_capacity`, iterators, tuple/ref/wildcard patterns, aggregate elements
- JS/TS/Python extract: `===` / `!==`, `void` / `typeof`, `in`, `??`, `and`, `++`
- C tree-front: relax parse-error abort so recoverable C still lowers

This release is the “make self-host not die on every stdlib shape” cut. 0.9.x then specialized Thumb and emit profiles.

---

## 0.7.13 — 2026-07-15

- Opaque type fallback and `Self` resolution for JIT
- Speculative Tree-sitter parsers gated behind **`parse-extended`** (default `in` binary stays smaller)
- Graceful function skipping for self-host JIT
- Class JIT + `match` wildcard
- `remove_dead_functions` receives the entry name (precursor to the 0.9.8 `main` keep)
- crates.io publish documented as **local**, not CI

---

## 0.7.12 / 0.7.11 — 2026-07-15

Lockfile hygiene. 0.7.11 removed **blake3** from the crate graph.

---

## 0.7.10 — 2026-07-14

**inlang kebab-case identifiers** (`foo-bar` instead of forcing `foo_bar`). Breaking for sources that relied on underscore-only names in the `.in` surface.

---

## 0.7.9 — 2026-07-13

Maintenance cut on the 0.7 native/vector line (see 0.7.8–0.7.4).

---

## 0.7.8 — 2026-07-08

Removed the **v-native** experiment. Testing tweaks. V remains a Tree-sitter front (`--features extended`), not a separate native backend.

---

## 0.7.7 — 2026-07-06

Release-gate fix: **macOS native executables** and the Linux job in the tag workflow. Needed so 0.7 binaries actually attached.

---

## 0.7.6 — 2026-07-06

Lockfile bump only.

---

## 0.7.5 — 2026-07-06

`in --debug` (and related flags) so default JIT runs stay quiet; verbose lowering/`slop` is opt-in.

---

## 0.7.4 — 2026-07-05

Docs-site became a real product:

- `docs-gen` builds the **full sidebar** from all markdown before writing pages (nav no longer shrinks per page)
- `[targets.docs]` hook, `CNAME`, **https://inauguration.tsc.hk**
- GitHub Pages workflow at the time (hosting later moved to **Cloudflare Pages**)

Native/vector work in this era (0.7.4–0.7.10), still relevant:

- Multi-file `.in` + **extern C** linking (`UNDEFINED` ELF symbols)
- Rust `Vec` / iterators / `Result` / format macros / JSON string escaping
- Nested call-arg temps and caller-save vs stack-arg ordering on native

---

## Older than 0.7.4

Tags `v0.7.3` … `v0.7.0` and earlier exist on GitHub for archaeology. The public site and crates.io story starts in practice at **0.7.4** (docs + tsc.hk) and **0.9.0** (Thumb + 0.9 crate line).

Prefer **0.9.8** for current `in`.
