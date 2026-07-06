use crate::types::Recipe;

// rust-toolchain — the /td/store Rust toolchain, as a first-class RECIPE (#380).
//
// The maintainer scope decision: the rust toolchain td uses to build rust packages
// is NOT part of the full-source bootstrap. It enters as a PINNED UPSTREAM Rust
// release (rustc + cargo + rust-std for x86_64-unknown-linux-gnu, a declared
// fixed-output fetch — seed/sources/rust-1.96.0.lock, sha256-pinned, guix-free)
// TRANSFORMED by this recipe: extract rustc/cargo + the rustlib sysroot, co-locate
// the runtime closure (glibc sonames + libgcc_s + libz, found via the upstream
// $ORIGIN/../lib RUNPATH), and RELINK rustc/cargo's ELF interpreter onto td's own
// /td/store x86_64 glibc loader (crate::elf::set_interp — GROWS the slot, #258 — no
// patchelf, no guix byte). Upstream-release bytes are not guix bytes: the north
// star's "zero guix bytes / no guix process" holds.
//
// buildSystem "rust-toolchain" (BuildSystem::RustToolchain) — the engine's
// build::run_rust_toolchain does the extract+co-locate+relink; there is no compile.
// This is the first-class, reproducible form of the retired `td-builder
// toolchain-recipe rust-x86_64` shell subcommand: the SAME pinned tarball + the
// SAME /td/store glibc/libgcc/libz inputs deterministically yield a byte-identical
// tree (the `td-builder check` double-build oracle proves it), and a missing/
// misdeclared release input reds `build-recipe` at drv-assembly, before any build.
//
// Inputs (by lock NAME, resolved to /td/store paths the engine relinks against):
//   rust-toolchain-source   the pinned upstream Rust release tarball (Class::Source)
//   rust-native-glibc       the /td/store x86_64 glibc 2.41 tree (interp target +
//                           the co-located libc/libdl/librt/libpthread/libm sonames)
//   rust-native-libgcc      a tree holding libgcc_s.so.1 (the native gcc's target libgcc)
//   rust-native-libz        a tree holding libz.so.1.* (libLLVM NEEDs libz)
pub fn recipe() -> Recipe {
    Recipe::rust_toolchain("rust-toolchain", "1.96.0")
        .inputs(&["rust-native-glibc", "rust-native-libgcc", "rust-native-libz"])
}
