use crate::types::Recipe;

// The stage0-posix mescc-tools SEED RUNG (#378 slice 1) — the first rung of the
// /td/store toolchain recipe graph. No `Source` (no fetch): the pinned seed tree
// is vendored at seed/stage0 (provenance in its README) and rides in through the
// lock's `stage0-source` entry. Executor + seal: the engine's build::run_stage0.
// The version is the pinned upstream commit.
pub fn recipe() -> Recipe {
    Recipe::stage0("stage0", "3b9c2bb")
}
