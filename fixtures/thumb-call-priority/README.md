# Thumb declared call priority

Run `python3 scripts/check-thumb-call-priority.py --compiler /absolute/path/to/in`
only with the corrected candidate (0.9.226 or later). Do not run the invoke
fixtures on older Thumb compilers: a declared wrapper could be mistaken for an
indirect-call intrinsic and a numeric argument executed as a code address.

Defined load32/store32 bodies must execute, as must declared legacy invoke,
invoke1/invoke2 and invoke-native wrappers. Nontrivial loops keep the tested
functions from being trivially inlined. Undeclared and bodyless extern MMIO
controls retain real word load/store behavior at initialized AN521 SRAM.

The optional runner requires installed Zig and qemu-system-arm, installs no
dependencies and builds no compiler. It reuses existing Thumb stack-argument
board glue and a shared Zig build directory. Five programs run original and
actual canonical source across all three profiles (30 QEMU executions).
Compiler snapshot/version/hash, per-case objects and executed ELF hashes,
compile/build reports and serial output remain in the evidence directory.

The all-widths bodyless-declaration control checks byte/halfword writes preserve
neighboring bytes, all eight load/store names, and a nested load through an
explicit initialized SRAM pointer. Thumb Int is 32 bits: load64/store64 expose
only the low word, and a neighboring-word sentinel verifies that bounded behavior.
