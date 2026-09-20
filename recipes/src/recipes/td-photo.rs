use crate::types::Recipe;

/// td-photo, the photo tool (td-photo/DESIGN.md), built as a TARGET recipe
/// from the checkout's own trees. The crate depends on the shared UI
/// toolkit `td-ui` by path, and td-ui mounts the compositor's font reader,
/// its pinned Unifont face and its Wayland wire codec by relative `#[path]`
/// and embeds the licence notices under `td-compositor/assets`, so both
/// sibling trees are staged beside td-photo and cargo compiles exactly what
/// the crate names: the td-editor and td-taskmgr shape. Its lock lists only
/// itself and td-ui, so the closure is std and the vendor set is empty; the
/// binary is linked fully static, as every td-owned program in the system
/// tree is. The image copies the complete output, debug companion included,
/// and links `/bin/td-photo` to it; `td-photo-test` is its realized-output
/// check.
pub fn recipe() -> Recipe {
    Recipe::rust("td-photo", "0.1.0")
        .local_source("td-photo")
        .local_source_trees(&["td-ui", "td-compositor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-photo")
        .cargo_lock("td-photo/Cargo.lock")
        .static_link()
        .bins(&["td-photo"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_checkout_package_has_the_complete_toolkit_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-photo-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-photo"));
        assert_eq!(
            r.local_source_trees,
            Some(vec!["td-ui".into(), "td-compositor".into()])
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-photo"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-photo/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-photo".into()]));
        assert!(crate::source_pins::by_key("td-photo-source").is_none());
    }
}
