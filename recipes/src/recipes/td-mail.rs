use crate::types::Recipe;

/// td-mail, td's JMAP mail client in a td-ui window and the program the
/// `mail` application packages (APPLICATIONS.md §W.8). Built as td-news
/// and td-taskmgr are: a static Cargo build from the checkout's own
/// `td-mail/` tree with the toolkit, whose editor core is the document
/// pane a message is read in, td-json and td-toml, and the compositor's
/// shared font and wire sources staged beside it, the `td-mail-source`
/// seed pinned by the compiled seed-digest table, and the crate's
/// committed lock naming itself, td-json, td-toml and the toolkit.
pub fn recipe() -> Recipe {
    Recipe::rust("td-mail", "0.1.0")
        .local_source("td-mail")
        .local_source_trees(&["td-json", "td-toml", "td-ui", "td-compositor"])
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
            Some(vec![
                "td-json".into(),
                "td-toml".into(),
                "td-ui".into(),
                "td-compositor".into()
            ])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("td-mail"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("td-mail/Cargo.lock"));
        assert_eq!(recipe.static_link, Some(true));
        assert_eq!(recipe.bins, Some(vec!["td-mail".into()]));
        // No fetch pin: the bytes are the committed tree.
        assert!(crate::source_pins::by_key("td-mail-source").is_none());
    }

    /// The lock the recipe names is the crate's own, and it closes over
    /// exactly the crate and its td siblings: a registry or git entry would
    /// be a dependency the gate refuses, and a missing sibling a build that
    /// could not resolve the window, the pane, JSON or TOML.
    #[test]
    fn td_mail_lock_names_the_crate_and_its_td_siblings() {
        let lock = include_str!("../../../td-mail/Cargo.lock");
        let names: Vec<&str> = lock
            .lines()
            .filter_map(|line| line.strip_prefix("name = \""))
            .filter_map(|rest| rest.strip_suffix('"'))
            .collect();
        assert_eq!(names, ["td-json", "td-mail", "td-toml", "td-ui"]);
        assert!(!lock.contains("source = "));
        let manifest = include_str!("../../../td-mail/Cargo.toml");
        for sibling in ["td-json", "td-toml", "td-ui"] {
            assert!(manifest.contains(&format!("{sibling} = {{ path = \"../{sibling}\" }}")));
        }
        assert!(!manifest.contains("td-editor"));
    }

    /// The modules the two trees share are one text: the four std modules
    /// copied into both (JSON and TOML are the td-json and td-toml crates
    /// instead). A fix that reached one tree and not the other would part
    /// them here.
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
                "kv.rs",
                include_str!("../../../td-mail/src/kv.rs"),
                include_str!("../../../td-news/src/kv.rs"),
            ),
        ] {
            assert!(mail == news, "{name} differs between td-mail and td-news");
        }
    }

    /// The request-body ceiling the client applies before an upload is
    /// the service's own: a change to one line without the other would
    /// have td-mail refuse an attachment the service takes, or upload
    /// one it refuses.
    #[test]
    fn the_client_request_body_ceiling_is_the_service_bound() {
        let bound = |text: &str, prefix: &str| {
            text.lines()
                .find_map(|line| line.trim_start().strip_prefix(prefix))
                .map(|rest| rest.trim_end_matches(';').trim().to_string())
        };
        let client = bound(
            include_str!("../../../td-mail/src/td_fetch.rs"),
            "pub const MAX_REQUEST_BODY: u64 =",
        );
        let service = bound(
            include_str!("../../../net/src/fetchd.rs"),
            "const MAX_REQUEST_BODY: u64 =",
        );
        assert_eq!(client.as_deref(), Some("32 * 1024 * 1024"));
        assert_eq!(client, service);
    }

    /// Likewise the streamed frame's bound: a client holding a smaller one
    /// would refuse the service's full frames as the protocol misread.
    #[test]
    fn the_client_frame_bound_is_the_service_bound() {
        let bound = |text: &str| {
            text.lines()
                .find_map(|line| line.trim_start().strip_prefix("const MAX_CHUNK: usize ="))
                .map(|rest| rest.trim_end_matches(';').trim().to_string())
        };
        let client = bound(include_str!("../../../td-mail/src/td_fetch.rs"));
        let service = bound(include_str!("../../../net/src/fetchd.rs"));
        assert_eq!(client.as_deref(), Some("64 * 1024"));
        assert_eq!(client, service);
    }
}
