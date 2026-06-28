#!/bin/sh
# tools/resolve-toolchain.sh — the consumer-DEFAULT toolchain resolver.
#
# "Loop substitutes too" (human, 2026-06-28): instead of rebuilding the ~18-rung
# from-seed bootstrap chain (~90 min) every run, a toolchain gate calls this to
# FETCH the lock-keyed signed /td/store toolchain from a PERSISTENT local substitute
# store, verified against a PINNED ed25519 public key. On a verified HIT it restores
# the tree and prints its path (exit 0); on ANY miss (no store, no entry, bad
# signature, wrong StorePath, corrupt nar, restore failure) it prints nothing and
# exits 1 so the caller FALLS BACK to the authoritative from-seed build.
#
# DELIBERATE directive-1 relaxation (human-approved, surfaced in the gate + PR): with
# this resolver the per-PR/local loop no longer builds the toolchain from source — the
# DAILY full suite (ci/daily-full-suite.sh, fresh main) is the SOLE remaining from-seed
# authoritative build AND the publisher of the signed substitute. Trust = the ed25519
# signature (pinned key) + the input-addressed NAME (the lock-computed StorePath); the
# toolchain is not byte-reproducible, so repro-equality is NOT the trust basis here
# (that is task 3). See plan/toolchain-subst-default.md.
#
# Usage:   tools/resolve-toolchain.sh LOCK NAME DEST
#   LOCK   the toolchain's pinned input set (tests/td-toolchain.lock) — derives the key
#   NAME   the component to resolve (e.g. glibc-2.41)
#   DEST   directory to restore the fetched tree into
# Env:
#   TD_SUBST_STORE   persistent signed substitute store dir (REQUIRED; MISS if unset/absent)
#   TD_SUBST_PUBKEY  pinned ed25519 public key, hex (default: tests/td-subst.pub)
#   TD_SUBST_BIN     the td-subst binary (serve/fetch) (REQUIRED)
#   TD_BUILDER       the td-builder binary (toolchain-path/nar-restore) (REQUIRED)
#   TD_STORE_DIR     logical store prefix (default: /td/store)
# stdout (HIT only): the restored toolchain path (DEST/<basename>)
# exit: 0 = HIT (path on stdout), 1 = MISS (caller builds from seed)
set -eu

lock=${1:?usage: resolve-toolchain.sh LOCK NAME DEST}
name=${2:?usage: resolve-toolchain.sh LOCK NAME DEST}
dest=${3:?usage: resolve-toolchain.sh LOCK NAME DEST}

# MISS — log to stderr (never stdout: stdout carries ONLY a HIT path) and fall back.
miss() { [ -n "${1:-}" ] && echo "resolve-toolchain: MISS — $1" >&2; exit 1; }

: "${TD_SUBST_BIN:?TD_SUBST_BIN unset}"
: "${TD_BUILDER:?TD_BUILDER unset}"
pub=${TD_SUBST_PUBKEY:-tests/td-subst.pub}
store=${TD_SUBST_STORE:-}
TD_STORE_DIR=${TD_STORE_DIR:-/td/store}; export TD_STORE_DIR

{ [ -n "$store" ] && [ -d "$store" ]; } || miss "no substitute store (TD_SUBST_STORE=${store:-unset})"
[ -s "$pub" ] || miss "no pinned public key ($pub)"

# 1. Compute the lock-keyed /td/store path — a pure function of the lock's inputs, so a
#    consumer with ONLY the lock can NAME exactly what to fetch (no content-address guess).
path=$("$TD_BUILDER" toolchain-path "$lock" "$name") || miss "toolchain-path failed"
case "$path" in "$TD_STORE_DIR"/*) : ;; *) miss "computed path not under $TD_STORE_DIR: $path" ;; esac
base=$(basename "$path")

# 2. Cheap negative: not in the store at all -> fall back without even serving.
[ -f "$store/$base.narinfo" ] || miss "no entry for $base in $store"

# 3. Serve the store on loopback (offline-allowed) and fetch by basename. `td-subst fetch`
#    verifies the ed25519 signature against the PINNED pub AND the NarHash; a bad sig /
#    wrong key / corrupt nar makes it non-zero -> MISS -> fall back.
work=$(mktemp -d)
spid=
trap 'kill "$spid" 2>/dev/null || true; rm -rf "$work"' EXIT
"$TD_SUBST_BIN" serve "$store" 127.0.0.1:0 >"$work/serve.log" 2>&1 &
spid=$!
port=
i=0
while [ "$i" -lt 100 ]; do
  port=$(sed -n 's#.*http://127.0.0.1:\([0-9]*\)/.*#\1#p' "$work/serve.log" 2>/dev/null)
  [ -n "$port" ] && break
  i=$((i + 1)); sleep 0.1
done
[ -n "$port" ] || miss "serve never bound a loopback port"

"$TD_SUBST_BIN" fetch "http://127.0.0.1:$port" "$base" "$work/fetch" "$pub" >/dev/null 2>"$work/fetch.err" \
  || miss "fetch/verify failed: $(tail -1 "$work/fetch.err" 2>/dev/null)"

# 4. The fetched narinfo's StorePath MUST equal the lock-computed path — the
#    input-addressed NAME is load-bearing alongside the signature (a validly-signed
#    substitute for a DIFFERENT path is not the toolchain we asked for).
fsp=$(grep '^StorePath: ' "$work/fetch/$base.narinfo" | cut -d' ' -f2)
[ "x$fsp" = "x$path" ] || miss "fetched StorePath ($fsp) != lock-computed path ($path)"

# 5. Restore the verified nar into DEST and hand the caller the path.
narfile=$(grep '^NarFile: ' "$work/fetch/$base.narinfo" | cut -d' ' -f2)
mkdir -p "$dest"
"$TD_BUILDER" nar-restore "$work/fetch/$narfile" "$dest/$base" >/dev/null 2>"$work/restore.err" \
  || miss "nar-restore failed: $(tail -1 "$work/restore.err" 2>/dev/null)"

echo "$dest/$base"
