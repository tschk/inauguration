# Thumb stack arguments

Run `IN_BIN=/absolute/path/to/in python3 scripts/check-thumb-stack-arguments.py`. Requires installed Zig and qemu-system-arm; no board or Subspace checkout is used. The runner retains compiler/build/UART logs in its printed temporary directory (`--output-dir` selects a new evidence directory).

The Cortex-M33 fixture checks five, six, and seven arguments, left-to-right once effects, repeated and nested calls, 32-bit argument word edges, and actual callee SP alignment through a naked assembly probe. Only return 42 produces the UART PASS marker. This optional hardware-emulation check is separate from the ordinary host suite.
