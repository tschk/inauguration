# Compiler performance notes

## Unchanged completion proofs

`preserve_nonvoid_completion_after` previously built source and lowered analysis
contexts even when the declarations were identical. An equality fast path avoids
that work without changing the output. Keep the equality assertion in the manual
`unchanged_completion_proof_timing` test when revisiting this path.

On this ARM64 macOS debug build, three measurements of 30 passes over 1,000
unchanged functions had a median of 182,252 microseconds before the fast path and
963 microseconds after it (samples: 182252/219753/180460 and 1179/963/883).
This measures only the unchanged-declaration microcase, not end-to-end compilation.

## Executable MIR verification

`verify_machine` previously called `verify` (which already walked the machine
CFG), then walked the same CFG again to reject staged typed instructions. The
runtime mode only adds that refusal. Use the shared verifier with runtime mode
so every structural and dataflow check still runs once per boundary.

A release-library stage probe over 100 samples per fixture/profile measured
54–139 microseconds for a standard machine verification on five small Harden
fixtures, versus roughly 0.5–14 microseconds under Default. Harden expanded the
fixtures to 296–776 machine instructions. These separate probes establish the
cost of repeated analysis; they do not exclusively attribute pipeline timings
or establish an end-to-end speedup. Keep the malformed-machine and valid-staged
refusal tests when changing these boundaries.

Paired source-CLI measurements after factoring runtime validation reduced Harden
medians 9–17% on these fixtures. Remaining overhead versus legacy native lowering
is recorded in `docs/benchmarks/hosted-scalar-mir-265.md`; do not report this as a
general compiler speedup.
