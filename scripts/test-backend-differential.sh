#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIFFERENTIAL_SCRIPT="${DIFFERENTIAL_SCRIPT:-$ROOT/scripts/check-backend-differential.sh}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/repo/scripts" "$tmp/repo/conformance"
cp "$DIFFERENTIAL_SCRIPT" "$tmp/repo/scripts/check-backend-differential.sh"
echo '# @expect result: 263' > "$tmp/repo/conformance/example.in"
echo 'agree conformance/example.in' > "$tmp/repo/conformance/backend-differential.txt"

cat > "$tmp/in" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  execute)
    [[ " $* " == *' --result-channel '* ]]
    case "$CASE" in
      jit-missing) ;;
      jit-malformed) printf 'INR1\002\002\000\000\000\000\000\000\000' >&3 ;;
      *) printf 'INR1\001\007\001\000\000\000\000\000\000' >&3 ;;
    esac
    [[ "$CASE" != jit-status-mismatch ]] || exit 0
    echo 'in: program exited with status 7' >&2
    exit 1
    ;;
  build)
    [[ " $* " == *' --result-channel '* ]]
    [[ "$CASE" != refused ]] || exit 1
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == --out ]]; then out="$2"; break; fi
      shift
    done
    cat > "$out" <<'PROGRAM'
#!/usr/bin/env bash
case "$CASE" in
  native-missing) ;;
  native-malformed) printf 'INR1\001' >&3 ;;
  full-width) printf 'INR1\001\007\002\000\000\000\000\000\000' >&3 ;;
  *) printf 'INR1\001\007\001\000\000\000\000\000\000' >&3 ;;
esac
case "$CASE" in
  crash) kill -TERM $$ ;;
  exit-mismatch) exit 8 ;;
  stdout-mismatch) echo different ;;
esac
exit 7
PROGRAM
    chmod +x "$out"
    ;;
esac
SH
chmod +x "$tmp/in"

check() {
  local case_name="$1" category="$2" update="${3:-}"
  local status=0
  CASE="$case_name" IN_BIN="$tmp/in" "$tmp/repo/scripts/check-backend-differential.sh" $update >"$tmp/output" 2>&1 || status=$?
  grep -Eq "^  $category +1$" "$tmp/output"
  if [[ "$case_name" == agree ]]; then
    [[ $status -eq 0 ]]
  else
    [[ $status -ne 0 ]]
  fi
}

check agree agree
check full-width disagree
check exit-mismatch disagree
check jit-status-mismatch disagree
check stdout-mismatch disagree
check crash native-crash --update-baseline
grep -qx 'agree conformance/example.in' "$tmp/repo/conformance/backend-differential.txt"
for case_name in jit-missing jit-malformed native-missing native-malformed; do
  category="${case_name/-/-frame-}"
  check "$case_name" "$category" --update-baseline
  grep -qx 'agree conformance/example.in' "$tmp/repo/conformance/backend-differential.txt"
done
check refused native-refused --update-baseline
grep -qx 'agree conformance/example.in' "$tmp/repo/conformance/backend-differential.txt"
echo 'backend differential oracle controls passed'
