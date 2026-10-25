#!/usr/bin/env python3
"""Link compiler ELF objects at arbitrary addresses and execute actual x86 QEMU."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import queue
import threading

repo = Path(__file__).resolve().parents[1]
fixture = repo / "fixtures/freestanding-object-data"
p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--compiler", required=True)
p.add_argument("--output-dir", type=Path)
p.add_argument(
    "--expect-failure", action="store_true", help="retain a known-bad baseline; require at least one failed case"
)
p.add_argument("--cases", nargs="*", choices=("control", "scalar", "strings", "addresses", "exports"))
a = p.parse_args()
for tool in ("nasm", "ld.lld", "llvm-objcopy", "llvm-readelf", "qemu-system-i386", "qemu-system-x86_64"):
    if not shutil.which(tool):
        p.error(f"required installed tool not found: {tool}")
out = a.output_dir.resolve() if a.output_dir else Path(tempfile.mkdtemp(prefix="in-object-data-"))
if a.output_dir:
    out.mkdir(parents=True, exist_ok=False)
compiler = out / "compiler-snapshot"
shutil.copy2(a.compiler, compiler)
(out / "compiler.json").write_text(
    json.dumps(
        {
            "source": str(Path(a.compiler).resolve()),
            "sha256": hashlib.sha256(compiler.read_bytes()).hexdigest(),
            "version": subprocess.check_output([compiler, "--version"], text=True).strip(),
        },
        indent=2,
    )
)
results = []
for arch in ("i386", "x86_64"):
    for case in a.cases or ("control", "scalar", "strings", "addresses", "exports"):
        if case in ("strings", "addresses") and arch == "i386":
            continue
        for profile in ("default", "lean", "harden"):
            for explicit_base in (False, True):
                directory = out / f"{arch}-{case}-{profile}-{'base' if explicit_base else 'no-base'}"
                directory.mkdir()
                shutil.copyfile(fixture / f"{case}.in", directory / "source.in")
                shutil.copyfile(fixture / "memory.ld", directory / "memory.ld")
                startup = fixture / f"{arch}-startup.asm"
                shutil.copyfile(startup, directory / "startup.asm")
                command = [
                    str(compiler),
                    "compile",
                    "--path",
                    str(directory / "source.in"),
                    "--entry",
                    "main",
                    "--target",
                    "native",
                    "--target-triple",
                    f"{arch}-unknown-none",
                    "--linkage",
                    "static-lib",
                    "--profile",
                    profile,
                    "--out",
                    str(directory / "values.o"),
                    "--json",
                ]
                if explicit_base:
                    command += ["--base", "0x20000000"]
                run = subprocess.run(command, capture_output=True, text=True, timeout=120)
                (directory / "compile.json").write_text(run.stdout)
                (directory / "compile.stderr").write_text(run.stderr)
                report = json.loads(run.stdout)
                compiled = run.returncode == 0 and report.get("success") and not report.get("degradations")
                linked = executed = False
                if compiled:
                    obj = (directory / "values.o").read_bytes()
                    assert obj[:4] == b"\x7fELF" and obj[4] == (1 if arch == "i386" else 2)
                    sections = subprocess.run(
                        ["llvm-readelf", "-S", "-r", "-s", str(directory / "values.o")],
                        capture_output=True,
                        text=True,
                        check=True,
                    )
                    (directory / "object-layout.log").write_text(sections.stdout)
                    if case in ("addresses", "exports"):
                        with (directory / "startup.asm").open("a") as assembly_source:
                            width = "dd" if arch == "i386" else "dq"
                            assembly_source.write(
                                f"\nextern other_export\nsection .rodata\nexport_link_probe: {width} other_export\n"
                            )
                    assembly = subprocess.run(
                        ["nasm", "-f", "elf32" if arch == "i386" else "elf64", "startup.asm", "-o", "startup.o"],
                        cwd=directory,
                        capture_output=True,
                        text=True,
                    )
                    (directory / "assembly.log").write_text(assembly.stdout + assembly.stderr)
                    link = subprocess.run(
                        [
                            "ld.lld",
                            "-m",
                            "elf_i386" if arch == "i386" else "elf_x86_64",
                            "-T",
                            "memory.ld",
                            "startup.o",
                            "values.o",
                            "-o",
                            "linked.elf",
                        ],
                        cwd=directory,
                        capture_output=True,
                        text=True,
                    )
                    (directory / "link.log").write_text(link.stdout + link.stderr)
                    linked = assembly.returncode == 0 and link.returncode == 0
                    if linked:
                        executable = directory / "linked.elf"
                        if arch == "x86_64":
                            # QEMU's Multiboot loader accepts ELF32. Only the
                            # container changes: startup explicitly enters 64-bit
                            # mode before calling the compiler's actual ELF64 code.
                            executable = directory / "multiboot.elf"
                            subprocess.run(
                                ["llvm-objcopy", "-O", "elf32-i386", str(directory / "linked.elf"), str(executable)],
                                check=True,
                            )
                        logpath = directory / "serial.log"
                        with logpath.open("w") as log:
                            qemu = subprocess.Popen(
                                [
                                    "qemu-system-i386" if arch == "i386" else "qemu-system-x86_64",
                                    "-kernel",
                                    str(executable),
                                    "-m",
                                    "64M",
                                    "-display",
                                    "none",
                                    "-serial",
                                    "stdio",
                                    "-monitor",
                                    "none",
                                    "-no-reboot",
                                    "-device",
                                    "isa-debug-exit,iobase=0xf4,iosize=0x04",
                                ],
                                stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT,
                            )
                            try:
                                q = queue.Queue()
                                def _reader():
                                    while True:
                                        line = qemu.stdout.readline()
                                        if not line:
                                            break
                                        q.put(line)
                                t = threading.Thread(target=_reader)
                                t.daemon = True
                                t.start()
                                text_accum = ""
                                end = time.monotonic() + 3
                                while time.monotonic() < end:
                                    remaining = end - time.monotonic()
                                    if remaining <= 0:
                                        break
                                    try:
                                        line = q.get(timeout=remaining)
                                        decoded = line.decode(errors="replace")
                                        log.write(decoded)
                                        log.flush()
                                        text_accum += decoded
                                        if "object data" in text_accum:
                                            break
                                    except queue.Empty:
                                        if qemu.poll() is not None:
                                            break
                            finally:
                                qemu.terminate()
                                try:
                                    qemu.wait(timeout=3)
                                except subprocess.TimeoutExpired:
                                    qemu.kill()
                                    qemu.wait()
                        serial = logpath.read_text(errors="replace")
                        executed = "PASS object data" in serial and "FAIL object data" not in serial
                row = {
                    "arch": arch,
                    "case": case,
                    "profile": profile,
                    "explicit_base": explicit_base,
                    "compiled": bool(compiled),
                    "linked": bool(linked),
                    "runtime_pass": bool(executed),
                    "reason": report.get("reason"),
                }
                results.append(row)
                print(json.dumps(row), flush=True)
(out / "summary.json").write_text(json.dumps(results, indent=2))
all_pass = all(row["runtime_pass"] for row in results)
if a.expect_failure:
    assert any(
        row["compiled"] and not row["runtime_pass"] for row in results
    ), "baseline must compile before demonstrating link/runtime failure"
assert (not all_pass) if a.expect_failure else all_pass, f"unexpected regression outcome; evidence: {out}"
print(
    f"{'EXPECTED BASELINE FAILURE' if a.expect_failure else 'PASS'}: {len(results)} object/link/runtime cases ({out})"
)
