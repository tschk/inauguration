# Thumb C/C++ empty definitions

Run `python3 scripts/check-thumb-empty-definitions.py --compiler /absolute/path/to/in`
with the compiler containing the empty-definition fix. The four C/C++ programs
place a prototype before or after an empty Void function definition, call that
function and return42. Definitions must remain definitions rather than becoming
unresolved external symbols.

The optional runner requires installed Zig and qemu-system-arm; no dependency
installation or compiler build. It reuses the existing Thumb stack-argument
board glue and shared build cache. All three profiles run each original C/C++
source (12 QEMU runs), retaining compiler/version/hash, objects,
executed ELF hashes, compile/build logs and serial output. A failed link never
runs in QEMU.

Because optimization can remove the empty call inside main, this runner also
forces a board-startup `bl noop` immediately before `bl main`. Returning42 then
requires the exported empty callee to execute and return safely.
