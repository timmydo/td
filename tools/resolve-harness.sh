#!/bin/sh
# tools/resolve-harness.sh — the guix-less-runner /td/store harness resolver (#314).
#
# A runner with an EMPTY .td-build-cache/harness (no local heavy build, and no guix to build
# one) FETCHES the whole /td/store harness — the busybox+make set, the staged C toolchain, the
# /td/store/ld loader, and the `rel` + `toolchain` metadata — as ONE signed nar from a PERSISTENT
# substitute store, verified against a PINNED ed25519 public key. On a verified HIT it restores
# the tree into DEST (atomic swap) and prints DEST (exit 0); on ANY miss (no store, no entry, bad
# signature, wrong StorePath, corrupt nar, restore failure) it prints nothing and exits 1 so
# `td-builder check check-harness` FAILS CLOSED with its provisioning message. There is NO
# from-source fallback on a guix-less runner — shipping the harness IS the provisioning path.
#
# Mirrors tools/resolve-toolchain.sh, but the harness is a fixed-name WHOLE-TREE substitute
# (`td-harness`), not a lock-keyed per-path closure: it is a content-addressed build output with
# no lock to recompute a name from, so trust rests on the ed25519 signature + the signed NarHash.
# The daily republishes it every green run, so a signed downgrade is bounded and still a real td
# harness. StorePath is still checked (== the fixed name) so a validly-signed substitute for a
# DIFFERENT path is rejected — the same defense resolve-toolchain applies to its lock path.
#
# Usage:   tools/resolve-harness.sh DEST
#   DEST   the harness dir to (re)create with store/ + rel + toolchain (e.g. .td-build-cache/harness)
# Env:
#   TD_SUBST_STORE   persistent signed substitute store dir (REQUIRED; MISS if unset/absent)
#   TD_SUBST_PUBKEY  pinned ed25519 public key, hex (default: tests/td-subst.pub)
#   TD_SUBST_BIN     the td-subst binary (serve/fetch) (REQUIRED)
#   TD_BUILDER       the td-builder binary (nar-restore) (REQUIRED)
#   TD_STORE_DIR     logical store prefix (default: /td/store)
# stdout (HIT only): DEST
# exit: 0 = HIT (harness restored at DEST), 1 = MISS (caller fails closed)
set -eu

dest=${1:?usage: resolve-harness.sh DEST}

# MISS — log to stderr (never stdout: stdout carries ONLY a HIT path) and fail closed.
miss() { [ -n "${1:-}" ] && echo "resolve-harness: MISS — $1" >&2; exit 1; }

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

: "${TD_SUBST_BIN:?TD_SUBST_BIN unset}"
: "${TD_BUILDER:?TD_BUILDER unset}"
pub=${TD_SUBST_PUBKEY:-tests/td-subst.pub}
store=${TD_SUBST_STORE:-}
TD_STORE_DIR=${TD_STORE_DIR:-/td/store}; export TD_STORE_DIR
name=td-harness
want="$TD_STORE_DIR/$name"

{ [ -n "$store" ] && [ -d "$store" ]; } || miss "no substitute store (TD_SUBST_STORE=${store:-unset})"
[ -s "$pub" ] || miss "no pinned public key ($pub)"

# Cheap negative: not in the store at all -> fall through without even serving.
[ -f "$store/$name.narinfo" ] || miss "no $name entry in $store"

# Serve the store on loopback (offline-allowed) and fetch by the fixed name. `td-subst fetch`
# verifies the ed25519 signature against the PINNED pub AND the NarHash; a bad sig / wrong key /
# corrupt nar makes it non-zero -> MISS -> fail closed.
work=$(mktemp -d)
spid=
trap 'kill "$spid" 2>/dev/null || true; rm -rf "$work"' EXIT
"$TD_SUBST_BIN" serve "$store" 127.0.0.1:0 >"$work/serve.log" 2>&1 &
spid=$!
port=
i=0
while [ "$i" -lt 100 ]; do
  port=$(serve_port "$work/serve.log" 2>/dev/null || true)
  [ -n "$port" ] && break
  i=$((i + 1)); sleep 0.1
done
[ -n "$port" ] || miss "serve never bound a loopback port"

"$TD_SUBST_BIN" fetch "http://127.0.0.1:$port" "$name" "$work/fetch" "$pub" >/dev/null 2>"$work/fetch.err" \
  || miss "fetch/verify failed: $(tail -1 "$work/fetch.err" 2>/dev/null)"

# The fetched narinfo's StorePath MUST equal the fixed harness name — a validly-signed
# substitute for a DIFFERENT path is not the harness we asked for.
fsp=$(narinfo_field StorePath "$work/fetch/$name.narinfo" 2>/dev/null || true)
[ "x$fsp" = "x$want" ] || miss "fetched StorePath ($fsp) != $want"

narfile=$(narinfo_field NarFile "$work/fetch/$name.narinfo" 2>/dev/null || true)

# Restore the WHOLE tree into a sibling temp, sanity-check the harness shape, then atomically
# swap it into DEST (mirrors gate 420's persist: assemble beside, then swap into place).
mkdir -p "$(dirname "$dest")"
htmp="$dest.tmp.$$"
rm -rf "$htmp"
"$TD_BUILDER" nar-restore "$work/fetch/$narfile" "$htmp" >/dev/null 2>"$work/restore.err" \
  || { rm -rf "$htmp"; miss "nar-restore failed: $(tail -1 "$work/restore.err" 2>/dev/null)"; }
{ [ -d "$htmp/store" ] && [ -s "$htmp/rel" ]; } \
  || { rm -rf "$htmp"; miss "restored tree is not a harness (missing store/ or rel)"; }
if [ -e "$dest" ]; then rm -rf "$dest.old.$$"; mv "$dest" "$dest.old.$$"; fi
mv "$htmp" "$dest"
rm -rf "$dest.old.$$" 2>/dev/null || true

echo "$dest"
