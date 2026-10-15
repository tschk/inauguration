# Native signed word-width runtime regression

Run explicitly with a source-built compiler:

```
python3 scripts/check-integer-word-width.py --compiler /absolute/path/in --output-dir /tmp/in-word-width
```

Requires installed NASM, ld.lld, Zig, qemu-system-i386 and qemu-system-arm.
No compiler or tool installation is performed. This optional check is outside
ordinary `in test`. It compiles and executes every Default/Lean/Harden object
on both 32-bit emulators, preserving reports, link and serial logs.
Thumb reuses the existing stack-argument startup/linker/build fixture. i386
uses the small ELF32 Multiboot startup here, never a 64-bit trampoline.

`word32.in` covers constant/volatile pairs, wrapping add/subtract/multiply,
signed high-bit comparison, unary negation, nested arithmetic, and dynamic
shift counts 0..31. The runner substitutes valid Cortex-M33 RAM for Thumb's
volatile identity. Host64 controls are generated separately with pure identity,
never execute MCU addresses, and check that every 32-bit-specific equality is false on the 64-bit host.
Dynamic Thumb bit-not is deliberately outside this fixture's supported scope.
