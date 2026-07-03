//! daemon-recipe — td's OWN persistent build daemon realizes a TD-ASSEMBLED real
//! recipe into td's OWN store (own-builder-daemon increment 8). The maintainer's
//! 2026-06-21 direction (closing #128: shipping goes through td's OWN daemon/store,
//! NOT the guix daemon) done right: td ASSEMBLES the recipe's `.drv` itself
//! (`assemble-recipe` — store::assemble_drv, NO `guix repl`/Guile, NO realize), then
//! the long-running daemon REALIZES that td-assembled drv over a Unix socket — the
//! loop's builder instead of guix-daemon. Unlike gate 358 (whose probe drvs are
//! emitted by `guix repl tests/daemon-drv.scm` — Guile in the daemon's input path),
//! here the drv handed to the daemon is td-native end to end, and the subject is a
//! REAL artifact, not a marker probe. The daemon builds with td's stage0 td-builder
//! (TD_BUILDER_* override — a binary guix never produced), so NO new `guix build -e'
//! packager site (move-off-Guile §5). assemble-recipe + the daemon both run with
//! guix/Guile SCRUBBED FROM PATH — the structural proof the path needs neither. The
//! recipe JSON is a given input here (the recipe-<n>.ts -> JSON lowering by td's OWN
//! evaluator is corpus-no-guix's concern); this gate's novelty is the assemble/realize
//! SPLIT across the daemon, not recipe authoring.
//! 
//! ALL-DURABLE (no guix oracle leg — there is no guix daemon in the realize to diff):
//! [STRUCTURAL move-off-Guile] td ASSEMBLED the drv (no guix repl/Guile), with
//! guix/Guile off PATH; the drv's builder is the td-bootstrapped stage0.
//! [DURABLE structural] the daemon-built output is under td's OWN scratch store
//! (NOT /gnu/store) — td owns the build the daemon served.
//! [DURABLE behavioral] the daemon-built artifact actually runs (hello greets).
//! [DURABLE guix-daemon parity] a 2nd request for the SAME drv is a CACHE HIT —
//! the daemon does not rebuild a valid output (the defining build-daemon property).
//! 
//! Verified-red: (a) skip the daemon (start nothing) → daemon-request reds (connection
//! refused); (b) revert the daemon's cached_realization check → the 2nd request rebuilds
//! (CACHE MISS) and the HIT assertion reds.
//! Reply format: `OK <canon> <host> <hit|built>` — the hit|built suffix landed with the
//! machine-wide limiter (fe58ab5); the old last-token parse here read the status word as
//! the output path (latent red hidden while the daily runner was down, #268). The recipe
//! takes token 3 (the host-side output), like tests/cache-lib.sh and gate 358.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "daemon-recipe",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> daemon-recipe: td's persistent build daemon realizes a TD-ASSEMBLED real recipe (hello) into td's OWN store over a Unix socket (no guix-daemon, no guix repl/Guile emitting the drv); the artifact runs; a repeat request is a cache HIT"
set -euo pipefail; \
lock="$PWD/tests/hello-no-guix.lock"; \
test -s "$lock" || { echo "ERROR: no lock $lock" >&2; exit 1; }; \
cu=`grep -- '-coreutils-' "$lock" | sed 's/^[^ ]* //' | head -1`; \
test -n "$cu" || { echo "ERROR: no coreutils in the lock for the scrubbed PATH" >&2; exit 1; }; \
if ls "$cu/bin" | grep -qE '^(guix|guile)$'; then echo "FAIL: guix/guile on the scrubbed PATH" >&2; exit 1; fi; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
grep ' /gnu/store/' "$lock" | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null || { echo "ERROR: could not realize the hello seed + source (regenerate the lock on a channel bump)" >&2; exit 1; }; \
scratch="$PWD/.td-build-cache/daemon-recipe"; chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; mkdir -p "$scratch/tmp" "$scratch/asm" "$scratch/d"; \
printf '{"name":"hello","version":"2.12.2","buildSystem":"gnu"}\n' > "$scratch/recipe.json"; \
test -s "$scratch/recipe.json" || { echo "ERROR: no recipe JSON" >&2; exit 1; }; \
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    "$tb" assemble-recipe "$scratch/recipe.json" "$lock" "$scratch/asm" > "$scratch/asm.out" 2>"$scratch/asm.err" \
  || { echo "FAIL: assemble-recipe (guix/Guile off PATH):" >&2; tail -20 "$scratch/asm.err" >&2; exit 1; }; \
drv=`sed -n 's/^DRV=//p' "$scratch/asm.out"`; \
test -n "$drv" -a -f "$drv" || { echo "FAIL: assemble-recipe produced no .drv" >&2; cat "$scratch/asm.out" "$scratch/asm.err" >&2; exit 1; }; \
grep -qF "$TD_BUILDER_PATH/bin/td-builder" "$drv" || { echo "FAIL: the td-assembled .drv builder is not the stage0 $TD_BUILDER_PATH" >&2; exit 1; }; \
echo "  [STRUCTURAL move-off-Guile] td ASSEMBLED the .drv ($drv) with guix/Guile off PATH — no guix repl/Guile emitted it; builder = the stage0 td-builder"; \
sock="$scratch/sock"; \
env -i HOME="$scratch" TMPDIR="$scratch/tmp" PATH="$cu/bin" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    "$tb" daemon "$sock" /gnu/store "$scratch/d" > "$scratch/daemon.log" 2>&1 & dpid=$!; \
trap 'kill $dpid 2>/dev/null || true' EXIT; \
tries=0; while [ ! -S "$sock" ] && [ $tries -lt 50 ]; do sleep 0.2; tries=$((tries+1)); done; \
[ -S "$sock" ] || { echo "FAIL: daemon socket never appeared" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
r1=`"$tb" daemon-request "$sock" "$drv"` || { echo "FAIL: request 1 errored: $r1" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
case "$r1" in "OK "*) : ;; *) echo "FAIL: request 1 not OK: $r1" >&2; cat "$scratch/daemon.log" >&2; exit 1 ;; esac; \
set -- $r1; host="$3"; \
case "$host" in "$scratch/d"/*) : ;; *) echo "FAIL: daemon output not under td's scratch store (not a td-owned build): $host" >&2; exit 1 ;; esac; \
case "$host" in /gnu/store/*) echo "FAIL: daemon output is under /gnu/store — not td's own store" >&2; exit 1 ;; esac; \
echo "  [DURABLE structural] the daemon built into td's OWN scratch store ($host) — NOT /gnu/store, NOT guix-daemon"; \
test -x "$host/bin/hello" || { echo "FAIL: no hello binary at $host/bin/hello" >&2; exit 1; }; \
test "`LD_LIBRARY_PATH="$host/lib" "$host/bin/hello"`" = "Hello, world!" || { echo "FAIL: the daemon-built hello did not greet" >&2; exit 1; }; \
echo "  [DURABLE behavioral] the daemon-built hello runs from td's own store output"; \
r2=`"$tb" daemon-request "$sock" "$drv"` || { echo "FAIL: request 2 (PERSISTENCE — same daemon, same drv) errored: $r2" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
case "$r2" in "OK "*) : ;; *) echo "FAIL: request 2 not OK: $r2" >&2; exit 1 ;; esac; \
grep -qF "CACHE HIT for $drv" "$scratch/daemon.log" || { echo "FAIL: the 2nd request was not a cache HIT — the daemon rebuilt a valid output (no guix-daemon parity)" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
echo "  [DURABLE guix-daemon parity] a 2nd request for the same drv is a CACHE HIT — the daemon does not rebuild a valid output"; \
"$tb" daemon-request "$sock" SHUTDOWN >/dev/null 2>&1 || true; \
kill $dpid 2>/dev/null || true; \
chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; \
echo "PASS: td's persistent build daemon realized a TD-ASSEMBLED real recipe (hello) into td's OWN store over a Unix socket — td assembled the drv with no guix repl/Guile, the daemon (not guix-daemon) built it, the artifact runs, and a repeat request is a cache HIT (a valid output is not rebuilt)."
"##,
    }
}
