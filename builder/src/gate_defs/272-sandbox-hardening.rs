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
//! (C) INPUT-ONLY STORE — bounded item counts, and the bound items reject writes.
//! (D) CONSTRUCTION-WINDOW REAPING — the same cascade when the kill lands while
//! the tree is still being built, which is where PID 1 has been forked but has
//! not yet armed. Unlike the others this SAMPLES a race rather than deciding
//! one: a red means the window is open, a green does not prove it shut.
//! Heavy (a td-builder compile + nested-sandbox probes), in the heavy pool.
//!
//! `non_blocking`, which matters most for (D): a red here is tolerated by the
//! run, so the one leg whose entire value is regression detection is also the
//! one whose failure can go unread.
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
