use crate::types::{Recipe, RecipeCheck, Source};

// rust-toolchain — the /td/store Rust toolchain, as a first-class RECIPE fully in the
// recipe-graph model (#410, building on #380).
//
// The maintainer scope decision: the rust toolchain td uses to build rust packages is NOT
// part of the full-source bootstrap. It enters as a PINNED UPSTREAM Rust release (rustc +
// cargo + rust-std for x86_64-unknown-linux-gnu, a declared fixed-output fetch — the
// `.source()` below, mirrored by seed/sources/rust-1.96.0.lock, guix-free) TRANSFORMED by
// this recipe: unpack the release tarball IN-SANDBOX, extract rustc/cargo + the rustlib
// sysroot, co-locate the runtime closure (glibc sonames + libgcc_s + libz, found via the
// upstream $ORIGIN/../lib RUNPATH), and RELINK rustc/cargo's ELF interpreter onto td's own
// /td/store x86_64 glibc loader (crate::elf::set_interp — GROWS the slot, #258 — no
// patchelf, no guix byte). Upstream-release bytes are not guix bytes: the north star's
// "zero guix bytes / no guix process" holds.
//
// buildSystem "rust-toolchain" (BuildSystem::RustToolchain) — the engine's
// build::run_rust_toolchain does the unpack+extract+co-locate+relink; there is no compile.
//
// Recipe-graph model (#410): the transform's inputs are its DECLARED native_inputs, resolved
// BY RECIPE NAME (no gate-assembled rust-toolchain lock), chained by `build-plan --auto`:
//   glibc-x86-64        the /td/store x86_64 glibc 2.41 (interp target + libc/libdl/librt/
//                       libpthread/libm sonames), at its staged stage/td/store/glibc-2.41-x86_64
//   gcc-x86-64-stage2   the cross gcc final — its libgcc_s.so.1 (rustc NEEDs it dynamically)
//   zlib-x86-64         the td-built libz.so.1 (libLLVM NEEDs libz)
// The pinned release tarball rides in as `rust-toolchain-source` (Class::Source); the engine
// unpacks it with the declared tar/gzip inputs (as ladder::unpack_into does), so `.source()`
// is the raw tarball. Same pinned source + same inputs => byte-identical tree (the
// `td-builder check` double-build oracle proves it); a missing input reds at drv-assembly.
//
// Validation is a recipe-owned RecipeCheck::daily (below): `build-plan --auto rust-toolchain`
// builds the tree, rustc RUNS from /td/store in an own-root (/gnu/store absent), and a missing
// declared input reds the recipe (byte-for-byte reproducibility is the daily force-cold backstop,
// per the mesboot-rung precedent — see tests/rust-toolchain-recipe-check.sh). The
// rustc-COMPILES-a-real-program proof + the `td shell` userland cutover (which need the NATIVE
// x86_64 gcc, a deliberately non-reproducible subcommand, not a recipe) lived on the
// rust-userland-x86_64-store-native / td-shell-userland gates; those were DISABLED with this
// cutover (maintainer-directed) pending re-coverage on the recipe-graph model (follow-up issue).
pub fn recipe() -> Recipe {
    Recipe::rust_toolchain("rust-toolchain", "1.96.0")
        .source(Source::one(
            "https://static.rust-lang.org/dist/rust-1.96.0-x86_64-unknown-linux-gnu.tar.gz",
            "104nb1mgsy2qd8jb4z8pg1m0s1gvn42v2qmhd9v31wkng57hw4y1",
        ))
        .source_input("rust-toolchain-source")
        .native_inputs(&["glibc-x86-64", "gcc-x86-64-stage2", "zlib-x86-64"])
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check rust-toolchain: build-plan --auto builds the /td/store rust toolchain (glibc-x86-64 + gcc-x86-64-stage2 + zlib-x86-64); rustc runs from /td/store in an own-root (/gnu/store absent); byte-reproducible; verified-red on a missing declared input"
sh tests/rust-toolchain-recipe-check.sh
"#,
        )])
}
