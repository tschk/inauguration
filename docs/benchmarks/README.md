# Benchmarks

Comparison docs for compile time, binary size, and runtime. Regenerate before publishing numbers.

| Guide | Comparison |
| --- | --- |
| [JIT](jit.md) | Cold / warm / daemon compile latency on macOS ARM64 |
| [Polyglot compilers](polyglot-compilers.md) | **`in compile`** median wall time vs **native compiler** per language (`in/native` ratio) |
| [Self-host vs native](self-host-vs-native.md) | **Owned Rust front** (`in build` on `main.rs`) vs **`cargo build --release`** (full crate) |
| [Scoped normalization](scoped-normalization-320.md) | Debug-only nested value fixtures with matching IR and machine-code hashes |

```bash
./scripts/bench-self-host.sh && python3 scripts/render-self-host-bench-md.py
./scripts/bench_polyglot_compilers.v
in bench   # JIT samples (see jit.md)
```

**How to read ratios:** On polyglot rows, **`in/native < 1`** means inauguration finished faster than that native check for the same sample. On self-host, the front-only `in build` wall time is compared to an incremental **Cargo release** rebuild (not a clean `cargo clean` cold build).