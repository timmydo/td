//! loop-rung (DESIGN §7.1; gate-2 "Loop tooling convergence"). Where loop-sandbox checks
//! the BASE sandbox surface, this checks the FULL loop env that `td-builder host-sandbox
//! --expose-cwd` provides — the worktree/cwd bound (rw), the cgroup hierarchy + the guix
//! cache, the caller's PATH (the toolchain, all /gnu/store), TD_SUBST_*/TD_DAEMON_*/USER
//! preserved, chdir into the cwd. INTRINSIC self-test (no guix shell -C oracle — the human's
//! direction 2026-06-14): the `eval` gate's exact command (`$(GUIX) repl $(LOAD)
//! tests/eval.scm` — loads every system/test module + prints "eval ok") RUNS and prints
//! "eval ok" inside td's --expose-cwd sandbox. That a real module-loading gate succeeds
//! proves the worktree + toolchain + cache are all correctly exposed; drop the cwd bind
//! and the modules vanish → the eval fails → red (self-discriminating). Heavy (a
//! td-builder compile + a guix repl eval), in the heavy pool.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "loop-rung",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        script: r##"
echo ">> loop-rung: a REAL gate (eval) runs + prints 'eval ok' inside td's full-env sandbox (--expose-cwd) — intrinsic, no guix shell -C oracle"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
user="${USER:-`id -un 2>/dev/null || echo nobody`}"; \
scratch="$PWD/.loop-rung-scratch"; rm -rf "$scratch"; mkdir -p "$scratch"; \
echo ">> the eval gate's exact command inside td's host-sandbox --expose-cwd (worktree + toolchain + cache exposed, chdir'd in)"; \
td=`USER="$user" "$tb" host-sandbox --expose-cwd -- $TD_GUIX repl -L . tests/eval.scm 2>"$scratch/td.err"` \
  || { echo "FAIL: the eval gate FAILED inside td's sandbox (stderr below) — the --expose-cwd full-env exposure is incomplete" >&2; cat "$scratch/td.err" >&2; exit 1; }; \
echo "   td stdout: [$td]"; \
test "$td" = "eval ok" \
  || { echo "FAIL: the eval gate printed [$td] inside td's sandbox, expected 'eval ok' — the full loop env (worktree modules) is not correctly exposed" >&2; exit 1; }; \
rm -rf "$scratch"; \
echo "PASS: a REAL loop gate (eval — loads every system/test module + prints 'eval ok', exit 0) ran inside td's OWN --expose-cwd sandbox (worktree + toolchain + cache + cgroups exposed, chdir'd into the worktree); intrinsic self-test that td's sandbox provides the full loop env, no guix shell -C oracle."
"##,
    }
}
