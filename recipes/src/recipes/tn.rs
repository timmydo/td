use crate::types::Recipe;

/// Timmy's News — `tn`, the terminal feed reader that the `news` application
/// packages. The source is one upstream commit archive pinned by SHA-256, and
/// its registry closure is the committed `recipes/locks/tn/Cargo.lock`, which
/// must equal the lock the archive ships. The manifest sits at the archive
/// root; `cargo_subdir(".")` names that root explicitly, which the warm plan
/// requires of every fixed-output archive so a multi-workspace repository can
/// never build the wrong crate by omission. The binary links fully static
/// because the `news` application package validates it as a static entry on
/// the empty runtime: the jail shows an application no `/td/store` loader.
pub fn recipe() -> Recipe {
    Recipe::rust("tn", "0.1.0-g3de5c9e")
        .source_input("tn-source")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir(".")
        .cargo_lock("recipes/locks/tn/Cargo.lock")
        .static_link()
        .bins(&["tn"])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    /// The pinned upstream commit; the version suffix and the pin's URL and
    /// file name must all name it.
    const TN_COMMIT: &str = "3de5c9e22b05527f9d90f9e9ae63256f24ba67b2";

    #[test]
    fn tn_is_a_root_workspace_rust_recipe_pinned_to_one_commit() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("tn-source"));
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("."));
        assert_eq!(
            recipe.cargo_lock.as_deref(),
            Some("recipes/locks/tn/Cargo.lock")
        );
        assert_eq!(recipe.bins, Some(vec!["tn".to_string()]));
        assert_eq!(recipe.static_link, Some(true));
        assert!(recipe.version.ends_with(&TN_COMMIT[..7]));
        let pin = crate::source_pins::by_key("tn-source").expect("tn-source pin");
        assert!(pin.url.contains(TN_COMMIT), "{}", pin.url);
        assert!(pin.file.contains(TN_COMMIT), "{}", pin.file);
    }
}
