# Thumb signed division and remainder

Run `python3 scripts/check-thumb-division.py --compiler /absolute/path/to/in`.
Requires installed Zig and qemu-system-arm; it does not build the compiler or
install dependencies. The runner reuses the Cortex-M33 AN521 board glue from
`fixtures/thumb-stack-arguments` and retains a compiler snapshot, version/hash,
compile reports, linked images, serial logs and summary in its evidence directory.
Use `--output-dir` to select a new evidence directory.

Every case runs under Default, Lean and Harden from original and actual
canonicalized source. Operands use runtime memory or nontrivial helpers so the
arithmetic is not merely constant folded. Controls cover signed truncation,
signed remainder, MIN32/-1 wrapping, and seven-argument calls.

A zero divisor returns **1 from the current function**, skipping the remainder
of that function. It is not a global abort: `nested-zero` verifies the caller
continues and adds 41. Entry-function zero cases therefore expect 1 directly.
The two effect cases require operand trace 12 exactly once, and require the
following trace-3 store to be skipped. The seventh-argument zero case exercises
failure while preparing arguments before a call with stack arguments.

The combined optimizer-exits case checks that zero multiplication on either
side and an unused local cannot erase division/remainder's current-function
exit. Each helper receives a runtime divisor and must return 1; its caller
continues through all six checks. The complete suite has 84 QEMU runs.

The runner reuses one board build directory through Zig's dependency graph,
replacing and verifying the current object before each build. Every case retains
its own object, glue, build log, copied executed ELF and object/ELF hashes; QEMU
runs that retained ELF copy.
