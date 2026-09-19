use crate::types::Recipe;

/// td-editor, the Wayland text editor td-mail composes in (APPLICATIONS.md
/// §W.5), built as a TARGET recipe from the checkout's own trees. The crate
/// depends on the shared UI toolkit `td-ui` by path, and td-ui mounts the
/// compositor's font reader, its pinned Unifont face and its Wayland wire
/// codec by relative `#[path]`, and embeds the licence notices under
/// `td-compositor/assets` by `include_str!`, so both sibling trees are
/// staged beside td-editor and cargo compiles exactly what the crate names:
/// the td-taskmgr shape. Its lock lists only itself and td-ui, so the
/// closure is std and the vendor set is empty; the binary is linked fully
/// static, as every td-owned program in an application closure must be,
/// since the static runtime shows no loader. The package meant to ship it
/// is `mail` (§W.5), copying the binary and its debug companion into the
/// application's `/app/bin`; the editor is not a system-tree program.
pub fn recipe() -> Recipe {
    Recipe::rust("td-editor", "0.1.0")
        .local_source("td-editor")
        .local_source_trees(&["td-ui", "td-compositor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-editor")
        .cargo_lock("td-editor/Cargo.lock")
        .static_link()
        .bins(&["td-editor"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_editor_is_built_from_the_checkout_with_its_siblings_staged() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("td-editor-source"));
        assert_eq!(recipe.local_source.as_deref(), Some("td-editor"));
        assert_eq!(
            recipe.local_source_trees,
            Some(vec!["td-ui".to_string(), "td-compositor".to_string()])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("td-editor"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("td-editor/Cargo.lock"));
        assert_eq!(recipe.bins, Some(vec!["td-editor".to_string()]));
        assert_eq!(recipe.static_link, Some(true));
        // No fetch pin: the bytes are the committed trees, pinned by the
        // compiled seed-digest table.
        assert!(crate::source_pins::by_key("td-editor-source").is_none());
    }

    /// The staged siblings are exactly the trees the crate reaches: td-ui by
    /// the manifest's one path dependency, and td-compositor through td-ui's
    /// `#[path]` mounts and embedded notices. A crate that grew a third
    /// reach, or td-ui a mount elsewhere, would build on the host from the
    /// whole checkout and fail only in the sandbox, so the reaches are
    /// pinned here against the sources themselves.
    #[test]
    fn the_staged_trees_are_the_ones_the_sources_reach() {
        // Every `path =` line in the manifest, whichever table it sits in:
        // cargo resolves dev- and build-dependencies for `cargo build` too,
        // so a sibling added there would also fail only in the sandbox.
        let manifest = include_str!("../../../td-editor/Cargo.toml");
        let declared: Vec<&str> = manifest
            .lines()
            .filter(|line| line.contains("path ="))
            .collect();
        assert_eq!(declared, ["td-ui = { path = \"../td-ui\" }"]);
        let toolkit = include_str!("../../../td-ui/src/lib.rs");
        let mounts: Vec<&str> = toolkit
            .lines()
            .filter_map(|line| line.trim().strip_prefix("#[path = \"../../"))
            .filter_map(|rest| rest.split('/').next())
            .collect();
        assert!(!mounts.is_empty(), "td-ui mounts nothing by path");
        assert!(
            mounts.iter().all(|dir| *dir == "td-compositor"),
            "{mounts:?}"
        );
        let notices = include_str!("../../../td-ui/src/notices.rs");
        for asset in ["PROVENANCE", "unifont-COPYING", "unifont-OFL-1.1.txt"] {
            assert!(
                notices.contains(&format!(
                    "include_str!(\"../../td-compositor/assets/{asset}\")"
                )),
                "{asset} is not embedded from the staged compositor tree"
            );
        }
    }
}
