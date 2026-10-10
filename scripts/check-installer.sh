#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/in-cli" "$fixture/tools" "$fixture/install"
cp "$ROOT/install.sh" "$fixture/install.sh"
printf '[package]\nname = "inauguration"\nversion = "9.9.9"\n' > "$fixture/in-cli/Cargo.toml"
cat > "$fixture/tools/cargo" <<'CARGO'
#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  pkgid) echo 'path+file:///fixture#inauguration@9.9.9' ;;
  build)
    mkdir -p "$IN_FAKE_ROOT/in-cli/target/release"
    cp "$IN_FAKE_CLI" "$IN_FAKE_ROOT/in-cli/target/release/in"
    ;;
  install)
    shift
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == --root ]]; then stage="$2"; break; fi
      shift
    done
    mkdir -p "$stage/bin"
    cp "$IN_FAKE_CLI" "$stage/bin/in"
    ;;
  *) exit 1 ;;
esac
CARGO
chmod +x "$fixture/tools/cargo"
previous=$'#!/bin/sh\nprintf "in previous\\n"\n'
for kind in damaged wrong valid; do
  printf '%s' "$previous" > "$fixture/install/in"
  chmod +x "$fixture/install/in"
  case "$kind" in
    damaged) printf 'damaged binary\n' > "$fixture/candidate"; chmod 0644 "$fixture/candidate" ;;
    wrong) printf '#!/bin/sh\nprintf "in 0.0.0\\n"\n' > "$fixture/candidate"; chmod +x "$fixture/candidate" ;;
    valid) printf '#!/bin/sh\nprintf "in 9.9.9\\n"\n' > "$fixture/candidate"; chmod +x "$fixture/candidate" ;;
  esac
  if env IN_USE_RELEASE=0 IN_INSTALL_DIR="$fixture/install" IN_FAKE_ROOT="$fixture" IN_FAKE_CLI="$fixture/candidate" PATH="$fixture/tools:$PATH" bash "$fixture/install.sh" > "$fixture/$kind.log" 2>&1; then
    [[ "$kind" == valid ]] || { cat "$fixture/$kind.log"; echo "installer accepted $kind candidate" >&2; exit 1; }
    [[ "$("$fixture/install/in" --version)" == 'in 9.9.9' ]]
  else
    [[ "$kind" != valid ]] || { cat "$fixture/$kind.log"; exit 1; }
    [[ "$(cat "$fixture/install/in")" == "${previous%$'\n'}" ]] || { echo 'installer damaged the previous CLI' >&2; exit 1; }
  fi
done
echo 'installer candidate verification passed'
