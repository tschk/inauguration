#!/usr/bin/env bash
# Cross-backend differential: do the JIT and the native backend compute the same
# answer for the same program?
#
# The conformance runner builds a native artifact but only ever executes the JIT
# path, so a native backend that crashes or disagrees can pass conformance. This
# runs the same fixture through both backends and compares stdout and typed
# result records, checking the native process status independently.
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

  local bin="$tmp_dir/bin"
  rm -f "$bin"
  if ! "${IN_CMD[@]}" build --path "$fixture" --module-id Differential --out "$bin" --result-channel >/dev/null 2>"$tmp_dir/build.err"; then
    printf 'backend differential: native build failed for %s\n' "$fixture" >&2
    cat "$tmp_dir/build.err" >&2
    echo "native-refused"
    return
  fi
  if [[ ! -x "$bin" ]]; then
    echo "native-refused"
    return
  fi

  # A nonzero program result makes the CLI exit with an error; distinguish that
  # diagnostic from an execution failure without treating the CLI status as the result.
  local jit_status=0
  "${IN_CMD[@]}" execute "$fixture" --module-id Differential --verbose --result-channel >"$jit_out" 2>"$jit_err" 3>"$tmp_dir/jit.frame" || jit_status=$?
  if [[ $jit_status -ne 0 ]]; then
    if ! grep -q 'program exited with status [0-9]' "$jit_err"; then
      echo "jit-error"
      return
    fi
  fi

  # `$?` cannot tell "exited with 191" from "killed by signal 63": both read as
  # 191. Ask Python for the real termination so a large exit value is not
  # mistaken for a crash. A negative code means the process died from a signal.
  local native_status
  native_status=$(python3 -c '
import subprocess, sys
with open(sys.argv[2], "wb") as out:
    proc = subprocess.run([sys.argv[1]], stdout=out, stderr=subprocess.DEVNULL, pass_fds=(3,))
print(proc.returncode)
' "$bin" "$tmp_dir/native.out" 3>"$tmp_dir/native.frame")
  if [[ $native_status -lt 0 ]]; then
    echo "native-crash"
    return
  fi

  python3 - "$tmp_dir/jit.frame" "$tmp_dir/native.frame" "$native_status" "$jit_out" "$tmp_dir/native.out" "$jit_status" <<'PY'
import pathlib, sys

def read_frame(path, backend):
    data = pathlib.Path(path).read_bytes()
    if not data:
        return None, backend + '-frame-missing'
    if (len(data) != 13 or data[:4] != b'INR1' or data[4] not in (1, 2, 3)
            or (data[4] == 2 and int.from_bytes(data[5:], 'little', signed=True) not in (0, 1))
            or (data[4] == 3 and data[5:] != bytes(8))):
        return None, backend + '-frame-malformed'
    return data, None

jit, error = read_frame(sys.argv[1], 'jit')
if not error:
    native, error = read_frame(sys.argv[2], 'native')
if error:
    print(error)
elif (jit != native or (int.from_bytes(native[5:], 'little', signed=True) & 255 if native[4] in (1, 2) else 0) != int(sys.argv[3])
      or int(sys.argv[6]) != (1 if jit[4] in (1, 2) and int.from_bytes(jit[5:], 'little', signed=True) & 255 else 0)
      or pathlib.Path(sys.argv[4]).read_bytes() != pathlib.Path(sys.argv[5]).read_bytes()):
    print('disagree')
else:
    print('agree')
PY
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
for category in agree native-refused native-crash disagree jit-error jit-frame-missing jit-frame-malformed native-frame-missing native-frame-malformed; do
  count=$(grep -c "^$category " "$actual" || true)
  echo "  $(printf '%-14s' "$category") $count"
done
agreed=$(grep -c '^agree ' "$actual" || true)
refused=$(grep -c '^native-refused ' "$actual" || true)
invalid=$(grep -Ec '^(jit|native)-frame-(missing|malformed) ' "$actual" || true)
failed=$(grep -Ec '^(jit-error|native-crash) ' "$actual" || true)
echo "backend differential: $agreed/$fixtures agree; $refused native refusals and $invalid missing or malformed frames were not compared"

if (( refused > 0 || invalid > 0 || failed > 0 )); then
  echo "backend differential incomplete: $invalid missing or malformed frames, $refused native refusals, and $failed execution failures" >&2
  echo 'incomplete results cannot pass or update the baseline' >&2
  exit 1
fi

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
