#!/usr/bin/env bash
set -euo pipefail

# Self-hosted language matrix gate.
# Runs each mandatory language example through the owned pipeline:
#   in build, in graph --json, in agent, in backend --target native --json
# Plus in execute for languages that lower to Core IR directly.
# Fails if any mandatory language requires an external compiler/runtime.
# Skips languages gracefully when their sample file doesn't exist.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

IN_CMD=("${IN_BIN:-in}")
POLYGLOT_DIR="apps/polyglot-sample"

# Language -> sample file mapping (mandatory languages only).
# Each entry: "lang|sample_path|extra_env|has_jit"
# has_jit: "1" if the language front lowers to .in Core IR directly
declare -a LANGS=(
  "in|sample.in||1"
  "icore|sample.icore||1"
  "c|sample.c||1"
  "cpp|sample.cpp||1"
  "go|sample.go||1"
  "v|sample.v||1"
  "objc|sample.m||0"           # no sample exists → skipped gracefully
  "objcpp|sample.mm||0"         # no sample exists → skipped gracefully
  "java|Sample.java||1"
  "kotlin|Sample.kt||1"
  "cs|Program.cs||0"
  "fsharp|sample.fs||0"
  "swift|sample.swift|IN_NATIVE_SWIFT_SIL=only|0"
  "rust|sample.rs||0"
  "js|sample.js||1"
  "ts|sample.ts||1"
  "python|sample.py||0"
  "ruby|sample.rb||0"
  "scala|sample.scala||0"
  "php|sample.php||0"
  "lua|sample.lua||0"

  "zig|sample.zig||0"
  "dart|sample.dart||0"
  "nim|sample.nim||0"
  "odin|sample.odin||0"
  "hare|sample.ha||0"
  "d|sample.d||0"
  "crystal|sample.cr||0"
  "clojure|sample.clj||0"
  "vb|sample.vb||1"
)

FAILED=0
SKIPPED=0
PASSED=0
JIT_PASSED=0
JIT_FAILED=0
JIT_SKIPPED=0

check_json_no_external() {
  local json_path="$1"
  local label="$2"
  python3 - "$json_path" "$label" <<'PY'
import json, sys
data = json.loads(open(sys.argv[1]).read())
label = sys.argv[2]

def require(condition, message):
    if not condition:
        raise SystemExit(f"[{label}] {message}")

external = data.get("external_invocations")
require(external is not None, "missing external_invocations field")
require(external == [], f"external_invocations was not empty: {external}")

owned = data.get("owned")
if owned is not None:
    require(owned is True, f"owned was not true: {owned}")
PY
}

check_agent_no_error() {
  local json_path="$1"
  local label="$2"
  python3 - "$json_path" "$label" <<'PY'
import json, sys
data = json.loads(open(sys.argv[1]).read())
label = sys.argv[2]

diags = data.get("diagnostics", [])
if diags:
    errors = [d for d in diags if d.get("severity") == "error"]
    if errors:
        raise SystemExit(f"[{label}] agent diagnostics contain errors: {errors}")
PY
}

run_matrix_for_lang() {
  local lang="$1"
  local sample_file="$2"
  local extra_env="$3"
  local has_jit="$4"

  local path="$POLYGLOT_DIR/$sample_file"

  if [[ ! -f "$path" ]]; then
    echo "SKIP [$lang]: sample file $path not found"
    SKIPPED=$((SKIPPED + 1))
    return 0
  fi

  local status="PASS"
  local tmp_json
  tmp_json="$(mktemp "${TMPDIR:-/tmp}/in-matrix-${lang}.XXXXXX")"

  local env_prefix=""
  if [[ -n "$extra_env" ]]; then
    env_prefix="env $extra_env"
  fi

  # 1. in build
  if ! $env_prefix "${IN_CMD[@]}" build --path "$path" --module-id App >/dev/null 2>&1; then
    echo "FAIL [$lang]: in build"
    status="FAIL"
  fi

  # 2. in graph --json
  if [[ "$status" == "PASS" ]]; then
    if $env_prefix "${IN_CMD[@]}" graph --json --path "$path" --module-id App >"$tmp_json" 2>/dev/null; then
      if ! python3 -c "import json; json.load(open('$tmp_json'))" 2>/dev/null; then
        echo "FAIL [$lang]: graph output is not valid JSON"
        status="FAIL"
      fi
    else
      echo "FAIL [$lang]: in graph --json"
      status="FAIL"
    fi
  fi

  # 3. in agent
  if [[ "$status" == "PASS" ]]; then
    if $env_prefix "${IN_CMD[@]}" agent --path "$path" --module-id App >"$tmp_json" 2>/dev/null; then
      check_agent_no_error "$tmp_json" "$lang" || status="FAIL"
    else
      echo "FAIL [$lang]: in agent"
      status="FAIL"
    fi
  fi

  # 4. in backend --target native --json
  if [[ "$status" == "PASS" ]]; then
    if $env_prefix "${IN_CMD[@]}" backend --target native --json --path "$path" --module-id App >"$tmp_json" 2>/dev/null; then
      check_json_no_external "$tmp_json" "$lang" || status="FAIL"
    else
      echo "FAIL [$lang]: in backend --target native --json"
      status="FAIL"
    fi
  fi

  # 5. JIT execution (self-hosted backend, only for languages that lower to Core IR directly)
  if [[ "$has_jit" == "1" ]]; then
    if "${IN_CMD[@]}" execute "$path" --module-id App >/dev/null 2>&1; then
      echo "  jit [$lang]: execute ok"
      JIT_PASSED=$((JIT_PASSED + 1))
    else
      echo "  jit [$lang]: execute failed"
      JIT_FAILED=$((JIT_FAILED + 1))
    fi
  fi

  rm -f "$tmp_json"

  if [[ "$status" == "PASS" ]]; then
    echo "PASS [$lang]: $path"
    PASSED=$((PASSED + 1))
  else
    FAILED=$((FAILED + 1))
  fi

  return 0
}

echo "=== self-hosted language matrix ==="
echo ""

for entry in "${LANGS[@]}"; do
  IFS='|' read -r lang sample_file extra_env has_jit <<<"$entry"
  run_matrix_for_lang "$lang" "$sample_file" "$extra_env" "$has_jit"
done

echo ""
echo "=== matrix summary ==="
echo "PASS:  $PASSED"
echo "SKIP:  $SKIPPED"
echo "FAIL:  $FAILED"
echo ""
echo "JIT execution:"
echo "  passed: $JIT_PASSED"
echo "  failed: $JIT_FAILED"

if [[ $FAILED -gt 0 || $JIT_FAILED -gt 0 ]]; then
  echo ""
  echo "Some language fronts failed the self-hosted language matrix."
  exit 1
fi

echo ""
echo "Self-hosted language matrix completed."
