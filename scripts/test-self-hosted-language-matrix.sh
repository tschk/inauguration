#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/matrix-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT

cat > "$tmp/in" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  build) [[ "${FAIL_BUILD:-0}" != 1 ]] ;;
  graph) printf '{}\n' ;;
  agent) printf '{"diagnostics":[]}\n' ;;
  backend) printf '{"external_invocations":[],"owned":true}\n' ;;
  execute) [[ "${FAIL_JIT:-0}" != 1 ]] ;;
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
echo 'matrix exit status reflects build, JSON, and JIT failures'
