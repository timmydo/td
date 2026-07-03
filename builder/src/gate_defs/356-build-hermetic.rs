//! build-hermetic — td's build SANDBOX is isolated from the loop container
//! (own-builder-daemon increments 5 + 6). A derivation realized by `td-builder
//! realize` cannot reach the loop in two ways the probe drv's builder ERRORS on:
//! (a) FILESYSTEM — /var/guix (the guix daemon db/socket/gc-roots, bound rw into
//! the loop container, never wanted in a build) must be ABSENT; holds only
//! because sandbox::build pivot_roots into a minimal root.
//! (b) PID NAMESPACE — the launching `td-builder' process (and the loop's process
//! tree) must be INVISIBLE in /proc; holds only because sandbox::build unshares
//! NEWPID, forks the builder to PID 1, and mounts a fresh procfs.
//! DURABLE/behavioral — no guix oracle leg: both hold with no daemon in the room (the
//! loop's daemon state AND process tree are ABSENT from the build). Verified-red:
//! drop the pivot_root → realize fails (build sees /var/guix); drop NEWPID → realize
//! fails (build sees the launching td-builder in /proc).

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "build-hermetic",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> build-hermetic: a td-realized build cannot see /var/guix or the loop's process tree — sandbox::build pivot_roots into a minimal root AND unshares NEWPID"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: no td-builder" >&2; exit 1; }; \
scratch="$PWD/.build-hermetic-scratch"; chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; mkdir -p "$scratch"; \
$TD_GUIX repl -L . tests/build-hermetic-drv.scm 2>"$scratch/repl.err" > "$scratch/facts.txt" \
  || { echo "FAIL: could not emit/realize the probe drv" >&2; cat "$scratch/repl.err" >&2; exit 1; }; \
drv=`sed -n 's/^PROBE_DRV=//p' "$scratch/facts.txt"`; \
out=`sed -n 's/^PROBE_OUT=//p' "$scratch/facts.txt"`; \
test -n "$drv" -a -n "$out" || { echo "FAIL: missing probe facts" >&2; cat "$scratch/facts.txt" >&2; exit 1; }; \
"$tb" drv-emit-to "$drv" "$scratch/emitted.drv" >/dev/null || { echo "FAIL: drv-emit-to" >&2; exit 1; }; \
if "$tb" realize "$scratch/emitted.drv" /gnu/store "$scratch/b" > "$scratch/out.txt" 2> "$scratch/realize.err"; then :; \
else echo "FAIL: realize errored — the probe builder saw /var/guix OR the launching td-builder in /proc (build-sandbox isolation regression: sandbox::build did not pivot the host fs away, or did not unshare NEWPID + mount a fresh /proc)" >&2; tail -8 "$scratch/realize.err" >&2; exit 1; fi; \
grep -qx "path $out" "$scratch/b/registration" || { echo "FAIL: probe output $out not registered" >&2; cat "$scratch/b/registration" >&2; exit 1; }; \
echo ">> [DURABLE: behavioral] td realized the probe with NO /var/guix reachable AND the loop's process tree invisible in the build sandbox (no guix oracle — the assertions are that the daemon state and the launching td-builder are absent from the build)"; \
chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; \
echo "PASS: td's build sandbox is isolated from the loop — a realized build cannot reach /var/guix (the rest of the invoking filesystem) nor see the loop's process tree; sandbox::build pivot_roots into a minimal root AND unshares NEWPID."
"##,
    }
}
