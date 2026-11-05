# Verification and self-host observations

## Ecosystem imports do not execute through the tested backends

The ecosystem sample installs packages and reports their imports as resolved,
but it cannot call those packages through the Linux x86-64 JIT or standalone
native executable. Package and agent checks establish that metadata and
declarations exist. They do not establish that an imported function runs.
This distinction matters for the project's claim of using libraries across
ecosystems. Language parser support is not evidence of executable imports.

On 2026-11-05, after installing all four sample dependencies, `in execute
--verbose` rejected `hono-greet` as an unresolved function. The sample calls
hyphenated names, but adapter metadata declares underscore names such as
`hono_greet`. Replacing all four calls with the declared names got past that
check, but JIT lowering then rejected all four as unresolved runtime imports.
A native build with an Int entry calling `hono_greet` also rejected that import.
The sample's original String entry is rejected earlier by the native entry
contract, so it cannot establish native package behavior on its own.

Installed adapter metadata produces bodyless extern declarations. The separate
`package_runtime` module can collect adapter commands and invoke an allowed
program, but no production caller connects it to JIT or native execution.
The adapter protocol currently accepts no arguments. This is an execution gap,
not evidence that arbitrary ecosystem libraries work as native functions. The
independent ecosystem execution CI job requires a local npm function to accept
two different arguments and return the corresponding values. It fails today at
the first call because the synthesized declaration accepts zero arguments. Its
failure does not prevent the other CI jobs from running.

Two responses are available, and the project owner must choose between them:

1. Implement the execution path. Symbol resolution and adapter invocation in
   both backends is a large, cross-backend change. A typed argument and result
   protocol is a separate medium-to-large change: the adapter currently models
   neither parameter types nor argument passing. End-to-end tests cover the
   contract, but the native process boundary and protocol still need design.
   These are rough scope estimates, not time estimates.
2. Correct the public claim to distinguish metadata and parser support from
   executable ecosystem imports until an execution path exists. This is a
   documentation change; it does not add runtime capability.

## Freestanding SCI arrays require runtime imports

Space reported that its pinned compiler revision `d4a70d23` and a later build
at `fe28e87a` emit `abort` in `app-sum.sci`, which its loader rejects before
entry. Source at both revisions emits `malloc` and `abort` for x86-64 array
allocation. A separate reproduction used the local release binary and Space's
`app-sum.in` at source blob `c01e0a069764ee5691d359cdc8316600af4a6331`.
The resulting SCI import table contains both names. This reproduction did not
run Space or independently inspect the two reported pinned artifacts.

The SCI loader resolves every import before entry, even when a call is only on
an allocation-failure path. The compiler must make its implicit allocator and
failure requirements explicit, while the target must provide any required
allocation service. The previous generated calls and failure behavior are
documented in `docs/native-backend.md`. A fixed-size array can avoid allocation
only if its lifetime and aliases permit stack storage; no such proof was made
for this component.

An inline `UD2` would remove `abort`, not `malloc`. It is not a demonstrated
safe replacement: Space's current invalid-opcode handler halts the nanokernel.
The target's allocation and failure ABI needs an agreed contract and runtime
verification before changing the emitted failure path.

This and the ecosystem failure both appear as unresolved imports, but their
causes differ. SCI array lowering creates implicit runtime calls that need
providers. Ecosystem adapter declarations have no production execution bridge
to their adapter commands. Fixing either path does not fix the other.

### Proposed target contract, not yet implemented

The compiler should require an explicit target runtime declaration when an SCI
contains generated heap allocation calls. The declaration supplies the target's
allocator and runtime-failure handler symbol names and contract version.
Generated service names must be explicitly namespaced and versioned, never
`malloc` or `abort`; their exact spelling remains to be agreed. It must not
silently default to libc symbols, a trap instruction, or a hosted runtime. If a
required declaration is absent or invalid, compilation should fail before
writing an SCI and name each missing service. An image with no generated heap
allocation should not require either service.
The compiler must distinguish its generated service calls from source-authored
externs and reject symbol collisions that would bind a service to the wrong
function. Both SCI layout passes must use the same declaration.

For x86-64, the proposed service ABI uses the existing SysV integer calling
convention. The allocator receives the required byte count in `RDI` and returns
an eight-byte-aligned writable pointer or null in `RAX`. Storage must remain
valid for every use of the array, including after the creating function returns.
The failure handler takes no arguments and must not return. Its behavior must
be appropriate to the target's isolation model. Allocations count against an
instance's bounded quota and are reclaimed at component teardown. No per-array
free is assumed. Configuration syntax and an error payload remain unspecified.

The emitter should use the declared target symbols in SCI imports, including in
every retained export, and report their requirements. It should separately
account for other generated imports such as `memcpy` when array copy needs it.
Checking the declaration at compile time does not prove that a provider will be
installed, reachable or authorized at load time. The loader must still check
the SCI import table against the deployed providers and their capabilities.
SCI v2 records names and call sites, not service signatures or provenance, and
its `call rel32` binding also requires a reachable provider address. A
namespaced, versioned name does not itself prove provider signature,
provenance or authority.

Space would declare the symbols and ABI it supports, provide an allocator and
failure handler for the component's actual execution domain, and arrange for
the loader to resolve and authorize them against a versioned binding. Its
provider must use bounded per-instance memory authority, not an unrestricted
kernel allocator. Its existing heap and fault handlers must be assessed against
that contract; their names or compatibility cannot be assumed. Space also needs
a test that loads and runs the emitted image and a failure-path test that
confirms allocation failure does not halt the nanokernel.
The no-argument failure handler reports a generic component runtime failure;
it cannot distinguish allocation failure from an invalid array-copy source.
Failure must terminate only the affected instance, revoke its authority and
reclaim resources as supported. A path without that containment must refuse
execution. Committed external effects are not assumed to roll back.

Proven non-escaping stack allocation can remove the heap requirement for
eligible arrays, but must preserve mutation, aliasing and lifetime semantics.
It does not replace the contract for escaping arrays. The current compiler does
not enforce this proposed declaration or generate target-selected service names.

## Verification results need observable evidence

Two verification tools reported on a different result from the one their users
could reasonably expect.

- Linux x86-64 coverage used the AArch64 lowerer, not the Linux ELF path used
  by `in build`. Coverage now requires an explicit backend and uses the ELF
  admission path for Linux x86-64.
- The backend differential compared stdout and process status, not full return
  values. String and Float returns were absent, and process status carries only
  the low eight bits of an Int. For example, Int 263 and Int 7 have the same
  low byte. Matching output and status cannot prove those values agree.

The differential uses an opt-in descriptor 3 channel to compare Int, Bool,
Void and Float results from the JIT and the executed Linux x86-64 ELF. It compares stdout
and process termination separately. Missing or malformed records and native
refusals make the check incomplete and prevent a baseline update. The oracle
controls include equal exit statuses with differing full-width integers.

An earlier run found one observable agreement, 72 unobservable results, and 16
native refusals. With the channel, a run of the same 89 fixtures found 72
agreements and 17 native refusals, with no disagreements or invalid records.
The additional refusal was `conformance/classes/class-methods.in`: its Void
entry built without the channel but was previously unobservable. Adding a
distinct Void result makes that case a real comparison. A subsequent run found
73 agreements and 16 refusals, with no disagreements or invalid records.
Adding exact-bit Float records moved `conformance/types/float-arithmetic.in`
to agreement: the next run found 74 agreements and 15 refusals. Thirteen first
refusals are String entries unsupported by the channel; two report unresolved
`print` and `str_concat` imports. Later limitations may still be hidden. The
check remains failing; the baseline has not been updated.

## Self-host compile memory

On 2026-11-05, a Linux x86-64 orb ran the repo-built release compiler in the
default serial `in test` group:

```sh
in compile --path in-cli/src/main.rs --target jit --out /dev/null
```

The group reported success after 1,055,664.994 ms, about 17 minutes 36 seconds.
This entry-less compile is not the strict CI self-host build and its success
does not establish that the compiler emitted an executable. The full `in test`
run failed later at the incomplete backend differential.

The process had one thread and consumed one CPU core during observation. Its
largest sampled resident set was 3,052,324 KiB, about 2.91 GiB, shortly before
it exited. Earlier samples were lower, but sampling was sparse and did not
capture the final peak. The samples establish a lower bound, not an exact peak,
a leak, or a monotonic growth curve. No allocation profile was collected.

README performance figures describe older macOS measurements and a separately
built release binary. They are not measurements of this Linux run or proof of a
native self-build. Repeat the exact compile under a peak-RSS recorder, collect a
time series and allocation profile, and identify retained objects before calling
the growth a leak or comparing speeds across runs.

## Conclusion

Two green results did not establish the behavior they appeared to measure.
Package checks confirmed metadata, not executable ecosystem imports. The
backend differential reported agreement without observing full return values.
Tests of verification tools must include controls that produce wrong results and
confirm that the tools reject them. Product claims also need execution tests,
not only checks that declarations or reports exist.
