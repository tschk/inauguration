# Emit profiles (`default` / `harden` / `lean`)

`in compile` and `in build` accept an emit profile that reshapes Core IR
optimization and (for `harden`) native codegen fingerprints.

```bash
in compile --path examples/compile/antidecomp_sample.in \
  --target native --target-triple x86_64-unknown-none \
  --linkage static-lib --entry main \
  --out /tmp/sample.o --profile harden

in build --path examples/compile/antidecomp_sample.in --out /tmp/sample --lean
# shorthands:
in compile ... --harden
in compile ... --lean
```

## Profiles

| Profile | Goal | IR | Native emit |
|---------|------|----|-------------|
| `default` | Fast owned pipeline (not a textbook SysV compiler) | Two-wave inlining (threshold 6 / depth 16) then fold / DCE | `push rbp; lea rbp, [rsp]` / `lea rsp, [rbp]` frames; **no** 2KiB `rep stosq` wipe |
| `lean` | Shortest internal calls | Aggressive inlining (higher stmt threshold + deeper recursion, two waves) then DCE | Same frames as default; fewer calls after inline |
| `harden` | Anti-decomp / fingerprint avoidance | After normal opts: MBA, opaque predicates, `_pc` dispatch, `_H` names | Runnable Linux ELF: `exit(status)` stub + XOR-scrambled **INISA** payload. Program is not host ISA |

### Default details (fast, non-classic)

Default is the **runtime** profile. It does not try to look like gcc/clang:

- Frame pointer via `lea`, not `mov rbp, rsp` / `mov rsp, rbp`
- No per-call `rep stosq` of the scratch frame (locals are written before use)
- More inlining than a textbook two-stmt helper pass

### Harden details (private ISA)

Harden dual-emit writes a **runnable Linux ELF**: a tiny `exit(status)` stub
plus the XOR-scrambled **INISA** payload in the same `PT_LOAD`. `./foo-harden`
runs. Ghidra sees the stub, not `mix`/`gate` as native functions. The program
body is the private stack ISA (`eval_module` at compile time supplies the
exit code the stub uses).

IR still runs anti-decomp passes (MBA, opaque predicates, `_pc` dispatch, `_H`
names) so a determined reverse engineer who writes an INISA loader still sees
noisy control flow. There is no claim of cryptographic strength: the scramble
is fingerprint noise, not encryption.

### Lean details

Lean raises the inliner threshold (`2` → `12` stmts) and recursion depth
(`10` → `24`), runs two inline waves, then the usual fold/DCE/dead-fn cleanup.
Prefer this when you want smaller call graphs inside a module without harden noise.

## Dual-emit

From one CLI invocation, emit **two** artifacts:

1. **runtime** — `default` or `lean` only (fast; **no** harden transforms — no CFG `_pc` dispatch, no alien MBA/junk harden shapes)
2. **distribution / anti-decomp sample** — always `EmitProfile::Harden`, written to `--harden-out` (or derived from `--out` by inserting `-harden` before the extension)

The hot-path production/runtime emit must **not** enable harden IR passes. Profiles stay orthogonal: `lean ≠ harden`. Dual-emit is rejected when the only requested profile is harden (`--harden` / `--profile harden`).

`--harden-out` together with `--out` implies dual-emit even without `--dual-emit`. `--out` is required when dual-emit / `--harden-out` is set (`in compile` already requires `--out`; `in build` requires it in this mode).

```bash
in compile --path examples/compile/antidecomp_sample.in \
  --target native --target-triple x86_64-unknown-none \
  --linkage static-lib --entry main \
  --out /tmp/sample.o --harden-out /tmp/sample-harden.o
# or:
in compile ... --out /tmp/sample.o --dual-emit
in build --path ... --out /tmp/sample --dual-emit --lean   # runtime=lean, harden beside it
```

Derived harden paths: `foo.o` → `foo-harden.o`, `foo` → `foo-harden`.

## Honest limits

- Harden **is** a private stack ISA (INISA) inside SCI, interpreted by `in` — not a silicon ISA and not cryptographic VM-protect. Lite CFG dispatch on the IR before INISA lower is still shallow (skips try/match/throw).
- No cryptographic string encryption; string obscuring is best-effort.
- Does not defeat a determined reverse engineer with dynamic tracing.
- Debug stripping: Core IR has no `debug_value`; SIL helpers already strip them when SIL is materialised. Harden does not add DWARF.
- Correctness first: transforms must preserve observable semantics for the owned subset.
- `Break` is not used for dispatch exit because native lower currently treats `Break` as a no-op; the dispatcher exits via `_pc` bounds instead.

## Ghidra / objdump smoke

See `scripts/ghidra-antidecomp-smoke.sh`. Prefer **dual-emit** (`--out` +
`--harden-out`, or `--dual-emit`) as the one-shot way to produce both metrics
artifacts. Separate `--profile default` / `--profile harden` compiles still
work. When Ghidra + Java are absent the script still writes objdump/nm metrics
under `docs/benchmarks/`, exiting 0 with a skip note for CI.
