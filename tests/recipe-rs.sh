#!/bin/sh
# tests/recipe-rs.sh — the `recipe-rs` gate driver (rust-recipe-surface track).
#
# td's package + system-spec surface is declared in Rust (the td-recipe crate,
# recipes/). boa/TypeScript are gone, so the differential-vs-boa oracle is gone too;
# the DURABLE proof that the recipes are CORRECT is the corpus differential
# (corpus-no-guix builds every recipe and proves NAR-equality to guix). This gate
# proves the surface is self-consistent and that the census's manifest is in sync:
#
#   (A) coverage         — `list`/`list-specs` are non-empty and every recipe + spec
#                          emits valid JSON that round-trips (parse+re-emit stable).
#   (B) manifest in sync — `td-recipe-eval meta` byte-equals the committed
#                          tests/recipes-meta.json (what the rust-free guix-dependence
#                          census reads); a stale manifest fails here.
#   (C) discrimination   — `verify` of a recipe against a DIFFERENT recipe's JSON
#                          FAILS (the always-on negative control; not vacuous).
#
# Input (env): TD_RECIPE_EVAL (the td-recipe-eval binary).
set -eu

: "${TD_RECIPE_EVAL:?TD_RECIPE_EVAL (the td-recipe-eval binary) must be set}"
test -x "$TD_RECIPE_EVAL" || { echo "FAIL: $TD_RECIPE_EVAL is not executable" >&2; exit 1; }

root=$(cd "$(dirname "$0")/.." && pwd)
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

echo ">> (A) coverage: every recipe + spec emits valid, round-tripping JSON"
recipes=$("$TD_RECIPE_EVAL" list); specs=$("$TD_RECIPE_EVAL" list-specs)
test -n "$recipes" || { echo "FAIL: empty recipe catalog (vacuous)" >&2; exit 1; }
test -n "$specs" || { echo "FAIL: empty spec catalog (vacuous)" >&2; exit 1; }
for s in $recipes; do
  "$TD_RECIPE_EVAL" emit "$s" > "$work/$s.json" || { echo "FAIL: emit $s" >&2; exit 1; }
  test -s "$work/$s.json" || { echo "FAIL: emit $s produced no JSON" >&2; exit 1; }
  "$TD_RECIPE_EVAL" verify "$s" "$work/$s.json" >/dev/null 2>&1 \
    || { echo "FAIL: $s does not round-trip" >&2; exit 1; }
done
for s in $specs; do
  "$TD_RECIPE_EVAL" emit-spec "$s" > "$work/spec-$s.json" || { echo "FAIL: emit-spec $s" >&2; exit 1; }
  "$TD_RECIPE_EVAL" verify-spec "$s" "$work/spec-$s.json" >/dev/null 2>&1 \
    || { echo "FAIL: spec $s does not round-trip" >&2; exit 1; }
done
echo "   ok: $(printf '%s\n' $recipes | wc -l | tr -d ' ') recipes + $(printf '%s\n' $specs | wc -l | tr -d ' ') specs emit valid, self-consistent JSON"

echo ">> (B) manifest in sync: td-recipe-eval meta == tests/recipes-meta.json"
"$TD_RECIPE_EVAL" meta > "$work/meta.json"
if [ "$(cat "$work/meta.json")" != "$(cat "$root/tests/recipes-meta.json")" ]; then
  echo "FAIL: tests/recipes-meta.json is stale — regenerate with:" >&2
  echo "      td-recipe-eval meta > tests/recipes-meta.json" >&2
  exit 1
fi
echo "   ok: the census manifest matches the Rust catalog"

echo ">> (C) discrimination: verify rejects a mismatched recipe (negative control)"
test -s "$work/gzip.json" -a -s "$work/hello.json" \
  || { echo "FAIL: missing emit fixtures for the negative control" >&2; exit 1; }
if "$TD_RECIPE_EVAL" verify hello "$work/gzip.json" >/dev/null 2>&1; then
  echo "FAIL: verify accepted hello against gzip's JSON — discrimination is vacuous." >&2
  exit 1
fi
echo "   ok: verify hello <gzip.json> correctly FAILS"

echo "PASS: recipe-rs — the Rust package + spec surface emits valid self-consistent JSON, the guix-dependence manifest is in sync, and verify discriminates mismatches. Correctness vs upstream is proven by the corpus differential (corpus-no-guix), not boa (retired)."
