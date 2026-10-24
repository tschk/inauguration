#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "target-matrix check: native host target contract"
(
  cd in-cli
  cargo test -q native_emit::lower::host_supports_native_subset -- --nocapture
  cargo test -q target::tests::registry_lists_rust_style_compiler_targets
)
python3 - <<'PY'
from pathlib import Path
root = Path(".")
triple = "aarch64-apple-darwin"
lower = (root / "in-cli/src/native_emit/lower.rs").read_text()
if "TARGET_TRIPLE" not in lower or triple not in lower:
    raise SystemExit("missing host target triple contract")
target = (root / "in-cli/src/target.rs").read_text()
for triple in [
    "x86_64-unknown-none",
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "armv7-unknown-linux-gnueabihf",
    "thumbv8m.main-none-eabi",
    "x86_64-pc-windows-msvc",
    "wasm32-unknown-unknown",
    "riscv64gc-unknown-none-elf",
]:
    if triple not in target:
        raise SystemExit(f"missing in target equivalent: {triple}")
PY
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
cat > "$tmpdir/object.in" <<'SRC'
fn answer() -> Int { return 42; }
fn main() -> void { return; }
SRC
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple x86_64-unknown-linux-gnu \
  --linkage static-lib \
  --entry answer \
  --out "$tmpdir/object.o" \
  --json > "$tmpdir/object.json"
python3 - "$tmpdir/object.o" "$tmpdir/object.json" <<'PY'
from pathlib import Path
import json
import sys
data = Path(sys.argv[1]).read_bytes()
report = json.loads(Path(sys.argv[2]).read_text())
if data[:4] != b"\x7fELF":
    raise SystemExit("x86_64 object missing ELF magic")
if int.from_bytes(data[16:18], "little") != 1:
    raise SystemExit("x86_64 object is not ET_REL")
if int.from_bytes(data[18:20], "little") != 62:
    raise SystemExit("x86_64 object is not EM_X86_64")
for needle in [b".text", b".symtab", b".strtab", b".shstrtab", b"answer"]:
    if needle not in data:
        raise SystemExit(f"x86_64 object missing {needle!r}")
if report.get("reason_code") != "native-object-subset":
    raise SystemExit("x86_64 object report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple aarch64-unknown-linux-gnu \
  --linkage static-lib \
  --entry answer \
  --out "$tmpdir/aarch64.o" \
  --json > "$tmpdir/aarch64.json"
python3 - "$tmpdir/aarch64.o" "$tmpdir/aarch64.json" <<'PY'
from pathlib import Path
import json
import sys
data = Path(sys.argv[1]).read_bytes()
report = json.loads(Path(sys.argv[2]).read_text())
if data[:4] != b"\x7fELF":
    raise SystemExit("aarch64 object missing ELF magic")
if data[4] != 2:
    raise SystemExit("aarch64 object is not ELFCLASS64")
if int.from_bytes(data[16:18], "little") != 1:
    raise SystemExit("aarch64 object is not ET_REL")
if int.from_bytes(data[18:20], "little") != 183:
    raise SystemExit("aarch64 object is not EM_AARCH64")
for needle in [b".text", b".symtab", b".strtab", b".shstrtab", b"answer"]:
    if needle not in data:
        raise SystemExit(f"aarch64 object missing {needle!r}")
if report.get("reason_code") != "native-object-subset":
    raise SystemExit("aarch64 object report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple armv7-unknown-linux-gnueabihf \
  --linkage static-lib \
  --entry answer \
  --out "$tmpdir/arm32.o" \
  --json > "$tmpdir/arm32.json"
python3 - "$tmpdir/arm32.o" "$tmpdir/arm32.json" <<'PY'
from pathlib import Path
import json
import sys
data = Path(sys.argv[1]).read_bytes()
report = json.loads(Path(sys.argv[2]).read_text())
if data[:4] != b"\x7fELF":
    raise SystemExit("arm32 object missing ELF magic")
if data[4] != 1:
    raise SystemExit("arm32 object is not ELFCLASS32")
if int.from_bytes(data[16:18], "little") != 1:
    raise SystemExit("arm32 object is not ET_REL")
if int.from_bytes(data[18:20], "little") != 40:
    raise SystemExit("arm32 object is not EM_ARM")
for needle in [b".text", b".symtab", b".strtab", b".shstrtab", b"answer"]:
    if needle not in data:
        raise SystemExit(f"arm32 object missing {needle!r}")
if report.get("reason_code") != "native-object-subset":
    raise SystemExit("arm32 object report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple aarch64-apple-darwin \
  --linkage static-lib \
  --entry answer \
  --out "$tmpdir/libanswer.a" \
  --json > "$tmpdir/macho.json"
python3 - "$tmpdir/libanswer.a" "$tmpdir/macho.json" <<'PY'
from pathlib import Path
import json
import sys
data = Path(sys.argv[1]).read_bytes()
report = json.loads(Path(sys.argv[2]).read_text())
if data[:8] != b"!<arch>\n":
    raise SystemExit("mach-o staticlib missing ar magic")
offset = 8
while offset + 60 <= len(data):
    header = data[offset:offset + 60]
    if header[58:60] != b"`\n":
        raise SystemExit("mach-o archive member has invalid header")
    size = int(header[48:58])
    payload = offset + 60
    name = header[:16].decode("ascii").strip()
    if name.startswith("#1/"):
        payload += int(name[3:])
    if data[payload:payload + 4] == bytes.fromhex("cffaedfe"):
        offset = payload
        break
    offset += 60 + size + size % 2
else:
    raise SystemExit("mach-o archive has no Mach-O member")
if int.from_bytes(data[offset:offset + 4], "little") != 0xFEEDFACF:
    raise SystemExit("mach-o staticlib missing Mach-O magic")
if int.from_bytes(data[offset + 4:offset + 8], "little", signed=True) != 0x0100000C:
    raise SystemExit("mach-o staticlib missing ARM64 CPU type")
if int.from_bytes(data[offset + 12:offset + 16], "little") != 1:
    raise SystemExit("mach-o staticlib member is not MH_OBJECT")
if b"_answer" not in data:
    raise SystemExit("mach-o staticlib missing _answer export")
if report.get("reason_code") != "native-object-subset":
    raise SystemExit("mach-o staticlib report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple x86_64-unknown-linux-gnu \
  --linkage executable \
  --entry answer \
  --out "$tmpdir/x86-exe" \
  --json > "$tmpdir/x86-exe.json"
python3 - "$tmpdir/x86-exe" "$tmpdir/x86-exe.json" <<'PY'
from pathlib import Path
import json
import sys
data = Path(sys.argv[1]).read_bytes()
report = json.loads(Path(sys.argv[2]).read_text())
if data[:4] != b"\x7fELF":
    raise SystemExit("x86_64 executable missing ELF magic")
if int.from_bytes(data[16:18], "little") != 2:
    raise SystemExit("x86_64 executable is not ET_EXEC")
if int.from_bytes(data[18:20], "little") != 62:
    raise SystemExit("x86_64 executable is not EM_X86_64")
# The executable now calls the lowered entry rather than embedding its result.
entry = int.from_bytes(data[24:32], "little")
text_offset = int.from_bytes(data[72:80], "little")
text_address = int.from_bytes(data[80:88], "little")
start = text_offset + entry - text_address
if data[start] != 0xe8 or data[start+5:start+15] != bytes([0x48,0x89,0xc7,0xb8,60,0,0,0,0x0f,0x05]):
    raise SystemExit("x86_64 executable missing entry call and result-preserving syscall exit")
if int.from_bytes(data[68:72], "little") != 5 or int.from_bytes(data[124:128], "little") != 6:
    raise SystemExit("x86_64 executable must separate RX code and RW data")
import platform, subprocess
if platform.system() == "Linux" and platform.machine() == "x86_64":
    if subprocess.run([sys.argv[1]]).returncode != 42:
        raise SystemExit("x86_64 executable did not execute its entry and exit 42")
if report.get("reason_code") != "native-x86_64-linux-exit-subset":
    raise SystemExit("x86_64 executable report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple x86_64-pc-windows-msvc \
  --linkage executable \
  --entry answer \
  --out "$tmpdir/answer.exe" \
  --json > "$tmpdir/windows-exe.json"
python3 - "$tmpdir/answer.exe" "$tmpdir/windows-exe.json" <<'PY'
from pathlib import Path
import json
import sys
data = Path(sys.argv[1]).read_bytes()
report = json.loads(Path(sys.argv[2]).read_text())
if data[:2] != b"MZ":
    raise SystemExit("windows exe missing MZ magic")
pe_off = int.from_bytes(data[0x3C:0x40], "little")
if data[pe_off:pe_off + 4] != b"PE\0\0":
    raise SystemExit("windows exe missing PE signature")
if int.from_bytes(data[pe_off + 4:pe_off + 6], "little") != 0x8664:
    raise SystemExit("windows exe is not AMD64")
characteristics = int.from_bytes(data[pe_off + 22:pe_off + 24], "little")
if characteristics & 0x2000:
    raise SystemExit("windows exe incorrectly marked DLL")
if b"KERNEL32.dll" not in data:
    raise SystemExit("windows exe missing kernel32 import")
if b"ExitProcess" not in data:
    raise SystemExit("windows exe missing ExitProcess import")
if report.get("reason_code") != "native-x86_64-windows-exe-subset":
    raise SystemExit("windows exe report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple aarch64-apple-darwin \
  --linkage executable \
  --entry answer \
  --out "$tmpdir/Answer.app" \
  --json > "$tmpdir/app.json"
python3 - "$tmpdir/Answer.app" "$tmpdir/app.json" <<'PY'
from pathlib import Path
import json
import sys
bundle = Path(sys.argv[1])
report = json.loads(Path(sys.argv[2]).read_text())
exe = bundle / "Contents" / "MacOS" / "Answer"
plist = bundle / "Contents" / "Info.plist"
if not exe.exists():
    raise SystemExit("app bundle missing executable")
if not plist.exists():
    raise SystemExit("app bundle missing Info.plist")
if exe.read_bytes()[:4] != (0xFEEDFACF).to_bytes(4, "little"):
    raise SystemExit("app executable missing Mach-O magic")
if report.get("reason_code") != "native-aarch64-darwin-app-subset":
    raise SystemExit("app bundle report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple x86_64-unknown-linux-gnu \
  --linkage executable \
  --entry answer \
  --out "$tmpdir/Answer.AppDir" \
  --json > "$tmpdir/appdir.json"
python3 - "$tmpdir/Answer.AppDir" "$tmpdir/appdir.json" <<'PY'
from pathlib import Path
import json
import sys
appdir = Path(sys.argv[1])
report = json.loads(Path(sys.argv[2]).read_text())
apprun = appdir / "AppRun"
desktop = appdir / "answer.desktop"
if not apprun.exists():
    raise SystemExit("AppDir missing AppRun")
if not desktop.exists():
    raise SystemExit("AppDir missing desktop file")
if apprun.read_bytes()[:4] != b"\x7fELF":
    raise SystemExit("AppDir AppRun missing ELF magic")
if report.get("reason_code") != "native-x86_64-linux-appdir-subset":
    raise SystemExit("AppDir report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple x86_64-unknown-linux-gnu \
  --linkage executable \
  --entry answer \
  --out "$tmpdir/Answer.AppImage" \
  --json > "$tmpdir/appimage.json"
python3 - "$tmpdir/Answer.AppImage" "$tmpdir/appimage.json" <<'PY'
from pathlib import Path
import json
import sys
report = json.loads(Path(sys.argv[2]).read_text())
if Path(sys.argv[1]).exists():
    raise SystemExit("AppImage fail-closed path unexpectedly wrote artifact")
if report.get("success") is not False:
    raise SystemExit("AppImage fail-closed report unexpectedly succeeded")
if report.get("reason_code") != "native-package-not-implemented":
    raise SystemExit("AppImage fail-closed report has wrong reason_code")
PY
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple wasm32-unknown-unknown \
  --linkage static-lib \
  --entry answer \
  --out "$tmpdir/object.wasm" \
  --json > "$tmpdir/wasm.json"
python3 - "$tmpdir/object.wasm" "$tmpdir/wasm.json" <<'PY'
from pathlib import Path
import json
import sys
data = Path(sys.argv[1]).read_bytes()
report = json.loads(Path(sys.argv[2]).read_text())
if data[:4] != b"\0asm":
    raise SystemExit("wasm32 module missing wasm magic")
if b"answer" not in data:
    raise SystemExit("wasm32 module missing answer export")
if report.get("reason_code") != "native-object-subset":
    raise SystemExit("wasm32 module report has wrong reason_code")
PY
if command -v bun >/dev/null 2>&1; then
  bun -e 'const {instance} = await WebAssembly.instantiate(await Bun.file(process.argv.at(-1)).arrayBuffer()); if (instance.exports.answer() !== 42) throw new Error("wasm answer did not return 42");' "$tmpdir/object.wasm"
else
  echo "wasm runtime check: skip (Bun unavailable)"
fi
env in compile \
  --path "$tmpdir/object.in" \
  --target native \
  --target-triple thumbv8m.main-none-eabi \
  --linkage static-lib \
  --entry answer \
  --out "$tmpdir/thumbv8m.o" \
  --json > "$tmpdir/thumbv8m.json"
python3 - "$tmpdir/thumbv8m.o" "$tmpdir/thumbv8m.json" <<'PY'
from pathlib import Path
import json
import sys
data = Path(sys.argv[1]).read_bytes()
report = json.loads(Path(sys.argv[2]).read_text())
if data[:4] != b"\x7fELF":
    raise SystemExit("thumbv8m object missing ELF magic")
if data[4] != 1:
    raise SystemExit("thumbv8m object is not ELFCLASS32")
if int.from_bytes(data[16:18], "little") != 1:
    raise SystemExit("thumbv8m object is not ET_REL")
if int.from_bytes(data[18:20], "little") != 40:
    raise SystemExit("thumbv8m object is not EM_ARM")
for needle in [b".text", b".symtab", b".strtab", b".shstrtab", b"answer"]:
    if needle not in data:
        raise SystemExit(f"thumbv8m object missing {needle!r}")
if report.get("reason_code") != "native-thumbv8m-main-none-eabi-freestanding":
    raise SystemExit("thumbv8m object report has wrong reason_code")
PY
echo "target-matrix checks passed"
