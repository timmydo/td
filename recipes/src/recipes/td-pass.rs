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

    /// Every literal `#[path]`, `include!`, `include_str!` and
    /// `include_bytes!` in `text`, with any `cfg_attr` path, as the file
    /// spells it; a name built with `concat!` is not one.
    fn named(text: &str) -> Vec<&str> {
        let mut found = Vec::new();
        for marker in [
            "path = \"",
            "include!(\"",
            "include_str!(\"",
            "include_bytes!(\"",
        ] {
            for (at, _) in text.match_indices(marker) {
                let rest = text.get(at + marker.len()..).unwrap_or_default();
                if let Some(end) = rest.find('"') {
                    found.push(rest.get(..end).unwrap_or_default());
                }
            }
        }
        found
    }

    /// `path` with its `..` and `.` components taken out lexically.
    fn normal(path: &std::path::Path) -> std::path::PathBuf {
        let mut out = std::path::PathBuf::new();
        for part in path.components() {
            match part {
                std::path::Component::ParentDir => {
                    assert!(out.pop(), "{} leaves the checkout", path.display());
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        out
    }

    /// The trees td-pass's build reads are exactly the ones staged: every
    /// file the four compiled crates name by a literal `#[path]` or
    /// `include` macro, test code included, and every file those name in
    /// turn, resolved against the naming file's directory, lies in
    /// td-pass or a staged tree, and every staged tree is reached.
    #[test]
    fn the_staged_trees_are_the_closure_its_files_name() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let mut pending: Vec<std::path::PathBuf> = Vec::new();
        for krate in ["td-pass", "td-secret", "td-tpm", "td-ui"] {
            let mut dirs = vec![root.join(krate).join("src")];
            while let Some(dir) = dirs.pop() {
                for entry in std::fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        dirs.push(path);
                    } else if path.extension().is_some_and(|e| e == "rs") {
                        pending.push(path.strip_prefix(&root).unwrap().to_path_buf());
                    }
                }
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut trees = std::collections::BTreeSet::new();
        let mut mounts = 0;
        while let Some(file) = pending.pop() {
            if !seen.insert(file.clone()) {
                continue;
            }
            let text = std::fs::read_to_string(root.join(&file)).unwrap();
            for name in named(&text) {
                let target = normal(&file.parent().unwrap().join(name));
                assert!(
                    root.join(&target).is_file(),
                    "{} names {name}, which is not a file",
                    file.display()
                );
                let tree = target.components().next().unwrap();
                trees.insert(tree.as_os_str().to_string_lossy().into_owned());
                mounts += 1;
                if target.extension().is_some_and(|e| e == "rs") {
                    pending.push(target);
                }
            }
        }
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
