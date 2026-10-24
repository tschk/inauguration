#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

IN_CMD=("${IN_BIN:-in}")
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/check-coverage.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

cat > "$tmp_dir/clean.in" <<'EOF'
fn add(a: Int, b: Int) -> Int { return a + b; }

fn main() -> Int { return add(1, 2); }
EOF

cat > "$tmp_dir/degraded.in" <<'EOF'
fn nothing() -> void { return; }

fn bad() -> Int {
  let v: [[Int]] = [[1]];
  let i = 0;
  while i < 1 { return v[i][i]; }
  return 1;
}

fn main() -> Int { return bad(); }
EOF

echo 'coverage ok: clean program is fully lowered'
"${IN_CMD[@]}" coverage --path "$tmp_dir/clean.in" --json > "$tmp_dir/clean.json"
python3 - "$tmp_dir/clean.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["status"] == "fully-lowered", data["status"]
assert data["functions_not_lowered"] == 0, data
assert data["degradations"] == [], data
assert data["unresolved_calls"] == 0, data
PY

if [[ "$(uname -s)" == "Linux" && "$(uname -m)" == "x86_64" ]]; then
  echo 'coverage ok: Linux strict lowering reports the same refusal as the build'
  "${IN_CMD[@]}" coverage --path "$tmp_dir/degraded.in" --json > "$tmp_dir/degraded.json"
  python3 - "$tmp_dir/degraded.json" <<'PYREPORT'
import json,sys
report=json.load(open(sys.argv[1]))
assert report["status"]=="rejected",report
assert report["reason_code"]=="coverage-lowering-failed",report
assert "nested indexing needs static array storage" in report["reason"],report
assert report["functions_analyzed"]==0 and report["functions_lowered"]==0,report
assert report["degradations"]==[],report
PYREPORT
  if "${IN_CMD[@]}" build --path "$tmp_dir/degraded.in" --out "$tmp_dir/degraded" > "$tmp_dir/build.out" 2> "$tmp_dir/build.err"; then
    echo 'strict Linux build unexpectedly accepted the rejected coverage input' >&2
    exit 1
  fi
  grep -q 'nested indexing needs static array storage' "$tmp_dir/build.err"
  [[ ! -e "$tmp_dir/degraded" ]]
  echo 'coverage ok: Linux fully lowered artifact executes its real result'
  "${IN_CMD[@]}" build --path "$tmp_dir/clean.in" --out "$tmp_dir/clean" > "$tmp_dir/build.out"
  status=0
  "$tmp_dir/clean" || status=$?
  [[ "$status" -eq 3 ]]
else
echo 'coverage ok: unlowered function is named with a code and marked reachable'
"${IN_CMD[@]}" coverage --path "$tmp_dir/degraded.in" --json > "$tmp_dir/degraded.json"
python3 - "$tmp_dir/degraded.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["status"] == "degraded", data["status"]
assert data["functions_not_lowered"] == 1, data
codes = {d["code"] for d in data["degradations"]}
assert "IN3001" in codes, codes
assert any(d["reachable"] for d in data["degradations"]), data["degradations"]
assert data["blockers"], data
PY

echo 'coverage ok: the reported degradation is what the artifact does at runtime'
"${IN_CMD[@]}" build --path "$tmp_dir/degraded.in" --out "$tmp_dir/degraded" >/dev/null 2>&1
status=0
"$tmp_dir/degraded" > "$tmp_dir/run.out" 2> "$tmp_dir/run.err" || status=$?
# 70 is inrt::INRT_TRAP_EXIT_CODE: the status a trapped program exits with.
if [ "$status" -ne 70 ]; then
  echo "expected the degraded artifact to trap with status 70, got $status" >&2
  cat "$tmp_dir/run.err" >&2
  exit 1
fi
grep -q 'IN3001' "$tmp_dir/run.err"

fi

echo 'coverage ok: a source that cannot be parsed reports no counts'
printf 'fn main() -> Int { return 1;\n' > "$tmp_dir/broken.in"
"${IN_CMD[@]}" coverage --path "$tmp_dir/broken.in" --json > "$tmp_dir/broken.json"
python3 - "$tmp_dir/broken.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
assert data["status"] == "rejected", data["status"]
assert data["reason_code"], data
assert data["functions_analyzed"] == 0, data
PY

echo 'coverage ok: lowerer codes are explainable'
"${IN_CMD[@]}" explain IN3001 >/dev/null
"${IN_CMD[@]}" explain IN3002 >/dev/null

echo 'coverage checks passed'
