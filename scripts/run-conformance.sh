#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

IN_CMD=("${IN_BIN:-in}")

SKIP_FIXTURES=()
if [[ -f "$ROOT/conformance/skipped.txt" ]]; then
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%%#*}"
    line="$(echo "$line" | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"
    [[ -z "$line" ]] && continue
    SKIP_FIXTURES+=("$line")
  done < "$ROOT/conformance/skipped.txt"
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

if [[ -n "${NO_COLOR:-}" ]]; then
  RED='' GREEN='' YELLOW='' NC=''
fi

declare -a FIXTURES=()
if [[ $# -gt 0 ]]; then
  for fixture in "$@"; do
    if [[ "$fixture" = /* ]]; then
      FIXTURES+=("$fixture")
    else
      FIXTURES+=("$ROOT/$fixture")
    fi
  done
else
  while IFS= read -r f; do
    FIXTURES+=("$f")
  done < <(find conformance -type f \( -name '*.in' -o -name '*.java' \) | sort)
fi

# Remove known-broken fixtures from the list
FILTERED=()
for f in "${FIXTURES[@]}"; do
  rel="${f#$ROOT/}"
  skip=0
  # An empty skip list must not break the run: it is the goal state.
  for skip_f in ${SKIP_FIXTURES[@]+"${SKIP_FIXTURES[@]}"}; do
    if [[ "$rel" == "$skip_f" ]]; then
      echo "  skipping known failure: $rel"
      skip=1
      break
    fi
  done
  if [[ $skip -eq 0 ]]; then
    FILTERED+=("$f")
  fi
done
FIXTURES=("${FILTERED[@]}")

results() { echo "$1" >> "${RESULTS_FILE:?}"; }

run_fixture() {
  local fixture="$1"
  local rel="${fixture#$ROOT/}"
  local failed=0

  echo ""
  echo "${YELLOW}--- $rel ---${NC}"

  local expects
  expects=$(grep -oE '//[[:space:]]*@expect[[:space:]]+[^:]+:[[:space:]]+.+' "$fixture" 2>/dev/null \
    | sed -E -e 's#^[[:space:]]*//[[:space:]]*@expect[[:space:]]+##' \
             -e 's/[[:space:]]*:[[:space:]]*/|/' || true)

  local has_graph=0 has_jit=0 expect_parse_fail=0

  while IFS='|' read -r check value; do
    [[ -z "$check" ]] && continue
    check="$(echo "$check" | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"
    value="$(echo "$value" | sed -E 's/^[[:space:]]+|[[:space:]]+$//g')"
    case "$check" in
      parse) [[ "$value" == "fail" ]] && expect_parse_fail=1 ;;
      ir|symbols|symbol-count|has-struct|has-function|has-call-edge|has-import|has-effect|package) has_graph=1 ;;
      bytecode|result) has_jit=1 ;;
    esac
  done <<< "$expects"

  local build_tmp
  build_tmp=$(mktemp "${TMPDIR:-/tmp}/conformance-build.XXXXXX")

  if "${IN_CMD[@]}" build --path "$fixture" --module-id Conformance >"$build_tmp" 2>&1; then
    if [[ $expect_parse_fail -eq 1 ]]; then
      echo "  ${RED}FAIL${NC} parse: should have failed"
      failed=1
    else
      echo "  ${GREEN}PASS${NC} parse: ok"
    fi
  else
    if [[ $expect_parse_fail -eq 1 ]]; then
      echo "  ${GREEN}PASS${NC} parse: fail (expected)"
      results "PASS|$rel"
      rm -f "$build_tmp"
      return 0
    else
      echo "  ${RED}FAIL${NC} parse: in build error:"
      while IFS= read -r line; do echo "    $line"; done < "$build_tmp"
      failed=1
    fi
  fi
  rm -f "$build_tmp"

  if [[ $failed -eq 1 ]]; then
    results "FAIL|$rel"
    return 0
  fi

  if [[ $has_graph -eq 1 ]]; then
    local graph_tmp
    graph_tmp=$(mktemp "${TMPDIR:-/tmp}/conformance-graph.XXXXXX")
    if "${IN_CMD[@]}" graph --json --path "$fixture" --module-id Conformance >"$graph_tmp" 2>/dev/null; then
      local gjson
      gjson=$(cat "$graph_tmp")
      while IFS='|' read -r check value; do
        [[ -z "$check" ]] && continue
        case "$check" in
          ir|symbols|symbol-count|has-struct|has-function|has-call-edge|has-import|has-effect|package)
            local ok=0
            case "$check" in
              has-struct)
                echo "$gjson" | python3 -c "
import json,sys
d=json.load(sys.stdin)
found=any(s.get('kind')=='struct' and s.get('name')=='$value' for s in d.get('symbols',[]))
sys.exit(0 if found else 1)" 2>/dev/null && ok=1 ;;
              has-function)
                echo "$gjson" | python3 -c "
import json,sys
d=json.load(sys.stdin)
found=any(s.get('kind')=='function' and s.get('name')=='$value' for s in d.get('symbols',[]))
sys.exit(0 if found else 1)" 2>/dev/null && ok=1 ;;
              has-call-edge)
                echo "$gjson" | python3 -c "
import json,sys
d=json.load(sys.stdin)
found=any('$value' in str(e) for e in d.get('call_edges',[]))
sys.exit(0 if found else 1)" 2>/dev/null && ok=1 ;;
              has-import)
                echo "$gjson" | python3 -c "
import json,sys
d=json.load(sys.stdin)
found=any('$value' in str(i) for i in d.get('imports',[]))
sys.exit(0 if found else 1)" 2>/dev/null && ok=1 ;;
              has-effect)
                echo "$gjson" | python3 -c "
import json,sys
d=json.load(sys.stdin)
found=any('$value' in str(e) for e in d.get('effects',[]))
sys.exit(0 if found else 1)" 2>/dev/null && ok=1 ;;
              package)
                echo "$gjson" | python3 -c "
import json,sys
d=json.load(sys.stdin)
pkg=d.get('package_identity',{}).get('package','')
sys.exit(0 if '$value' in str(pkg) else 1)" 2>/dev/null && ok=1 ;;
              symbols|symbol-count)
                echo "$gjson" | python3 -c "
import json,sys
d=json.load(sys.stdin)
c=len(d.get('symbols',[]))
sys.exit(0 if c >= int('$value') else 1)" 2>/dev/null && ok=1 ;;
            esac
            if [[ $ok -eq 1 ]]; then
              echo "  ${GREEN}PASS${NC} graph: $check $value"
            else
              echo "  ${RED}FAIL${NC} graph: $check $value"
              failed=1
            fi
            ;;
        esac
      done <<< "$expects"
    else
      echo "  ${RED}FAIL${NC} graph: command failed"
      failed=1
    fi
    rm -f "$graph_tmp"
  fi

  if [[ $has_jit -eq 1 ]] && [[ $failed -eq 0 ]]; then
    local exec_tmp
    exec_tmp=$(mktemp "${TMPDIR:-/tmp}/conformance-exec.XXXXXX")
    if "${IN_CMD[@]}" execute --verbose "$fixture" >"$exec_tmp" 2>&1 \
      || grep -q "Execution completed with result:" "$exec_tmp"; then
      while IFS='|' read -r check value; do
        [[ -z "$check" ]] && continue
        case "$check" in
          bytecode)
            [[ "$value" == "executes" ]] && echo "  ${GREEN}PASS${NC} jit: executes" ;;
          result)
            local matched=0
            grep -q "result: Int($value)" "$exec_tmp" 2>/dev/null && matched=1
            if [[ $matched -eq 0 ]]; then
              grep -q "result: \"$value\"" "$exec_tmp" 2>/dev/null && matched=1
            fi
            if [[ $matched -eq 0 ]]; then
              grep -q "result: $value" "$exec_tmp" 2>/dev/null && matched=1
            fi
            if [[ $matched -eq 1 ]]; then
              echo "  ${GREEN}PASS${NC} result: $value"
            else
              local actual
              actual=$(grep "Execution completed with result:" "$exec_tmp" 2>/dev/null | head -1 | sed 's/.*result: //')
              echo "  ${RED}FAIL${NC} result: expected $value, got ${actual:-N/A}"
              failed=1
            fi
            ;;
        esac
      done <<< "$expects"
    else
      echo "  ${RED}FAIL${NC} jit: execution failed"
      while IFS= read -r line; do echo "    $line"; done < "$exec_tmp"
      failed=1
    fi
    rm -f "$exec_tmp"
  fi

  if [[ $failed -eq 0 ]]; then
    echo "  ${GREEN}==> PASS${NC}"
    results "PASS|$rel"
  else
    echo "  ${RED}==> FAIL${NC}"
    results "FAIL|$rel"
  fi
}

RESULTS_FILE=$(mktemp "${TMPDIR:-/tmp}/conformance-results.XXXXXX")
trap 'rm -f "$RESULTS_FILE"' EXIT

echo "=== inauguration conformance suite ==="

for fixture in "${FIXTURES[@]}"; do
  run_fixture "$fixture"
done

echo ""
echo "=== conformance summary ==="
passed=$(grep -c '^PASS|' "$RESULTS_FILE" 2>/dev/null || true)
failed=$(grep -c '^FAIL|' "$RESULTS_FILE" 2>/dev/null || true)
passed="${passed:-0}"
failed="${failed:-0}"
total=$((passed + failed))
echo "PASS:  $passed"
echo "FAIL:  $failed"
echo "TOTAL: $total"

if [[ $failed -gt 0 ]]; then
  echo ""
  echo "Failed fixtures:"
  grep '^FAIL|' "$RESULTS_FILE" | while IFS='|' read -r _ rel; do
    echo "  - $rel"
  done
  exit 1
fi
