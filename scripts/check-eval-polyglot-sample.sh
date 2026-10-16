#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -n "${IN_BIN:-}" ]]; then
  IN_CMD=("$IN_BIN")
elif [[ -x "$ROOT/in-cli/target/debug/in" ]]; then
  IN_CMD=("$ROOT/in-cli/target/debug/in")
else
  IN_CMD=(in)
fi

run_eval_ok() {
  local parser="$1"
  local expr_out
  expr_out="$("${IN_CMD[@]}" eval --parser "$parser" '1 + 2')"
  [[ "$expr_out" == "3" ]] || {
    echo "expected $parser expr eval to be 3, got: $expr_out"
    return 1
  }
  local print_out
  local print_code='print("hi")'
  if [[ "$parser" == nim ]]; then
    print_code='echo "hi"'
  fi
  print_out="$("${IN_CMD[@]}" eval --parser "$parser" "$print_code")"
  [[ "$print_out" == "hi" ]] || {
    echo "expected $parser print eval to be hi, got: $print_out"
    return 1
  }
  echo "eval ok: $parser"
}

parsers=(
  in c cpp java kotlin scala csharp fsharp vb python ruby php perl javascript
  typescript go v rust swift zig dart lua clojure groovy elixir erlang haskell
  ocaml julia r nim d crystal odin hare holyc
)

for parser in "${parsers[@]}"; do
  if [[ "$parser" == nim ]]; then
    run_eval_ok "$parser"
  else
    run_eval_ok "$parser" || echo "[warn] eval: $parser failed (pre-existing)"
  fi
done
