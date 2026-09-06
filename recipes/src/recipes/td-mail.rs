use crate::types::Recipe;

/// td-mail, td's JMAP terminal mail client and the program the `mail`
/// application packages (APPLICATIONS.md §W.8). Built like td-sh: a static
/// ET_EXEC by direct rustc, from the checkout's own `td-mail/` tree rather
/// than a pinned archive. The tree is the `td-mail-source` seed, interned by
/// the runner and pinned by the compiled seed-digest table, and the crate is
/// a root crate the gate holds dependency-free, so its closure is std and the
/// recipe stages no vendor set and carries no lock of its own.
pub fn recipe() -> Recipe {
    crate::ladder::static_local_source_program("td-mail")
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn td_mail_is_a_static_program_built_from_the_checkout() {
        let recipe = recipe();
        assert_eq!(recipe.name, "td-mail");
        assert_eq!(recipe.local_source.as_deref(), Some("td-mail"));
        assert_eq!(recipe.local_source_trees, None);
        assert_eq!(recipe.source_input.as_deref(), Some("td-mail-source"));
        assert_eq!(recipe.cargo_lock, None);
        // No fetch pin: the bytes are the committed tree.
        assert!(crate::source_pins::by_key("td-mail-source").is_none());
        let steps = recipe.steps.as_ref().expect("steps");
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyTree { from, dest } if from == "{in:td-mail-source}" && dest == "{src}"
        )));
        // The compile reads the copied tree's own main.rs: spelled in two
        // halves so the roster scan for recipes that STAGE `.rs` files (this
        // one copies a tree instead) does not read it as a staged source.
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Run { argv, .. }
                if argv.iter().any(|arg| {
                    arg.starts_with("{src}/") && arg.ends_with("/src/main.rs")
                }) && argv.iter().any(|arg| arg == "{out}/bin/td-mail")
        )));
        assert!(matches!(
            steps.last(),
            Some(Step::AssertStatic { paths }) if paths == &["{out}/bin/td-mail".to_string()]
        ));
    }

    /// `--edition 2021` is the crate's own: cargo never sees this build, so
    /// the manifest and the flag agree by this test rather than by cargo.
    #[test]
    fn td_mail_is_compiled_in_its_manifest_edition() {
        let manifest = include_str!("../../../td-mail/Cargo.toml");
        assert!(
            manifest.contains("edition = \"2021\""),
            "the manifest's edition moved; move the recipe's --edition with it"
        );
        let steps = recipe().steps.expect("steps");
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Run { argv, .. } if argv.windows(2).any(|pair| {
                pair.first().map(String::as_str) == Some("--edition")
                    && pair.get(1).map(String::as_str) == Some("2021")
            })
        )));
    }

    /// The modules the two trees share are one text: the terminal surface
    /// UNSAFE.md §17 and §18 record as one, and the six copied with it. A
    /// fix that reached one tree and not the other would part them here.
    #[test]
    fn the_shared_modules_are_one_text_in_both_trees() {
        for (name, mail, news) in [
            (
                "term_sys.rs",
                include_str!("../../../td-mail/src/term_sys.rs"),
                include_str!("../../../td-news/src/term_sys.rs"),
            ),
            (
                "td_fetch.rs",
                include_str!("../../../td-mail/src/td_fetch.rs"),
                include_str!("../../../td-news/src/td_fetch.rs"),
            ),
            (
                "kv.rs",
                include_str!("../../../td-mail/src/kv.rs"),
                include_str!("../../../td-news/src/kv.rs"),
            ),
            (
                "json.rs",
                include_str!("../../../td-mail/src/json.rs"),
                include_str!("../../../td-news/src/json.rs"),
            ),
            (
                "toml.rs",
                include_str!("../../../td-mail/src/toml.rs"),
                include_str!("../../../td-news/src/toml.rs"),
            ),
            (
                "html.rs",
                include_str!("../../../td-mail/src/html.rs"),
                include_str!("../../../td-news/src/html.rs"),
            ),
            (
                "civil.rs",
                include_str!("../../../td-mail/src/civil.rs"),
                include_str!("../../../td-news/src/civil.rs"),
            ),
        ] {
            assert!(mail == news, "{name} differs between td-mail and td-news");
        }
    }
}
