# Changelog

Release notes for **inauguration** (`in` CLI, crates.io crate, GitHub binaries).

Install: `cargo install inauguration` or `./install.sh`. GitHub tarballs: [releases](https://github.com/tschk/inauguration/releases).

## 0.9.8 — 2026-09-16

Current crates.io and GitHub release.

- **JIT:** keep `main` as an implicit dead-function-elimination root when no `--entry` is set. Without this, AArch64 hosts dropped `fn main` and failed macOS prerelease tests (`native-lower: module has no functions`).
- **Packages:** npm `shasum` verification now hashes with SHA-1 (was SHA-256, so integrity could never match). Package export invoke no longer allowlists `sh`; the `go:fiber` adapter uses `go run ./inauguration-invoke`.
- **CI:** `contents: read` on pull-request CI; checkout does not persist credentials.
- **Emit:** dual-emit (`--dual-emit` / `--harden-out`) plus `default` / `harden` / `lean` profiles (anti-decomp vs aggressive inlining). Runtime artifacts stay default or lean.
- **GitHub assets:** `in-linux-x86_64.tar.gz` and `in-macos-aarch64.tar.gz` with `.sha256` sidecars.

## 0.9.7 — skipped

Tag `v0.9.7` exists but **was not published**. The GitHub Release workflow failed the macOS prerelease gate (see 0.9.8 JIT DCE fix). crates.io never received 0.9.7.

## 0.9.6 — 2026-07-30

Previous stable crate and GitHub release.

- Patch bump of the 0.9 line with lockfile refresh.
- ELF symbol-index allocation cleanup.

## 0.9.5 — 2026-07-29

- Patch release on the 0.9 line (GitHub + crates.io).

## 0.9.4 — 2026-07-29

- Lockfile update for the 0.9.4 crate.

## 0.9.3 — 2026-07-23

- `tree-sitter-holyc` 0.1.2 and lockfile refresh.

## 0.9.2 / 0.9.1 / 0.9.0 — 2026-07-23

Initial 0.9 crates.io series for the hybrid Core IR → JIT pipeline (no LLVM, no bytecode VM).

## Older tags

`v0.8.0` and `v0.7.x` remain on GitHub for historical checkouts. Prefer 0.9.8 for current `in`.
