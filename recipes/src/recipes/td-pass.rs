use crate::types::Recipe;

/// td-pass, the encrypted notebook (td-pass/DESIGN.md), built as a TARGET
/// recipe from the checkout's own trees. The crate depends by path on the
/// shared UI toolkit `td-ui` and on td-secret's notebook library. td-ui
/// mounts the compositor's font reader and wire codec and embeds its licence
/// notices, so `td-compositor` is staged as for td-photo; td-secret mounts
/// td-authd's request, descriptor and consent modules, td-busd's D-Bus codec
/// and td-firstboot's principal loader by relative `#[path]`, and those reach
/// td-authd's account file and engine's principal source, and td-secret's
/// crypto mounts engine's SHA-256, so those trees are staged beside it, with
/// td-tpm, the TPM client crate td-secret depends on by path. Its lock lists
/// only td-pass, td-secret, td-tpm and td-ui, so the closure is std and
/// the vendor set is empty; the binary is linked fully static. The image
/// copies the complete output, debug companion included, and links
/// `/bin/td-pass` to it, so the same executable can be carried to a foreign
/// host; on td itself the window refuses until td mode is built.
/// `td-pass-test` is its realized-output check.
pub fn recipe() -> Recipe {
    Recipe::rust("td-pass", "0.1.0")
        .local_source("td-pass")
        .local_source_trees(&[
            "td-secret",
            "td-ui",
            "td-compositor",
            "td-authd",
            "td-busd",
            "td-firstboot",
            "engine",
            "td-tpm",
            "td-test-compositor",
        ])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "td-sh",
        ])
        .cargo_subdir("td-pass")
        .cargo_lock("td-pass/Cargo.lock")
        .static_link()
        .bins(&["td-pass"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_checkout_package_has_the_complete_library_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-pass-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-pass"));
        assert_eq!(
            r.local_source_trees,
            Some(
                [
                    "td-secret",
                    "td-ui",
                    "td-compositor",
                    "td-authd",
                    "td-busd",
                    "td-firstboot",
                    "engine",
                    "td-tpm",
                    "td-test-compositor"
                ]
                .map(String::from)
                .to_vec()
            )
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-pass"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-pass/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-pass".into()]));
        // The test vault (td-pass/src/backend/fixture.rs) is a feature no
        // build of the shipped binary enables.
        assert_eq!(r.features, None);
        assert_eq!(r.no_default_features, None);
        assert!(crate::source_pins::by_key("td-pass-source").is_none());
    }

    /// The trees td-pass's build reads are exactly the ones staged: every
    /// file the four compiled crates name by a literal `#[path]` or
    /// `include` macro, test code included, and every file those name in
    /// turn, resolved against the naming file's directory, lies in
    /// td-pass or a staged tree, and every staged tree is reached.
    #[test]
    fn the_staged_trees_are_the_closure_its_files_name() {
        let (mut trees, mounts) =
            crate::source_closure::trees_named(&["td-pass", "td-secret", "td-tpm", "td-ui"]);
        assert!(mounts > 100, "{mounts}");
        trees.remove("td-pass");
        trees.insert("td-secret".to_owned());
        trees.insert("td-tpm".to_owned());
        trees.insert("td-ui".to_owned());
        // The native harness: a dev-dependency, which cargo reads to
        // resolve the lock though no shipped file names it.
        trees.insert("td-test-compositor".to_owned());
        let staged: std::collections::BTreeSet<String> = recipe()
            .local_source_trees
            .unwrap_or_default()
            .into_iter()
            .collect();
        assert_eq!(trees, staged);
    }

    /// The manifest names exactly the two path dependencies staged here,
    /// and the native harness its tests alone use.
    #[test]
    fn the_manifest_depends_on_the_staged_library_and_toolkit() {
        let manifest = include_str!("../../../td-pass/Cargo.toml");
        let paths: Vec<&str> = manifest
            .lines()
            .filter(|line| line.contains("path ="))
            .collect();
        assert_eq!(
            paths,
            [
                "td-ui = { path = \"../td-ui\" }",
                "td-secret = { path = \"../td-secret\" }",
                "td-test-compositor = { path = \"../td-test-compositor\" }"
            ]
        );
        let lock = include_str!("../../../td-pass/Cargo.lock");
        assert!(!lock.contains("source ="));
    }
}
