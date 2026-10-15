# Thumb loop continue runtime check

Run `python3 scripts/check-thumb-continue.py --compiler /absolute/path/to/in`.
Requires installed Zig and qemu-system-arm; builds no compiler and flashes no hardware.
Reuses the existing thumb-stack-arguments Cortex-M33 AN521 startup, linker script and build file.

Runs original and actual canonicalized source in Default, Lean and Harden. Finite
loops test continue backedges, nearest nested-loop selection, break interaction,
and condition side effects on each retest. SRAM accesses use AN521 RAM0x38000000.
All checks must return42; the first check alone returns126 if continue is ignored.
Compiler snapshot/version/hash, source, reports, link logs and serial evidence are retained.
