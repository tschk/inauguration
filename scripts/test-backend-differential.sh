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
      jit-void-malformed) printf 'INR1\003\001\000\000\000\000\000\000\000' >&3 ;;
      void|native-void-malformed|void-native-status|void-jit-status) printf 'INR1\003\000\000\000\000\000\000\000\000' >&3 ;;
      jit-float-truncated) printf 'INR1\004\001' >&3 ;;
      jit-float-malformed) printf 'INR1\004\001\000\000\000\000\000\370\177\000' >&3 ;;
      float-zero) printf 'INR1\004\000\000\000\000\000\000\000\000' >&3 ;;
      float*|native-float-truncated|native-float-malformed) printf 'INR1\004\001\000\000\000\000\000\370\177' >&3 ;;
      bool) printf 'INR1\002\001\000\000\000\000\000\000\000' >&3 ;;
      *) printf 'INR1\001\007\001\000\000\000\000\000\000' >&3 ;;
    esac
    case "$CASE" in void|native-void-malformed|void-native-status|float|float-bits|float-zero|float-native-status|*-float-truncated|*-float-malformed) exit 0 ;; esac
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
  native-void-malformed) printf 'INR1\003\001\000\000\000\000\000\000\000' >&3 ;;
  void|void-native-status|void-jit-status) printf 'INR1\003\000\000\000\000\000\000\000\000' >&3 ;;
  native-float-truncated) printf 'INR1\004\001' >&3 ;;
  native-float-malformed) printf 'INR1\004\001\000\000\000\000\000\370\177\000' >&3 ;;
  float-bits) printf 'INR1\004\002\000\000\000\000\000\370\177' >&3 ;;
  float-zero) printf 'INR1\004\000\000\000\000\000\000\000\200' >&3 ;;
  float*|jit-float-truncated|jit-float-malformed) printf 'INR1\004\001\000\000\000\000\000\370\177' >&3 ;;
  bool) printf 'INR1\002\001\000\000\000\000\000\000\000' >&3 ;;
  full-width) printf 'INR1\001\007\002\000\000\000\000\000\000' >&3 ;;
  *) printf 'INR1\001\007\001\000\000\000\000\000\000' >&3 ;;
esac
case "$CASE" in
  crash) kill -TERM $$ ;;
  exit-mismatch) exit 8 ;;
  stdout-mismatch) echo different ;;
  void|void-jit-status) exit 0 ;;
  float|float-bits|float-zero|float-jit-status|*-float-truncated|*-float-malformed) exit 0 ;;
  float-native-status) exit 1 ;;
  void-native-status|bool) exit 1 ;;
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
  if [[ "$category" == agree ]]; then
    [[ $status -eq 0 ]]
  else
    [[ $status -ne 0 ]]
  fi
}

check agree agree
check bool agree
check void agree
check void-native-status disagree
check void-jit-status disagree
check float agree
check float-bits disagree
check float-zero disagree
check float-native-status disagree
check float-jit-status disagree
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
check jit-void-malformed jit-frame-malformed --update-baseline
check native-void-malformed native-frame-malformed --update-baseline
check jit-float-truncated jit-frame-malformed --update-baseline
check native-float-truncated native-frame-malformed --update-baseline
check jit-float-malformed jit-frame-malformed --update-baseline
check native-float-malformed native-frame-malformed --update-baseline
grep -qx 'agree conformance/example.in' "$tmp/repo/conformance/backend-differential.txt"
check refused native-refused --update-baseline
grep -qx 'agree conformance/example.in' "$tmp/repo/conformance/backend-differential.txt"
echo 'backend differential oracle controls passed'
