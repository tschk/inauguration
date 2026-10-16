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
