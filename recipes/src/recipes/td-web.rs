use crate::types::Recipe;

/// td-web, td's own document browser (td-web/DESIGN.md) and the program the
/// `web` application packages. Built as the other toolkit consumers are: a
/// static Cargo build of its own tree with its path dependencies staged
/// beside it, td-compositor for the font and wire sources td-ui mounts,
/// engine for td-png's CRC and inflater, and td-util for td-png's DEFLATE
/// encoder, the `td-web-source` seed pinned by the compiled local-source
/// roster, and the crate's committed lock naming itself and those crates.
pub fn recipe() -> Recipe {
    Recipe::rust("td-web", "0.1.0")
        .local_source("td-web")
        .local_source_trees(&[
            "td-fetch-client",
            "td-html",
            "td-jpeg",
            "td-png",
            "td-ui",
            "td-url",
            "td-util",
            "td-compositor",
            "engine",
        ])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-web")
        .cargo_lock("td-web/Cargo.lock")
        .static_link()
        .bins(&["td-web"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_web_is_a_static_toolkit_program_built_from_the_checkout() {
        let r = recipe();
        assert_eq!(r.name, "td-web");
        assert_eq!(r.source_input.as_deref(), Some("td-web-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-web"));
        assert_eq!(
            r.local_source_trees,
            Some(vec![
                "td-fetch-client".into(),
                "td-html".into(),
                "td-jpeg".into(),
                "td-png".into(),
                "td-ui".into(),
                "td-url".into(),
                "td-util".into(),
                "td-compositor".into(),
                "engine".into(),
            ])
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-web"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-web/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-web".into()]));
        assert_eq!(r.features, None);
        assert_eq!(r.no_default_features, None);
        // No fetch pin: the bytes are the committed tree.
        assert!(crate::source_pins::by_key("td-web-source").is_none());
    }

    /// The lock the recipe names is the crate's own, and it closes over
    /// exactly the crate and its td siblings: a registry or git entry would
    /// be a dependency the gate refuses.
    #[test]
    fn td_web_lock_names_the_crate_and_its_td_siblings() {
        let lock = include_str!("../../../td-web/Cargo.lock");
        let names: Vec<&str> = lock
            .lines()
            .filter_map(|line| line.strip_prefix("name = \""))
            .filter_map(|rest| rest.strip_suffix('"'))
            .collect();
        assert_eq!(
            names,
            [
                "td-fetch-client",
                "td-html",
                "td-jpeg",
                "td-png",
                "td-ui",
                "td-url",
                "td-web",
            ]
        );
        assert!(!lock.contains("source = "));
    }

    /// The staged trees are exactly what the build reads: the manifest's
    /// path dependencies, and every tree their files and td-web's name by a
    /// literal `#[path]` or `include` macro, transitively.
    #[test]
    fn the_staged_trees_are_the_closure_its_manifest_and_files_name() {
        let manifest = include_str!("../../../td-web/Cargo.toml");
        let mut crates = vec!["td-web"];
        for line in manifest.lines() {
            if let Some((_, rest)) = line.split_once("{ path = \"../") {
                if !line.trim_start().starts_with('#') {
                    crates.extend(rest.split('"').next());
                }
            }
        }
        let (mut trees, mounts) = crate::source_closure::trees_named(&crates);
        assert!(mounts > 0, "{mounts}");
        trees.extend(crates.iter().map(|name| name.to_string()));
        trees.remove("td-web");
        let staged: std::collections::BTreeSet<String> = recipe()
            .local_source_trees
            .unwrap_or_default()
            .into_iter()
            .collect();
        assert_eq!(trees, staged);
    }
}
