//! build-daemon — td's OWN persistent build daemon serves realize requests over a
//! Unix socket (own-builder-daemon increment 7): a long-running td-builder realizes
//! the derivations the loop hands it, instead of guix-daemon. ONE daemon process
//! serves TWO distinct realize requests over a single socket (persistence — the
//! defining daemon property a one-shot `realize` lacks), each producing td's OWN
//! output (under the scratch store, NOT /gnu/store) with the expected marker, the
//! realize itself daemon-free. DURABLE/behavioral — no guix oracle leg.
//! Verified-red: make build_daemon::serve exit after one request → the 2nd request
//! reds (connection refused / no response).
//! Reply format: `OK <canon> <host> <hit|built>` — the hit|built suffix landed with
//! the machine-wide limiter (fe58ab5) and the old last-token parse here read "built"
//! as the output path (a latent red hidden while the daily runner was down, #268);
//! the recipe takes token 3 (the host-side output) like tests/cache-lib.sh does.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "build-daemon",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> build-daemon: one long-running td-builder serves multiple realize requests over a Unix socket (the loop's builder, not guix-daemon)"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: no td-builder" >&2; exit 1; }; \
scratch="$PWD/.build-daemon-scratch"; chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; mkdir -p "$scratch"; \
$TD_GUIX repl -L . tests/daemon-drv.scm 2>"$scratch/repl.err" > "$scratch/facts.txt" \
  || { echo "FAIL: could not emit/realize the probe drvs" >&2; cat "$scratch/repl.err" >&2; exit 1; }; \
da=`sed -n 's/^DRV_A=//p' "$scratch/facts.txt"`; db_=`sed -n 's/^DRV_B=//p' "$scratch/facts.txt"`; \
test -n "$da" -a -n "$db_" || { echo "FAIL: missing probe facts" >&2; cat "$scratch/facts.txt" >&2; exit 1; }; \
"$tb" drv-emit-to "$da" "$scratch/a.drv" >/dev/null || { echo "FAIL: drv-emit-to A" >&2; exit 1; }; \
"$tb" drv-emit-to "$db_" "$scratch/b.drv" >/dev/null || { echo "FAIL: drv-emit-to B" >&2; exit 1; }; \
sock="$scratch/sock"; \
"$tb" daemon "$sock" /gnu/store "$scratch/d" > "$scratch/daemon.log" 2>&1 & dpid=$!; \
trap 'kill $dpid 2>/dev/null || true' EXIT; \
tries=0; while [ ! -S "$sock" ] && [ $tries -lt 50 ]; do sleep 0.2; tries=$((tries+1)); done; \
[ -S "$sock" ] || { echo "FAIL: daemon socket never appeared" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
ra=`"$tb" daemon-request "$sock" "$scratch/a.drv"` || { echo "FAIL: request A errored: $ra" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
rb=`"$tb" daemon-request "$sock" "$scratch/b.drv"` || { echo "FAIL: request B (PERSISTENCE — 2nd request to the SAME daemon) errored: $rb" >&2; cat "$scratch/daemon.log" >&2; exit 1; }; \
case "$ra" in "OK "*) : ;; *) echo "FAIL: A response not OK: $ra" >&2; exit 1 ;; esac; \
case "$rb" in "OK "*) : ;; *) echo "FAIL: B response not OK: $rb" >&2; exit 1 ;; esac; \
set -- $ra; ha="$3"; set -- $rb; hb="$3"; \
case "$ha" in "$scratch"/*) : ;; *) echo "FAIL: A output not under td's scratch store (not a td-owned build): $ha" >&2; exit 1 ;; esac; \
grep -q "td-daemon-built:a" "$ha/marker" || { echo "FAIL: A marker missing/wrong at $ha/marker" >&2; exit 1; }; \
grep -q "td-daemon-built:b" "$hb/marker" || { echo "FAIL: B marker missing/wrong at $hb/marker" >&2; exit 1; }; \
"$tb" daemon-request "$sock" SHUTDOWN >/dev/null 2>&1 || true; \
kill $dpid 2>/dev/null || true; \
echo ">> [DURABLE: behavioral] one long-running td daemon served TWO distinct realize requests over a Unix socket; each built td's OWN output ($ha, $hb) with the expected marker, no guix-daemon in the realize"; \
chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; \
echo "PASS: td's persistent build daemon realizes derivations served over a Unix socket — one process serves multiple requests (the loop's builder, not guix-daemon)."
"##,
    }
}
