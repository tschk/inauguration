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

The differential now treats an unobservable result or a native refusal as an
incomplete check. Neither can pass the check or update its baseline. Fault
injection in `scripts/test-backend-differential.sh` checks an unobservable
String, a truncated Int, and differing exit statuses. The old differential
fails those controls. CI invokes the control script.

On Linux x86-64, one run compared 89 fixtures. It reported one observable
agreement, 72 unobservable results, and 16 native refusals. The check failed as
intended. A typed native result channel is still needed to compare full return
values. After adding it, rerun the refused fixtures because each first refusal
can conceal another limitation.

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
