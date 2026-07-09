use crate::types::Recipe;

// The stage0-posix mescc-tools SEED RUNG (#378 slice 1) — the first rung of the
// /td/store toolchain recipe graph. The pinned upstream source tarball is
// interned through the lock's `stage0-source` entry. Executor + seal: the
// engine's build::run_stage0.
pub fn recipe() -> Recipe {
    Recipe::stage0("stage0", "1.9.1").source_input("stage0-source")
}
