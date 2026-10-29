import subprocess
import json

def run_command(command):
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, shell=True)
    out, err = process.communicate()
    if process.returncode != 0:
        raise Exception(f"Command '{command}' failed with error: {err.decode('utf-8')}")
    return out.decode('utf-8')

# Ensure we are on a clean branch
run_command("git checkout -b fix-jit-emission-tests-2")
run_command("git add in-cli/src/native_emit/x86_64_lower.rs")
run_command('git commit -m "🧹 Fix string pattern matching type inference for aliased strings"')

title = "🧹 Fix string pattern matching type inference for aliased strings"
description = """🎯 **What:** Adjusted `x86_64_lower.rs` to call `.canonical()` on the inferred type of a string pattern scrutinee to handle `Typ::Named` aliases.
💡 **Why:** Fixes a `string pattern requires a String scrutinee` lowering error in `jit_runtime` tests when the scrutinee is an aliased string type instead of the direct `String` type.
✅ **Verification:** Passed `string_patterns_match_independent_payloads_by_content` test locally.
✨ **Result:** Resolved GitHub CI Check Suite Failure."""

print(f"Submitting with title: {title}")
print(f"Description:\n{description}")
