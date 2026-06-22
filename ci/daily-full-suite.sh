#!/usr/bin/env bash
# ci/daily-full-suite.sh — the DAILY BACKSTOP runner (loop-governance, human 2026-06-21).
#
# Main no longer blocks PRs on the full ./check.sh (Option B, DESIGN §7.2): engine/heavy
# PRs land on `check-engine` smoke + lint + check-fast + review. The full heavy+system
# suite is instead run ONCE DAILY on fresh main by a scheduled agent, which heals any
# regression by opening a FIX-OR-REVERT PR (no auto-merge — a human merges). This script
# is the mechanical half: run the whole suite on fresh main and write a machine-readable
# verdict; the agent reads the verdict and does the triage + PR.
#
# Usage:
#   ci/daily-full-suite.sh [--no-system] [--verdict FILE]
# Exit: 0 all green; 1 heavy red; 2 system red; 3 both red; >=4 setup error.
set -uo pipefail

verdict=".td-daily-verdict"
run_system=1
while [ $# -gt 0 ]; do
  case "$1" in
    --no-system) run_system=0; shift ;;
    --verdict) verdict=$2; shift 2 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 4 ;;
  esac
done

git fetch origin main -q || { echo "daily-full-suite: fetch failed" >&2; exit 5; }
main=$(git rev-parse --short origin/main)
hlog=$(mktemp); slog=$(mktemp)
trap 'rm -f "$hlog" "$slog"' EXIT

heavy_rc=0; system_rc=0; system_fail=""
echo ">> daily backstop: full ./check.sh on origin/main ($main)"
TD_BUILD_JOBS=${TD_BUILD_JOBS:-4} ./check.sh >"$hlog" 2>&1 || heavy_rc=$?
heavy_fail=$(grep -E '^FAIL' "$hlog" | head -5 | tr '\n' ';')

if [ "$run_system" = 1 ]; then
  echo ">> daily backstop: ./check.sh check-system on origin/main ($main)"
  TD_BUILD_JOBS=${TD_BUILD_JOBS:-4} ./check.sh check-system >"$slog" 2>&1 || system_rc=$?
  system_fail=$(grep -E '^FAIL' "$slog" | head -5 | tr '\n' ';')
fi

{
  echo "commit=$main"
  echo "date=$(date -Is)"
  echo "heavy=$([ $heavy_rc -eq 0 ] && echo green || echo red)"
  echo "heavy_rc=$heavy_rc"
  echo "heavy_fail=$heavy_fail"
  echo "system=$([ "$run_system" = 1 ] && { [ $system_rc -eq 0 ] && echo green || echo red; } || echo skipped)"
  echo "system_rc=$system_rc"
  echo "system_fail=$system_fail"
} > "$verdict"

rc=0
[ $heavy_rc -ne 0 ] && rc=$((rc+1))
[ "$run_system" = 1 ] && [ $system_rc -ne 0 ] && rc=$((rc+2))
if [ $rc -eq 0 ]; then
  echo "$main" > .td-last-green   # seed of the future `stable` marker
  echo ">> daily backstop: ALL GREEN at $main (recorded .td-last-green)"
else
  echo ">> daily backstop: RED (heavy_rc=$heavy_rc system_rc=$system_rc) — agent: triage \`git log <last-green>..$main\`, reproduce the failing gate, open a FIX-OR-REVERT PR (no auto-merge). Suspect-revert helper: ci/revert-suspect.sh --ref <sha> --open-pr"
fi
cat "$verdict"
exit $rc
