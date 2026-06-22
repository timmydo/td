#!/bin/sh
# tests/store-ns.sh — user-pm Phase 0: td OWNS ITS OWN ROOT with its own store at /td/store,
# BREAKING FROM GUIX (human 2026-06-21). `td-builder store-ns STORE-DIR -- CMD` enters a
# user namespace pivoted into a minimal td-owned root that binds STORE-DIR at `/td/store`
# and binds NOTHING from /gnu/store or /var/guix — so inside, `/td/store` IS the store and
# the host `/gnu/store` + guix install are ABSENT. Rootless (no daemon, no root). This is
# the unmixed base the /td/store package manager runs in; the dynamic toolchain is relocated
# to /td/store in Phase 2 (here a STATIC binary sidesteps relocation to prove the root).
#
# This gate: assemble a STORE-DIR holding a static binary (bash-static, from hello's seed
# closure — td's own reader, no guix process), run it inside the store-ns, and assert it
# runs from /td/store with /gnu/store ABSENT. td-builder is the guix-free stage0.
#
# Legs:
#   [DURABLE behavioral] a binary runs from /td/store in the own-root (rootless userns)
#   [DURABLE structural] /td/store is the store AND /gnu/store is ABSENT (unmixed from guix)
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
echo ">> td-builder under test (stage0, guix-free): $TB"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

# A static binary to run from /td/store: bash-static, from hello's seed closure (td's own
# store-closure reader over the store db — no guix process). Static ⇒ no RUNPATH, so it runs
# from /td/store before the dynamic toolchain is relocated there (Phase 2).
bash=`grep -- '-bash-' tests/hello-no-guix.lock | grep -v static | sed 's/^[^ ]* //' | head -1`
test -n "$bash" || fail "no bash in hello's lock"
bs=`"$TB" store-closure /var/guix/db/db.sqlite "$bash" | grep -- '-bash-static-' | head -1`
test -n "$bs" -a -x "$bs/bin/bash" || fail "no static bash in the closure of $bash"
# (That it runs in the own-root with /gnu/store absent IS the proof it is self-sufficient —
# a dynamic binary would fail to find its libs, so no separate `file`/`ldd' check needed.)

# The user's /td/store: place the static package at $store/<base>.
store="$work/td-store"; mkdir -p "$store"
base=`basename "$bs"`; cp -a "$bs" "$store/$base"; chmod -R u+w "$store"
echo "   placed $base into the td-owned store $store"

# Run inside the own-root store-ns (rootless): /td/store = $store, /gnu/store absent.
out=`"$TB" store-ns "$store" -- "/td/store/$base/bin/bash" -c '
  [ -d /td/store ] && echo TDSTORE-OK
  [ -d /td/store/'"$base"'/bin ] && echo PKG-AT-TDSTORE
  [ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
  echo "RAN:$BASH_VERSION"
'` || fail "store-ns run exited nonzero"
printf '%s\n' "$out" | sed 's/^/     /'

# --- Leg A: DURABLE behavioral — the binary ran from /td/store -----------------
printf '%s\n' "$out" | grep -q '^RAN:5' || fail "the static binary did not run from /td/store"
printf '%s\n' "$out" | grep -q '^PKG-AT-TDSTORE$' || fail "the package is not at /td/store/<base> inside the root"
echo "   [DURABLE behavioral] a binary ran from /td/store in td's own root (rootless userns)"

# --- Leg B: DURABLE structural — /td/store is the store, /gnu/store ABSENT ------
printf '%s\n' "$out" | grep -q '^TDSTORE-OK$' || fail "/td/store is not present in the own-root"
printf '%s\n' "$out" | grep -q '^GNU-ABSENT$' || fail "/gnu/store is PRESENT in the own-root — mixed with the guix install!"
echo "   [DURABLE structural] /td/store is the store and /gnu/store is ABSENT — unmixed from the local guix install"

echo "PASS: td owns its own root with its own store at /td/store — a static package runs from"
echo "      /td/store in a rootless user namespace with /gnu/store and the guix install ABSENT."
echo "      The unmixed /td/store base the user package manager runs in (user-pm Phase 0)."
