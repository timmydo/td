#!/bin/sh
# tests/ts-emit.sh — the shared TS spec front-end step for the `ts-diff` rung
# (DESIGN §7.1 ts-frontend). Transpile a td `.ts` spec to JS with the pinned tsc
# (td-typescript, under node) and evaluate it with the boa evaluator (td-ts-eval),
# printing the captured `system()` config as one line of JSON — the input the
# Guile lowering bridge (tests/ts-diff.scm) turns into a td-config + derivation.
#
# Inputs (env): TD_NODE, TD_TSC (td-typescript dir), TD_TS_EVAL (the binary),
# TD_TSDIR (the dialect dir). Arg 1: the spec `.ts` path. JSON goes to stdout;
# tsc chatter (none on success) and errors go to stderr.
set -eu

: "${TD_NODE:?}"; : "${TD_TSC:?}"; : "${TD_TS_EVAL:?}"; : "${TD_TSDIR:?}"
spec="${1:?usage: ts-emit.sh SPEC.ts}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

# Same pinned flags as the `ts` rung's transpile, so the JS the differential
# evaluates is exactly the golden-checked emit.
"$TD_NODE" "$TD_TSC/bin/tsc" --strict --target es2020 --lib es2020 \
  --newLine lf --removeComments --outDir "$work" \
  "$TD_TSDIR/td-spec.d.ts" "$spec" >&2

js="$work/$(basename "$spec" .ts).js"
test -s "$js" || { echo "ts-emit: tsc produced no JS for $spec" >&2; exit 1; }

"$TD_TS_EVAL" < "$js"
