#!/bin/sh
# tests/harness-seed.sh — HARNESS FROM A SEED (host-sandbox-stage0 inc2a, North-Star):
# the loop CONTAINER stands up with NO guix and NO host /gnu/store — the substrate a
# guix-less cloud VM needs to run ci/daily-full-suite.sh.
#
# rust-seed (#134) proved td can BUILD its engine (td-builder) from a frozen rust seed.
# But that gate runs on a guix host where /gnu/store is PRESENT, so it never proved the
# loop's own toolchain (make/bash/coreutils/…) resolves when the host store is ABSENT —
# its own verified-red log flags this gap. This gate closes it: capture the loop
# toolchain closure into a seed, then enter td's host-sandbox with that seed bound AT
# /gnu/store and the host /gnu/store + /var/guix NOT bound (`--store-from`/`--no-daemon`,
# new flags) — and run the real toolchain inside. If the seed weren't self-sufficient the
# toolchain could not run (no host-store fallback), so a green run proves the container
# substrate is guix-free.
#
# SCOPE: the seed is CAPTURED here via guix (the one-time capture SOURCE, run on a guix
# host — exactly as rust-seed/warm-seed do; build-seed-tarball.sh is documented "run ONCE
# on a guix host, not in the loop"). The PORTABLE proof is the CONSUME half — host-sandbox
# --store-from + the in-sandbox toolchain run — which touches no guix at all. Shipping a
# PRE-captured seed so the VM skips the capture is inc2c (daily-suite wiring); swapping
# check.sh's prelude to provision from the seed when guix is absent is inc2b.
#
# Legs (differential + durable discipline):
#   [DURABLE behavioral]  the loop toolchain (make/tar/sed/grep/gzip/find) RUNS inside the
#                         container whose /gnu/store is the SEED — versions print.
#   [DURABLE structural]  inside, /gnu/store IS the seed (a seed path present; a host-only
#                         /gnu/store path — guix itself — ABSENT), guix is NOT resolvable,
#                         and /var/guix is absent. No host store, no daemon, no guix.
#   [REMOVABLE oracle]    the seed toolchain's versions == the host-store toolchain's
#                         (own, then diverge: the seed COPY behaves identically to guix's
#                         original — drop this leg when guix retires).
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }
root=$(pwd)

. tests/cache-lib.sh
export TD_STAGE0_BASE="$root/.td-build-cache/stage0"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder (the harness)"
echo ">> harness td-builder (guix-free stage0): $TB"

# --- Resolve the loop toolchain store paths (the SAME set check.sh provisions) ----------
# guix is the one-time capture SOURCE here (offline, --no-substitutes): it REALIZES +
# prints the toolchain package outputs. Their union closure becomes the seed.
echo ">> realize + enumerate the loop toolchain closure (capture source = host guix, offline)"
roots=$(guix build --no-substitutes --no-offload \
  make bash coreutils sed grep findutils tar gzip crun util-linux sqlite) \
  || fail "could not realize the loop toolchain on the host (capture source)"
roots=$(printf '%s\n' "$roots" | grep '^/gnu/store/' | sort -u)
test -n "$roots" || fail "no toolchain store paths to seed"

# Build the inner PATH from the toolchain roots that carry a /bin (mirrors check.sh's
# profile PATH, package-by-package). bash's absolute path is the in-sandbox command.
seedpath=""
bash_abs=""
for p in $roots; do
  if [ -d "$p/bin" ]; then
    seedpath="${seedpath:+$seedpath:}$p/bin"
    [ -x "$p/bin/bash" ] && bash_abs="$p/bin/bash"
  fi
done
test -n "$seedpath" || fail "no /bin among the toolchain roots"
test -n "$bash_abs"  || fail "no bash among the toolchain roots"

# A sentinel that MUST be in the seed (coreutils — provides the structural probes), and a
# host-only /gnu/store path that must NOT be (guix itself never enters a toolchain closure).
sentinel=$(printf '%s\n' $roots | grep -- '-coreutils-' | head -1)
test -n "$sentinel" || fail "no coreutils root for the seed sentinel"
guixreal=$(readlink -f "$(command -v guix)" 2>/dev/null || true)
hostonly=$(printf '%s' "$guixreal" | sed -E 's#^(/gnu/store/[^/]+).*#\1#')
case "$hostonly" in /gnu/store/*) : ;; *) fail "could not resolve a host-only /gnu/store path (guix) for the absence probe" ;; esac

# --- Warm (capture + unpack ONCE) the toolchain seed into the content-addressed rail -----
echo ">> warm the loop-toolchain seed (capture+unpack once; content-addressed by root set)"
seedline=$(TB="$TB" TD_SEED_DB=/var/guix/db/db.sqlite \
  sh tools/warm-seed.sh "$root/.td-build-cache/seed" $roots) \
  || fail "warm-seed (loop toolchain) failed"
SEED_STORE=$(echo "$seedline" | cut -d' ' -f1)
SEED_DB=$(echo "$seedline" | cut -d' ' -f2)
SEED_MANIFEST=$(echo "$seedline" | cut -d' ' -f3)
test -d "$SEED_STORE" -a -s "$SEED_DB" -a -s "$SEED_MANIFEST" || fail "warm-seed produced no usable toolchain seed"
test -e "$SEED_STORE/$(basename "$sentinel")" || fail "the seed store is missing the sentinel ($(basename "$sentinel"))"
ns=$(grep -c . "$SEED_MANIFEST")
echo "   warmed the loop-toolchain seed: $ns paths at $SEED_STORE"

scratch="$root/.td-build-cache/harness-seed"; rm -rf "$scratch"; mkdir -p "$scratch"

# --- The in-container probe: toolchain runs + the substrate is guix-free -----------------
# Runs as the sandbox's PID 1 with /gnu/store = the SEED. Prints machine-checkable lines.
probe='set -e
echo INNER-START
[ -e "'"$sentinel"'" ] && echo SENTINEL-PRESENT || echo SENTINEL-ABSENT
[ -e "'"$hostonly"'" ] && echo HOSTONLY-PRESENT || echo HOSTONLY-ABSENT
command -v guix >/dev/null 2>&1 && echo GUIX-PRESENT || echo GUIX-ABSENT
[ -e /var/guix ] && echo VARGUIX-PRESENT || echo VARGUIX-ABSENT
for t in make tar sed grep gzip find; do
  # Capture WITHOUT a trailing pipe: v=$(cmd) takes the cmd exit status (a pipe
  # would take head'\''s, always 0, masking a missing tool — a false green). Then
  # require non-empty output. A tool absent from the seed (no host-store fallback)
  # fails here, so the behavioral leg is load-bearing.
  if v=$("$t" --version 2>/dev/null) && [ -n "$v" ]; then
    echo "TOOL $t :: $(printf %s "$v" | head -1)"
  else
    echo "TOOL-FAIL $t"; exit 7
  fi
done
echo INNER-OK'

# --- Run 1: the SEED container (host /gnu/store + /var/guix ABSENT, guix off PATH) -------
echo ">> enter host-sandbox with the SEED store at /gnu/store (--store-from), no daemon (--no-daemon)"
set +e
env PATH="$seedpath" HOME=/tmp \
  "$TB" host-sandbox --store-from "$SEED_STORE" --no-daemon --expose-cwd -- \
  "$bash_abs" -c "$probe" > "$scratch/seed.out" 2>&1
seed_rc=$?
set -e
echo "---- seed-container output ----"; sed 's/^/   | /' "$scratch/seed.out"; echo "-------------------------------"
test "$seed_rc" -eq 0 || fail "the seed container exited $seed_rc (toolchain not self-sufficient from the seed?)"

# Leg: DURABLE behavioral — every loop tool ran inside the seed-only container.
grep -q '^TOOL-FAIL ' "$scratch/seed.out" && fail "a tool failed to run from the seed: $(grep '^TOOL-FAIL ' "$scratch/seed.out")"
grep -q '^INNER-OK$' "$scratch/seed.out" || fail "the in-seed probe did not reach INNER-OK"
for t in make tar sed grep gzip find; do
  grep -q "^TOOL $t :: " "$scratch/seed.out" || fail "$t did not run inside the seed container"
done
echo "   [DURABLE behavioral] make/tar/sed/grep/gzip/find all RAN inside the container — /gnu/store = the seed"

# Leg: DURABLE structural — /gnu/store is the seed; no host store, no guix, no daemon.
grep -q '^SENTINEL-PRESENT$' "$scratch/seed.out" || fail "the seed sentinel was not visible at /gnu/store inside (store not bound from the seed?)"
grep -q '^HOSTONLY-ABSENT$'  "$scratch/seed.out" || fail "a host-only /gnu/store path ($hostonly) was visible inside — /gnu/store is the HOST store, not the seed"
grep -q '^GUIX-ABSENT$'      "$scratch/seed.out" || fail "guix was resolvable inside the seed container"
grep -q '^VARGUIX-ABSENT$'   "$scratch/seed.out" || fail "/var/guix (the daemon socket) was present inside the seed container"
echo "   [DURABLE structural] /gnu/store IS the seed (sentinel present, host-only guix path absent); guix unresolvable; /var/guix absent"

# --- Run 2: the REMOVABLE oracle — same toolchain, host /gnu/store (guix's original) -----
oracle_probe='for t in make tar sed grep gzip find; do echo "TOOL $t :: $("$t" --version 2>/dev/null | head -1)"; done'
env PATH="$seedpath" HOME=/tmp \
  "$TB" host-sandbox --expose-cwd -- \
  "$bash_abs" -c "$oracle_probe" > "$scratch/host.out" 2>&1 \
  || fail "the oracle run (host /gnu/store) failed"
for t in make tar sed grep gzip find; do
  sv=$(grep "^TOOL $t :: " "$scratch/seed.out" | head -1)
  hv=$(grep "^TOOL $t :: " "$scratch/host.out" | head -1)
  test -n "$hv" -a "$sv" = "$hv" || fail "[oracle] $t differs seed-vs-host:\n  seed: $sv\n  host: $hv"
done
echo "   [REMOVABLE oracle] the seed toolchain's versions == the host-store toolchain's (own, then diverge)"

echo "PASS: td's loop CONTAINER stands up from a SEED alone — host /gnu/store + the guix"
echo "      daemon ABSENT, guix off PATH — and the loop toolchain runs inside it. The"
echo "      guix-less substrate ci/daily-full-suite.sh needs on a VM with no guix installed."
