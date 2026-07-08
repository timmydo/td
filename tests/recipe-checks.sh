#!/usr/bin/env bash
# tests/recipe-checks.sh — run the recipe-owned package checks. td-recipe-eval lists
# the recipes that carry checks (recipes/src/recipes/<stem>.rs) and runs each one
# through its Rust check runner; each builds its package on td's OWN mes-rooted
# /td/store toolchain (the store-native ladder), GUIX-FREE. There is no package table
# and no guix helper lib; the retired guix-seeded corpus path is gone, and every
# surviving check is store-native.

set -euo pipefail

: "${TD_GATE_GOALS:=recipe-checks-daily}"

scope=${TD_RECIPE_CHECK_SCOPE:-daily}
case "$scope" in
  pr|daily|all) ;;
  *) echo "ERROR: TD_RECIPE_CHECK_SCOPE must be pr, daily, or all (got '$scope')" >&2; exit 1 ;;
esac

echo ">> recipe-checks: recipe-owned /td/store package checks (scope=$scope; goals=$TD_GATE_GOALS)"

if [ -z "${TD_RECIPE_EVAL:-}" ]; then
  re="${TD_RECIPE_EVAL_BASE:-$PWD/.td-build-cache/recipe-eval}/recipe-eval-path"
  test -s "$re" || { echo "FAIL: no td-recipe-eval sentinel ($re) — the build-recipes prelude must run first" >&2; exit 1; }
  TD_RECIPE_EVAL=`cat "$re"`
fi
test -x "$TD_RECIPE_EVAL" || { echo "FAIL: td-recipe-eval not executable at $TD_RECIPE_EVAL" >&2; exit 1; }
case "$TD_RECIPE_EVAL" in
  *.td-build-cache/*) : ;;
  *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;;
esac
export TD_RECIPE_EVAL
export TD_STAGE0_BASE="${TD_STAGE0_BASE:-$PWD/.td-build-cache/stage0}"

checks=`"$TD_RECIPE_EVAL" check-list "$scope"`
test -n "$checks" || { echo "FAIL: no recipe checks selected for scope=$scope" >&2; exit 1; }

n=0
failures=0
for spec in $checks; do
  count=`"$TD_RECIPE_EVAL" check-count "$spec" "$scope"`
  case "$count" in
    ''|*[!0-9]*) echo "FAIL: non-numeric check-count for $spec: '$count'" >&2; exit 1 ;;
  esac
  test "$count" -gt 0 || { echo "FAIL: check-list selected $spec but check-count is 0" >&2; exit 1; }
  i=1
  while [ "$i" -le "$count" ]; do
    n=$((n + 1))
    label="$spec#$i"
    echo "================ recipe-check $label ($scope) ================"
    if TD_RECIPE_CHECK_SPEC="$spec" TD_RECIPE_CHECK_INDEX="$i" \
      "$TD_RECIPE_EVAL" check-run "$spec" "$scope" "$i"; then
      echo "================ recipe-check $label ($scope): PASS ================"
    else
      rc=$?
      failures=$((failures + 1))
      echo "================ recipe-check $label ($scope): FAIL (exit $rc) ================" >&2
    fi
    i=$((i + 1))
  done
done

if [ "$failures" -ne 0 ]; then
  echo "FAIL: recipe-checks — $failures of $n recipe-owned check(s) failed (scope=$scope)" >&2
  exit 1
fi

echo "PASS: recipe-checks — ran $n recipe-owned /td/store check(s) from the Rust recipe catalog (scope=$scope); package behavior/repro assertions live with the package recipes."
