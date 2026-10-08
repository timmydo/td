use crate::types::Recipe;

/// td-agent, the coding agent's window (td-agent/DESIGN.md), built for the
/// image as the other toolkit consumers are: a static Cargo build of its own
/// tree with its path dependencies staged beside it, td-compositor for the
/// font and wire sources td-ui mounts, engine for the SHA-256 td-agent mounts,
/// and td-test-compositor, the native harness its tests use, which cargo
/// reads to resolve the lock. No
/// feature is enabled: `test-key-root`, which moves the key walk's top for
/// the native fixtures, never ships. It runs as td's account outside
/// application confinement, as td-review does, and launches the image's
/// `/bin/td-jail --workspace` for its tools.
pub fn recipe() -> Recipe {
    Recipe::rust("td-agent", "0.1.0")
        .local_source("td-agent")
        .local_source_trees(&[
            "td-civil",
            "td-fetch-client",
            "td-fs",
            "td-html",
            "td-json",
            "td-toml",
            "td-ui",
            "td-compositor",
            "engine",
            "td-test-compositor",
        ])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "td-sh",
        ])
        .cargo_subdir("td-agent")
        .cargo_lock("td-agent/Cargo.lock")
        .static_link()
        .bins(&["td-agent"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_package_has_its_path_closure_and_no_feature() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-agent-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-agent"));
        assert_eq!(
            r.local_source_trees,
            Some(vec![
                "td-civil".into(),
                "td-fetch-client".into(),
                "td-fs".into(),
                "td-html".into(),
                "td-json".into(),
                "td-toml".into(),
                "td-ui".into(),
                "td-compositor".into(),
                "engine".into(),
                "td-test-compositor".into(),
            ])
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-agent"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-agent/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-agent".into()]));
        assert_eq!(r.features, None);
        assert_eq!(r.no_default_features, None);
        assert!(crate::source_pins::by_key("td-agent-source").is_none());
    }

    /// The staged trees are exactly what the build reads: the manifest's
    /// path dependencies, and every tree their files and td-agent's name
    /// by a literal `#[path]` or `include` macro, transitively.
    #[test]
    fn the_staged_trees_are_the_closure_its_manifest_and_files_name() {
        let manifest = include_str!("../../../td-agent/Cargo.toml");
        let mut crates = vec!["td-agent"];
        for line in manifest.lines() {
            if let Some((_, rest)) = line.split_once("{ path = \"../") {
                if !line.trim_start().starts_with('#') {
                    crates.push(rest.split('"').next().unwrap());
                }
            }
        }
        let (mut trees, mounts) = crate::source_closure::trees_named(&crates);
        assert!(mounts > 10, "{mounts}");
        trees.extend(crates.iter().map(|name| name.to_string()));
        trees.remove("td-agent");
        let staged: std::collections::BTreeSet<String> = recipe()
            .local_source_trees
            .unwrap_or_default()
            .into_iter()
            .collect();
        assert_eq!(trees, staged);
    }
}
