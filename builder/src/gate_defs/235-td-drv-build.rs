//! td-drv-build (DESIGN §7.1; the §5 move-off-Guile arc). td-builder EMITS a canonical
//! hello `.drv` AND EXECUTES the emitted file in its own user-namespace sandbox — construct
//! (#22 drv-emit) AND execute (#21 autotools-build via `td-builder build`) are td's Rust,
//! with NO guile and NO guix anywhere.
//! 
//! R4 slice 2b (guix-retirement ladder → #261): this gate USED to be a differential vs guix
//! — it lowered hello with `guix repl` (subject `.drv` + daemon-recorded oracle facts),
//! staged the closure with `guix gc -R`, asserted td's emitted `.drv` was byte-identical to
//! GUIX's, and that the executed output equalled the daemon's recorded NAR hash/size/deriver.
//! Removing guix removes the byte-identical-to-guix oracle, so the gate is re-pointed at its
//! ENDURING td-only feature — canonical drv construction + direct execution — with guix off
//! PATH:
//! • SUBJECT: `cached_build hello` — td ASSEMBLES the `.drv` (assemble-recipe, builder = the
//! bootstrapped stage0 td-builder, guix/Guile off PATH) and the shared td daemon builds it.
//! • CONSTRUCT: `drv-emit-to` re-emits the `.drv`; asserted byte-identical to the assembled
//! `.drv` (a canonical/deterministic round-trip — td's own construction is stable), which
//! replaces "byte-identical to guix's `.drv`".
//! • CLOSURE: `td-builder realize` computes the build-input closure by CONTENT-SCANNING the
//! seed store (no `guix gc`) and writes closure.txt, staging the stage0 builder via the
//! TD_BUILDER_* override (the realize engine change this slice adds). (realize also builds +
//! registers the assembled drv as a side effect; the EXECUTE step below independently runs
//! the EMITTED file through the raw `build` executor — a deliberate, distinct assertion, so
//! the gate builds hello twice.)
//! • EXECUTE: `td-builder build` runs the EMITTED `.drv` in its own userns sandbox to the
//! canonical output path (OUT=out $out) with a registration — the S3/S4 executor, no daemon.
//! • BEHAVIORAL: the built hello RUNS and prints "Hello, world!".
//! 
//! Directive 3 (called out for sign-off): DROPS the removable guix DIFFERENTIAL oracles — the
//! `.drv` byte-identical-to-guix comparison and the `== the daemon's recorded facts` legs (both
//! only proved td's construct/execute equalled guix's; the §5 removable oracle). td's OWN
//! construction (canonical round-trip) + direct execution — the actual features — are KEPT.
//! 
//! Heavy (a stage0 td-builder + hello builds) → heavy pool; BUILD_GATE so the build-recipes
//! prelude warms hello + td-recipe-eval. Per-gate scratch (.td-drv-build-scratch), removed on
//! green, kept on red for triage.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-drv-build",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: true,
        specs: &[],
        script: r##"
echo ">> td-drv-build: td-builder EMITS a canonical hello .drv (round-trip byte-identical) AND EXECUTES the emitted .drv in its own userns sandbox (td-builder build) → the canonical output; guix off PATH, no guile"
set -euo pipefail; \
. tests/cache-lib.sh; \
export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; load_recipe_eval; \
CU=`grep -- '-coreutils-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`; export CU; \
test -n "$CU" || { echo "ERROR: no coreutils in tests/hello-no-guix.lock" >&2; exit 1; }; \
export CACHE="$PWD/.td-drv-build-scratch"; chmod -R u+w "$CACHE" 2>/dev/null || true; rm -rf "$CACHE"; mkdir -p "$CACHE"; \
echo ">> td ASSEMBLES + builds the hello .drv (assemble-recipe + shared td daemon; guix off PATH)"; \
cached_build hello tests/hello-no-guix.lock || exit 1; \
test -n "${out:-}" -a -n "${ns:-}" || { echo "FAIL: cached_build set no out/ns" >&2; exit 1; }; \
drvf=`ls "$sd/b/"*.drv 2>/dev/null | head -1`; \
test -n "$drvf" || { echo "FAIL: no assembled hello .drv under $sd/b" >&2; exit 1; }; \
canon=`grep -hoE '/gnu/store/[a-z0-9]+-hello-[^ ]+\.drv' "$sd/err" "$sd/bout" 2>/dev/null | head -1`; \
test -n "$canon" || { echo "FAIL: could not read the canonical hello .drv path from the build log ($sd/err)" >&2; exit 1; }; \
sdrv="$CACHE/`basename "$canon"`"; cp "$drvf" "$sdrv"; \
echo "   assembled .drv (builder=stage0): $drvf  ->  canonical $canon  ->  output $out"; \
echo ">> (1) CONSTRUCT: td re-emits the .drv (drv-emit-to) and recomputes its CONTENT-ADDRESSED path — equal to assemble-recipe's canonical path proves the re-emission is byte-identical (the path is a hash of the .drv content+refs; no external cmp needed):"; \
computed=`"$TB" drv-emit-to "$sdrv" "$CACHE/emitted.drv" 2>"$CACHE/emit.err"` \
  || { echo "FAIL: drv-emit-to failed:" >&2; cat "$CACHE/emit.err" >&2; exit 1; }; \
test "$computed" = "$canon" \
  || { echo "FAIL: drv-emit-to recomputed $computed, but assemble-recipe's canonical path is $canon — the re-emitted content hashes differently, so td's drv construction is not canonical/deterministic" >&2; exit 1; }; \
echo "   re-emitted .drv hashes to the SAME canonical content-addressed path $computed (⟹ byte-identical construction)"; \
echo ">> (2) CLOSURE: realize content-scans the seed store for the build-input closure (stages the stage0 builder via TD_BUILDER_*) — no guix gc:"; \
"$TB" realize "$sdrv" /gnu/store "$CACHE/rz" > "$CACHE/rz.out" 2>&1 \
  || { echo "FAIL: td-builder realize could not compute the input closure / build:" >&2; tail -20 "$CACHE/rz.out" >&2; exit 1; }; \
test -s "$CACHE/rz/closure.txt" || { echo "FAIL: realize wrote no closure.txt" >&2; exit 1; }; \
echo "   input closure: $(wc -l < "$CACHE/rz/closure.txt") paths (content-scanned, guix-free)"; \
echo ">> (3) EXECUTE: td-builder build runs the EMITTED .drv (raw userns executor, builder=stage0) → the canonical output:"; \
"$TB" build "$CACHE/emitted.drv" "$CACHE/rz/closure.txt" "$CACHE/b" > "$CACHE/buildout.txt" 2>&1 \
  || { echo "FAIL: td-builder build of the emitted .drv failed:" >&2; tail -20 "$CACHE/buildout.txt" >&2; exit 1; }; \
grep -qx "OUT=out $out" "$CACHE/buildout.txt" \
  || { echo "FAIL: td-builder build produced a different output than $out:" >&2; cat "$CACHE/buildout.txt" >&2; exit 1; }; \
test -s "$CACHE/b/registration" || { echo "FAIL: td-builder build wrote no registration record" >&2; exit 1; }; \
echo "   td-builder build executed the emitted .drv to the canonical output $out (registration written)"; \
echo ">> (4) BEHAVIORAL: the built hello RUNS:"; \
say=`"$ns/bin/hello"`; \
test "$say" = "Hello, world!" || { echo "FAIL: the built hello printed '$say', expected 'Hello, world!'" >&2; exit 1; }; \
echo "   [DURABLE behavioral] hello prints '$say'"; \
chmod -R u+w "$CACHE" 2>/dev/null || true; rm -rf "$CACHE"; \
echo "PASS: td-builder EMITTED a canonical hello .drv (drv-emit-to round-trip byte-identical to the td-assembled .drv) AND EXECUTED the emitted file in its own userns sandbox (td-builder build, builder = bootstrapped stage0) to the canonical output $out, and the built hello runs ('Hello, world!'). Construct AND execute are td's Rust with guix/Guile OFF PATH — no guix repl, no guix gc, no guix build; the removable byte-identical-to-guix + daemon-facts oracles were dropped (directive 3)."
"##,
    }
}
