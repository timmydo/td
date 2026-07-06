#!/usr/bin/env bash
# tests/recipe-checks.sh — run the recipe-owned package checks. td-recipe-eval lists the
# recipes that carry checks (recipes/src/recipes/<stem>.rs) and emits their bodies; each
# builds its package on td's OWN mes-rooted /td/store toolchain (the store-native ladder),
# GUIX-FREE. There is no package table and no guix helper lib — the guix-seeded recipe
# checks (recipe_cached_build / recipe_link_seed against a *-no-guix.lock realized with
# `guix build`) retired with the guix corpus; every surviving check is store-native.

set -euo pipefail

: "${TD_GATE_GOALS:=recipe-checks-daily}"

scope=${TD_RECIPE_CHECK_SCOPE:-daily}
case "$scope" in
  pr|daily|all) ;;
  *) echo "ERROR: TD_RECIPE_CHECK_SCOPE must be pr, daily, or all (got '$scope')" >&2; exit 1 ;;
esac

echo ">> recipe-checks: recipe-owned /td/store package checks (scope=$scope; goals=$TD_GATE_GOALS)"

# The prelude the check bodies need: the stage0 td-builder ($TB / $TD_BUILDER_*, compiled
# from source with the environment's rust — no guix) and td's Rust recipe evaluator
# ($TD_RECIPE_EVAL). CU/ROOT are exported for the store-native bodies (which stage the
# gate's declared coreutils input and resolve paths against the repo root).
. tests/cache-lib.sh
export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"
load_stage0
load_recipe_eval
case "$TD_RECIPE_EVAL" in
  *.td-build-cache/*) : ;;
  *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; exit 1 ;;
esac
export CU="${TD_GATE_INPUT_COREUTILS:-}" CACHE="$PWD/.td-build-cache/pkg" ROOT="$PWD"
mkdir -p "$CACHE"

scratch="$PWD/.td-build-cache/recipe-checks/$scope-$$"
rm -rf "$scratch"
mkdir -p "$scratch/scripts"

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
    script="$scratch/scripts/$spec-$i.sh"
    "$TD_RECIPE_EVAL" check-script "$spec" "$scope" "$i" > "$script"
    test -s "$script" || { echo "FAIL: empty check script for $label" >&2; exit 1; }
    if (
      set -euo pipefail
      export TD_RECIPE_CHECK_SPEC="$spec" TD_RECIPE_CHECK_INDEX="$i"
      . "$script"
    ); then
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
