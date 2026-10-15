# Native backend contract

`inauguration` owns these backend paths today:

1. **Bytecode VM subset** (all hosts): supported Core IR fronts lower to textual SIL, then to `.bca` bytecode assembly, then execute in the in-tree stack VM.
2. **Native exit-stub subset** (`aarch64-apple-darwin` only): scalar-return entry functions in the Core IR subset are const-evaluated through the bytecode pipeline and emitted as a tiny owned Mach-O executable that exits with the evaluated code. No `swiftc`, `clang`, or linker invocation occurs on this path.
3. **Target-triple object/module subsets**: selected non-host triples emit inspectable object, archive, module, or minimal executable artifacts for const-evaluable scalar entry functions without invoking an external linker or language compiler.

All languages including Swift go through the unified Core IR pipeline. The old `swiftc`/SwiftPM fallback has been removed — Swift is now a Tree-sitter Core IR front like all other languages.

## Stable status

### Bytecode backend

| Field | Value |
|-------|-------|
| `name` | `bytecode` |
| `implemented` | `true` (all hosts) |
| `stage` | `owned-runtime-subset` |
| `reason_code` | `bytecode-vm-subset` |
| `reason` | `inauguration owns this bytecode assembly format, SIL-to-bytecode lowering path, and stack VM runtime for the supported Core IR subset` |
| `input_stage` | `core-ir-to-textual-sil` |
| `artifact_kind` | `bytecode-assembly` |

### Native backend

| Host | `implemented` | `stage` | `reason_code` | `artifact_kind` |
|------|---------------|---------|---------------|-----------------|
| `aarch64-apple-darwin` host executable | `true` | `owned-native-subset` | `native-aarch64-subset` | `mach-o-executable` |
| `aarch64-apple-darwin` staticlib | `true` | `owned-native-subset-aarch64-static` | `native-object-subset` | `mach-o-static-archive` |
| `aarch64-apple-darwin` app bundle | `true` | `owned-native-subset-aarch64-app` | `native-aarch64-darwin-app-subset` | `.app` bundle |
| `x86_64-unknown-linux-gnu` staticlib | `true` | `owned-object-subset` | `native-object-subset` | `elf-relocatable-object` |
| `x86_64-unknown-linux-gnu` executable | `true` | `owned-native-subset-x86_64` | `native-x86_64-linux-exit-subset` | `elf-executable` |
| `x86_64-unknown-linux-gnu` AppDir | `true` | `owned-native-subset-x86_64-appdir` | `native-x86_64-linux-appdir-subset` | `AppDir` |
| `x86_64-pc-windows-msvc` executable | `true` | `owned-native-subset-x86_64` | `native-x86_64-windows-exe-subset` | `pe-executable` |
| `aarch64-unknown-linux-gnu` staticlib | `true` | `owned-native-subset-aarch64-static` | `native-object-subset` | `elf-relocatable-object` |
| `aarch64-unknown-linux-gnu` executable | `true` | `owned-native-subset-aarch64` | `native-aarch64-linux-exit-subset` | `elf-executable` |
| `armv7-unknown-linux-gnueabihf` staticlib | `true` | `owned-object-subset` | `native-object-subset` | `elf32-relocatable-object` |
| `armv7-unknown-linux-gnueabihf` executable | `true` | `owned-native-subset-arm32` | `native-armv7-linux-exit-subset` | `elf32-executable` |
| `wasm32-unknown-unknown` staticlib | `true` | `owned-object-subset` | `native-object-subset` | `wasm-module` |
| other | `false` | `contract-only` | `native-backend-not-implemented` | `none` |

On Apple Silicon macOS, `in compile --path apps/polyglot-sample/sample.in --target native --entry answer --out target/in/answer-sample` produces an owned executable that exits `42`. On Linux and other hosts, the same command reports `native-backend-not-implemented` and bytecode remains the primary owned executable path.

`in backend --path <file> --target bytecode --json` reports the owned bytecode backend and artifact facts for supported inputs. `in backend --target native --json` mirrors the host-specific native status above.

The target registry also carries checked-in In target equivalents for the Rust target triple matrix. These names are compiler target identities for planning, reports, manifests, and future lowering work. They do not imply object emission, linking, ABI lowering, or a native runtime until a target-specific backend is implemented and tested in this repository.

The object backends include `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `armv7-unknown-linux-gnueabihf`, and `wasm32-unknown-unknown`. x86_64 ELF objects contain real function bodies, writable `.data`, external call relocations, and section-relative references to globals, strings and function addresses. The freestanding `i386-unknown-none`, `i686-unknown-none` and `x86_64-unknown-none` object paths preserve these sections and export every function. Hosted x86_64 objects export the requested entry while keeping sibling bodies local. The linker chooses final section addresses independently of `--base`; Harden preserves library export and external names. `scripts/check-freestanding-object-data.py` verifies i386 and x86_64 linking and execution in QEMU across all profiles, including separate code/data placement. `aarch64-unknown-linux-gnu` and `aarch64-unknown-none` emit ELF64 AArch64 objects containing real function bodies, including parameters, internal calls, and every exported function at its actual code offset. External calls and writable globals are rejected until ELF relocations support them. The emitted AArch64 bodies are tested on the host; Linux runtime execution remains unverified. ARMv7 objects and WebAssembly modules still cover const-evaluable scalar entry functions.

`in compile --target native --target-triple aarch64-apple-darwin --linkage static-lib --entry answer --out target/libanswer.a` emits an `ar` archive containing a Mach-O ARM64 object member with an exported `_answer` symbol. The archive contains real Core IR function bodies, a linker index, and external call relocations. Integer parameters, multiple exports, and libc calls are verified by linking and running C callers. Writable-global references and degraded exports are rejected until the object writer can represent them.

`in compile --target native --target-triple x86_64-unknown-linux-gnu --linkage executable --entry answer --out target/answer` emits a minimal ELF64 Linux executable that exits with the const-evaluated scalar value through the Linux `exit` syscall. This is not general x86_64 native lowering: it has no linker, libc, dynamic loader, relocations, argv/envp contract, heap, imports, or general function ABI support.

`aarch64-unknown-linux-gnu` and `armv7-unknown-linux-gnueabihf` also emit minimal Linux executables for the same const-evaluated scalar subset. They use target-native Linux `exit` syscalls and are checked structurally on all hosts, with QEMU runtime checks when `qemu-aarch64` or `qemu-arm` is installed.

`in compile --target native --target-triple x86_64-pc-windows-msvc --linkage executable --entry answer --out target/answer.exe` emits a PE32+ AMD64 console executable whose entry imports `kernel32.dll!ExitProcess` and exits with the const-evaluated scalar value. It does not link a CRT, emit unwind metadata, or claim Windows ABI/runtime coverage beyond this minimal process-exit artifact.

`in compile --target native --target-triple aarch64-apple-darwin --linkage executable --entry answer --out target/Answer.app` emits a macOS `.app` bundle containing the owned AArch64 Mach-O executable plus `Info.plist` and `PkgInfo`. A normal host executable path without `.app` remains the host native route.

`in compile --target native --target-triple x86_64-unknown-linux-gnu --linkage executable --entry answer --out target/Answer.AppDir` emits a Linux AppDir with an `AppRun` ELF executable and desktop entry. `.AppImage` remains fail-closed with `native-package-not-implemented` until the repository owns an AppImage runtime and SquashFS writer.

Explicit `--target-triple` requests fail closed when the owned backend has no target/linkage implementation. They do not fall through to the host Mach-O path.

The runnable cross-artifact example lives in `apps/native-artifact-sample/`. Run `bash scripts/check-native-artifact-sample.sh` to build and inspect the supported ELF, PE, Mach-O bundle, AppDir, object, archive, and WASM outputs from one `.in` source file.

`bash scripts/check-native-linkable-objects.sh` links the x86_64 ELF relocatable object into a C harness and runs it on Linux x86_64 hosts with `cc`; other hosts skip that runtime gate.

`bash scripts/check-native-windows-executable.sh` validates the PE32+ import table and runs the generated `.exe` through Wine when Wine or Docker is available. This is a runtime smoke test, not a replacement for native Windows CI.

## Compile cache (Wave 6)

`in compile` hashes source path + content into `target/in/cache/<frontend_hash>/metadata.json`, storing the serialized owned compile report (including `frontend_hash`). Repeated compiles with the same frontend input reuse cached metadata when target, entry, and module id match.

## Scope

The native backend is the stage after source fronts, Core IR, textual SIL, and SIL analysis. It is responsible for turning a checked program into a runnable artifact without silently delegating code generation to a language toolchain.

| Area | First contract |
|------|----------------|
| Input | A checked Core IR subset with explicit functions, locals, calls, returns, and scalar values. |
| Output | `.bca` bytecode assembly on all hosts; Mach-O exit stubs on `aarch64-apple-darwin` for const-evaluable scalar entry functions. |
| Runtime | Only the runtime pieces present in this repository may be claimed. |
| Diagnostics | Unsupported constructs fail closed with `native-backend-not-implemented` or a narrower backend reason code. |
| Observability | Backend reports include input language, frontend level, IR stage, backend stage, artifact kind, timing, jobs, cache hit, and reason codes. |

## Non-goals for the first backend slice

- No claim of arbitrary Swift, C++, Rust, Go, V, JavaScript, Python, JVM, CLR, or Ruby native execution on every host.
- No silent `swiftc`, `clang`, `rustc`, `go`, `v`, or system linker fallback on a self-hosted native path.
- No broad ABI promise before the value model, call convention, symbols, object format, and runtime ownership are documented with tests.

## Integration points

- `docs/architecture/universal-compiler-roadmap.md`: keeps the native runtime spine and production-claim ladder honest.
- `docs/architecture/general-compiler.md`: defines the current source front to Core IR to textual SIL path.
- `in-cli/src/native_backend.rs`: owns backend status records for bytecode and native targets.
- `in-cli/src/native_emit/`: emits the aarch64 Mach-O exit stub.
- `in-cli/src/compile_cache.rs`: owns compile metadata cache under `target/in/cache/`.
- `in-cli/src/owned_compile.rs`: owned `in compile` pipeline and reports.
