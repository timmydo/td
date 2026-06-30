#!/bin/sh
# gate-timing-report.sh — reduce one `make check` run's per-recipe START/END
# events (logged by tools/gate-time.sh) into a per-gate wall-clock table,
# longest first. Makes latency regressions visible and lets the heavy-gate LPT
# order (mk/gates/<NNN> filename prefixes) be re-sorted from DATA rather than the
# hand-run numbers (task L1).
#
# Pure POSIX sh + coreutils (sort/ls/date): NO bash, NO awk — it leans on nothing
# beyond what the gate recipes already need. Timestamps are integer nanoseconds
# (date +%s%N) so all reduction is integer arithmetic.
#
# Usage: sh gate-timing-report.sh <timing-dir> [artifact-out]
#   Reads the newest non-empty run-*.log under <timing-dir>.
#   env TD_HEAVY_GATES (space-separated) tags rows heavy vs cheap.
set -eu

dir=${1:?usage: gate-timing-report.sh <timing-dir> [artifact-out]}
out=${2:-}
TAB=$(printf '\t')

# Newest non-empty run log (ls -t = mtime, newest first) via command
# substitution — no bash process substitution. A standalone
# `make gate-timing-report` thus reports the most recent run's events.
log=$(ls -1t "$dir"/run-*.log 2>/dev/null | while IFS= read -r f; do
  [ -s "$f" ] && { printf '%s\n' "$f"; break; }
done)
if [ -z "$log" ]; then
  echo "gate-timing: no run log in $dir yet (nothing to report)"
  exit 0
fi

heavyset=" ${TD_HEAVY_GATES:-} "

# Reduce to one "<dur-ns><TAB><kind><TAB><gate>" row per gate. Sorting by gate
# (k1) then timestamp (k3, numeric) makes each gate's events contiguous and
# time-ordered, so a SINGLE pass takes the first START (min) and last END (max)
# per gate — no associative arrays needed (POSIX has none).
rows=$(sort -t"$TAB" -k1,1 -k3,3n "$log" | {
  cur='' ; s='' ; e=''
  emit() {
    [ -n "$cur" ] && [ -n "$s" ] && [ -n "$e" ] || return 0
    case "$heavyset" in *" $cur "*) kind=heavy ;; *) kind=cheap ;; esac
    printf '%s%s%s%s%s\n' "$(( e - s ))" "$TAB" "$kind" "$TAB" "$cur"
  }
  while IFS="$TAB" read -r gate ev ts; do
    [ -n "$gate" ] && [ -n "$ts" ] || continue
    if [ "$gate" != "$cur" ]; then emit; cur=$gate; s='' ; e='' ; fi
    case "$ev" in
      START) [ -n "$s" ] || s=$ts ;;   # first (earliest) START in the sorted group
      END)   e=$ts ;;                   # keep overwriting -> last (latest) END
    esac
  done
  emit
})

# Sum the heavy gates' spans (footer) — a second pass over the rows, in a
# subshell that echoes the total out (POSIX, no shared associative state).
sum_heavy=$(printf '%s\n' "$rows" | {
  tot=0
  while IFS="$TAB" read -r dur kind gate; do
    [ -n "${gate:-}" ] || continue
    [ "$kind" = heavy ] && tot=$(( tot + dur ))
  done
  echo "$tot"
})

fmt() { ns=$1; printf '%d.%03d' "$(( ns / 1000000000 ))" "$(( (ns % 1000000000) / 1000000 ))"; }

report=$(
  echo "# td gate wall-clock — $(date -u '+%Y-%m-%dT%H:%M:%SZ') — $(basename "$log")"
  echo "# per-gate wall span (gates run in parallel under make -j; the sum is NOT the wall time)."
  echo "# heavy rows drive the mk/gates/<NNN> LPT order — renumber longest-first when this drifts."
  printf '%-34s %-6s %10s\n' GATE KIND SECONDS
  printf '%s\n' "$rows" | sort -rn | while IFS="$TAB" read -r dur kind gate; do
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
ls -1t "$dir"/run-*.log 2>/dev/null | tail -n +11 | while IFS= read -r f; do rm -f "$f"; done || true
