use crate::types::Recipe;

// td-util-boot — td-util compiled by the Rust snapshot, for the builds that run
// before td has a Rust of its own.
//
// cmake-x86-64 and rust-toolchain run on the bootstrap root's GNU userland,
// whose coreutils build has no `uname`; CMake's `bootstrap`, its system probe
// and LLVM's config.guess all run it. td-util serves it, and is std-only, so
// the snapshot's rustc compiles it the same way rust-toolchain's compiles the
// shipped one: the same sources, flags, static link and debug split, through
// `td_util::build`. The snapshot's prebuilt std is not recompiled with td's
// frame-pointer flags; this is a build-only tool, outside that contract.
//
// This adds no trust root: rust-stage0 already compiles rust-toolchain. It does
// widen the snapshot's reach to cmake, which builds before Rust; that is the
// reviewed cost of keeping BusyBox out of the bootstrap.
//
// Build-only. It is an ancestor of rust-toolchain, so the ladder's boundary
// guard refuses it to every recipe past the boundary: the shipped td-util, and
// every post-Rust tool farm, is the stage2-built one.
pub fn recipe() -> Recipe {
    super::td_util::build("td-util-boot", "rust-stage0")
}
