#!/bin/sh
# tools/resolve-seed.sh — realize a pinned /gnu/store SEED lock WITHOUT a guix process
# (#311: seed realizations via td-subst, not `guix build`).
#
# The loop's host prelude (tests/cache-lib.sh provision_stage0) used to run
# `guix build <lock paths>` when a pinned seed path was missing. This resolver replaces
# that guix-process dependency: a missing seed root is FETCHED from td's own signed
# substitute store (td-subst — the same ~/.td/subst mechanism the toolchain gates use,
# published by td-builder daily via tools/publish-seed-subst.sh) and restored by
# td's own nar-restore, walking each fetched narinfo's References breadth-first so the
# fetched set is ref-closed. A tree already PRESENT under the seed root is trusted as
# complete and not descended into — the same trust the retired presence check placed in
# present roots. Every fetched tree is restored to a pid-scoped temp next to its final
# name and PLACED only after the whole walk, members before lock roots — so an
# interrupted resolve never leaves a root present with its closure incomplete (a dead
# resolver can leave stale `.resolve-seed.<pid>.*` temps under the root; they are inert
# and re-resolving ignores them). Trust = the ed25519 signature (pinned key) + the
# expected NAME (every fetched narinfo's StorePath must be the /gnu/store path asked
# for). Seed BYTES stay the guix-built pin (retired last, per the north star); only the
# guix PROCESS is gone.
#
# FAIL-CLOSED: if a seed path is missing and the substitute store cannot supply it (no
# store, no entry, bad signature, wrong StorePath, restore failure), exit non-zero with
# a clear message. There is NO guix fallback — warm the host store out-of-band or
# publish the seed substitutes (the daily does). Deliberately NOT gated on
# TD_SUBST_FORCE_BUILD (the daily's "always build from seed" knob): the guix-built seed
# has no from-source alternative for the daily to protect, so fetching it never
# undermines the daily's from-seed authority over the toolchain.
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

narinfo_field() {
  _nf_key=$1
  _nf_file=$2
  while IFS= read -r _nf_line; do
    case "$_nf_line" in
      "$_nf_key":\ *) printf '%s\n' "${_nf_line#*: }"; return 0 ;;
    esac
  done < "$_nf_file"
  return 1
}

serve_port() {
  _sp_file=$1
  while IFS= read -r _sp_line; do
    case "$_sp_line" in
      *http://127.0.0.1:*)
        _sp_rest=${_sp_line#*http://127.0.0.1:}
        _sp_port=${_sp_rest%%/*}
        case "$_sp_port" in ''|*[!0-9]*) ;; *) printf '%s\n' "$_sp_port"; return 0 ;; esac
        ;;
    esac
  done < "$_sp_file"
  return 1
}

roots=
while IFS=' ' read -r _name _path _rest; do
  case "$_path" in /gnu/store/*) roots="$roots $_path" ;; esac
done < "$lock"
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
  _exp=$(sh "$(dirname "$0")/warm-subst.sh" 2>/dev/null || true)
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
trap 'kill "$spid" 2>/dev/null || true; rm -rf "$work" "$root"/.resolve-seed.$$.* 2>/dev/null || true' EXIT

# One loopback server for the whole walk; td-subst fetch verifies sig + NarHash per item.
"$TD_SUBST_BIN" serve "$TD_SUBST_STORE" 127.0.0.1:0 >"$work/serve.log" 2>&1 &
spid=$!
port=
i=0
while [ "$i" -lt 100 ]; do
  port=`serve_port "$work/serve.log" 2>/dev/null || true`
  [ -n "$port" ] && break
  i=$((i + 1)); sleep 0.1
done
[ -n "$port" ] || fail "td-subst serve never bound a loopback port"

rootbases=" "
for p in $roots; do rootbases="$rootbases${p##*/} "; done

# Walk over basenames: the missing roots, then each fetched narinfo's References. Every
# fetched tree is verified and restored to `.resolve-seed.<pid>.<base>` next to its
# final name (same filesystem, so placement below is an atomic rename); each nar is
# dropped right after its restore, bounding transient disk to one nar at a time.
queue=""
for p in $missing; do queue="$queue ${p##*/}"; done
seen=" "
members=""
rootlist=""
fetched=0
while :; do
  queue="${queue# }"
  [ -n "$queue" ] || break
  case "$queue" in *" "*) base="${queue%% *}"; queue="${queue#* }" ;; *) base="$queue"; queue="" ;; esac
  case "$seen" in *" $base "*) continue ;; esac
  seen="$seen$base "
  [ -e "$root/$base" ] && continue
  [ -f "$TD_SUBST_STORE/$base.narinfo" ] \
    || fail "no substitute entry for $base in $TD_SUBST_STORE (the seed closure is not published — run the daily's tools/publish-seed-subst.sh, or warm this host out-of-band)"
  fd="$work/f$fetched"
  "$TD_SUBST_BIN" fetch "http://127.0.0.1:$port" "$base" "$fd" "$pub" >/dev/null 2>"$work/fetch.err" \
    || fail "fetch/verify of $base failed (bad signature vs $pub, or corrupt nar): $(tail -1 "$work/fetch.err" 2>/dev/null)"
  # The NAME is load-bearing alongside the signature: a validly-signed substitute for a
  # DIFFERENT store path is not the seed we asked for.
  fsp=`narinfo_field StorePath "$fd/$base.narinfo" 2>/dev/null || true`
  [ "x$fsp" = "x/gnu/store/$base" ] || fail "fetched StorePath ($fsp) != the expected /gnu/store/$base"
  narfile=`narinfo_field NarFile "$fd/$base.narinfo" 2>/dev/null || true`
  [ -n "$narfile" ] && [ -f "$fd/$narfile" ] || fail "fetched narinfo for $base names no nar file"
  tmp="$root/.resolve-seed.$$.$base"
  rm -rf "$tmp"
  "$tb" nar-restore "$fd/$narfile" "$tmp" >/dev/null 2>"$work/restore.err" \
    || { rm -rf "$tmp"; fail "nar-restore of $base failed: $(tail -1 "$work/restore.err" 2>/dev/null)"; }
  rm -f "$fd/$narfile"
  refs=`narinfo_field References "$fd/$base.narinfo" 2>/dev/null || true`
  [ -n "$refs" ] && queue="$queue $refs"
  case "$rootbases" in
    *" $base "*) rootlist="$rootlist $base" ;;
    *) members="$members $base" ;;
  esac
  fetched=$((fetched + 1))
done

# Place members first, lock roots LAST, so a root only ever appears with its whole
# fetched closure already in place. `mv -T` FAILS rather than nesting the temp into a
# concurrently-placed tree — losing that race is a skip, not an error.
place() {
  _t="$root/.resolve-seed.$$.$1"
  if [ -e "$root/$1" ]; then rm -rf "$_t"; return 0; fi
  mv -T "$_t" "$root/$1" 2>/dev/null \
    || { rm -rf "$_t"; [ -e "$root/$1" ] || fail "could not place $1 under $root"; }
}
for b in $members $rootlist; do place "$b"; done

for p in $roots; do
  [ -e "$root/${p##*/}" ] || fail "seed root $p still missing after substitute resolution"
done
echo "resolve-seed: fetched + restored $fetched path(s) into $root (ed25519 sig + StorePath + NarHash verified; no guix process)" >&2
