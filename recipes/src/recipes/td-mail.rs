use crate::types::Recipe;

/// td-mail, td's JMAP mail client in a td-ui window and the program the
/// `mail` application packages (APPLICATIONS.md §W.8). Built as td-news
/// and td-taskmgr are: a static Cargo build from the checkout's own
/// `td-mail/` tree with the toolkit and the compositor's shared font and
/// wire sources staged beside it, the `td-mail-source` seed pinned by the
/// compiled seed-digest table, and the crate's committed lock naming
/// itself and the one sibling.
pub fn recipe() -> Recipe {
    Recipe::rust("td-mail", "0.1.0")
        .local_source("td-mail")
        .local_source_trees(&["td-ui", "td-compositor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-mail")
        .cargo_lock("td-mail/Cargo.lock")
        .static_link()
        .bins(&["td-mail"])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn td_mail_is_a_static_toolkit_program_built_from_the_checkout() {
        let recipe = recipe();
        assert_eq!(recipe.name, "td-mail");
        assert_eq!(recipe.source_input.as_deref(), Some("td-mail-source"));
        assert_eq!(recipe.local_source.as_deref(), Some("td-mail"));
        assert_eq!(
            recipe.local_source_trees,
            Some(vec!["td-ui".into(), "td-compositor".into()])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("td-mail"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("td-mail/Cargo.lock"));
        assert_eq!(recipe.static_link, Some(true));
        assert_eq!(recipe.bins, Some(vec!["td-mail".into()]));
        // No fetch pin: the bytes are the committed tree.
        assert!(crate::source_pins::by_key("td-mail-source").is_none());
    }

    /// The lock the recipe names is the crate's own, and it closes over
    /// exactly the crate and the toolkit: a registry or git entry would be
    /// a dependency the gate refuses, and a missing `td-ui` entry a build
    /// that could not resolve the window.
    #[test]
    fn td_mail_lock_names_the_crate_and_the_toolkit_alone() {
        let lock = include_str!("../../../td-mail/Cargo.lock");
        let names: Vec<&str> = lock
            .lines()
            .filter_map(|line| line.strip_prefix("name = \""))
            .filter_map(|rest| rest.strip_suffix('"'))
            .collect();
        assert_eq!(names, ["td-mail", "td-ui"]);
        assert!(!lock.contains("source = "));
        let manifest = include_str!("../../../td-mail/Cargo.toml");
        assert!(manifest.contains("td-ui = { path = \"../td-ui\" }"));
    }

    /// The modules the two trees share are one text: the six std modules
    /// copied into both. A fix that reached one tree and not the other
    /// would part them here.
    #[test]
    fn the_shared_modules_are_one_text_in_both_trees() {
        for (name, mail, news) in [
            (
                "td_fetch.rs",
                include_str!("../../../td-mail/src/td_fetch.rs"),
                include_str!("../../../td-news/src/td_fetch.rs"),
            ),
            (
                "civil.rs",
                include_str!("../../../td-mail/src/civil.rs"),
                include_str!("../../../td-news/src/civil.rs"),
            ),
            (
                "html.rs",
                include_str!("../../../td-mail/src/html.rs"),
                include_str!("../../../td-news/src/html.rs"),
            ),
            (
                "json.rs",
                include_str!("../../../td-mail/src/json.rs"),
                include_str!("../../../td-news/src/json.rs"),
            ),
            (
                "kv.rs",
                include_str!("../../../td-mail/src/kv.rs"),
                include_str!("../../../td-news/src/kv.rs"),
            ),
            (
                "toml.rs",
                include_str!("../../../td-mail/src/toml.rs"),
                include_str!("../../../td-news/src/toml.rs"),
            ),
        ] {
            assert!(mail == news, "{name} differs between td-mail and td-news");
        }
    }
}
