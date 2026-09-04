use crate::types::Recipe;

/// Timmy's Mail Console — `tmc`, the JMAP terminal mail client that the `mail`
/// application packages. The source is one upstream commit archive pinned by
/// SHA-256, and its registry closure is the committed
/// `recipes/locks/tmc/Cargo.lock`, which must equal the lock the archive
/// ships. The manifest sits at the archive root; `cargo_subdir(".")` names
/// that root explicitly, as every fixed-output archive must. The binary links
/// fully static because the `mail` application package validates it as a
/// static entry on the empty runtime: the jail shows an application no
/// `/td/store` loader.
pub fn recipe() -> Recipe {
    Recipe::rust("tmc", "0.1.0-g8f04e38")
        .source_input("tmc-source")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir(".")
        .cargo_lock("recipes/locks/tmc/Cargo.lock")
        .static_link()
        .bins(&["tmc"])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    /// The pinned upstream commit; the version suffix and the pin's URL and
    /// file name must all name it.
    const TMC_COMMIT: &str = "8f04e380bd378f735152fae18e1bd1c189b6eddf";

    #[test]
    fn tmc_is_a_root_workspace_rust_recipe_pinned_to_one_commit() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("tmc-source"));
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("."));
        assert_eq!(
            recipe.cargo_lock.as_deref(),
            Some("recipes/locks/tmc/Cargo.lock")
        );
        assert_eq!(recipe.bins, Some(vec!["tmc".to_string()]));
        assert_eq!(recipe.static_link, Some(true));
        assert!(recipe.version.ends_with(&TMC_COMMIT[..7]));
        let pin = crate::source_pins::by_key("tmc-source").expect("tmc-source pin");
        assert!(pin.url.contains(TMC_COMMIT), "{}", pin.url);
        assert!(pin.file.contains(TMC_COMMIT), "{}", pin.file);
    }
}
