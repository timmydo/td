//! td-check (DESIGN §7.1; gate-2 of the move-off-Guile arc — td OWNS the reproducibility
//! oracle). Prime directive 1: *reproducibility is a test*. td computes that verdict
//! ITSELF — its OWN build daemon realizes the TD-BUILT hello `.drv` in two INDEPENDENT
//! user-namespace sandboxes (the #25 executor, via `realize_drv`) — the build verb's
//! realization plus one fresh rebuild in the CHECK verb — and compares the per-output NAR
//! hashes (the #21/S2 NAR serializer + SHA-256): equal ⇒ reproducible, with NO guix-daemon
//! and NO `guix build --check` anywhere.
//! 
//! R4 (guix-retirement ladder → #261): the SUBJECT is now td-BUILT and the verdict is td's
//! own, so this gate runs with guix OFF PATH. It used to take the hello `.drv` + the
//! daemon-recorded oracle facts from `guix repl`, stage the closure with `guix gc -R`, and
//! cross-check the verdict with `guix build --check`. Now:
//! • SUBJECT: td ASSEMBLES hello's `.drv` (assemble-recipe, guix/Guile off PATH — builder =
//! the bootstrapped stage0 td-builder) and the shared td build daemon builds it
//! (cache-lib `cached_build`; the daemon content-scans the seed /gnu/store for the input
//! closure, #267 — no `guix repl`, no `guix gc`).
//! • VERDICT: cache-lib `cached_check` submits a daemon CHECK — the daemon rebuilds the
//! SAME `.drv` once more and compares its NAR hash against the build it already realized
//! (two independent builds, td's own reproducibility verdict), no `guix build --check`.
//! • BEHAVIORAL: the reproducible hello binary RUNS and prints "Hello, world!".
//! 
//! Directive 3 (called out for sign-off): this DROPS the removable guix DIFFERENTIAL oracles
//! — the `== the daemon's recorded NAR hash` comparison and the `guix build --check` agree
//! leg (both were "prove td's verdict equals guix's", the §5 removable oracle). td's OWN
//! double-build verdict — the actual feature — is KEPT and is now the whole gate. Nothing is
//! loosened: the reproducibility assertion is unchanged (two independent builds must be
//! NAR-equal), plus a NEW durable behavioral leg (the reproducible binary runs); only the
//! guix cross-checks are gone.
//! 
//! Heavy (a stage0 td-builder + a hello build + the daemon's double-build) → heavy pool;
//! BUILD_GATE so the build-recipes prelude warms hello + td-recipe-eval (daemon cache-hit).
//! Per-gate scratch (.td-check-scratch), removed on green, kept on red for triage.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-check",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> td-check: td computes the reproducibility verdict ITSELF — its build daemon builds the TD-BUILT hello .drv in two independent userns sandboxes (the realized build + one fresh rebuild), NAR-equal; guix off PATH, no guix build --check"
set -euo pipefail; \
. tests/cache-lib.sh; \
export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
CU=`grep -- '-coreutils-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`; export CU; \
test -n "$CU" || { echo "ERROR: no coreutils in tests/hello-no-guix.lock" >&2; exit 1; }; \
export CACHE="$PWD/.td-check-scratch"; chmod -R u+w "$CACHE" 2>/dev/null || true; rm -rf "$CACHE"; mkdir -p "$CACHE"; \
echo ">> td BUILDS the subject hello (assemble-recipe + the shared td daemon; guix off PATH, input closure content-scanned)"; \
cached_build hello tests/hello-no-guix.lock || exit 1; \
test -n "${out:-}" -a -n "${ns:-}" || { echo "FAIL: cached_build hello set no out/ns" >&2; exit 1; }; \
drvf=`ls "$sd/b/"*.drv 2>/dev/null | head -1`; \
test -n "$drvf" || { echo "FAIL: no assembled hello .drv under $sd/b" >&2; exit 1; }; \
echo "   subject .drv (td-assembled, builder=stage0): $drvf  ->  output $out"; \
echo ">> td's OWN reproducibility verdict: the daemon rebuilds the .drv once more and compares its per-output NAR hashes against the build it already realized — two independent builds (no guix build --check)"; \
cached_check hello || { echo "FAIL: td's double-build verdict says hello is NON-reproducible" >&2; exit 1; }; \
echo ">> behavioral: the reproducible hello binary RUNS"; \
got=`"$ns/bin/hello"`; \
test "$got" = "Hello, world!" || { echo "FAIL: the td-built hello printed '$got', expected 'Hello, world!'" >&2; exit 1; }; \
echo "   [DURABLE behavioral] the reproducible hello prints '$got'"; \
chmod -R u+w "$CACHE" 2>/dev/null || true; rm -rf "$CACHE"; \
echo "PASS: td computed the reproducibility verdict ITSELF — its OWN build daemon realized the TD-ASSEMBLED hello .drv (builder = bootstrapped stage0, guix/Guile off PATH) in two independent userns sandboxes (the realized build + one fresh rebuild) to a byte-identical NAR, and the reproducible binary runs ('Hello, world!'). No guix-daemon, no guix repl, no guix gc, no guix build --check anywhere — the removable guix differential oracles were dropped (directive 3); td's own two-independent-build verdict is the whole gate."
"##,
    }
}
