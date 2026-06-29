#!/bin/sh
# tests/recipe-rs.sh — the `recipe-rs` gate driver (rust-recipe-surface track).
#
# Proves td's package-recipe surface, now declared in RUST (the `td-recipe` crate,
# recipes/), is equivalent to the boa/TypeScript surface it replaces — with the
# DURABLE assertions that survive boa's retirement carried alongside the removable
# migration oracle (the "durable + removable oracle" discipline, CLAUDE.md).
#
# Legs:
#   (A) DURABLE coverage      — the Rust catalog and tests/ts/recipe-*.ts are in
#                               1:1 correspondence (no orphan Rust recipe, no
#                               unmigrated .ts silently dropped).
#   (B) DURABLE structural    — every recipe `emit`s valid JSON that round-trips
#                               (verify a recipe against its OWN emit; no boa).
#   (C) DURABLE discrimination— `verify` of a recipe against a DIFFERENT recipe's
#                               JSON FAILS (the always-on negative control, so a
#                               green is not the vacuous "verify always passes").
#   (D) MIGRATION ORACLE      — REMOVABLE when boa is retired: for every recipe,
#       (boa, retired last)     boa's JSON (tsc->boa via ts-emit) canon-equals the
#                               Rust recipe. Delete this leg (not the gate) the day
#                               boa goes.
#
# Inputs (env): TD_RECIPE_EVAL (the built td-recipe-eval binary), and for leg (D)
# the ts-emit trio TD_TSGO / TD_TS_EVAL / TD_TSDIR.
set -eu

: "${TD_RECIPE_EVAL:?TD_RECIPE_EVAL (the td-recipe-eval binary) must be set}"
test -x "$TD_RECIPE_EVAL" || { echo "FAIL: $TD_RECIPE_EVAL is not executable" >&2; exit 1; }

root=$(cd "$(dirname "$0")/.." && pwd)
tsdir="$root/tests/ts"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

# --- (A) DURABLE coverage: Rust catalog == the .ts recipe set ------------------
echo ">> (A) coverage: the Rust catalog and tests/ts/recipe-*.ts are 1:1"
"$TD_RECIPE_EVAL" list | sort > "$work/rust-stems"
ls "$tsdir"/recipe-*.ts | sed 's#.*/recipe-##; s#\.ts$##' | sort > "$work/ts-stems"
if ! cmp -s "$work/rust-stems" "$work/ts-stems"; then
  echo "FAIL: the Rust catalog and the .ts recipe set differ:" >&2
  echo "  only in Rust:" $(comm -23 "$work/rust-stems" "$work/ts-stems") >&2
  echo "  only in .ts :" $(comm -13 "$work/rust-stems" "$work/ts-stems") >&2
  exit 1
fi
n=$(wc -l < "$work/rust-stems" | tr -d ' ')
test "$n" -ge 1 || { echo "FAIL: empty catalog (vacuous run)" >&2; exit 1; }
echo "   ok: $n recipes, 1:1 with the .ts surface"

# --- (B) DURABLE structural: every recipe emits valid, round-tripping JSON -----
echo ">> (B) structural: every recipe emits valid JSON that round-trips"
while read -r stem; do
  "$TD_RECIPE_EVAL" emit "$stem" > "$work/$stem.rs.json" \
    || { echo "FAIL: emit $stem" >&2; exit 1; }
  test -s "$work/$stem.rs.json" || { echo "FAIL: emit $stem produced no JSON" >&2; exit 1; }
  # verify a recipe against its OWN emit — proves emit is parseable + stable (no boa).
  "$TD_RECIPE_EVAL" verify "$stem" "$work/$stem.rs.json" >/dev/null 2>&1 \
    || { echo "FAIL: $stem does not round-trip through emit/verify" >&2; exit 1; }
done < "$work/rust-stems"
echo "   ok: all $n recipes emit valid, self-consistent JSON"

# --- (C) DURABLE discrimination: verify must REJECT a different recipe ---------
echo ">> (C) discrimination: verify rejects a mismatched recipe (negative control)"
test -s "$work/gzip.rs.json" -a -s "$work/hello.rs.json" \
  || { echo "FAIL: missing emit fixtures for the negative control" >&2; exit 1; }
if "$TD_RECIPE_EVAL" verify hello "$work/gzip.rs.json" >/dev/null 2>&1; then
  echo "FAIL: verify accepted hello against gzip's JSON — discrimination is vacuous." >&2
  exit 1
fi
echo "   ok: verify hello <gzip.json> correctly FAILS"

# --- (D) MIGRATION ORACLE (REMOVABLE — boa retired last) ----------------------
# Build it both ways and diff: boa evaluates the .ts, the Rust catalog emits its
# twin, and the two canon-equal. This is the Guix-equivalent "own, then diverge"
# guardrail for the recipe surface; delete THIS leg (not the gate) when boa goes.
echo ">> (D) migration oracle: boa(.ts) canon-equals the Rust recipe [REMOVABLE]"
: "${TD_TSGO:?TD_TSGO must be set for the boa oracle leg}"
: "${TD_TS_EVAL:?TD_TS_EVAL (boa) must be set for the boa oracle leg}"
: "${TD_TSDIR:?TD_TSDIR must be set for the boa oracle leg}"
while read -r stem; do
  TD_TSGO="$TD_TSGO" TD_TS_EVAL="$TD_TS_EVAL" TD_TSDIR="$TD_TSDIR" \
    sh "$root/tests/ts-emit.sh" "$tsdir/recipe-$stem.ts" > "$work/$stem.boa.json" 2>/dev/null \
    || { echo "FAIL: boa ts-emit recipe-$stem.ts" >&2; exit 1; }
  "$TD_RECIPE_EVAL" verify "$stem" "$work/$stem.boa.json" \
    || { echo "FAIL: $stem — Rust recipe diverges from boa" >&2; exit 1; }
done < "$work/rust-stems"
echo "   ok: all $n Rust recipes canon-equal their boa-evaluated .ts twin"

echo "PASS: recipe-rs — the Rust package surface ($n recipes) is 1:1 with the .ts set, emits valid self-consistent JSON, discriminates mismatches, and (oracle, removable) canon-equals boa's evaluation of every .ts recipe."
