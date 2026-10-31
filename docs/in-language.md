# inlang (`.in`)

**inlang** is inauguration’s surface for compiler orchestration, agent reports, and bounded programs. You can write it in **human-readable** form (colons + indentation, no braces) or **brace form**; the parser normalizes human syntax to braces before Core IR. Pipeline: **Core IR** → **MIR** → **native JIT** (no LLVM, no bytecode VM).

**Identifiers** use **kebab-case** (`read-file`, `str-eq`). Underscores are not part of names; the lone `_` remains the match wildcard. Binary minus needs spacing or numeric operands (`a - b`, `1-2`); `foo-bar` is one identifier.

**Crepuscularity** is the sibling UI stack (`.crepus`, GPUI, WASM sites). Inauguration owns compilation; crepuscularity owns rendering. The **docs-site** is a crepuscularity web target (`crepus web build` / `web serve`), same as crepuscularity’s own docs-site.

Workflow flags and CI entry points stay in the repo [README](../../README.md#core-commands). This page is grammar, imports, and IR shape.

## Quick start (human-readable)

Indent with spaces. Headers end with **`:`**; the compiler expands this to brace form internally (`normalize_human_in_source` in `in_lang_parse`).

```in
import std.io

needs process.stdout

host-log(text: String) uses process.stdout

Message:
  text: String

main:
  print "hello from inlang"
  host-log "visible to in agent"
```

Same program in **brace form** (what you see after canonicalize / in tools that emit braces):

```in
import std.io;

capability process.stdout;

extern human fn host-log(text: String) -> void requires process.stdout;

struct Message {
  String text;
}

fn main() -> void {
  print("hello from inlang");
  host-log("visible to in agent");
  return;
}
```

```bash
in build --path hello.in
in execute --path hello.in
in graph --path hello.in --json
```

## Values from conditions and blocks

Brace-form expressions can select a value with `if` or evaluate statements before a trailing block value:

```in
fn main() -> Int {
  let base = 20;
  let answer = (if (false) { 0 } else { let value = 22; value });
  return base + answer;
}
```

A semicolon discards the last expression, leaving a unit value. A block-local binding stays inside the block. Returns and loop jumps inside a value block still target the enclosing function or loop; evaluating a block does not create a closure.

## Syntax by example

Params are always **`name: Type`** in signatures. Prefer **human-readable** for docs and scripts; use **brace form** when you want explicit `{` / `}` or C-like bodies.

### Human-readable rules

| You write | Meaning |
| --- | --- |
| `import std.io` or `import "./lib.in"` | Semicolon optional on human lines |
| `needs capability.name` | `capability capability.name;` |
| `enable my-extension` | `enable my-extension;` |
| `Name:` then indented lines | Struct (`field: Type` per line) or function body |
| `fn name(a: Int) -> String:` | Function with indented body |
| `main:` | Entry function (same as `fn main() -> void`) |
| `print "text"` | Call — becomes `print("text")` |
| `host-log "msg"` | Bare words → call with string arg |
| `bump` on its own line | Bare identifier → `bump()` |
| `done` | `return` |
| `sig uses cap.a, cap.b` | `extern human fn sig -> void requires cap.a, cap.b;` |
| `parallel:` | `parallel { … }` block |
| `distributed train:` | `distributed fn train() -> void { … }` |

Full example (human):

```in
package demo.app
module demo.app.main

import std.io
import std.process

needs process.stdout

host_log(msg: String) uses process.stdout

Message:
  text: String

main:
  print "ok"
  host_log "docs hook ran"
  process_run "./scripts/build-docs-site.sh"
  done
```

### Package, module, imports (brace form)

```in
package demo.app;
module demo.app.main;

import "./helpers.in";
import std.io;
import std.process;

fn main() -> void {
  print("ok");
  return;
}
```

Semantic package wiring (needs `inauguration.package` nearby):

```in
use my.lib;
bind my.lib as lib;
```

### Types, constants, variables

```in
const MAX = 128;
var counter: Int = 0;

fn bump() -> Int {
  counter = counter + 1;
  return counter;
}
```

Built-ins: `Int`, `String`, `Bool`, `void` / `Void`, plus struct/class names you declare.

### Struct and class

```in
struct Point {
  Int x;
  Int y;
}

class Greeter {
  name: String;

  fn hello() -> String {
    return "hi " + name;
  }
}

fn main() -> String {
  let p = Point { x: 1, y: 2 };
  let g = Greeter { name: "Ada" };
  return g.hello();
}
```

### Interface and component (contracts)

```in
interface Writer {
  fn write(msg: String) -> void;
}

component AppShell {
  target "aarch64-apple-darwin"
  deterministic true

  export main: BootEntry
  capability serial: DebugConsole(write)
}
```

Interfaces are surface contracts; components declare exports, targets, and capability needs for `in agent` / SCI-style checks. Canonical formatting preserves these declarations and their metadata.

Double-quoted component targets use the same string escapes as ordinary string literals (`\\`, `\"`, `\n`, `\r`, `\t`); write `\\` for a literal backslash. Bare targets remain supported. A trailing semicolon is optional.

### Functions, control flow, calls

```in
fn fib(n: Int) -> Int {
  if n <= 1 {
    return n;
  } else {
    return fib(n - 1) + fib(n - 2);
  }
}

fn sum_to(limit: Int) -> Int {
  let acc: Int = 0;
  let i: Int = 0;
  while i < limit {
    acc = acc + i;
    i = i + 1;
  }
  return acc;
}

fn main() -> Int {
  return fib(10) + sum_to(5);
}
```

`for i in 0..8 { … }` is recognized where the front supports bounded ranges.

### Raw native function pointers

`invoke-native(pointer)`, `invoke-native1(pointer, value)`, and `invoke-native2(pointer, first, second)` call a nonthrowing native function with zero, one, or two `Int` parameters and an `Int` result. Underscore spellings are aliases. Target and arguments are evaluated once, left to right.

This is an unsafe machine-ABI boundary: the caller must supply a valid address with the correct target ABI and callee-saved register behavior. It must not point to a function that propagates language exceptions. Known incompatible or throwing targets are refused; a null dynamic target traps. Arbitrary integer addresses cannot prove this contract statically. Integer carriers use the existing backend width: 64 bits in x86_64, AArch64 and C emission, and 32 bits in Thumb.

Ordinary `invoke` / `invoke1` / `invoke2` retain language exception propagation and require its writable runtime context. Use the explicit native boundary for CPU capsules and foreign native functions; do not disable exception checks on ordinary calls.

### Freestanding boot data

For x86_64, i386 and i686 `--emit boot`, the image loads contiguously at `0x100000`: a 4096-byte trampoline, a 256-byte boot header, code and exports, then padding and initialized writable data at `0x200000`. The packager refuses code/export overlap with data and physical addresses beyond 4 GiB before replacing the output. Global references and data pointers use that fixed data address; boot code must retain the loaded initializers rather than zeroing them again.

The boot-only header records the data address as little-endian `u64` at offset 64 and byte length at offset 72. Both fields are zero when there is no data. Existing header fields are unchanged. These fields extend the 256-byte boot wrapper, not the separate 64-byte dynamic component format. A kernel must reserve the entire loaded data range before placing its heap. AArch64 boot emission still refuses writable data relocations.

On i386 and i686, global code references use four-byte absolute addresses.
Static data retains eight-byte cells, with pointer addresses in the low word
and zero in the high word. Relocation rejects out-of-range addresses rather than
truncating them. `scripts/check-i386-boot-data.py --compiler <source-built-in>`
boots scalar mutation and pointer-relocation cases in QEMU for both targets
across default, lean and harden profiles.

### Extern and capabilities

```in
capability memory;

extern rust fn alloc(size: Int) -> Int;
extern c fn free(ptr: Int) -> void requires memory;

fn main() -> void {
  return;
}
```

Missing `capability` lines for `requires` show up as `in agent` warnings (`AGENT-MISSING-CAPABILITY`).

### Orchestration metadata

```in
enable distributed-workers;

@pure
fn hash(data: String) -> Int {
  return 0;
}

distributed fn train(data: String) -> Int {
  return 0;
}

parallel {
  step(0);
  step(1);
}

fn step(id: Int) -> void {
  return;
}
```

`distributed`, `parallel`, and `enable` are **facts** for graphs and agents; remote/threaded execution is not guaranteed yet.

### Stdlib-style I/O (JIT builtins)

```in
import std.io;
import std.fs;
import std.process;

fn main() -> void {
  print("hello");
  write_file("/tmp/out.txt", "data");
  process_run("./scripts/build-docs-site.sh");
  return;
}
```

### Kernel / bare-metal subset (when lowering allows)

```in
const PORT: Int = 0x3F8;

fn uart_put(c: Int) -> void {
  outb(PORT, c);
  return;
}

fn read_word(addr: Int) -> Int {
  return load64(addr);
}

fn halt_loop() -> void {
  while true {
    hlt();
  }
}
```

`load*` / `store*`, `inb` / `outb`, `hlt`, `sti`, `cli`, and friends target freestanding emit paths; availability depends on target and type support in `native-emit`.

### Expressions and operators

Integer literals support decimal, hexadecimal (`0x`), octal (`0o`) and binary (`0b`) notation; uppercase prefixes are accepted. The signed 64-bit minimum is accepted with unary minus in every base, including `-0x8000000000000000`. Invalid digits and values outside the signed 64-bit range are refused.

Float literals support decimal and scientific notation, including `1e3`, `1e+3`, and `1.25E-2`. Exponent signs remain part of the literal inside arithmetic expressions; names such as `e` and hexadecimal integers remain ordinary operands.

```in
fn ops(a: Int, b: Int) -> Bool {
  let sum = a + b;
  let masked = (a << 4) | (b & 0xFF);
  return sum == 42 && masked != 0;
}
```

`&&` and `||` evaluate left to right and short-circuit: `&&` evaluates its
right operand only when the left is true; `||` evaluates it only when the left
is false. Skipped operands perform no calls, side effects, or runtime checks.
Their result is a Boolean (the native/Core IR representation is normalized to
0 or 1). Bitwise `&` and `|` evaluate both operands.

For integers, `!value` is true when the value is zero, and `!!value` is true
when it is nonzero. Both return `Bool` and evaluate the operand once. Boolean
and integer literal negations fold in global initializers, so
`var enabled: Bool = !!42;` is valid.

Logical negation of a Float compares its numeric value with zero. Both signed
zeros yield true; nonzero values and NaN yield false. Literal Float negations
also fold in Boolean global initializers.

Arithmetic, bitwise, and comparisons chain as in the table in [parser-surface.md](parser-surface.md); method calls desugar to mangled functions on classes.

## Ideology

- **Ultraminimal surface**: declarations first; bounded bodies in Core IR today; richer control flow grows in `in_lang_parse.rs` / `lower_core.rs`.
- **Agent-native**: `package`, `module`, `import`, `use`, `capability`, `extern`, and orchestration metadata feed `in agent` and `in graph` without hidden side effects.
- **Shared pipeline**: every front that emits `UnifiedModule` shares the same driver and JIT path ([multi-frontend-ir.md](multi-frontend-ir.md)).

## Top-level declarations (v0.2+)

| Form | Role |
|------|------|
| `package name;` / `module name;` | Dotted identity for reports (at most one each per file) |
| `import "./lib.in";` | Merge local `.in` declarations |
| `import std.io;` | Synthesize stdlib bindings (`print`, …) |
| `use pkg.key;` / `bind pkg.key as alias;` | Semantic package imports (manifest resolution) |
| `capability name;` | Declared outside-world capability facts |
| `enable ext;` | Orchestration extension fact (registry validation) |
| `struct` / `class` / `interface` / `component` | Types and contracts (see skill + conformance) |
| `fn name(params) -> Ret { … }` | Functions; **`fn main`** required for entry |
| `extern rust fn …;` | Foreign binding stub + capability `requires` |

### Stdlib imports

`std.io`, `std.fs`, `std.http`, `std.json`, `std.process`, `std.cli`, `std.env`, `std.path` add extern-style declarations. JIT-backed today includes **`print`**, **`read_file`**, **`write_file`**, **`process_run`**, **`env_get`**, **`env_set`**, **`env_has`**, and path helpers where wired in `native_stdlib` / `lower_stdlib`. `http_get`, full `json_parse`, and remote package install remain contract/diagnostic surfaces until runtime tests land.

### Orchestration (v0.4 contract)

| Command | Purpose |
|---------|---------|
| `in canonicalize --path <file>` | Deterministic `.in` formatting |
| `in graph --path <file> [--json]` | Parser decision, symbols, calls, package identity |
| `in package --path <dir\|manifest\|source>` | Manifest + dependency graph facts |
| `in agent` | Effects, capabilities, orchestration facts |

Canonicalization preserves identity directives, imports, capabilities, foreign
binding metadata, annotations, and topology/orchestration blocks in source order.
Generated import
bindings are omitted from source output. Extern declarations and grouped struct
methods retain their source syntax.

See [orchestration-compiler.md](orchestration-compiler.md).

## Types and bodies

- Types: `Int`, `String`, `Bool`, `void` / `Void`, named structs, fixed-size array types (`[Type]`).
- Params: `name: Type`.
- Bodies: `let`, assign, `return`, `if` / `else`, `while`, calls, literals, array indexing, field access and assignment (bounded subset).

## Runtime boundaries

Treat roadmap bullets as **contracts** until `in test` and runtime code back them: GPU scheduling, remote `distributed fn`, semantic `use` install, and full FFI execution.

## `hybrid_sil` merge

Merged textual SIL keeps legacy last-`sil @` `function_id` while exposing per-function records in `SilArtifact::functions`. Emitters place **`@main` last** for single-function views. Details: [multi-frontend-ir.md](multi-frontend-ir.md).

## See also

- [docs-site.md](docs-site.md) — crepuscularity web site layout
- [general-compiler.md](general-compiler.md) — multi-language driver
- [native-backend.md](native-backend.md) — AArch64 / x86_64 JIT
- [parser-surface.md](parser-surface.md) — `ParserId` resolution
