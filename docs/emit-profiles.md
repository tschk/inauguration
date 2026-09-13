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
| `default` | Conventional owned pipeline | Standard inline / fold / DCE | Classic SysV prologue (`push rbp; mov rbp, rsp`) |
| `lean` | Shortest internal calls | Aggressive inlining (higher stmt threshold + deeper recursion, two waves) then DCE | Same frames; fewer calls after inline |
| `harden` | Casual anti-decomp / fingerprint avoidance | After normal opts: opaque predicates, bogus blocks, lite CFG dispatch (`while _pc`), literal obscuring, junk stmts, `_H<fnv>` symbol hashing | Unusual prologue variants (`push rbx` + frame + rotating junk), MBA constant materialization (`(imm^m)^m` / `±m`), rotating junk pads before calls |

### Harden details (intentional anti-patterns)

Harden does **not** claim to match commercial obfuscators. It deliberately
emits shapes that stock Ghidra / Hex-Rays heuristics are less tuned for:

- Non-classic prologue/epilogue pairing (extra callee-saved `rbx`) with rotating post-frame junk / `sub rsp,0` noise
- Constants not as bare `mov reg, imm64` (rotating MBA identities)
- Mangled internal names (`_H` + 16 hex digits)
- Opaque `if` predicates and never-taken bogus blocks
- Lite CFG dispatch: eligible straight-line bodies become `while _pc < N` state machines (no full industrial flattener)
- Semantics-preserving junk `let`/`assign` plus native junk pads before calls

### Lean details

Lean raises the inliner threshold (`2` → `12` stmts) and recursion depth
(`10` → `24`), runs two inline waves, then the usual fold/DCE/dead-fn cleanup.
Prefer this when you want smaller call graphs inside a module without harden noise.

## Honest limits

- No virtualization / VM-protect. Lite CFG dispatch is intentionally shallow (straight-line bodies only; skips loops/try/match/throw) — not an industrial flattener or VM-protect equivalent.
- No cryptographic string encryption; string obscuring is best-effort.
- Does not defeat a determined reverse engineer with dynamic tracing.
- Debug stripping: Core IR has no `debug_value`; SIL helpers already strip them when SIL is materialised. Harden does not add DWARF.
- Correctness first: transforms must preserve observable semantics for the owned subset.
- `Break` is not used for dispatch exit because native lower currently treats `Break` as a no-op; the dispatcher exits via `_pc` bounds instead.

## Ghidra / objdump smoke

See `scripts/ghidra-antidecomp-smoke.sh`. When Ghidra + Java are absent the
script still builds default vs harden artifacts and writes objdump/nm metrics
under `docs/benchmarks/`, exiting 0 with a skip note for CI.
