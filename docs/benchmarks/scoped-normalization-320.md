# Scoped normalization debug measurements

All 27 retained fixtures execute to 42 with no imports or degradations. Source, normalized IR and generated machine-code hashes match the 319 baseline and all three 320 candidates for every fixture.

The baseline is an immutable prepublication 0.9.319 r5 debug snapshot, and the 0.9.320 r2/r3/r5 builds are unpublished candidates. Exact artifact identities are retained in the data.

Three normalization samples per fixture were collected using immutable debug libraries. The r3 candidate includes the generic-argument pass; r5 includes source-r18 correctness fixes. These snapshots precede the later exact import-alias correction. Concurrent system load was uncontrolled; shallow-case noise and additional work mean these results do not establish uniform speedup. They are not release compilation or full-selfhost measurements.

| Shape | Depth | 319 median ms | 320 r2 median ms | 320 candidate r3 median ms | 320 candidate r5 median ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| nested-let | 0 | 1.401 | 0.488 | 1.738 | 0.845 |
| nested-let | 4 | 5.810 | 1.613 | 1.941 | 3.080 |
| nested-let | 8 | 187.908 | 4.902 | 3.618 | 3.728 |
| typed-call | 0 | 0.457 | 0.602 | 1.328 | 1.680 |
| typed-call | 5 | 0.958 | 0.971 | 1.817 | 0.925 |
| conditional | 0 | 0.570 | 0.532 | 0.582 | 1.088 |
| conditional | 5 | 27.844 | 2.122 | 3.932 | 1.785 |
| bool-match | 0 | 0.550 | 0.438 | 0.782 | 0.583 |
| bool-match | 5 | 28.512 | 3.124 | 3.275 | 4.260 |

Nested-let depths 0–8 and typed-call, conditional and boolean-match depths 0–5 are the complete measured scope. See the [measurement data](scoped-normalization-320.json) for all 324 raw samples, receipts, source-r18 and historical source-r11 manifests, fixture and probe source, hashes and library/binary identities. No claim is made about asymptotic complexity or optimized release performance.

The 319 baseline was built without `--release` and copied from `target/debug`; both standalone probes use optimization level 2. The data includes build evidence correcting the nested-let r5 raw receipt’s mistaken release label.
