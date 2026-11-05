#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/matrix-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/in" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  build)
    [[ "${FAIL_BUILD:-0}" != 1 ]] || exit 1
    [[ -z "${FAIL_BUILD_FILE:-}" || "$*" != *"$FAIL_BUILD_FILE"* ]] || exit 1
    [[ -z "${FAIL_BUILD_EXTRA:-}" || "$*" != *"$FAIL_BUILD_EXTRA"* ]] || exit 1
    ;;
  graph)
    [[ -z "${FAIL_GRAPH_FILE:-}" || "$*" != *"$FAIL_GRAPH_FILE"* ]] || exit 1
    printf '{}\n' ;;
  agent) printf '{"diagnostics":[]}\n' ;;
  backend) printf '{"external_invocations":[],"owned":true}\n' ;;
  execute)
    [[ "${FAIL_JIT:-0}" != 1 ]] || exit 1
    [[ -z "${FAIL_JIT_FILE:-}" || "$*" != *"$FAIL_JIT_FILE"* ]] || exit 1
    ;;
esac
EOF
chmod +x "$tmp/in"

IN_BIN="$tmp/in" bash "$root/scripts/check-self-hosted-language-matrix.sh" > "$tmp/pass"
grep -q '^FAIL:  0$' "$tmp/pass"
for kind in FAIL_BUILD FAIL_JIT; do
  if env "$kind=1" IN_BIN="$tmp/in" bash "$root/scripts/check-self-hosted-language-matrix.sh" > "$tmp/fail"; then
    echo "matrix returned success with $kind" >&2
    exit 1
  fi
done

if IN_BIN=/usr/bin/true bash "$root/scripts/check-self-hosted-language-matrix.sh" > "$tmp/empty" 2>&1; then
  echo 'matrix returned success for empty JSON output' >&2
  exit 1
fi

printf 'build fsharp\njit vb\n' > "$tmp/known"
env IN_BIN="$tmp/in" IN_MATRIX_KNOWN_FAILURES="$tmp/known" \
  FAIL_BUILD_FILE=sample.fs FAIL_JIT_FILE=sample.vb \
  bash "$root/scripts/check-self-hosted-language-matrix.sh" > "$tmp/known-pass"
for scenario in extra changed fixed; do
  case "$scenario" in
    extra) build=sample.fs; jit=sample.vb; extra=sample.c; graph='' ;;
    changed) build=''; jit=sample.vb; extra=''; graph=sample.fs ;;
    fixed) build=sample.fs; jit=''; extra=''; graph='' ;;
  esac
  if env IN_BIN="$tmp/in" IN_MATRIX_KNOWN_FAILURES="$tmp/known" \
      FAIL_BUILD_FILE="$build" FAIL_JIT_FILE="$jit" FAIL_BUILD_EXTRA="$extra" \
      FAIL_GRAPH_FILE="$graph" bash "$root/scripts/check-self-hosted-language-matrix.sh" > "$tmp/$scenario" 2>&1; then
    echo "matrix accepted $scenario known failures" >&2
    exit 1
  fi
  grep -q 'Matrix failures differ from the known-failures list' "$tmp/$scenario"
done

if IN_BIN="$tmp/in" IN_MATRIX_KNOWN_FAILURES="$tmp/missing" \
    bash "$root/scripts/check-self-hosted-language-matrix.sh" > "$tmp/missing-output" 2>&1; then
  echo 'matrix accepted a missing known-failures file' >&2
  exit 1
fi
grep -q 'Missing known-failures file' "$tmp/missing-output"
echo 'matrix rejects unexpected and resolved failures'
