use crate::types::Recipe;

// Select features explicitly: fd's optional jemalloc (`use-jemalloc`,
// `base`) compiles C and must stay out whatever upstream defaults. The
// committed lock and declared native inputs let `td shell` and
// `build-plan --auto` share the same offline crate closure and toolchain,
// in the build root.
pub fn recipe() -> Recipe {
    Recipe::rust("fd", "10.5.0")
        .source_input("fd-source")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_lock("recipes/locks/fd/Cargo.lock")
        .bins(&["fd"])
        .no_default_features()
        .features(&["completions"])
}
