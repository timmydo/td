#!/usr/bin/env bash
# gate-timing-report.sh — reduce one `make check` run's per-recipe START/END
# events (logged by tools/gate-time.sh) into a per-gate wall-clock table,
# longest first. Makes latency regressions visible and lets the heavy-gate LPT
# order (mk/gates/<NNN> filename prefixes) be re-sorted from DATA rather than the
# hand-run numbers in plan/loop-latency.md (task L1).
#
# Pure bash + coreutils on purpose: the loop sandbox has no awk. Timestamps are
# integer nanoseconds (date +%s%N) so all reduction is integer arithmetic.
#
# Usage: bash gate-timing-report.sh <timing-dir> [artifact-out]
#   Reads the newest non-empty run-*.log under <timing-dir>.
#   env TD_HEAVY_GATES (space-separated) tags rows heavy vs cheap.
set -euo pipefail

dir=${1:?usage: gate-timing-report.sh <timing-dir> [artifact-out]}
out=${2:-}

# Newest non-empty run log (ls -t = mtime, newest first). A standalone
# `make gate-timing-report` thus reports the most recent run's events.
log=""
if [ -d "$dir" ]; then
  while IFS= read -r f; do
    [ -s "$f" ] && { log=$f; break; }
  done < <(ls -1t "$dir"/run-*.log 2>/dev/null || true)
fi
if [ -z "$log" ]; then
  echo "gate-timing: no run log in $dir yet (nothing to report)"
  exit 0
fi

declare -A st en
while IFS=$'\t' read -r gate ev ts; do
  [ -n "${gate:-}" ] && [ -n "${ts:-}" ] || continue
  case "$ev" in
    START) if [ -z "${st[$gate]:-}" ] || [ "$ts" -lt "${st[$gate]}" ]; then st[$gate]=$ts; fi ;;
    END)   if [ -z "${en[$gate]:-}" ] || [ "$ts" -gt "${en[$gate]}" ]; then en[$gate]=$ts; fi ;;
  esac
done < "$log"

heavyset=" ${TD_HEAVY_GATES:-} "
# nanoseconds -> "S.mmm"
fmt() { local ns=$1; printf '%d.%03d' $(( ns / 1000000000 )) $(( (ns % 1000000000) / 1000000 )); }

rows=""
sum_heavy=0
for gate in "${!st[@]}"; do
  s=${st[$gate]}; e=${en[$gate]:-}
  [ -n "$e" ] || continue
  dur=$(( e - s ))
  case "$heavyset" in
    *" $gate "*) kind=heavy; sum_heavy=$(( sum_heavy + dur )) ;;
    *)           kind=cheap ;;
  esac
  rows+="$dur"$'\t'"$kind"$'\t'"$gate"$'\n'
done

report=$(
  echo "# td gate wall-clock — $(date -u '+%Y-%m-%dT%H:%M:%SZ') — $(basename "$log")"
  echo "# per-gate wall span (gates run in parallel under make -j; the sum is NOT the wall time)."
  echo "# heavy rows drive the mk/gates/<NNN> LPT order — renumber longest-first when this drifts."
  printf '%-34s %-6s %10s\n' GATE KIND SECONDS
  printf '%s' "$rows" | sort -rn | while IFS=$'\t' read -r dur kind gate; do
    [ -n "${gate:-}" ] || continue
    printf '%-34s %-6s %10s\n' "$gate" "$kind" "$(fmt "$dur")"
  done
  echo "# heavy work total (sum across heavy gates, not wall): $(fmt "$sum_heavy")s"
)

printf '%s\n' "$report"
if [ -n "$out" ]; then
  mkdir -p "$(dirname "$out")" 2>/dev/null || true
  printf '%s\n' "$report" > "$out"
  echo "gate-timing: report written to $out"
fi

# Keep the timing dir bounded — newest 10 run logs.
if [ -d "$dir" ]; then
  ls -1t "$dir"/run-*.log 2>/dev/null | tail -n +11 | while IFS= read -r f; do rm -f "$f"; done || true
fi
