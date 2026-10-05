use crate::types::Recipe;

/// td-open, the link and file opener the `mail` and `news` packages ship
/// at `/app/bin/td-open` and name as `$BROWSER` (and mail as `$OPENER`)
/// (APPLICATIONS.md §W.6). It hands one link to the desktop portal's
/// `OpenURI`, or one file's descriptor to `OpenFile`, so the applications
/// carry no D-Bus code of their own. Built as td-news is: a static Cargo
/// build from the checkout's `td-open/` tree, with td-busd staged beside it
/// for the broker codec and td-secret for the shared descriptor module
/// (UNSAFE.md §12, §23) the crate mounts by `#[path]`, and a committed lock
/// naming the crate alone.
pub fn recipe() -> Recipe {
    Recipe::rust("td-open", "0.1.0")
        .local_source("td-open")
        .local_source_trees(&["td-busd", "td-secret"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "td-sh",
        ])
        .cargo_subdir("td-open")
        .cargo_lock("td-open/Cargo.lock")
        .static_link()
        .bins(&["td-open"])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn td_open_is_a_static_program_built_from_the_checkout_with_the_codec() {
        let recipe = recipe();
        assert_eq!(recipe.name, "td-open");
        assert_eq!(recipe.source_input.as_deref(), Some("td-open-source"));
        assert_eq!(recipe.local_source.as_deref(), Some("td-open"));
        assert_eq!(
            recipe.local_source_trees,
            Some(vec!["td-busd".into(), "td-secret".into()])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("td-open"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("td-open/Cargo.lock"));
        assert_eq!(recipe.static_link, Some(true));
        assert_eq!(recipe.bins, Some(vec!["td-open".into()]));
        assert!(crate::source_pins::by_key("td-open-source").is_none());
    }

    /// The lock closes over the crate alone, and every source it mounts
    /// from another tree is in the one sibling tree the recipe stages.
    #[test]
    fn td_open_lock_and_mounts_stay_inside_the_staged_trees() {
        let lock = include_str!("../../../td-open/Cargo.lock");
        let names: Vec<&str> = lock
            .lines()
            .filter_map(|line| line.strip_prefix("name = \""))
            .filter_map(|rest| rest.strip_suffix('"'))
            .collect();
        assert_eq!(names, ["td-open"]);
        assert!(!lock.contains("source = "));
        let main = include_str!("../../../td-open/src/main.rs");
        let mounts: Vec<&str> = main
            .lines()
            .filter_map(|line| line.strip_prefix("#[path = \""))
            .collect();
        assert_eq!(mounts.len(), 4);
        assert_eq!(
            mounts
                .iter()
                .filter(|mount| mount.starts_with("../../td-busd/src/"))
                .count(),
            3
        );
        assert!(mounts.contains(&"../../td-secret/src/sys.rs\"]"));
    }
}
