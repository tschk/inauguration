#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

IN_CMD=("${IN_BIN:-in}")
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/check-jit.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

cat > "$tmp_dir/lib.in" <<'EOF'
fn ready(flag: Bool) -> String {
  if flag == true {
    return "ready";
  } else {
    return "wait";
  }
}
EOF

cat > "$tmp_dir/sample.in" <<'EOF'
import "./lib.in";
fn answer() -> Int { return 42; }
fn main() -> Int {
  print(ready(true));
  return answer();
}
EOF

echo 'jit compile ok: polyglot sample'
# `in execute` reports a nonzero program status as an error, and this sample's
# main returns 42, so a nonzero CLI status is expected here. Capture it instead
# of letting `set -e` abort, then assert on the output and the propagated status.
status=0
output="$("${IN_CMD[@]}" execute --verbose "$tmp_dir/sample.in" --module-id App 2>&1)" || status=$?
printf '%s\n' "$output" | grep -q 'result: Int(42)'
printf '%s\n' "$output" | grep -q '^ready'
printf '%s\n' "$output" | grep -q 'program exited with status 42'
if [ "$status" -eq 0 ]; then
  echo "expected in execute to fail on the program's nonzero status" >&2
  exit 1
fi
