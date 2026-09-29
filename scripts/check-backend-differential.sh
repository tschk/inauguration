#!/usr/bin/env bash
# Cross-backend differential: do the JIT and the native backend compute the same
# answer for the same program?
#
# The conformance runner builds a native artifact but only ever executes the JIT
# path, so a native backend that crashes or disagrees can pass conformance. This
# runs the same fixture through both backends and compares stdout and the
# program's status.
#
# Divergences are pinned in conformance/backend-differential.txt so the set can
# only change deliberately: the check fails when a fixture moves between
# categories in either direction, which keeps a fix from going unnoticed as much
# as it keeps a regression from creeping in.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

IN_CMD=("${IN_BIN:-in}")
BASELINE="$ROOT/conformance/backend-differential.txt"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/check-backend-diff.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

SKIP_FIXTURES=()
if [[ -f "$ROOT/conformance/skipped.txt" ]]; then
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%%#*}"
    line="$(echo "$line" | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"
    [[ -z "$line" ]] && continue
    SKIP_FIXTURES+=("$line")
  done < "$ROOT/conformance/skipped.txt"
fi

is_skipped() {
  local rel="$1" skip
  for skip in ${SKIP_FIXTURES[@]+"${SKIP_FIXTURES[@]}"}; do
    [[ "$rel" == "$skip" ]] && return 0
  done
  return 1
}

classify() {
  local fixture="$1"
  local rel="${fixture#"$ROOT"/}"
  local jit_out="$tmp_dir/jit.out" jit_err="$tmp_dir/jit.err"

  # JIT through the user-facing command. A nonzero program status is reported as
  # an error, so read the status out of the message instead of the exit code.
  local jit_status=0
  "${IN_CMD[@]}" execute "$fixture" --module-id Differential >"$jit_out" 2>"$jit_err" || jit_status=$?
  local jit_code=0
  if [[ $jit_status -ne 0 ]]; then
    jit_code="$(sed -n 's/.*program exited with status \([0-9]*\).*/\1/p' "$jit_err" | head -1)"
    if [[ -z "$jit_code" ]]; then
      echo "jit-error"
      return
    fi
  fi

  local bin="$tmp_dir/bin"
  rm -f "$bin"
  if ! "${IN_CMD[@]}" build --path "$fixture" --out "$bin" >/dev/null 2>"$tmp_dir/build.err"; then
    echo "native-refused"
    return
  fi
  if [[ ! -x "$bin" ]]; then
    echo "native-refused"
    return
  fi

  # `$?` cannot tell "exited with 191" from "killed by signal 63": both read as
  # 191. Ask Python for the real termination so a large exit value is not
  # mistaken for a crash. A negative code means the process died from a signal.
  local native_status
  native_status=$(python3 -c '
import subprocess, sys
with open(sys.argv[2], "wb") as out:
    proc = subprocess.run([sys.argv[1]], stdout=out, stderr=subprocess.DEVNULL)
print(proc.returncode)
' "$bin" "$tmp_dir/native.out")
  if [[ $native_status -lt 0 ]]; then
    echo "native-crash"
    return
  fi

  if [[ "$jit_code" == "$native_status" ]] && cmp -s "$jit_out" "$tmp_dir/native.out"; then
    echo "agree"
  else
    echo "disagree"
  fi
}

actual="$tmp_dir/actual.txt"
: > "$actual"
fixtures=0
for fixture in $(grep -rl '@expect result:' conformance --include='*.in' | sort); do
  rel="${fixture#"$ROOT"/}"
  is_skipped "$rel" && continue
  fixtures=$((fixtures + 1))
  printf '%s %s\n' "$(classify "$ROOT/$rel")" "$rel" >> "$actual"
done

echo "backend differential: $fixtures fixtures compared (JIT vs native artifact)"
for category in agree native-refused native-crash disagree jit-error; do
  count=$(grep -c "^$category " "$actual" || true)
  echo "  $(printf '%-14s' "$category") $count"
done

if [[ ! -f "$BASELINE" ]]; then
  echo "error: missing baseline $BASELINE" >&2
  exit 1
fi

# `--update-baseline` rewrites the pin after a deliberate change to what the two
# backends agree on, so the next run compares against the new set.
if [[ "${1:-}" == "--update-baseline" ]]; then
  {
    echo "# Which fixtures the JIT and the native backend agree on, one per line."
    echo "# Regenerate with: scripts/check-backend-differential.sh --update-baseline"
    sort "$actual"
  } > "$BASELINE"
  echo "backend differential baseline updated: $BASELINE"
  exit 0
fi

# Compare against the pinned baseline in both directions.
baseline_sorted="$tmp_dir/baseline.sorted"
sort "$BASELINE" | grep -v '^#' | grep -v '^$' > "$baseline_sorted" || true
sort "$actual" > "$tmp_dir/actual.sorted"

if ! diff -u "$baseline_sorted" "$tmp_dir/actual.sorted" > "$tmp_dir/diff.txt"; then
  echo "" >&2
  echo "backend differential changed against conformance/backend-differential.txt:" >&2
  sed -n '3,200p' "$tmp_dir/diff.txt" >&2
  echo "" >&2
  echo "fix the divergence, or update the baseline if the change is intended" >&2
  exit 1
fi

echo "backend differential matches the pinned baseline"
