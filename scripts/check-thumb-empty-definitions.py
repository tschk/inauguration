#!/usr/bin/env python3
"""Optional Cortex-M33 C/C++ empty-definition preservation regression; no compiler build."""

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
root = (
    args.output_dir.resolve()
    if args.output_dir
    else Path(tempfile.mkdtemp(prefix="in-thumb-empty-definitions-"))
)
if args.output_dir:
    root.mkdir(parents=True, exist_ok=False)
compiler = root / "compiler-snapshot"
shutil.copy2(args.compiler, compiler)
(root / "compiler.json").write_text(
    json.dumps(
        {
            "source": str(Path(args.compiler).resolve()),
            "sha256": hashlib.sha256(compiler.read_bytes()).hexdigest(),
            "version": subprocess.check_output(
                [compiler, "--version"], text=True
            ).strip(),
        },
        indent=2,
    )
    + "\n"
)
print(f"Evidence: {root}", flush=True)
cases = json.loads(
    (repository / "fixtures/thumb-empty-definitions/cases.json").read_text()
)
results = []
board = root / "board-build"
board.mkdir()
for name in ("startup.S", "build.zig", "memory.ld"):
    shutil.copyfile(repository / "fixtures/thumb-stack-arguments" / name, board / name)
for case in cases:
    source = (
        repository
        / "fixtures/thumb-empty-definitions"
        / (case["name"] + "." + case["extension"])
    ).read_text()
    for profile in ("default", "lean", "harden"):
        directory = root / f"{case['name']}-{profile}"
        directory.mkdir()
        path = directory / ("program." + case["extension"])
        path.write_text(source)
        compile_run = subprocess.run(
            [
                compiler,
                "compile",
                "--path",
                str(path),
                "--module-id",
                "ThumbEmptyDefinitions",
                "--entry",
                "main",
                "--profile",
                profile,
                "--target",
                "native",
                "--target-triple",
                "thumbv8m.main-none-eabi",
                "--linkage",
                "static-lib",
                "--out",
                str(directory / "values.o"),
                "--json",
            ],
            capture_output=True,
            text=True,
            timeout=120,
        )
        (directory / "compile.json").write_text(compile_run.stdout)
        (directory / "compile.stderr").write_text(compile_run.stderr)
        report = json.loads(compile_run.stdout)
        assert (
            compile_run.returncode == 0
            and report["success"]
            and not report.get("degradations")
        ), report
        object_bytes = (directory / "values.o").read_bytes()
        for name in ("startup.S", "build.zig", "memory.ld"):
            shutil.copyfile(
                repository / "fixtures/thumb-stack-arguments" / name, directory / name
            )
        startup = directory / "startup.S"
        startup_text = startup.read_text()
        assert startup_text.count("  bl main\n") == 1, "board main call changed"
        startup_text = startup_text.replace("  bl main\n", "  bl noop\n  bl main\n")
        assert startup_text.count("cmp r0, #42") == 1, "board return assertion changed"
        startup.write_text(
            startup_text.replace("stack arguments", "empty definitions").replace(
                "cmp r0, #42", f"cmp r0, #{case['expected']}"
            )
        )
        shutil.copyfile(directory / "values.o", board / "values.o")
        assert (
            board / "values.o"
        ).read_bytes() == object_bytes, "board object differs from current case"
        shutil.copyfile(startup, board / "startup.S")
        build = subprocess.run(
            ["zig", "build"],
            cwd=board,
            capture_output=True,
            text=True,
            timeout=120,
        )
        (directory / "build.log").write_text(build.stdout + build.stderr)
        assert build.returncode == 0, build.stderr
        executable = directory / "executed.elf"
        shutil.copyfile(board / "zig-out/bin/stack-args", executable)
        (directory / "artifacts.json").write_text(
            json.dumps(
                {
                    "object_sha256": hashlib.sha256(object_bytes).hexdigest(),
                    "executed_elf_sha256": hashlib.sha256(
                        executable.read_bytes()
                    ).hexdigest(),
                },
                indent=2,
            )
            + "\n"
        )
        serial = directory / "serial.log"
        with serial.open("w") as log:
            process = subprocess.Popen(
                [
                    "qemu-system-arm",
                    "-machine",
                    "mps2-an521",
                    "-cpu",
                    "cortex-m33",
                    "-nographic",
                    "-kernel",
                    str(executable),
                ],
                stdout=log,
                stderr=subprocess.STDOUT,
            )
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    text = serial.read_text(errors="replace")
                    if "empty definitions" in text or process.poll() is not None:
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
        passed = (
            "PASS empty definitions" in text and "FAIL empty definitions" not in text
        )
        results.append({"case": case["name"], "profile": profile, "passed": passed})
        (root / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
(root / "summary.json").write_text(json.dumps(results, indent=2) + "\n")
failed = [item for item in results if not item["passed"]]
assert not failed, failed
print(f"PASS: {len(results)} Thumb empty-definition runs; C/C++ prototype ordering")
