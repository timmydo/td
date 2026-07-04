//! guix-surface — ratchet td's guix-as-PACKAGER seed surface toward zero
//! (move-off-Guile §5 ENFORCEMENT; sibling to the guix-dependence census 070).
//! move-off-Guile removes guix from td's BUILD path: the build tool
//! (td-builder->stage0), the evaluator (td-recipe-eval), the transpiler (node->td-tsgo).
//! The standing rule it makes enforceable: a new external seed is a pinned
//! fixed-output FETCH the loop realises + td PLACES (store-add-recursive), NEVER a
//! guix (build-system ...) package td asks the daemon to build by resolving
//! (@ (system M) PKG). tests/guix-surface.sh statically scans the loop's
//! orchestration sources (Makefile, mk/gates/*.mk, tests/*.sh, ci/*.sh) for that
//! resolve form, classifies each (@ (system M) NAME) by reading system/M.scm (a package
//! define => PACKAGER; an origin/fetch define => an allowed FETCHER), and snapshots
//! the sorted PACKAGER sites in tests/guix-surface.expected.
//! 
//! One-way RATCHET (the DIGESTS pattern, monotone): FAIL if a current packager site
//! is absent from the snapshot (the surface grew — a regression needing sign-off +
//! a deliberate .expected edit per directive 3); PASS when the set only shrinks (a
//! retiring track removed a seed; re-baseline to lock the win). PURELY ADDITIVE —
//! it removes/loosens/skips nothing, records a surface, and fails closed on
//! undocumented growth. Static, offline, no guix invoked => cheap pool, fails fast.
//! Re-baseline: TD_SURFACE_WRITE=1 ./check.sh guix-surface  (commit the .expected).

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "guix-surface",
        pools: &[Pool::Cheap],
        needs: &[],
        build_gate: false,
        specs: &[],
        inputs: &[],
        store: StoreMode::Shared,
        non_blocking: false,
        script: r##"
echo ">> guix-surface: ratchet td's guix-as-packager seed surface (move-off-Guile §5) — new seeds are td-placed fixed-output fetches, not guix-built packages; the snapshot may only shrink"
sh tests/guix-surface.sh
"##,
    }
}
