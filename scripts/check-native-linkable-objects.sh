#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Linux" ]] || [[ "$(uname -m)" != "x86_64" ]]; then
  echo "native-linkable-objects: skip x86_64 linker runtime (requires Linux x86_64)"
  exit 0
fi

if ! command -v cc >/dev/null 2>&1; then
  echo "native-linkable-objects: skip x86_64 linker runtime (cc not installed)"
  exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

cat > "$TMP/answer.in" <<'SRC'
fn answer() -> Int { return 42; }
fn main() -> void { return; }
SRC

cat > "$TMP/harness.c" <<'SRC'
extern long answer(void);
int main(void) { return (int)answer(); }
SRC

env in compile --path "$TMP/answer.in" --target native --target-triple x86_64-unknown-linux-gnu --linkage static-lib --entry answer --out "$TMP/answer.o" --json > "$TMP/answer.json"

python3 - "$TMP" <<'PY'
from pathlib import Path
import json
import sys

tmp = Path(sys.argv[1])
data = json.loads((tmp / "answer.json").read_text())
obj = (tmp / "answer.o").read_bytes()

def require(condition, message):
    if not condition:
        raise SystemExit(message)

require(data.get("success") is True, "compile failed")
require(data.get("reason_code") == "native-object-subset", "object reason mismatch")
require(obj[:4] == b"\x7fELF", "object missing ELF magic")
require(obj[4] == 2, "object class mismatch")
require(int.from_bytes(obj[16:18], "little") == 1, "object type mismatch")
require(int.from_bytes(obj[18:20], "little") == 62, "object machine mismatch")
PY

cc "$TMP/harness.c" "$TMP/answer.o" -o "$TMP/harness"
set +e
"$TMP/harness"
code=$?
set -e
if [[ "$code" -ne 42 ]]; then
  echo "native-linkable-objects: linked harness exit mismatch: $code" >&2
  exit 1
fi

cat > "$TMP/global.in" <<'SRC'
var count: Int = 41
extern C fn host_adjust(value: Int) -> Int
fn answer() -> Int { count = host_adjust(count); return count; }
fn main() -> void { return; }
SRC
cat > "$TMP/global-harness.c" <<'SRC'
extern long answer(void);
long host_adjust(long value) { return value + 1; }
int main(void) { return (int)answer(); }
SRC
for profile in default lean harden; do
  env in compile --path "$TMP/global.in" --target native --target-triple x86_64-unknown-linux-gnu --linkage static-lib --entry answer --profile "$profile" --out "$TMP/global.o" --json > "$TMP/global.json"
  python3 - "$TMP/global.json" <<'PYREPORT'
import json
import sys
report = json.load(open(sys.argv[1]))
assert report.get("success") and not report.get("degradations"), report
PYREPORT
  # These owned object bodies use absolute references rather than PIC.
  cc -no-pie "$TMP/global-harness.c" "$TMP/global.o" -o "$TMP/global-harness"
  set +e
  "$TMP/global-harness"
  code=$?
  set -e
  if [[ "$code" -ne 42 ]]; then
    echo "native-linkable-objects: $profile global/external-call exit mismatch: $code" >&2
    exit 1
  fi
done

echo "native linkable object checks passed"
