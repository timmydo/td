use crate::types::Recipe;

// rust-stage0 is the explicit downloaded trust root for Rust 1.96.0. The three
// component tarballs and their hashes come directly from rustc 1.96.0's
// `src/stage0` manifest: rustc, rust-std, and Cargo 1.95.0 dated 2026-04-16.
//
// The engine-native transform assembles only those components, co-locates the
// declared td glibc/libgcc/zlib runtime, and retargets rustc, rustdoc, and Cargo
// with td's ELF editor. This output is a build input to `rust-toolchain`; it is
// never the shipped toolchain and no downloaded byte may enter that final output.
pub fn recipe() -> Recipe {
    Recipe::rust_stage0("rust-stage0", "1.95.0")
        .source_input("rust-stage0-rustc-source")
        .inputs(&["rust-stage0-std-source", "rust-stage0-cargo-source"])
        .native_inputs(&["glibc-x86-64", "gcc-x86-64-stage2", "zlib-x86-64"])
}
