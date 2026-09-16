# Freestanding `thumbv8m.main-none-eabi`

Generic Cortex-M33 freestanding target support in Inauguration.

## Status

| Piece | Status |
| --- | --- |
| Target registry identity | Implemented |
| Native-emit registry | Implemented (`elf32-relocatable-object+raw+uf2`) |
| Core IR → Thumb-2 lower | Implemented (`native_emit::thumb_lower`) |
| Thumb-2 encoder | Implemented (`native_emit::thumb`) |
| ELF32 relocatable object | Implemented (real lowered bodies) |
| Raw binary helper | Existing `native_emit::raw` |
| UF2 packing helper | Implemented (`native_emit::uf2`) |
| Linker layout script generator | Implemented (`native_emit::linker_layout`) |
| UF2 / raw / linker CLI | Implemented (`in pack uf2|raw|linker`) |
| QEMU argv helper | Implemented (`native_emit::qemu_harness`) |
| Vector table / IRQ synthesis | Product-owned (e.g. Subspace) |
| MPU / board capsules | Product-owned |

## Owned scalar subset

- Int/Bool params (AAPCS r0–r3) and locals
- `return`, `let`, assign, expr statements
- `if` / `else`, `while`
- arithmetic `+ - * & | ^`, unary `-` / `!`
- compares `== != < <= > >=`
- direct same-module calls (`bl`)
- `extern <lang> fn` bindings (`extern zig fn ...` etc.) emitting `R_ARM_THM_CALL` relocations for static-linking against C/Rust/Zig/Go/V objects
- MMIO memory ops: `load8` / `load16` / `load32` / `load64`, `store8` / `store16` / `store32` / `store64`
  (plain `ldrb`/`ldrh`/`ldr`/`strb`/`strh`/`str`; treated as volatile at the IR call boundary)

Supported: structs, fixed-size array locals with index/load/store, >4 params (AAPCS stack args), short-circuit `&&`/`||`, `break`.
Rejected for now: strings, floats, closures, heap, array type annotations with explicit length.
`load64` / `store64` touch the low 32-bit word only (freestanding `Int` is machine word).

## ABI contract (MVP)

- freestanding, no libc, no heap
- AAPCS: r0 return, r0–r3 args, r4–r7/lr saved in prologue
- entry symbol name supplied by caller (`--entry`)
- ELF machine `EM_ARM` (40), class ELF32, little-endian
- Thumb-2 instruction stream (T16/T32)

Foreign application bodies (C/Zig) must still pass product-level freestanding
contract checks (stack, alloc=none/pool, blocking bounds). Inauguration only
emits the object/raw/UF2 bytes once those contracts are verified upstream.

## Example

```bash
in compile \
  --path answer.in \
  --target native \
  --target-triple thumbv8m.main-none-eabi \
  --linkage static-lib \
  --entry answer \
  --out answer.o \
  --json
```

## Ownership

Inauguration owns generic emit helpers. Board bases, SCI-E, resource ledgers,
and RTOS runtime stay in product repositories such as Subspace.
