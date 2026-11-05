#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIFFERENTIAL_SCRIPT="${DIFFERENTIAL_SCRIPT:-$ROOT/scripts/check-backend-differential.sh}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/in" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  execute)
    if [[ "$CASE" == unobservable ]]; then
      echo '[jit] Execution completed with result: String("value")' >&2
    else
      echo '[jit] Execution completed with result: Int(263)' >&2
      echo 'in: program exited with status 7' >&2
      exit 1
    fi
    ;;
  build)
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == --out ]]; then out="$2"; break; fi
      shift
    done
    case "$CASE" in unobservable) code=0;; truncated) code=7;; *) code=8;; esac
    printf '#!/usr/bin/env bash\nexit %s\n' "$code" > "$out"
    chmod +x "$out"
    ;;
esac
SH
chmod +x "$tmp/in"

for case in unobservable truncated mismatch; do
  CASE="$case" IN_BIN="$tmp/in" "$DIFFERENTIAL_SCRIPT" >"$tmp/$case.out" 2>&1 || true
done

grep -Eq '^  agree +0$' "$tmp/unobservable.out"
grep -Eq '^  result-unobservable +[1-9][0-9]*$' "$tmp/unobservable.out"
grep -Eq 'refused to claim agreement for [1-9][0-9]* unobservable results' "$tmp/unobservable.out"
grep -Eq '^  agree +0$' "$tmp/truncated.out"
grep -Eq '^  result-unobservable +[1-9][0-9]*$' "$tmp/truncated.out"
grep -Eq '^  agree +0$' "$tmp/mismatch.out"
grep -Eq '^  disagree +[1-9][0-9]*$' "$tmp/mismatch.out"
echo 'backend differential oracle controls passed'
