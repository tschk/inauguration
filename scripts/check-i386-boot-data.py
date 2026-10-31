#!/usr/bin/env python3
"""Execute fixed-base Word32 boot images with writable globals in real QEMU."""

import argparse
from pathlib import Path
import shutil
import subprocess
import tempfile


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--compiler", required=True, type=Path)
parser.add_argument("--expect-writable-refusal", action="store_true",
                    help="verify the old compiler refuses data cases without emitting an image")
args = parser.parse_args()
compiler = args.compiler.resolve()
GUEST_SUCCESS = 5
for tool in ("nasm", "qemu-system-i386"):
    if not shutil.which(tool):
        parser.error(f"required installed tool not found: {tool}")


def run_image(image):
    return subprocess.run(
        ["qemu-system-i386", "-kernel", str(image), "-m", "32M",
         "-display", "none", "-serial", "none", "-monitor", "none", "-no-reboot",
         "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"],
        capture_output=True, text=True, timeout=10,
    )


# Multiboot loads the flat image at 1 MiB; the boot emitter places code
# after the 4 KiB trampoline and 256-byte header, and data at 2 MiB.
trampoline = """
BITS 32
org 0x100000
header:
dd 0x1badb002, 0x10000, -(0x1badb002 + 0x10000)
dd header, 0x100000, 0, 0, start
start:
cli
cld
mov esp, 0x90000
mov eax, 0x101100
call eax
cmp eax, 42
sete al
movzx eax, al
inc eax
mov dx, 0xf4
out dx, eax
halt:
hlt
jmp halt
times 4096-($-$$) db 0
"""

cases = {
    "control": "fn main()->Int{return 42;}",
    "wrong-return": "fn main()->Int{return 41;}",
    "scalars": """
var count:Int=3;
var neighbor:Int=7;
fn main()->Int{
    count=count+39;
    if neighbor!=7{return 1;}
    if count!=42{return 2;}
    neighbor=neighbor+1;
    if neighbor!=8{return 3;}
    return count;
}
""",
    "word32": """
var count:Int=2147483647;
var neighbor:Int=7;
fn main()->Int{
    count=count+1;
    if count>=0{return 1;}
    if neighbor!=7{return 2;}
    count=42;
    return count;
}
""",
    "pointers": """
var text:String="hello";
var other:String="world";
fn main()->Int{
    if load8(load32(0x200000))!=104{return 1;}
    if load8(load32(0x200008))!=119{return 2;}
    text=other;
    if load8(load32(0x200000))!=119{return 3;}
    store8(load32(0x200000),120);
    if load8(load32(0x200008))!=120{return 4;}
    return 42;
}
""",
}

with tempfile.TemporaryDirectory(prefix="in-i386-boot-data-") as temporary:
    root = Path(temporary)
    corrupt = root / "corrupt.bin"
    corrupt.write_bytes(b"not a boot image")
    for image in (corrupt, root / "missing.bin"):
        rejected = run_image(image)
        assert rejected.returncode == 1, (image.name, rejected.returncode, rejected.stderr)
        assert rejected.returncode != GUEST_SUCCESS, ("invalid image accepted", image.name)
        print(f"PASS {image.name} rejected (QEMU exit {rejected.returncode})", flush=True)
    (root / "start.asm").write_text(trampoline)
    subprocess.run(
        ["nasm", "-f", "bin", str(root / "start.asm"), "-o", str(root / "start.bin")],
        check=True,
    )
    for triple in ("i386-unknown-none", "i686-unknown-none"):
        for profile in ("default", "lean", "harden"):
            for name, source in cases.items():
                (root / "source.in").write_text(source)
                image = root / "image.bin"
                image.unlink(missing_ok=True)
                compiled = subprocess.run(
                    [str(compiler), "compile", "--path", str(root / "source.in"),
                     "--entry", "main", "--emit", "boot", "--trampoline", str(root / "start.bin"),
                     "--target", "native", "--target-triple", triple,
                     "--linkage", "static-lib", "--profile", profile, "--out", str(image)],
                    capture_output=True, text=True,
                )
                if args.expect_writable_refusal and name in ("scalars", "word32", "pointers"):
                    assert compiled.returncode != 0, (triple, profile, name, "expected refusal")
                    assert "32-bit x86 boot images do not yet relocate writable globals" in compiled.stderr
                    assert not image.exists(), (triple, profile, name, "refusal emitted an image")
                    print(f"PASS {triple} {profile} {name} refused without artifact", flush=True)
                    continue
                assert compiled.returncode == 0, (triple, profile, name, compiled.stdout, compiled.stderr)
                result = run_image(image)
                # isa-debug-exit reports (value << 1) | 1: 5 means success,
                # 3 means the actual compiled main returned something other than 42.
                expected = 3 if name == "wrong-return" else GUEST_SUCCESS
                assert result.returncode == expected, (triple, profile, name, result.returncode, result.stderr)
                print(f"PASS {triple} {profile} {name} (QEMU exit {result.returncode})", flush=True)
