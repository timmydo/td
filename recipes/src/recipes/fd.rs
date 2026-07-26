use crate::types::Recipe;

// Keep fd's C-building jemalloc default disabled. The committed lock and
// declared native inputs let `td shell` and `build-plan --auto` share the same
// offline crate closure and source-built target toolchain.
pub fn recipe() -> Recipe {
    Recipe::rust("fd", "10.2.0")
        .source_input("fd-source")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_lock("recipes/locks/fd/Cargo.lock")
        .bins(&["fd"])
        .no_default_features()
        .features(&["completions"])
}
