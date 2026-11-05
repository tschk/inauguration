#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
in_cmd=("${IN_BIN:-in}")
argument_root="$(mktemp -d)"
trap 'rm -rf "$argument_root"' EXIT

echo "check imported ecosystem function execution with an argument"
mkdir -p "$argument_root/vendor/demo"
cat > "$argument_root/inauguration.package" <<'EOF'
name: package-argument
version: 0.1.0
dependencies:
  npm:demo:
    version: path:vendor/demo
    kind: npm
EOF
cat > "$argument_root/vendor/demo/package.json" <<'EOF'
{"name":"demo","version":"1.0.0","main":"index.js"}
EOF
cat > "$argument_root/vendor/demo/index.js" <<'EOF'
exports.greet = name => `hello ${name}`;
EOF
"${in_cmd[@]}" install --path "$argument_root" --offline --json > /dev/null
for name in Ada Lin; do
  cat > "$argument_root/main.in" <<EOF
package package-argument;
module package-argument.main;
use npm:demo;
fn main() -> String { return demo_greet("$name"); }
EOF
  argument_log="$argument_root/execute.log"
  if ! "${in_cmd[@]}" execute --verbose "$argument_root/main.in" > "$argument_log" 2>&1; then
    echo "error: imported ecosystem function did not execute with argument $name:" >&2
    cat "$argument_log" >&2
    echo "Known gap: executable ecosystem imports are unimplemented on this path, awaiting an owner decision. Do not disable this gate to make CI green." >&2
    exit 1
  fi
  if ! grep -Fxq "[jit] Execution completed with result: String(\"hello $name\")" "$argument_log"; then
    echo "error: imported ecosystem function returned the wrong value for $name:" >&2
    cat "$argument_log" >&2
    echo "Do not disable this gate to make CI green." >&2
    exit 1
  fi
done
