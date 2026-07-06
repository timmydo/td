#!/usr/bin/env bash
# tests/recipe-checks.sh — run package checks declared on recipes themselves.
#
# The old one-gate-per-package files carried shell case tables and package
# assertions. This driver has no package table: td-recipe-eval lists recipes
# with checks and emits the recipe-owned bodies.

set -euo pipefail

: "${TD_GATE_GOALS:=recipe-checks}"

scope=${TD_RECIPE_CHECK_SCOPE:-}
if [ -z "$scope" ]; then
  scope=pr
fi
case "$scope" in
  pr|daily|all) ;;
  *) echo "ERROR: TD_RECIPE_CHECK_SCOPE must be pr, daily, or all (got '$scope')" >&2; exit 1 ;;
esac

echo ">> recipe-checks: running recipe-owned package checks (scope=$scope; goals=$TD_GATE_GOALS)"
. tests/recipe-check-lib.sh
recipe_checks_prelude

scratch="$PWD/.td-build-cache/recipe-checks/$scope-$$"
rm -rf "$scratch"
mkdir -p "$scratch/scripts"

if [ "$scope" != pr ]; then
  # Daily/all includes the seeded C link checks. Resolve that tool seed once in
  # the parent so each per-body subshell can reuse the exported paths.
  recipe_link_seed
fi

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

echo "PASS: recipe-checks — ran $n recipe-owned check(s) from the Rust recipe catalog (scope=$scope); package behavior/repro assertions now live with the package recipes instead of one gate file per package."
