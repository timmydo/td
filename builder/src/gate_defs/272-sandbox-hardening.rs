//! sandbox-hardening (the loop-sandbox honesty fixes). Behavioral self-tests that
//! td's loop container (`td-builder host-sandbox`) is actually isolated and cleans
//! up after itself — the two High findings:
//! (A) MINIMAL /dev — the sandbox exposes only the standard char devices, NOT the
//! host device tree (no /dev/kmsg kernel-log leak, no /dev/kvm, raw disks,
//! /dev/mem, input devices). Re-add the blanket host /dev rbind and (A) reds.
//! (B) ORPHAN REAPING — killing the top-level td-builder SIGKILL-cascades via
//! PR_SET_PDEATHSIG so the inner PID-1 tree (build + mounts) is fully reaped,
//! not left running on a CI cancel/timeout. Drop the pdeathsig arming and (B)
//! reds (descendants survive the kill).
//! Heavy (a td-builder compile + nested-sandbox probes), in the heavy pool.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "sandbox-hardening",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Private, // cold by design (#317 audit): the sandbox isolation probe must not see warm state
        non_blocking: true,
        script: r##"
echo ">> sandbox-hardening: td's loop sandbox has a minimal /dev (no host device leak) and reaps its inner tree when killed"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
bash tests/sandbox-hardening.sh "$tb"
"##,
    }
}
