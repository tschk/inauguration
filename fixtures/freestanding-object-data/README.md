# Relocatable freestanding object data

Optional emulator/linker regression, no compiler or tool installation:

```
python3 scripts/check-freestanding-object-data.py --compiler /absolute/path/in --output-dir /tmp/in-object-data
```

Requires installed NASM, ld.lld, llvm-objcopy, llvm-readelf and both x86 QEMU
machines. Each output directory must be new. The selected compiler is copied
and hashed, and each source/report/object/link/serial log is retained.

The linker places compiled text at 4 MiB and writable data at 9 MiB, independently
of the compiler's --base. Every supported case runs with no --base and with
--base 0x20000000, across Default/Lean/Harden. Scalar global 41 increments to 42
and exercises a zero-initialized slot on i386 and x86_64. x86_64 additionally
checks String/String-array payload fixups and code-pool literals, and a native
function address/invoke-native/external assembly call with multiple exports.
The separate exports.in case uses eligible direct external calls on BOTH
architectures; startup has its own relocation to other_export, proving public
symbol availability even if the compiler internally folds that function.
The i386 public subset currently refuses the address/invoke fixture; it is not
silently represented as supported by this regression.

The standalone x86_64 Multiboot startup enters actual 64-bit CPU mode before
calling ELF64 compiler code. llvm-objcopy changes only its final executable
container to ELF32 for QEMU's Multiboot loader. It does not turn 64-bit code
into 32-bit code. The control fixture proves both startup modes independently.
No Space source, boot ABI, or OS services are used.

--expect-failure is for retained old-compiler evidence only. It requires an
actual successful compilation followed by link/runtime failure, so parse errors
cannot count as proof.
