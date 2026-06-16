#!/bin/sh
# tests/td-check-repro.sh — the recipe gates' shared DURABLE reproducibility leg
# (DESIGN §7.1 input-recipes; prime directive 1 on td's OWN terms).
#
# `td-builder check` builds DRV TWICE in two independent user-namespace sandboxes
# and compares the per-output NAR hashes — td's own reproducibility verdict, with
# NO `guix build --check` and no daemon in it. This is the assertion that survives
# Guix's retirement (unlike the byte-identity / NAR-equal "migration oracle" legs),
# so corpus-pkgconfig / corpus-libatomic / corpus-popt / corpus-gzip all share it
# rather than each re-deriving reproducibility from Guix.
#
#   td-check-repro.sh TD_BUILDER DRV INPUTS_FILE SCRATCH
#
# INPUTS_FILE: the drv's direct-input output paths (the build-closure seed — the
#   recipe `*-drv.scm` scripts emit them as `TD_IN=`). This script runs
#   `$TD_GUIX gc -R` over them + DRV to stage the FULL build closure td's sandbox
#   binds in.
# SCRATCH: a writable dir (created fresh, removed on exit).
# TD_GUIX (env): how to invoke guix for the closure walk (e.g.
#   "guix time-machine -C channels.scm --").
#
# Exits non-zero (printing the td-builder check output) if td's two builds disagree
# or the build errors — so a non-reproducible recipe reds the gate, on td's terms.
#
# VERDICT MEMOIZATION (plan/fast-check.md; mirrors tests/check-memo.sh, which
# memoizes `guix build --check`). The td-builder double-build recompiles the recipe
# from source TWICE every run — the single biggest avoidable cost in the warm inner
# loop. So: if a FRESH verdict recorded in THIS environment shows this exact DRV
# already proved reproducible — and its outputs are still valid in the daemon DB
# with the recorded NAR hashes — the double-build is SKIPPED. On ANY miss (forced
# full, no environment identity, no/foreign/expired/malformed verdict, or DB
# disagreement) the real double-build below runs unchanged and, when green, records
# a fresh verdict. The key is the DRV store path (content-addressed): a changed or
# perturbed recipe is a DIFFERENT drv ⇒ always a miss ⇒ the real double-build runs
# ⇒ the verified-red discipline is intact; memoization can never green a recipe
# whose reproducibility has not actually been observed in this environment.
#
# LOOSENING (CLAUDE.md directive 3 / DESIGN §4.3 gate-2): skipping the double-build
# on a hit is the same trade the human approved for check-memo. Knobs/guards reuse
# check.sh's already-exported TD_CHECK_* identity:
#   TD_CHECK_ENV       environment identity (machine-id:store-fs:pin). Empty under
#                      CI or when it cannot be established ⇒ never hit, never record
#                      (fail closed) — a verdict greens only the environment that
#                      recorded it.
#   TD_CHECK_FULL      non-empty ⇒ bypass ALL verdicts (force the double-build).
#   TD_CHECK_TTL_DAYS  freshness window, default 7; values above 14 are REFUSED
#                      (loosening beyond the check-memo cap).
#   TD_TDCHECK_VERDICTS verdict directory, default .td-check-verdicts — host-local,
#                      gitignored, NEVER committed. Separate from check-memo's
#                      .check-verdicts/ to avoid the per-drv filename collision.
set -eu

tb="$1"; drv="$2"; infile="$3"; sc="$4"
: "${TD_GUIX:?TD_GUIX must say how to invoke guix}"

# --- verdict memoization guards (mirrors tests/check-memo.sh) ----------------
ttl=${TD_CHECK_TTL_DAYS:-7}
case "$ttl" in
  ''|*[!0-9]*)
    echo "td-check-repro: FATAL: TD_CHECK_TTL_DAYS='$ttl' is not a positive integer" >&2
    exit 1;;
esac
if [ "$ttl" -lt 1 ] || [ "$ttl" -gt 14 ]; then
  echo "td-check-repro: FATAL: TTL ${ttl}d is outside 1..14 — loosening the verdict" >&2
  echo "  TTL beyond 14 days re-opens gate 2 (plan/check-memo.md constraint 3)." >&2
  exit 1
fi
vdir=${TD_TDCHECK_VERDICTS:-.td-check-verdicts}
envid=${TD_CHECK_ENV-}
vf="$vdir/$(basename "$drv").verdict"
now=$(date +%s)

# Daemon-DB facts for $drv's outputs: one "<name> <path> <base16 nar sha256>
# <nar size>" line per VALID output, "INVALID <name> <path>" otherwise. The gate
# realizes $drv into the store (`guix build $drv`) BEFORE calling us, so its
# outputs are present. A failed query yields no INFO lines ⇒ treated as a miss /
# no record (fail closed). The hash + size are the daemon's OWN records — the same
# oracle check-memo compares against — never recomputed here.
db_facts() {
  raw=$(TD_DRVS="$drv" $TD_GUIX repl tests/check-memo-info.scm 2>/dev/null) || {
    echo "   td-check-repro: WARNING: store DB query failed — treating $drv as a memo miss" >&2
    return 0
  }
  printf '%s\n' "$raw" \
    | sed -n -e "s|^INFO=$drv ||p" -e "s|^INVALID=$drv |INVALID |p"
}

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
    reason="malformed verdict (bad or future timestamp)"
  elif [ $((now - vrec)) -gt $((ttl * 86400)) ]; then
    reason="expired ($(( (now - vrec) / 86400 ))d old, ttl ${ttl}d)"
  else
    # A hit is a cheap assertion, not a no-op: every output the verdict names must
    # still be valid in the store DB RIGHT NOW with the same NAR hash + size, and
    # the DB must name the same output set (check-memo constraint 5).
    vouts=$(sed -n 's/^output //p' "$vf" | sort)
    douts=$(db_facts | sort)
    if [ -z "$vouts" ]; then
      reason="malformed verdict (no outputs)"
    elif printf '%s\n' "$douts" | grep -q '^INVALID '; then
      reason="verdict/DB mismatch (an output is no longer valid)"
    elif [ "$vouts" != "$douts" ]; then
      reason="verdict/DB mismatch (output vanished, or NAR hash changed)"
    fi
  fi
fi

if [ -z "$reason" ]; then
  echo "   TD-CHECK MEMO HIT $drv — fresh verdict ($(( now - vrec ))s old), same drv + environment; outputs valid in the store DB with the verdict's NAR hashes; skipping td-builder's double-build"
  sed -n 's/^repro /   td double-build agrees (memoized): /p' "$vf"
  exit 0
fi
echo "   TD-CHECK MEMO MISS ($reason) $drv — running td-builder's real double-build"

# --- the real double-build (unchanged behavior) -----------------------------
chmod -R u+w "$sc" 2>/dev/null || true; rm -rf "$sc"; mkdir -p "$sc"
cleanup() { chmod -R u+w "$sc" 2>/dev/null || true; rm -rf "$sc"; }

# Realize the drv's build inputs first. A fixed-output SOURCE may have been GC'd
# (its output dropped once the package was built), which would make the closure
# walk below fail and starve td's rebuild of the source. Re-realizing the input
# derivations re-fetches it (a permitted offline fixed-output fetch); deps already
# in the store are returned from cache (fast).
$TD_GUIX gc --references "$drv" 2>/dev/null | grep '\.drv$' \
  | xargs -r $TD_GUIX build >/dev/null 2>&1 || true

{ cat "$infile"; echo "$drv"; } | xargs $TD_GUIX gc -R | sort -u > "$sc/paths.txt"
echo "   staged build closure: $(wc -l < "$sc/paths.txt") store items"

if ! "$tb" check "$drv" "$sc/paths.txt" "$sc/c" > "$sc/out.txt" 2>"$sc/err.txt"; then
  echo "FAIL: td-builder check reported NON-reproducible (or errored):" >&2
  cat "$sc/out.txt" "$sc/err.txt" >&2
  cleanup; exit 1
fi
# td-builder check exits 0 only when EVERY output's two builds agree; require at
# least one "reproducible" line and no "NOT reproducible" as a defensive backstop.
if ! grep -q 'reproducible' "$sc/out.txt" || grep -qi 'not reproducible' "$sc/out.txt"; then
  echo "FAIL: td-builder check did not confirm the outputs reproducible:" >&2
  cat "$sc/out.txt" >&2
  cleanup; exit 1
fi
grep '^CHECK ' "$sc/out.txt" | sed 's/^CHECK /   td double-build agrees: /'

# --- record a fresh verdict (only with an environment identity) --------------
# The td double-build just proved this drv reproducible; the daemon DB facts come
# from the output the gate already realized. No identity ⇒ record nothing (the
# verdict could not be safely reused). A DB query that finds an INVALID/missing
# output ⇒ record nothing (so the next run re-runs the real double-build).
if [ -n "$envid" ]; then
  facts=$(db_facts)
  if [ -z "$facts" ] || printf '%s\n' "$facts" | grep -q '^INVALID '; then
    echo "   td-check-repro: WARNING: no valid DB record for $drv; not recording a verdict" >&2
  else
    mkdir -p "$vdir"
    tmp=$(mktemp "$vdir/.tmp.XXXXXX")
    { echo "td-builder-check-memo 1"
      echo "drv $drv"
      echo "env $envid"
      echo "recorded $now"
      grep '^CHECK ' "$sc/out.txt" | sed 's/^CHECK \(.*\) reproducible$/repro \1/'
      printf '%s\n' "$facts" | sed 's/^/output /'
    } > "$tmp"
    mv "$tmp" "$vf"
    echo "   TD-CHECK MEMO RECORD $drv"
  fi
fi
cleanup
