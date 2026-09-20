use crate::types::Recipe;

/// td-news, td's feed reader in a td-ui window and the program the `news`
/// application packages (APPLICATIONS.md §W.8). Built as td-taskmgr is: a
/// static Cargo build from the checkout's own `td-news/` tree with the
/// toolkit, the editor whose document view reads an article, and the
/// compositor's shared font and wire sources staged beside it, the
/// `td-news-source` seed pinned by the compiled seed-digest table, and
/// the crate's committed lock naming itself and its two siblings.
pub fn recipe() -> Recipe {
    Recipe::rust("td-news", "0.1.0")
        .local_source("td-news")
        .local_source_trees(&["td-ui", "td-compositor", "td-editor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-news")
        .cargo_lock("td-news/Cargo.lock")
        .static_link()
        .bins(&["td-news"])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn td_news_is_a_static_toolkit_program_built_from_the_checkout() {
        let recipe = recipe();
        assert_eq!(recipe.name, "td-news");
        assert_eq!(recipe.source_input.as_deref(), Some("td-news-source"));
        assert_eq!(recipe.local_source.as_deref(), Some("td-news"));
        assert_eq!(
            recipe.local_source_trees,
            Some(vec![
                "td-ui".into(),
                "td-compositor".into(),
                "td-editor".into()
            ])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("td-news"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("td-news/Cargo.lock"));
        assert_eq!(recipe.static_link, Some(true));
        assert_eq!(recipe.bins, Some(vec!["td-news".into()]));
        // No fetch pin: the bytes are the committed tree.
        assert!(crate::source_pins::by_key("td-news-source").is_none());
    }

    /// The lock the recipe names is the crate's own, and it closes over
    /// exactly the crate, the toolkit and the editor: a registry or git
    /// entry would be a dependency the gate refuses, and a missing
    /// sibling entry a build that could not resolve the window or the
    /// pane.
    #[test]
    fn td_news_lock_names_the_crate_the_toolkit_and_the_editor_alone() {
        let lock = include_str!("../../../td-news/Cargo.lock");
        let names: Vec<&str> = lock
            .lines()
            .filter_map(|line| line.strip_prefix("name = \""))
            .filter_map(|rest| rest.strip_suffix('"'))
            .collect();
        assert_eq!(names, ["td-editor", "td-news", "td-ui"]);
        assert!(!lock.contains("source = "));
        let manifest = include_str!("../../../td-news/Cargo.toml");
        assert!(manifest.contains("td-ui = { path = \"../td-ui\" }"));
        assert!(manifest.contains("td-editor = { path = \"../td-editor\" }"));
    }
}
