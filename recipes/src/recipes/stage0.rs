use crate::types::Recipe;

// The stage0-posix mescc-tools SEED RUNG as a recipe (#378 slice 1) — the first
// rung of the /td/store toolchain recipe graph. No `Source` (no fetch): the
// pinned seed tree is vendored in-repo at seed/stage0 (stage0-posix-x86 commit
// 3b9c2bb…, see its README for the byte-level provenance) and is interned by the
// caller; the lock's `stage0-source` entry carries the interned tree. The
// engine's `stage0` build system (builder/src/build.rs run_stage0) places a
// writable copy and execs the kaem interpreter over the two vendored build
// scripts — hex0-seed → … → M2, blood-elf-0, kaem-0 → M1, hex2, kaem — the only
// place a raw binary seed is exec'd. The version is the pinned upstream commit.
pub fn recipe() -> Recipe {
    Recipe::stage0("stage0", "3b9c2bb")
}
