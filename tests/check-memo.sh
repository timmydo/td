#!/bin/sh
# tests/check-memo.sh — verdict memoization for the `guix build --check`
# reproducibility legs (plan/check-memo.md; DESIGN §7.1 check-memo — the
# §4.3 gate-2 sign-off and the BINDING constraints 1-6 live in that file).
#
# usage: check-memo.sh DRV [DRV...]
#
# For each DRV: if a FRESH verdict recorded in THIS environment shows this
# exact derivation already rebuilt bit-identically, the rebuild is skipped —
# after re-asserting (cheap, on every hit — constraint 5) that every output
# is still VALID in the daemon's DB with exactly the verdict's NAR hash and
# size. On ANY miss (forced full, no identity, no verdict, foreign
# environment, expired, malformed, or DB/verdict disagreement) the real
# `guix build --check` runs unchanged for the missed drvs, and a green run
# records fresh verdicts. A red `--check` stays red — nothing is recorded.
#
# Environment:
#   TD_GUIX            the pinned guix command
#                      (default "guix time-machine -C channels.scm --")
#   TD_CHECK_ENV       environment identity. check.sh computes it ON THE HOST
#                      (machine-id:store-fs-type:pinned-commit — the container
#                      cannot see /etc/machine-id) and leaves it EMPTY under
#                      CI (constraint 2: CI verdict reuse is OFF) or when the
#                      identity cannot be established. Empty/unset => never
#                      hit, never record — the full --check always runs
#                      (fail closed).
#   TD_CHECK_FULL      non-empty => bypass ALL verdicts (constraint 4 — the
#                      force-full knob; oracle re-baselines and suspected
#                      nondeterminism MUST use it). The forced green run
#                      still records fresh verdicts.
#   TD_CHECK_TTL_DAYS  verdict freshness window in days, default 7.
#                      Values above 14 are REFUSED here, structurally:
#                      loosening beyond 14 re-opens gate 2 (constraint 3).
#   TD_CHECK_VERDICTS  verdict directory, default .check-verdicts —
#                      host-local state, gitignored, NEVER committed
#                      (constraint 2).
set -eu

test "$#" -ge 1 || { echo "usage: check-memo.sh DRV..." >&2; exit 2; }

GUIXCMD=${TD_GUIX:-"guix time-machine -C channels.scm --"}
ttl=${TD_CHECK_TTL_DAYS:-7}
case "$ttl" in
  ''|*[!0-9]*)
    echo "check-memo: FATAL: TD_CHECK_TTL_DAYS='$ttl' is not a positive integer" >&2
    exit 1;;
esac
if [ "$ttl" -lt 1 ] || [ "$ttl" -gt 14 ]; then
  echo "check-memo: FATAL: TTL ${ttl}d is outside 1..14 — loosening the verdict" >&2
  echo "  TTL beyond 14 days re-opens gate 2 (plan/check-memo.md constraint 3)." >&2
  exit 1
fi
vdir=${TD_CHECK_VERDICTS:-.check-verdicts}
envid=${TD_CHECK_ENV-}

info=$(mktemp)
trap 'rm -f "$info"' EXIT

# One daemon query for all drvs (constraint 5's cheap assertion): for every
# output, INFO=<drv> <name> <path> <base16 nar sha256> <nar size> when valid,
# INVALID=<drv> <name> <path> when not. A failed query treats every drv as a
# miss — memoization fails CLOSED into the real --check.
if [ -n "$envid" ] && [ -z "${TD_CHECK_FULL:-}" ]; then
  TD_DRVS="$*" $GUIXCMD repl tests/check-memo-info.scm 2>/dev/null > "$info" || {
    echo "check-memo: WARNING: store DB query failed — treating every drv as a miss" >&2
    : > "$info"
  }
fi

now=$(date +%s)
misses=""
for drv in "$@"; do
  vf="$vdir/$(basename "$drv").verdict"
  reason=""
  if [ -n "${TD_CHECK_FULL:-}" ]; then
    reason="forced full"
  elif [ -z "$envid" ]; then
    reason="no environment identity"
  elif [ ! -f "$vf" ]; then
    reason="no verdict"
  else
    vdrv=$(sed -n 's/^drv //p' "$vf")
    venv=$(sed -n 's/^env //p' "$vf")
    vrec=$(sed -n 's/^recorded //p' "$vf")
    if [ "$vdrv" != "$drv" ]; then
      reason="verdict drv mismatch"
    elif [ "$venv" != "$envid" ]; then
      reason="foreign environment"
    elif ! [ "$vrec" -ge 0 ] 2>/dev/null || [ "$vrec" -gt "$now" ]; then
      # A FUTURE timestamp would stay "fresh" past the TTL bound until the
      # clock caught up — an effective TTL beyond the 14-day cap. Treat it
      # as malformed (constraint 3): miss, re-check, re-record honestly.
      reason="malformed verdict (bad or future timestamp)"
    elif [ $((now - vrec)) -gt $((ttl * 86400)) ]; then
      reason="expired ($(( (now - vrec) / 86400 ))d old, ttl ${ttl}d)"
    else
      # Constraint 5: a hit is a cheap assertion, not a no-op — every output
      # the verdict names must be valid in the store DB RIGHT NOW with the
      # same NAR hash and size, and the DB must name the same output set.
      # $drv is interpolated into the BRE unescaped: store paths carry no
      # sed metachars beyond `.`, and the 32-char content hash makes a
      # wildcard-dot cross-match between two real drv paths impossible.
      vouts=$(sed -n 's/^output //p' "$vf" | sort)
      douts=$(sed -n "s|^INFO=$drv ||p" "$info" | sort)
      if [ -z "$vouts" ]; then
        reason="malformed verdict (no outputs)"
      elif [ "$vouts" != "$douts" ]; then
        reason="verdict/DB mismatch (output invalid, vanished, or NAR hash changed)"
      fi
    fi
  fi
  if [ -z "$reason" ]; then
    echo "MEMO HIT $drv — fresh verdict ($(( now - vrec ))s old), same drv + environment; outputs valid in the store DB with the verdict's NAR hashes; skipping the --check rebuild"
  else
    echo "MEMO MISS ($reason) $drv — running the real guix build --check"
    misses="$misses $drv"
  fi
done

test -n "$misses" || exit 0

# The real thing, exactly as the rungs ran it before this helper existed.
# Its exit status is THIS script's exit status: a red --check reds the rung.
$GUIXCMD build --check $misses

# Record fresh verdicts for the drvs that just proved themselves — only with
# an environment identity to bind them to (constraint 2).
test -n "$envid" || exit 0
TD_DRVS="${misses# }" $GUIXCMD repl tests/check-memo-info.scm 2>/dev/null > "$info" || {
  echo "check-memo: WARNING: store DB re-query failed after the green --check; recording no verdicts" >&2
  exit 0
}
mkdir -p "$vdir"
rec=$(date +%s)
for drv in $misses; do
  outs=$(sed -n "s|^INFO=$drv |output |p" "$info")
  if [ -z "$outs" ] || grep -q "^INVALID=$drv " "$info"; then
    echo "check-memo: WARNING: no valid DB record for $drv after the green --check; not recording" >&2
    continue
  fi
  tmp=$(mktemp "$vdir/.tmp.XXXXXX")
  { echo "td-check-memo 1"
    echo "drv $drv"
    echo "env $envid"
    echo "recorded $rec"
    printf '%s\n' "$outs"
  } > "$tmp"
  mv "$tmp" "$vdir/$(basename "$drv").verdict"
  echo "MEMO RECORD $drv"
done
