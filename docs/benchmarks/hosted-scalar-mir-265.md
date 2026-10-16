# Hosted scalar MIR checkpoint 0.9.265

Measured on AArch64 macOS using source-built release binaries. These are small
fixtures, including arithmetic, calls, recursion, nested loops and a mutable
global fallback. Some calls and arithmetic fold to constants before emission.
This is not a whole-repository or application workload benchmark.

Each comparison uses 30 measured pairs after three warmup pairs for each of six
fixtures and three profiles (540 measured pairs per comparison). Invocation
order alternates. Every report must execute, return the exact Int(42), avoid
degradations and report cache_hit=false. The report reason must identify the
expected MIR or native route. `timing_micros` includes JIT execution and excludes
CLI startup. All medians below are microseconds; lower ratios are better.

The verification refactor reduced Harden medians 9–17% against the preserved
pre-refactor MIR candidate. Machine verification now performs one CFG traversal
per boundary while retaining structural checks and staged-instruction refusal.
Harden still costs 24–60% more than legacy native lowering in this run; Default
and Lean range from about 2% faster to 17% slower on the five scalar fixtures.
The unchanged global fallback controls fluctuate too. This establishes a
bounded Harden improvement, not a general compiler speedup or runtime speedup.

| Fixture | Profile | Native µs | MIR µs | MIR/native | Refactor/before |
|---|---|---:|---:|---:|---:|
| arithmetic | default | 431 | 436 | 1.012 | 1.024 |
| arithmetic | lean | 427.5 | 436 | 1.020 | 1.039 |
| arithmetic | harden | 767 | 948 | 1.236 | 0.826 |
| add | default | 455 | 484 | 1.064 | 1.026 |
| add | lean | 496.5 | 487 | 0.981 | 1.084 |
| add | harden | 716.5 | 935.5 | 1.306 | 0.887 |
| recursion | default | 568.5 | 625.5 | 1.100 | 1.083 |
| recursion | lean | 610 | 711.5 | 1.166 | 1.003 |
| recursion | harden | 1049 | 1681 | 1.602 | 0.833 |
| nested-loop | default | 625.5 | 707 | 1.130 | 1.029 |
| nested-loop | lean | 602 | 681.5 | 1.132 | 0.931 |
| nested-loop | harden | 959.5 | 1331 | 1.387 | 0.913 |
| call-spill | default | 473.5 | 484 | 1.022 | 1.052 |
| call-spill | lean | 478.5 | 480.5 | 1.004 | 0.991 |
| call-spill | harden | 692 | 907.5 | 1.311 | 0.834 |
| global-fallback | default | 568 | 534 | 0.940 | 0.982 |
| global-fallback | lean | 563 | 592 | 1.052 | 1.052 |
| global-fallback | harden | 911.5 | 916.5 | 1.005 | 1.063 |

[Machine-readable medians, raw samples and binary/fixture hashes](hosted-scalar-mir-265.json)
retain both comparisons. Full raw CLI reports remain in local verification
evidence. The baseline is native 0.9.263; both MIR candidates are unpublished
0.9.265 builds distinguished by their binary hashes.

Before the verification refactor, the full Cargo suite passed 2,151 library
checks (one manual test ignored), 119 CLI checks, 11 evaluation checks and 21
release-semantics checks. After the refactor, all 33 MIR tests, five hosted MIR
tests and the exact-entry regression passed, along with strict Clippy and all
required CLI gates: 119 conformance cases, 89 backend comparisons, target matrix,
owned-native, protocol generation and installer verification. All 45 source-CLI
profile controls and 51 actual x86 executions under Rosetta passed. All 51 x86
code hashes match the pre-refactor outputs. The x86 checks exercise machine
emission, not a hosted x86 CLI build. `in bench` ran but found zero metrics rows;
it is not the source of these measurements.
