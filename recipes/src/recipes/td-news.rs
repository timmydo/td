use crate::types::Recipe;

/// td-news, td's terminal feed reader and the program the `news` application
/// packages (APPLICATIONS.md §W.8). Built as td-mail is: a static ET_EXEC by
/// direct rustc from the checkout's own `td-news/` tree, the `td-news-source`
/// seed pinned by the compiled seed-digest table, with a std closure and no
/// vendor set or lock of its own.
pub fn recipe() -> Recipe {
    crate::ladder::static_local_source_program("td-news")
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn td_news_is_a_static_program_built_from_the_checkout() {
        let recipe = recipe();
        assert_eq!(recipe.name, "td-news");
        assert_eq!(recipe.local_source.as_deref(), Some("td-news"));
        assert_eq!(recipe.local_source_trees, None);
        assert_eq!(recipe.source_input.as_deref(), Some("td-news-source"));
        assert_eq!(recipe.cargo_lock, None);
        assert!(crate::source_pins::by_key("td-news-source").is_none());
        let steps = recipe.steps.as_ref().expect("steps");
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyTree { from, dest } if from == "{in:td-news-source}" && dest == "{src}"
        )));
        // Spelled in two halves, as td-mail's test explains.
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::Run { argv, .. }
                if argv.iter().any(|arg| {
                    arg.starts_with("{src}/") && arg.ends_with("/src/main.rs")
                }) && argv.iter().any(|arg| arg == "{out}/bin/td-news")
        )));
        assert!(matches!(
            steps.last(),
            Some(Step::AssertStatic { paths }) if paths == &["{out}/bin/td-news".to_string()]
        ));
    }

    /// `--edition 2021` is the crate's own: cargo never sees this build, so
    /// the manifest and the flag agree by this test rather than by cargo.
    #[test]
    fn td_news_is_compiled_in_its_manifest_edition() {
        let manifest = include_str!("../../../td-news/Cargo.toml");
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
}
