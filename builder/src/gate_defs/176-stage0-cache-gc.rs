//! stage0-cache-gc — the stage0 placement cache GCs stale entries (#309).
//! tests/stage0-builder.sh appends a fresh `<hash>-td-builder-0.1.0` placement to
//! BASEDIR/store on every builder/ fingerprint change (a new content-addressed hash ⇒
//! a new dir) and used to leave the old ones forever, so a long-lived warm runner grew
//! one stale placement per builder/ change — unbounded disk, and a latent hazard for
//! any glob-style resolver (the #293 daemon-budget red, where a lexicographic-first
//! pick chose a STALE binary predating a subcommand). The slow (placement) path now
//! sweeps every non-current placement while holding the .stage0.lock.
//!
//! This gate drives the REAL entry point (load_stage0 → stage0-builder.sh) on an
//! ISOLATED BASEDIR seeded to look like a warm cache that accumulated N>1 placements,
//! and asserts the OBSERVABLE sweep — exactly the current placement survives — then a
//! consumer check: load_stage0 resolves that survivor and it RUNS a real td-builder
//! subcommand. Verified-red: with the sweep removed, the two injected stale placements
//! survive and the `after -eq 1` assertion reds (3 != 1). The full check's real
//! daemon-budget (which uses the SHARED base) is the end-to-end "a stage0 consumer
//! still passes" leg; this gate isolates the sweep behavior itself.
//!
//! Isolated BASEDIR (its OWN .td-build-cache/stage0-gc-test, NOT the shared
//! TD_STAGE0_BASE) so injecting/removing placements never disturbs a concurrent gate's
//! stage0. The toolchain seed (tests/td-builder-rust.lock) is realized up front, the
//! same guix-built pin the other stage0 gates use (§5, retired last).

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "stage0-cache-gc",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> stage0-cache-gc: stage0-builder.sh sweeps stale placements on a fresh placement (under .stage0.lock); load_stage0 resolves the survivor and it runs (#309)"
set -euo pipefail; \
grep ' /gnu/store/' tests/td-builder-rust.lock | sed 's/^[^ ]* //' | xargs $TD_GUIX build >/dev/null \
  || { echo "ERROR: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }; \
. tests/cache-lib.sh; \
B="$PWD/.td-build-cache/stage0-gc-test"; chmod -R u+w "$B" 2>/dev/null || true; rm -rf "$B"; mkdir -p "$B"; \
trap 'chmod -R u+w "$B" 2>/dev/null || true; rm -rf "$B"' EXIT; \
export TD_STAGE0_BASE="$B"; \
echo ">> place the current stage0 via load_stage0 (the sole sanctioned resolver)"; \
load_stage0; cur=`basename "$TD_BUILDER_PATH"`; \
test -x "$TB" || { echo "FAIL: load_stage0 did not place a stage0 td-builder" >&2; exit 1; }; \
echo ">> simulate a warm cache that accumulated stale placements from prior builder/ fingerprints"; \
for h in 0000000000000000000000000000000a 0000000000000000000000000000000b; do \
  mkdir -p "$B/store/$h-td-builder-0.1.0/bin"; \
  printf '#!/bin/sh\nexit 0\n' > "$B/store/$h-td-builder-0.1.0/bin/td-builder"; \
  chmod +x "$B/store/$h-td-builder-0.1.0/bin/td-builder"; \
done; \
rm -rf "$B/store/$cur"; \
before=`ls "$B/store" | wc -l`; \
test "$before" -gt 1 || { echo "FAIL: setup did not produce N>1 placements (got $before)" >&2; exit 1; }; \
echo "   cache now holds $before placements (N>1); the current one is removed so the next resolve takes the slow (placement) path"; \
echo ">> load_stage0 again -> slow path re-places the current stage0 AND sweeps the stale ones (holding .stage0.lock)"; \
load_stage0; \
after=`ls "$B/store" | wc -l`; \
test "$after" -eq 1 || { echo "FAIL: sweep left $after placement(s), expected exactly 1 (the current) — stale placements were NOT swept" >&2; ls "$B/store" >&2; exit 1; }; \
test -d "$B/store/$cur" || { echo "FAIL: the sweep removed the CURRENT placement ($cur)" >&2; exit 1; }; \
echo "   ok: exactly 1 placement remains ($cur) — the stale ones were swept"; \
echo ">> consumer check: load_stage0 resolves the survivor and it RUNS a real subcommand"; \
test "`basename "$TD_BUILDER_PATH"`" = "$cur" || { echo "FAIL: load_stage0 resolved a different placement ($TD_BUILDER_PATH) than the survivor after the sweep" >&2; exit 1; }; \
interp=`"$TB" elf-interp "$TB"` || { echo "FAIL: the surviving stage0 td-builder does not run (elf-interp on itself errored)" >&2; exit 1; }; \
case "$interp" in /gnu/store/*ld-linux*) : ;; *) echo "FAIL: surviving stage0 elf-interp returned an unexpected loader ($interp)" >&2; exit 1 ;; esac; \
echo "PASS: stage0-cache-gc — stage0-builder.sh sweeps stale placements on a fresh placement (under .stage0.lock); after a slow-path re-place exactly the current placement survives, load_stage0 resolves it, and it runs a real td-builder subcommand (#309)."
"##,
    }
}
