use crate::types::Recipe;

// The package source and Cargo.lock-selected registry closure are fixed,
// checksum-verified inputs. The declared native inputs make `td shell` and
// `build-plan --auto` use the same toolchain, in the build root.
pub fn recipe() -> Recipe {
    Recipe::rust("ripgrep", "14.1.1")
        .source_input("ripgrep-source")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_lock("recipes/locks/ripgrep/Cargo.lock")
        .bins(&["rg"])
}
