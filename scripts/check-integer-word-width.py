#!/usr/bin/env python3
"""Optional actual Thumb/i386 word-width regression; no compiler build or install."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

repository = Path(__file__).resolve().parents[1]
fixture = repository / "fixtures/integer-word-width"
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--compiler", required=True)
parser.add_argument("--output-dir", type=Path)
args = parser.parse_args()
for tool in ("nasm", "ld.lld", "zig", "qemu-system-arm", "qemu-system-i386"):
    if not shutil.which(tool):
        parser.error(f"required installed tool not found: {tool}")
root = args.output_dir.resolve() if args.output_dir else Path(tempfile.mkdtemp(prefix="in-word-width-"))
if args.output_dir:
    root.mkdir(parents=True, exist_ok=False)
compiler = root / "compiler-snapshot"
shutil.copy2(args.compiler, compiler)
(root / "compiler.json").write_text(
    json.dumps(
        {
            "source": str(Path(args.compiler).resolve()),
            "sha256": hashlib.sha256(compiler.read_bytes()).hexdigest(),
            "version": subprocess.check_output([compiler, "--version"], text=True).strip(),
        },
        indent=2,
    )
)
print(f"Evidence: {root}", flush=True)
source = (fixture / "word32.in").read_text()
results = []
for target, triple in (
    ("thumb", "thumbv8m.main-none-eabi"),
    ("i386", "i386-unknown-none"),
):
    for profile in ("default", "lean", "harden"):
        directory = root / f"{target}-{profile}"
        directory.mkdir()
        path = directory / "values.in"
        path.write_text(source.replace("0x300000", "0x38000000") if target == "thumb" else source)
        command = [
            str(compiler),
            "compile",
            "--path",
            str(path),
            "--entry",
            "main",
            "--profile",
            profile,
            "--json",
        ]
        compile_run = subprocess.run(
            command
            + [
                "--target",
                "native",
                "--target-triple",
                triple,
                "--linkage",
                "static-lib",
                "--out",
                str(directory / "values.o"),
            ],
            capture_output=True,
            text=True,
            timeout=120,
        )
        (directory / "compile.json").write_text(compile_run.stdout)
        (directory / "compile.stderr").write_text(compile_run.stderr)
        report = json.loads(compile_run.stdout)
        assert compile_run.returncode == 0 and report.get("success") and not report.get("degradations"), report
        elf = (directory / "values.o").read_bytes()
        assert elf[:5] == b"\x7fELF\x01" and int.from_bytes(elf[18:20], "little") == (
            40 if target == "thumb" else 3
        ), "expected actual ELF32 target object"
        if target == "thumb":
            for name in ("startup.S", "build.zig", "memory.ld"):
                shutil.copyfile(
                    repository / "fixtures/thumb-stack-arguments" / name,
                    directory / name,
                )
            startup = directory / "startup.S"
            startup.write_text(startup.read_text().replace("stack arguments", "integer word width"))
            build = subprocess.run(
                ["zig", "build"],
                cwd=directory,
                capture_output=True,
                text=True,
                timeout=120,
            )
            executable = directory / "zig-out/bin/stack-args"
            qemu_command = [
                "qemu-system-arm",
                "-machine",
                "mps2-an521",
                "-cpu",
                "cortex-m33",
                "-nographic",
                "-kernel",
                str(executable),
            ]
        else:
            shutil.copyfile(fixture / "i386-startup.asm", directory / "startup.asm")
            shutil.copyfile(fixture / "i386-memory.ld", directory / "memory.ld")
            startup = directory / "startup.asm"
            startup.write_text(startup.read_text().replace("integer width i386", "integer word width"))
            assembly = subprocess.run(
                ["nasm", "-f", "elf32", "startup.asm", "-o", "startup.o"],
                cwd=directory,
                capture_output=True,
                text=True,
                timeout=30,
            )
            (directory / "assembly.log").write_text(assembly.stdout + assembly.stderr)
            assert assembly.returncode == 0, assembly.stderr
            build = subprocess.run(
                [
                    "ld.lld",
                    "-m",
                    "elf_i386",
                    "-T",
                    "memory.ld",
                    "startup.o",
                    "values.o",
                    "-o",
                    "kernel.elf",
                ],
                cwd=directory,
                capture_output=True,
                text=True,
                timeout=30,
            )
            executable = directory / "kernel.elf"
            qemu_command = [
                "qemu-system-i386",
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
            ]
        (directory / "build.log").write_text(build.stdout + build.stderr)
        assert build.returncode == 0, build.stderr
        logpath = directory / "serial.log"
        with logpath.open("w") as log:
            qemu = subprocess.Popen(qemu_command, stdout=log, stderr=subprocess.STDOUT)
            try:
                deadline = time.monotonic() + 10
                delay = 0.005
                while time.monotonic() < deadline:
                    if "integer word width" in logpath.read_text(errors="replace") or qemu.poll() is not None:
                        break
                    time.sleep(delay)
                    delay = min(delay * 2, 0.05)
            finally:
                qemu.terminate()
                try:
                    qemu.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    qemu.kill()
                    qemu.wait()
        serial = logpath.read_text(errors="replace")
        assert "PASS integer word width" in serial and "FAIL integer word width" not in serial, serial
        results.append({"target": target, "profile": profile, "compile": True, "runtime": True})
        print(
            f"PASS {target}/{profile}: compiled ELF32 and actual QEMU execution",
            flush=True,
        )
# Separate pure host controls: all of these 32-bit-specific equalities are false
# for the 64-bit Int host. No host program dereferences the target RAM scratch.
conditions = [
    "(2147483647+1)==-2147483648",
    "(identity(2147483647)+identity(1))==-2147483648",
    "0xffffffff<0",
    "identity(0xffffffff)<identity(0)",
    "-(-2147483648)==-2147483648",
    "-identity(-2147483648)==-2147483648",
    "((2147483647+1)*2)==0",
    "((identity(2147483647)+identity(1))*identity(2))==0",
    "(-2147483648-1)==2147483647",
    "(identity(-2147483648)-identity(1))==2147483647",
    "(65536*65536)==0",
    "(identity(65536)*identity(65536))==0",
    "(1<<31)==-2147483648",
    "(identity(1)<<identity(31))==-2147483648",
    "-0xffffffff==1",
    "-identity(0xffffffff)==1",
]
host_path = root / "host64.in"
host_path.write_text(
    "fn identity(value:Int)->Int{return value;}\nfn main()->Int{"
    + "".join(f"if {condition}{{return {index+1};}}" for index, condition in enumerate(conditions))
    + "return 0;}\n"
)
for profile in ("default", "lean", "harden"):
    host = subprocess.run(
        [
            str(compiler),
            "compile",
            "--path",
            str(host_path),
            "--target",
            "jit",
            "--entry",
            "main",
            "--profile",
            profile,
            "--out",
            str(root / f"host-{profile}"),
            "--json",
        ],
        capture_output=True,
        text=True,
        timeout=120,
    )
    (root / f"host-{profile}.json").write_text(host.stdout)
    report = json.loads(host.stdout)
    assert host.returncode == 0 and report.get("success") and report.get("eval_result") == {"Int": 0}, report
    print(f"PASS host64/{profile}: pure controls", flush=True)
(root / "summary.json").write_text(json.dumps(results, indent=2))
