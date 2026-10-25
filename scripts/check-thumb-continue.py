#!/usr/bin/env python3
"""Optional Cortex-M33 continue/backedge regression; no compiler build."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

repository = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--compiler", required=True)
parser.add_argument("--output-dir", type=Path)
args = parser.parse_args()
for tool in ("zig", "qemu-system-arm"):
    if not shutil.which(tool):
        parser.error(f"required installed tool not found: {tool}")
root = args.output_dir.resolve() if args.output_dir else Path(tempfile.mkdtemp(prefix="in-thumb-continue-"))
if args.output_dir:
    root.mkdir(parents=True, exist_ok=False)
compiler = root / "compiler-snapshot"
shutil.copy2(args.compiler, compiler)
(root / "compiler.json").write_text(json.dumps({
    "source": str(Path(args.compiler).resolve()),
    "sha256": hashlib.sha256(compiler.read_bytes()).hexdigest(),
    "version": subprocess.check_output([compiler, "--version"], text=True).strip(),
}, indent=2) + "\n")
print(f"Evidence: {root}", flush=True)
# Public preparation must refuse a jump without an enclosing loop.
for jump in ("continue", "break"):
    for profile in ("default", "lean", "harden"):
        path = root / f"outside-{jump}-{profile}.in"
        path.write_text(f"fn main()->Int{{{jump};return 42;}}")
        output = root / f"outside-{jump}-{profile}.o"
        refused = subprocess.run(
            [compiler, "compile", "--path", str(path), "--target", "native",
             "--target-triple", "thumbv8m.main-none-eabi", "--linkage", "static-lib",
             "--profile", profile, "--out", str(output), "--json"],
            capture_output=True, text=True, timeout=120,
        )
        (root / f"outside-{jump}-{profile}.json").write_text(refused.stdout)
        (root / f"outside-{jump}-{profile}.stderr").write_text(refused.stderr)
        refusal_report = json.loads(refused.stdout)
        assert not refusal_report["success"] and not output.exists(), f"accepted outside-loop {jump}/{profile}"
        assert "loop jump outside a loop" in (refusal_report.get("error") or ""), refusal_report
source = (repository / "fixtures/thumb-continue/loops.in").read_text()
results = []
board = root / "board-build"
board.mkdir()
for name in ("startup.S", "build.zig", "memory.ld"):
    shutil.copyfile(repository / "fixtures/thumb-stack-arguments" / name, board / name)
startup = board / "startup.S"
startup.write_text(startup.read_text().replace("stack arguments", "loop continue"))
for profile in ("default", "lean", "harden"):
    original_object = None
    for form in ("original", "canonical"):
        directory = root / f"{profile}-{form}"
        directory.mkdir()
        path = directory / "program.in"
        path.write_text(source)
        if form == "canonical":
            canonical = subprocess.run(
                [compiler, "canonicalize", "--path", str(path)],
                capture_output=True, text=True, check=True, timeout=30,
            )
            path.write_text(canonical.stdout)
        compile_run = subprocess.run(
            [compiler, "compile", "--path", str(path), "--module-id", "ThumbContinue",
             "--entry", "main", "--profile", profile, "--target", "native",
             "--target-triple", "thumbv8m.main-none-eabi", "--linkage", "static-lib",
             "--out", str(directory / "values.o"), "--json"],
            capture_output=True, text=True, timeout=120,
        )
        (directory / "compile.json").write_text(compile_run.stdout)
        (directory / "compile.stderr").write_text(compile_run.stderr)
        report = json.loads(compile_run.stdout)
        assert compile_run.returncode == 0 and report["success"] and not report.get("degradations"), report
        object_bytes = (directory / "values.o").read_bytes()
        if form == "original":
            original_object = object_bytes
        else:
            assert object_bytes == original_object, "canonical source changed emitted object"
        shutil.copyfile(directory / "values.o", board / "values.o")
        assert (board / "values.o").read_bytes() == object_bytes, "board object differs from current case"
        build = subprocess.run(
            ["zig", "build"], cwd=board, capture_output=True, text=True, timeout=120,
        )
        (directory / "build.log").write_text(build.stdout + build.stderr)
        assert build.returncode == 0, build.stderr
        executable = directory / "executed.elf"
        shutil.copyfile(board / "zig-out/bin/stack-args", executable)
        (directory / "artifacts.json").write_text(json.dumps({
            "object_sha256": hashlib.sha256(object_bytes).hexdigest(),
            "executed_elf_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
        }, indent=2) + "\n")
        serial = directory / "serial.log"
        with serial.open("w") as log:
            process = subprocess.Popen(
                ["qemu-system-arm", "-machine", "mps2-an521", "-cpu", "cortex-m33",
                 "-nographic", "-kernel", str(executable)],
                stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    text = serial.read_text(errors="replace")
                    if "loop continue" in text or process.poll() is not None:
                        break
                    time.sleep(0.005)
            finally:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        text = serial.read_text(errors="replace")
        assert "PASS loop continue" in text and "FAIL loop continue" not in text, text
        results.append({"profile": profile, "form": form, "frontend_hash": report["frontend_hash"]})
(root / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
print("PASS: six Thumb runs: finite continue, nested loops, condition effects and break interaction")
