#!/usr/bin/env python3
"""Inject self-host-vs-native.json into docs/benchmarks/self-host-vs-native.md."""
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MD = ROOT / "docs/benchmarks/self-host-vs-native.md"
JSON = ROOT / "docs/benchmarks/self-host-vs-native.json"
START = "<!-- BENCH:SELF_HOST_START -->"
END = "<!-- BENCH:SELF_HOST_END -->"
CMP_START = "<!-- BENCH:RUSTC_CMP_START -->"
CMP_END = "<!-- BENCH:RUSTC_CMP_END -->"


def main() -> int:
    if not JSON.is_file():
        print(f"missing {JSON}", file=sys.stderr)
        return 1
    data = json.loads(JSON.read_text())
    sh = data["self_host_parse"]
    nat = data["native_self_build"]
    block = f"""| Field | Value |
|-------|------:|
| Generated (UTC) | {data.get("generated_at_utc", "?")} |
| `in` version | {data.get("in_version", "?")} |
| Host / CPU | {data.get("host_os", "?")} / {data.get("cpu", "?")} |
| Functions parsed | {sh.get("functions_parsed", 0):,} |
| Functions typed | {sh.get("functions_typed", 0):,} |
| Call edges | {sh.get("call_edges", 0):,} |
| `in build` wall ms (avg) | {sh.get("wall_ms_avg", 0):.1f} |
| JIT lowering µs (last run) | {sh.get("jit_compile_us", 0):,} |
| Front stats OK | {sh.get("front_ok", False)} |
| Native `--out` | {nat.get("status", "?")} — {str(nat.get("note", ""))[:100]} |"""

    cmp = data.get("comparison") or {}
    rustc = data.get("rustc_release") or {}
    ratio = cmp.get("compile_speed_ratio_in_over_rustc")
    in_w = float(cmp.get("in_front_wall_ms_avg") or 0)
    rustc_w = float(cmp.get("rustc_release_wall_ms_avg") or 0)
    if ratio is not None and rustc_w > 0:
        faster = "in front" if ratio < 1 else "Cargo release"
        speed_note = f"{ratio:.3f}× wall (in ÷ rustc); **{faster}** on this incremental compile"
    else:
        speed_note = "n/a"
    br = cmp.get("binary_size_ratio")
    br_s = f"{br:.3f}×" if br is not None else "1.000×"
    exec_d = cmp.get("cold_exec_version_ms_avg") or {}
    cmp_block = f"""| Metric | `in` owned front (`in build` on `main.rs`) | `rustc` / Cargo release (full crate) |
|--------|----------------------------------------:|----------------------------------:|
| Compile wall avg (ms) | {in_w:.1f} | {rustc_w:.1f} |
| Relative compile time | {speed_note} | baseline (incremental `touch main.rs`) |
| Shipped `in` binary | {cmp.get("binary_human", "?")} ({cmp.get("binary_bytes_in", 0):,} B) | {rustc.get("binary_human", "?")} ({cmp.get("binary_bytes_rustc", 0):,} B) |
| Binary size ratio (in ÷ rustc) | {br_s} | same artifact until native self-link |
| Cold `--version` startup (ms) | {exec_d.get("in", 0):.2f} | {exec_d.get("rustc_binary", 0):.2f} |

**Notes:** Compares **front-only** parse/type/JIT attempt on `in-cli/src/main.rs` vs **Cargo** linking the whole `inauguration` crate (not a clean `cargo clean` build). Binary row is today's single `target/release/in`. Startup row is process launch, not compiler throughput."""

    text = MD.read_text()
    if START not in text or END not in text:
        print("markers missing in md", file=sys.stderr)
        return 1
    pre, rest = text.split(START, 1)
    _, post = rest.split(END, 1)
    text = f"{pre}{START}\n{block}\n{END}{post}"
    if CMP_START in text and CMP_END in text:
        pre2, rest2 = text.split(CMP_START, 1)
        _, post2 = rest2.split(CMP_END, 1)
        text = f"{pre2}{CMP_START}\n{cmp_block}\n{CMP_END}{post2}"
    MD.write_text(text)
    print(MD)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())