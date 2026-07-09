use crate::types::Recipe;

// The stage0-posix mescc-tools SEED RUNG (#378 slice 1) — the first rung of the
// /td/store toolchain recipe graph. No network `Source`: the pinned local seed
// tarball in seed/stage0.lock rides in through the lock's `stage0-source` entry.
// Executor + seal: the engine's build::run_stage0.
// The version is the pinned upstream commit.
pub fn recipe() -> Recipe {
    Recipe::stage0("stage0", "3b9c2bb").source_input("stage0-source")
}
