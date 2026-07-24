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
//!
//! Native (#318 axis 3): the gate body is typed Rust in `gate_bodies::sandbox_hardening`;
//! `script: ""` marks it native, so the runner execs `td-builder gate-body sandbox-hardening`.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "sandbox-hardening",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        non_blocking: true,
        script: "",
    }
}
