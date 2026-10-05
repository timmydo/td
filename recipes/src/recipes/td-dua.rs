use crate::types::Recipe;

/// td-dua, the disk usage analyzer (td-dua/DESIGN.md), built as a TARGET
/// recipe from the checkout's own trees. Its closure is td-photo's with
/// td-json added: the crate depends on td-civil, td-json (the report's
/// writer) and the shared UI toolkit `td-ui` by path, and td-ui mounts the
/// compositor's font reader, its pinned Unifont face and its Wayland wire
/// codec by relative `#[path]` and embeds the licence notices under
/// `td-compositor/assets`, so those four trees are staged beside td-dua.
/// Its lock lists only itself, td-civil, td-json and td-ui, so the closure
/// is std and the vendor set is empty; the binary is linked fully
/// static. The image copies the complete output, debug companion included,
/// and links `/bin/td-dua` to it; `td-dua-test` is its realized-output
/// check.
pub fn recipe() -> Recipe {
    Recipe::rust("td-dua", "0.1.0")
        .local_source("td-dua")
        .local_source_trees(&["td-civil", "td-json", "td-ui", "td-compositor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "td-sh",
        ])
        .cargo_subdir("td-dua")
        .cargo_lock("td-dua/Cargo.lock")
        .static_link()
        .bins(&["td-dua"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_checkout_package_has_the_complete_toolkit_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-dua-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-dua"));
        assert_eq!(
            r.local_source_trees,
            Some(vec![
                "td-civil".into(),
                "td-json".into(),
                "td-ui".into(),
                "td-compositor".into()
            ])
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-dua"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-dua/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-dua".into()]));
        assert!(crate::source_pins::by_key("td-dua-source").is_none());
    }
}
