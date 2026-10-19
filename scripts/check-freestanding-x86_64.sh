#!/usr/bin/env bash
# check-freestanding-x86_64.sh — Verify the freestanding x86_64-unknown-none target.
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
IN="${IN_BIN:-$DIR/../in-cli/target/release/in}"
BUILD_DIR="${BUILD_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/in-freestanding-check.XXXXXX")}"

PASS=0
FAIL=0

check() {
    local label="$1"
    if [ "$2" = "true" ]; then
        echo "  ok: $label"
        PASS=$((PASS + 1))
    else
        echo "  FAIL: $label"
        FAIL=$((FAIL + 1))
    fi
}

rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR"
TRAMPOLINE="$BUILD_DIR/trampoline.bin"
python3 - "$TRAMPOLINE" <<'PY'
import struct, sys
path = sys.argv[1]
buf = bytearray(4096)
magic = 0x1BADB002
flags = 0
checksum = (-(magic + flags)) & 0xffffffff
buf[:12] = struct.pack("<III", magic, flags, checksum)
open(path, "wb").write(buf)
PY

echo "[1/3] Compiling minimal freestanding kernel..."
cat > "$BUILD_DIR/test.in" << 'EOF'
component freestanding_check {
  target "x86_64-unknown-none"
  deterministic true
  capability boot: Multiboot(read)
}

fn kernel_entry(mb_info: Int) -> Int {
  return 42
}
EOF

"$IN" compile \
    --path "$BUILD_DIR/test.in" --entry kernel_entry --emit boot \
    --trampoline "$TRAMPOLINE" \
    --target native --target-triple x86_64-unknown-none --linkage static-lib \
    --out "$BUILD_DIR/test.bin" 2>/dev/null

check "boot image produced" "$([ -f "$BUILD_DIR/test.bin" ] && echo true)"

SIZE=$(stat -f%z "$BUILD_DIR/test.bin" 2>/dev/null || stat -c%s "$BUILD_DIR/test.bin" 2>/dev/null || echo 0)
check "boot image is >= 4096 bytes" "$([ "$SIZE" -ge 4096 ] && echo true)"

echo "[2/3] Checking multiboot header..."
# The multiboot header is at offset 0, magic 0x1BADB002 (little-endian: 02 b0 ad 1b)
MB_MAGIC=$(xxd -p -l4 -s 0 "$BUILD_DIR/test.bin" 2>/dev/null || od -A n -t x1 -N4 "$BUILD_DIR/test.bin" 2>/dev/null | tr -d ' ')
check "multiboot magic present" "$(echo "$MB_MAGIC" | grep -qi "02b0ad1b" && echo true)"

echo "[3/3] Checking kernel code..."
KERNEL_OFFSET=4096
SCI_MAGIC_OFFSET=$((KERNEL_OFFSET + 8))
SCI_MAGIC=$(xxd -p -l8 -s $SCI_MAGIC_OFFSET "$BUILD_DIR/test.bin" 2>/dev/null || od -A n -t x1 -j $SCI_MAGIC_OFFSET -N8 "$BUILD_DIR/test.bin" 2>/dev/null | tr -d ' ')
SCI_CODE_OFFSET=$(python3 - "$BUILD_DIR/test.bin" <<PY
import struct, sys
with open(sys.argv[1], "rb") as f:
    f.seek($KERNEL_OFFSET + 24)
    print(struct.unpack("<Q", f.read(8))[0])
PY
)
check "SCI magic present" "$(echo "$SCI_MAGIC" | grep -qi "5343490000000001" && echo true)"
check "SCI code offset is 0x100" "$([ "$SCI_CODE_OFFSET" -eq 256 ] && echo true)"

META="$BUILD_DIR/test.component-metadata.json"
check "component metadata sidecar produced" "$([ -f "$META" ] && echo true)"
python3 - "$META" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    meta = json.load(f)
for key in ["component", "target", "entry", "capabilities_required", "provenance"]:
    if key not in meta:
        raise SystemExit(f"missing metadata key: {key}")
if meta["component"] != "freestanding_check":
    raise SystemExit("unexpected metadata component")
if meta["target"] != "x86_64-unknown-none":
    raise SystemExit("unexpected metadata target")
if meta["entry"] != "kernel_entry":
    raise SystemExit("unexpected metadata entry")
if meta["provenance"].get("compiler") != "inauguration":
    raise SystemExit("unexpected metadata compiler")
if meta.get("code_size", 0) <= 0:
    raise SystemExit("metadata code_size was not positive")
PY
check "component metadata sidecar has boot keys" "true"

if command -v objdump &>/dev/null; then
    objdump -D -b binary -m i386 -M x86-64,intel "$BUILD_DIR/test.bin" 2>/dev/null | sed -n '1,20p' || true
fi

echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
