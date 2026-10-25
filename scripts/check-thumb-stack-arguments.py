#!/usr/bin/env python3
"""Cortex-M33 regression for AAPCS stack argument values, effects, and alignment."""
import argparse
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import time

repository = pathlib.Path(__file__).resolve().parent.parent
fixture = repository / "fixtures/thumb-stack-arguments"
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--compiler",
    default=os.environ.get("IN_BIN"),
    required=not bool(os.environ.get("IN_BIN")),
)
parser.add_argument(
    "--output-dir",
    type=pathlib.Path,
    help="new directory for retained build and serial evidence",
)
args = parser.parse_args()
for tool in ["zig", "qemu-system-arm"]:
    if not shutil.which(tool):
        parser.error(f"required tool not found: {tool}")
root = (
    args.output_dir.resolve()
    if args.output_dir
    else pathlib.Path(tempfile.mkdtemp(prefix="in-thumb-stack-"))
)
if args.output_dir:
    root.mkdir(parents=True, exist_ok=False)
for source in fixture.iterdir():
    if source.is_file():
        shutil.copyfile(source, root / source.name)
print(f"Evidence: {root}", flush=True)
with (root / "compile.log").open("w") as log:
    result = subprocess.run(
        [
            str(pathlib.Path(args.compiler).resolve()),
            "compile",
            "--path",
            str(root / "values-and-order.in"),
            "--entry",
            "main",
            "--target",
            "native",
            "--target-triple",
            "thumbv8m.main-none-eabi",
            "--linkage",
            "static-lib",
            "--out",
            str(root / "values.o"),
            "--json",
        ],
        stdout=subprocess.PIPE,
        stderr=log,
        text=True,
        check=True,
        timeout=120,
    )
    log.write(result.stdout)
    report = json.loads(result.stdout)
    assert report["success"] and not report.get("degradations"), report
with (root / "build.log").open("w") as log:
    subprocess.run(
        ["zig", "build"],
        cwd=root,
        stdout=log,
        stderr=subprocess.STDOUT,
        check=True,
        timeout=120,
    )
with (root / "serial.log").open("w") as log:
    proc = subprocess.Popen(
        [
            "qemu-system-arm",
            "-machine",
            "mps2-an521",
            "-cpu",
            "cortex-m33",
            "-nographic",
            "-kernel",
            str(root / "zig-out/bin/stack-args"),
        ],
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            text = (root / "serial.log").read_text(errors="replace")
            if "stack arguments" in text or proc.poll() is not None:
                break
            try:
                proc.wait(timeout=0.005)
            except subprocess.TimeoutExpired:
                pass
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
text = (root / "serial.log").read_text(errors="replace")
assert "PASS stack arguments" in text and "FAIL stack arguments" not in text, text
print(
    "PASS: 5/6/7 argument values, effect order, nested/repeated calls, "
    "word edges, and callee SP alignment"
)
