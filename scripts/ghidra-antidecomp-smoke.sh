#!/usr/bin/env bash
# Compare default vs harden emit for anti-decomp smoke metrics.
# Prefer dual-emit (`--out` + `--harden-out`) so one invocation writes both
# artifacts without running harden transforms on the runtime object.
# Separate `--profile default` / `--profile harden` compiles still work.
# Prefer Ghidra analyzeHeadless when available; always emit objdump/nm fallback.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

SAMPLE="${SAMPLE:-examples/compile/antidecomp_sample.in}"
OUT_DIR="${OUT_DIR:-target/in/antidecomp-smoke}"
BENCH_MD="${BENCH_MD:-docs/benchmarks/ghidra-antidecomp-smoke.md}"
SKIP_MD="${SKIP_MD:-docs/benchmarks/ghidra-antidecomp-SKIPPED.md}"
TRIPLE="${TRIPLE:-x86_64-unknown-none}"
ENTRY="${ENTRY:-main}"

mkdir -p "$OUT_DIR" "$(dirname "$BENCH_MD")"

IN_BIN="${IN_BIN:-}"
if [[ -z "$IN_BIN" ]]; then
  if [[ -x "$ROOT/in-cli/target/debug/in" ]]; then
    IN_BIN="$ROOT/in-cli/target/debug/in"
  elif command -v in >/dev/null 2>&1; then
    IN_BIN="$(command -v in)"
  else
    echo "building in-cli..."
    cargo build --manifest-path "$ROOT/in-cli/Cargo.toml" -q
    IN_BIN="$ROOT/in-cli/target/debug/in"
  fi
fi

DEFAULT_OBJ="$OUT_DIR/sample-default.o"
HARDEN_OBJ="$OUT_DIR/sample-harden.o"
echo "==> compile dual-emit (runtime=default → $DEFAULT_OBJ; harden → $HARDEN_OBJ)" >&2
"$IN_BIN" compile \
  --path "$SAMPLE" \
  --target native \
  --target-triple "$TRIPLE" \
  --linkage static-lib \
  --entry "$ENTRY" \
  --out "$DEFAULT_OBJ" \
  --harden-out "$HARDEN_OBJ" >&2
if [[ ! -f "$DEFAULT_OBJ" || ! -f "$HARDEN_OBJ" ]]; then
  echo "dual-emit compile failed (expected $DEFAULT_OBJ and $HARDEN_OBJ)" >&2
  exit 1
fi

metric_file() {
  local obj="$1"
  local label="$2"
  local meta="$OUT_DIR/${label}.metrics.txt"
  {
    echo "file: $obj"
    echo "size_bytes: $(wc -c < "$obj" | tr -d ' ')"
    if command -v nm >/dev/null 2>&1; then
      echo "--- nm ---"
      nm -g "$obj" 2>/dev/null || nm "$obj" 2>/dev/null || true
      echo "named_symbols: $(nm "$obj" 2>/dev/null | grep -c ' [Tt] ' || true)"
      echo "hashed_symbols: $(nm "$obj" 2>/dev/null | grep -c '_H[0-9a-f]' || true)"
    fi
    if command -v objdump >/dev/null 2>&1; then
      echo "--- objdump -d (summary) ---"
      local disasm
      disasm="$(objdump -d "$obj" 2>/dev/null || true)"
      echo "disasm_lines: $(printf '%s\n' "$disasm" | wc -l | tr -d ' ')"
      echo "push_rbx_count: $(printf '%s\n' "$disasm" | grep -c 'push *%rbx' || true)"
      echo "classic_prologue_mov: $(printf '%s\n' "$disasm" | grep -c 'mov *%rsp,%rbp' || true)"
      echo "xor_imm_noise: $(printf '%s\n' "$disasm" | grep -c 'xor' || true)"
    fi
  } > "$meta"
  cat "$meta"
}

echo "==> metrics"
DEFAULT_METRICS="$(metric_file "$DEFAULT_OBJ" default)"
HARDEN_METRICS="$(metric_file "$HARDEN_OBJ" harden)"

HARDER=0
# Heuristics: harden should show hashed symbols and/or push %rbx and/or larger size
H_SIZE=$(wc -c < "$HARDEN_OBJ" | tr -d ' ')
D_SIZE=$(wc -c < "$DEFAULT_OBJ" | tr -d ' ')
H_HASH=$(nm "$HARDEN_OBJ" 2>/dev/null | grep -c '_H[0-9a-f]' || true)
D_HASH=$(nm "$DEFAULT_OBJ" 2>/dev/null | grep -c '_H[0-9a-f]' || true)
H_RBX=$(objdump -d "$HARDEN_OBJ" 2>/dev/null | grep -c 'push *%rbx' || true)

if [[ "$H_HASH" -gt "$D_HASH" ]]; then HARDER=1; fi
if [[ "$H_RBX" -gt 0 ]]; then HARDER=1; fi
if [[ "$H_SIZE" -ge "$D_SIZE" && "$H_HASH" -gt 0 ]]; then HARDER=1; fi

GHIDRA_NOTE="Ghidra not run"
GHIDRA_SECTION=""
GHIDRA_DEFAULT_SUMMARY=""
GHIDRA_HARDEN_SUMMARY=""
if [[ -n "${GHIDRA_INSTALL_DIR:-}" && -x "${GHIDRA_INSTALL_DIR}/support/analyzeHeadless" ]]; then
  if command -v java >/dev/null 2>&1; then
    GHIDRA_OUT="$OUT_DIR/ghidra"
    mkdir -p "$GHIDRA_OUT"
    GHIDRA_LOG="$GHIDRA_OUT/analyzeHeadless.log"
    DUMP_SCRIPT="$ROOT/scripts/GhidraDumpFuncs.java"
    # Ghidra forbids path elements starting with '.'; keep projects outside $ROOT
    # when ROOT contains e.g. .worktrees / .git directories.
    GHIDRA_PROJ_ROOT="${GHIDRA_PROJ_ROOT:-/tmp/inauguration-ghidra-antidecomp}"
    mkdir -p "$GHIDRA_PROJ_ROOT"
    # Import each object in its own project so postScript metrics stay attributed.
    run_ghidra_one() {
      local obj="$1"
      local label="$2"
      local proj="$GHIDRA_PROJ_ROOT/proj-$label"
      local log="$GHIDRA_OUT/${label}.log"
      rm -rf "$proj" "${proj}.rep" "${proj}.gpr" 2>/dev/null || true
      mkdir -p "$proj"
      set +e
      "${GHIDRA_INSTALL_DIR}/support/analyzeHeadless" \
        "$proj" "Antidecomp_${label}" \
        -import "$obj" \
        -scriptPath "$ROOT/scripts" \
        -postScript GhidraDumpFuncs.java \
        -deleteProject >"$log" 2>&1
      local rc=$?
      set -e
      echo "ghidra_${label}_exit=$rc" >>"$log"
      # Extract tagged metrics from headless log
      local summary
      summary="$(grep -E '^GHIDRA_' "$log" || true)"
      printf '%s\n' "$summary" > "$GHIDRA_OUT/${label}.metrics.txt"
      printf '%s' "$summary"
    }
    echo "==> ghidra analyzeHeadless default" >&2
    GHIDRA_DEFAULT_SUMMARY="$(run_ghidra_one "$DEFAULT_OBJ" default)"
    echo "==> ghidra analyzeHeadless harden" >&2
    GHIDRA_HARDEN_SUMMARY="$(run_ghidra_one "$HARDEN_OBJ" harden)"
    cat "$GHIDRA_OUT/default.log" "$GHIDRA_OUT/harden.log" > "$GHIDRA_LOG" || true
    D_FC=$(printf '%s\n' "$GHIDRA_DEFAULT_SUMMARY" | sed -n 's/^GHIDRA_FUNC_COUNT=//p' | tail -1)
    H_FC=$(printf '%s\n' "$GHIDRA_HARDEN_SUMMARY" | sed -n 's/^GHIDRA_FUNC_COUNT=//p' | tail -1)
    D_NAMED=$(printf '%s\n' "$GHIDRA_DEFAULT_SUMMARY" | sed -n 's/^GHIDRA_NAMED_COUNT=//p' | tail -1)
    H_NAMED=$(printf '%s\n' "$GHIDRA_HARDEN_SUMMARY" | sed -n 's/^GHIDRA_NAMED_COUNT=//p' | tail -1)
    D_HASHG=$(printf '%s\n' "$GHIDRA_DEFAULT_SUMMARY" | sed -n 's/^GHIDRA_HASHED_COUNT=//p' | tail -1)
    H_HASHG=$(printf '%s\n' "$GHIDRA_HARDEN_SUMMARY" | sed -n 's/^GHIDRA_HASHED_COUNT=//p' | tail -1)
    D_DEF=$(printf '%s\n' "$GHIDRA_DEFAULT_SUMMARY" | sed -n 's/^GHIDRA_DEFAULTED_COUNT=//p' | tail -1)
    H_DEF=$(printf '%s\n' "$GHIDRA_HARDEN_SUMMARY" | sed -n 's/^GHIDRA_DEFAULTED_COUNT=//p' | tail -1)
    D_DOK=$(printf '%s\n' "$GHIDRA_DEFAULT_SUMMARY" | sed -n 's/^GHIDRA_DECOMP_OK=//p' | tail -1)
    H_DOK=$(printf '%s\n' "$GHIDRA_HARDEN_SUMMARY" | sed -n 's/^GHIDRA_DECOMP_OK=//p' | tail -1)
    D_DCHARS=$(printf '%s\n' "$GHIDRA_DEFAULT_SUMMARY" | sed -n 's/^GHIDRA_DECOMP_CHARS=//p' | tail -1)
    H_DCHARS=$(printf '%s\n' "$GHIDRA_HARDEN_SUMMARY" | sed -n 's/^GHIDRA_DECOMP_CHARS=//p' | tail -1)
    GHIDRA_LABEL="$(basename "$GHIDRA_INSTALL_DIR")"
    GHIDRA_NOTE="analyzeHeadless OK (Ghidra $GHIDRA_LABEL); funcs default/harden: ${D_FC:-?}/${H_FC:-?}; named ${D_NAMED:-?}/${H_NAMED:-?}; hashed ${D_HASHG:-?}/${H_HASHG:-?}; FUN_ ${D_DEF:-?}/${H_DEF:-?}; decomp_ok ${D_DOK:-?}/${H_DOK:-?}; decomp_chars ${D_DCHARS:-?}/${H_DCHARS:-?}"
    GHIDRA_SECTION=$(cat << GSEC
## Ghidra headless metrics

- ghidra_release: \`$GHIDRA_LABEL\`
- java: \`$(java -version 2>&1 | head -1)\`

### Default program
\`\`\`
$GHIDRA_DEFAULT_SUMMARY
\`\`\`

### Harden program
\`\`\`
$GHIDRA_HARDEN_SUMMARY
\`\`\`
GSEC
)
    # Ghidra ran successfully — drop SKIPPED marker if present
    rm -f "$SKIP_MD"
  else
    GHIDRA_NOTE="GHIDRA_INSTALL_DIR set but java missing"
  fi
else
  cat > "$SKIP_MD" << SKIP
# Ghidra antidecomp smoke — skipped

Ghidra headless was not available on this host.

## Install (optional)

1. Install a JDK 17+ (\`java -version\`).
2. Download Ghidra from https://ghidra-sre.org/ and unpack.
3. Export \`GHIDRA_INSTALL_DIR=/path/to/ghidra_*\`.
4. Re-run \`bash scripts/ghidra-antidecomp-smoke.sh\`.

Objdump/nm fallback metrics were still written to \`$BENCH_MD\`.
SKIP
fi

{
  echo "# Ghidra / objdump anti-decomp smoke"
  echo
  echo "Generated by \`scripts/ghidra-antidecomp-smoke.sh\`."
  echo
  echo "Preferred emit is dual-emit (\`--out\` runtime default + \`--harden-out\` harden sample) so the runtime object does not receive harden transforms."
  echo
  echo "- sample: \`$SAMPLE\`"
  echo "- triple: \`$TRIPLE\`"
  echo "- default object: \`$DEFAULT_OBJ\` ($D_SIZE bytes)"
  echo "- harden object: \`$HARDEN_OBJ\` ($H_SIZE bytes)"
  echo "- hashed symbols default/harden: $D_HASH / $H_HASH"
  echo "- harden push %rbx count: $H_RBX"
  echo "- harder_heuristic: $HARDER"
  echo "- ghidra: $GHIDRA_NOTE"
  echo
  echo "## Default metrics"
  echo '```'
  echo "$DEFAULT_METRICS"
  echo '```'
  echo
  echo "## Harden metrics"
  echo '```'
  echo "$HARDEN_METRICS"
  echo '```'
  if [[ -n "$GHIDRA_SECTION" ]]; then
    echo
    printf '%s\n' "$GHIDRA_SECTION"
  fi
} > "$BENCH_MD"

echo "wrote $BENCH_MD"
if [[ "$HARDER" -ne 1 ]]; then
  echo "warning: harden did not look harder by heuristics (continuing; check metrics)" >&2
fi
echo "OK"
