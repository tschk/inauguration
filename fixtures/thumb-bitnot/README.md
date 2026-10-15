# Thumb dynamic unary operators

Run `python3 scripts/check-thumb-bitnot.py --compiler /absolute/path/to/in`. Requires installed Zig and qemu-system-arm, and reuses the standalone Cortex-M33 board glue from `fixtures/thumb-stack-arguments`. No Subspace checkout or hardware is used.

Six actual QEMU runs cover original and compiler-canonicalized source under default, lean, and harden profiles. Dynamic memory reads prevent constant folding. The fixture checks 32-bit complement patterns and typed logical NOT of zero, nonzero, and negative values, a side-effect counter proving one operand evaluation, and nested logical NOT, and dynamic skipped branches proving no evaluation there. Every run must return 42 and print the UART PASS marker. Compiler hashes and compile/build/serial evidence remain in the printed output directory.
