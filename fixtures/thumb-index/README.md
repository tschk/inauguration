# Thumb checked array indexing

Run `python3 scripts/check-thumb-index.py --compiler /absolute/path/to/in`
only with a compiler containing the checked-index fix (0.9.223 or later).
Do not execute these invalid-index cases on older Thumb compilers: their
negative-index guard could use stale flags and access outside the array.

Requires installed Zig and qemu-system-arm; no compiler build or dependencies
are installed. The runner reuses `fixtures/thumb-stack-arguments` board glue and
one Zig board-build directory. All three profiles run original and actual
canonical source, for six QEMU runs. Every case retains its source, object,
glue, compile/build logs, executed ELF and object/ELF hashes. The evidence root
also records the exact compiler snapshot, version/hash and results.

The combined program requires valid reads/stores, length-boundary rejection,
dynamic and immediate negative/carrier rejection, and return1 from the current
helper on failure while its caller continues. Trace assertions prove source
operand order, single evaluation, and skipped following statements. Multiplying
an invalid read by zero and discarding an unused read must preserve the exit;
unselected lazy branches must avoid invalid reads. Success returns42.

The executable fixture uses explicitly typed multi-element arrays and stages
store values before assignment. Current Thumb preparation separately refuses
inferred array locals and direct call-bearing immediate-index stores (the latter
can create a synthetic array local without a literal initializer). These are
existing compilation limits, not successful runtime cases in this test. Typed
empty arrays are supported and both their read and store failures are exercised.
