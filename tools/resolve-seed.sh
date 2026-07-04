#!/bin/sh
# tools/resolve-seed.sh — realize a pinned /gnu/store SEED lock WITHOUT a guix process
# (#311: seed realizations via td-subst, not `guix build`).
#
# The loop's host prelude (tests/cache-lib.sh provision_stage0) used to run
# `guix build <lock paths>` when a pinned seed path was missing. This resolver replaces
# that guix-process dependency: a missing seed root is FETCHED from td's own signed
# substitute store (td-subst — the same ~/.td/subst mechanism the toolchain gates use,
# published by ci/daily-full-suite.sh via tools/publish-seed-subst.sh) and restored by
# td's own nar-restore, CLOSURE AND ALL (each fetched narinfo's References basenames are
# walked breadth-first, so a cold host gets the full runtime closure — exactly what
# `guix build` used to realize). Trust = the ed25519 signature (pinned key) + the
# expected NAME (every fetched narinfo's StorePath must be the /gnu/store path asked
# for). Seed BYTES stay the guix-built pin (retired last, per the north star); only the
# guix PROCESS is gone.
#
# FAIL-CLOSED: if a seed path is missing and the substitute store cannot supply it (no
# store, no entry, bad signature, wrong StorePath, restore failure), exit non-zero with
# a clear message. There is NO guix fallback — warm the host store out-of-band or
# publish the seed substitutes (the daily does).
#
# Usage: resolve-seed.sh LOCK
#   LOCK lines: `NAME /gnu/store/<base>` (the pinned seed roots; other lines ignored)
# Env:
#   TD_SEED_ROOT    where seed trees must exist / are restored (default /gnu/store;
#                   tests point it at a scratch dir)
#   TD_SUBST_BIN / TD_SUBST_STORE / TD_SUBST_PUBKEY
#                   the substitute-store exposure; when TD_SUBST_BIN is unset it is
#                   derived via tools/warm-subst.sh (the daily-populated ~/.td/subst),
#                   so the check prelude needs no pre-exported env
#   TD_BUILDER      td-builder for nar-restore; else builder/target/release/td-builder,
#                   the pre-placed stage0, or `td-builder` on PATH
# exit: 0 = every lock root present under TD_SEED_ROOT (already there, or fetched);
#       1 = fail closed (message on stderr)
set -eu

lock=${1:?usage: resolve-seed.sh LOCK}
root=${TD_SEED_ROOT:-/gnu/store}

fail() { echo "resolve-seed: FAIL — $1" >&2; exit 1; }

roots=`sed -n 's/^[^ ]* \(\/gnu\/store\/[^ ]*\)$/\1/p' "$lock" 2>/dev/null` || roots=""
[ -n "$roots" ] || fail "no /gnu/store seed paths in $lock (missing/malformed lock — regenerate it on a channel bump)"

# The common warm path: every root already present — nothing to fetch, no server spawned.
missing=""
for p in $roots; do
  [ -e "$root/${p##*/}" ] || missing="$missing $p"
done
[ -z "$missing" ] && exit 0

# The substitute-store exposure: explicit env wins; otherwise derive it the same way the
# check prelude does (tools/warm-subst.sh echoes the exports iff ~/.td/subst is usable).
if [ -z "${TD_SUBST_BIN:-}" ]; then
  _exp=`sh "$(dirname "$0")/warm-subst.sh" 2>/dev/null || true`
  [ -n "$_exp" ] && eval "$_exp"
fi
{ [ -n "${TD_SUBST_BIN:-}" ] && [ -x "${TD_SUBST_BIN:-}" ] \
  && [ -n "${TD_SUBST_STORE:-}" ] && [ -d "${TD_SUBST_STORE:-}" ]; } \
  || fail "pinned seed path(s) missing under $root:
$(printf '  %s\n' $missing)
and no td-subst substitute store is exposed (TD_SUBST_BIN/TD_SUBST_STORE unset, and no
daily-populated ~/.td/subst for tools/warm-subst.sh to expose). The loop no longer
realizes seeds with a guix process (#311) — warm this host's store out-of-band, or
publish the seed substitutes (tools/publish-seed-subst.sh in the daily) and retry."
pub=${TD_SUBST_PUBKEY:-tests/td-subst.pub}
[ -s "$pub" ] || fail "no pinned public key ($pub)"

# td-builder for nar-restore: explicit, the host build, the pre-placed stage0, PATH.
tb=${TD_BUILDER:-}
[ -n "$tb" ] || { [ -x builder/target/release/td-builder ] && tb=builder/target/release/td-builder; } || true
[ -n "$tb" ] || tb=`ls .td-build-cache/stage0/store/*/bin/td-builder 2>/dev/null | head -1 || true`
[ -n "$tb" ] || tb=`command -v td-builder 2>/dev/null || true`
{ [ -n "$tb" ] && [ -x "$tb" ]; } || fail "no td-builder for nar-restore (set TD_BUILDER)"

mkdir -p "$root" 2>/dev/null || true
{ [ -d "$root" ] && [ -w "$root" ]; } \
  || fail "seed root $root is not a writable directory — cannot restore the fetched seed into it"

work=`mktemp -d`
spid=
trap 'kill "$spid" 2>/dev/null || true; rm -rf "$work"' EXIT

# One loopback server for the whole walk; td-subst fetch verifies sig + NarHash per item.
"$TD_SUBST_BIN" serve "$TD_SUBST_STORE" 127.0.0.1:0 >"$work/serve.log" 2>&1 &
spid=$!
port=
i=0
while [ "$i" -lt 100 ]; do
  port=`sed -n 's#.*http://127.0.0.1:\([0-9]*\)/.*#\1#p' "$work/serve.log" 2>/dev/null`
  [ -n "$port" ] && break
  i=$((i + 1)); sleep 0.1
done
[ -n "$port" ] || fail "td-subst serve never bound a loopback port"

# BFS over basenames: the missing roots, then each fetched narinfo's References — so the
# restored set is ref-closed (what `guix build` used to guarantee).
queue=""
for p in $missing; do queue="$queue ${p##*/}"; done
seen=" "
restored=0
while :; do
  queue="${queue# }"
  [ -n "$queue" ] || break
  case "$queue" in *" "*) base="${queue%% *}"; queue="${queue#* }" ;; *) base="$queue"; queue="" ;; esac
  case "$seen" in *" $base "*) continue ;; esac
  seen="$seen$base "
  [ -e "$root/$base" ] && continue
  [ -f "$TD_SUBST_STORE/$base.narinfo" ] \
    || fail "no substitute entry for $base in $TD_SUBST_STORE (the seed closure is not published — run the daily's tools/publish-seed-subst.sh, or warm this host out-of-band)"
  fd="$work/f$restored"
  "$TD_SUBST_BIN" fetch "http://127.0.0.1:$port" "$base" "$fd" "$pub" >/dev/null 2>"$work/fetch.err" \
    || fail "fetch/verify of $base failed (bad signature vs $pub, or corrupt nar): $(tail -1 "$work/fetch.err" 2>/dev/null)"
  # The NAME is load-bearing alongside the signature: a validly-signed substitute for a
  # DIFFERENT store path is not the seed we asked for.
  fsp=`grep '^StorePath: ' "$fd/$base.narinfo" | cut -d' ' -f2`
  [ "x$fsp" = "x/gnu/store/$base" ] || fail "fetched StorePath ($fsp) != the expected /gnu/store/$base"
  narfile=`grep '^NarFile: ' "$fd/$base.narinfo" | cut -d' ' -f2`
  [ -n "$narfile" ] && [ -f "$fd/$narfile" ] || fail "fetched narinfo for $base names no nar file"
  # Restore next to the final name, then rename into place — atomic on one filesystem,
  # and a concurrent resolver winning the race is a skip, not an error.
  tmp="$root/.resolve-seed.$$.$base"
  rm -rf "$tmp"
  "$tb" nar-restore "$fd/$narfile" "$tmp" >/dev/null 2>"$work/restore.err" \
    || { rm -rf "$tmp"; fail "nar-restore of $base failed: $(tail -1 "$work/restore.err" 2>/dev/null)"; }
  if [ -e "$root/$base" ]; then
    rm -rf "$tmp"
  else
    mv "$tmp" "$root/$base" 2>/dev/null \
      || { rm -rf "$tmp"; [ -e "$root/$base" ] || fail "could not place $base under $root"; }
  fi
  restored=$((restored + 1))
  refs=`sed -n 's/^References: //p' "$fd/$base.narinfo"`
  [ -n "$refs" ] && queue="$queue $refs"
done

for p in $roots; do
  [ -e "$root/${p##*/}" ] || fail "seed root $p still missing after substitute resolution"
done
echo "resolve-seed: fetched + restored $restored path(s) into $root (ed25519 sig + StorePath + NarHash verified; no guix process)" >&2
