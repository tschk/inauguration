import subprocess
import json

def run_command(command):
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, shell=True)
    out, err = process.communicate()
    if process.returncode != 0:
        raise Exception(f"Command '{command}' failed with error: {err.decode('utf-8')}")
    return out.decode('utf-8')

# Ensure we are on a clean branch
run_command("git checkout -b fix-jit-emission-tests")
run_command("git add in-cli/src/compiler/mir_emit.rs")
run_command('git commit -m "🧪 Add tests for JIT emission in mir_emit.rs"')

title = "🧪 Add tests for JIT emission in mir_emit.rs"
description = """🎯 **What:** Added tests for the `emit_jit` function in `mir_emit.rs` to ensure proper JIT emission of MIR modules.
📊 **Coverage:** Added basic test for an empty module and a simple function.
✨ **Result:** Improved test coverage and reliability of JIT emission."""

print(f"Submitting with title: {title}")
print(f"Description:\n{description}")
