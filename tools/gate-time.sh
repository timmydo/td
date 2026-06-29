#!/usr/bin/env bash
# gate-time.sh — the per-gate wall-clock SHELL wrapper for `make check` (task L1). The Makefile points the TIMED gate targets'
# `.SHELLFLAGS` at this script, so make runs each gate recipe line as
#   bash tools/gate-time.sh <gate-name> -c '<recipe>'
# (a non-default .SHELLFLAGS also disables make's direct-exec fast path, so
# EVERY recipe line is wrapped). We log a START event before the recipe and an
# END event after it, tagged with the gate name, to $TD_GATE_TIMING_LOG;
# tools/gate-timing-report.sh later reduces each gate's min(START)/max(END) to a
# wall-clock span (correct across multi-line recipes and parallel -j gates).
#
# Invoked as `bash <this>` from the Makefile, so it needs NO shebang resolution
# (the loop sandbox does not guarantee an absolute /bin/sh) and needs no exec bit.
#
# FAIL-SAFE is the prime rule: this wraps the loop spine, so a logging hiccup
# must NEVER change a gate's behavior or exit status. Every logging step is
# best-effort (`|| :`) and we always run the real recipe under bash and return
# its true exit code.

# First arg is the injected gate name (.SHELLFLAGS = $@ -c). Be defensive: if it
# looks like a flag, no name was injected (a plain `-c` invocation) — don't shift.
case "${1:-}" in
  ''|-*) gate='' ;;
  *)     gate="$1"; shift ;;
esac

log="${TD_GATE_TIMING_LOG:-}"

if [ -n "$gate" ] && [ -n "$log" ]; then
  mkdir -p "$(dirname "$log")" 2>/dev/null || :
  printf '%s\tSTART\t%s\n' "$gate" "$(date +%s%N 2>/dev/null)" >> "$log" 2>/dev/null || :
fi

bash "$@"
rc=$?

if [ -n "$gate" ] && [ -n "$log" ]; then
  printf '%s\tEND\t%s\n' "$gate" "$(date +%s%N 2>/dev/null)" >> "$log" 2>/dev/null || :
fi

exit "$rc"
