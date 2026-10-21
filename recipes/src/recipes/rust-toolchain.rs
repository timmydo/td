use crate::types::{CheckRunner, Recipe, RecipeCheck};

// rust-toolchain is the shipped Rust 1.96.0 toolchain: upstream's prebuilt
// x86_64-unknown-linux-gnu rustc, rust-std, Cargo and Clippy release
// components, pinned by their channel-manifest SHA-256. It is a declared
// build-tool trust root (AGENTS.md "Rust toolchain"), not a source build.
//
// The engine-native `rust-dist` transform assembles the components, links
// td glibc, gcc-runtime's libgcc_s and td zlib into the tree's lib/ (which
// upstream's RUNPATH `$ORIGIN/../lib` already reaches), and relinks every
// shipped executable onto td glibc's loader. Nothing is compiled, so its
// binaries keep upstream's frame-pointer coverage and carry no debug
// companions: td-profiler reports them as a coverage boundary, and every
// Rust output's assembly-exception marker names its prebuilt std.
//
// Kept: rustc, rustdoc, cargo, cargo-clippy, clippy-driver and rust-lld,
// with `gcc-ld/ld.lld` linked to rust-lld for upstream's default
// `-fuse-ld=lld`. Dropped: the debugger scripts, the rust-analyzer server,
// the other linker flavors, wasm-component-ld and rust-objcopy.
pub fn recipe() -> Recipe {
    Recipe::rust_dist("rust-toolchain", "1.96.0")
        .source_input("rust-rustc-dist")
        .inputs(&["rust-std-dist", "rust-cargo-dist", "rust-clippy-dist"])
        .native_inputs(&["glibc-x86-64", "gcc-runtime", "zlib-x86-64"])
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check rust-toolchain: retarget upstream Rust 1.96.0 onto td glibc/libgcc/zlib, run it under own-root /td/store, and build and run the td shell userland there (rust-toolchain-test grades its ELF, links and smokes)"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run rust-toolchain 1
"#,
        )
        .with_runner(CheckRunner::RustToolchain)])
}
