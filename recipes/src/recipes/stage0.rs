use crate::types::Recipe;

// The stage0-posix mescc-tools SEED RUNG as a recipe (#378 slice 1) — the first
// rung of the /td/store toolchain recipe graph. No `Source` (no fetch): the
// pinned seed tree is vendored in-repo at seed/stage0 (see its README for the
// byte-level provenance) and rides in through the lock's `stage0-source` entry,
// interned by the caller. The executor and its seal live in the engine's
// `stage0` build system — build::run_stage0 (builder/src/build.rs) is the
// canonical doc. The version is the pinned upstream commit.
pub fn recipe() -> Recipe {
    Recipe::stage0("stage0", "3b9c2bb")
}
