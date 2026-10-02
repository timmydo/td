use crate::types::Recipe;

/// Build the native installer front end with the target Rust toolchain. The
/// pure installer plan and toolkit are Cargo siblings. The toolkit's font,
/// notices and Wayland codec reach the compositor tree by relative source
/// paths, so all four trees are staged.
/// The system image carries it, and a live boot's session starts it
/// (td-install/INSTALLER.md "Live startup").
pub fn recipe() -> Recipe {
    Recipe::rust("td-setup", "0.1.0")
        .local_source("td-setup")
        .local_source_trees(&["td-install", "td-ui", "td-compositor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-setup")
        .cargo_lock("td-setup/Cargo.lock")
        .static_link()
        .bins(&["td-setup"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_source_closure_matches_the_installer_and_toolkit() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("td-setup-source"));
        assert_eq!(recipe.local_source.as_deref(), Some("td-setup"));
        assert_eq!(
            recipe.local_source_trees,
            Some(vec![
                "td-install".into(),
                "td-ui".into(),
                "td-compositor".into()
            ])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("td-setup"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("td-setup/Cargo.lock"));
        assert_eq!(recipe.bins, Some(vec!["td-setup".into()]));
        assert_eq!(recipe.static_link, Some(true));
        assert!(crate::source_pins::by_key("td-setup-source").is_none());

        let manifest = include_str!("../../../td-setup/Cargo.toml");
        let siblings: Vec<_> = manifest
            .lines()
            .filter(|line| line.contains("path ="))
            .collect();
        assert_eq!(
            siblings,
            [
                "td-install = { path = \"../td-install\" }",
                "td-ui = { path = \"../td-ui\" }",
            ]
        );
        let toolkit = include_str!("../../../td-ui/src/lib.rs");
        let mounts: Vec<_> = toolkit
            .lines()
            .filter_map(|line| line.trim().strip_prefix("#[path = \"../../"))
            .filter_map(|line| line.split('/').next())
            .collect();
        assert!(!mounts.is_empty());
        assert!(mounts.iter().all(|tree| *tree == "td-compositor"));
    }
}
