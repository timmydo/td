use crate::types::Recipe;

/// td-mixer, the sound panel (APPLICATIONS.md §K.5), built as a TARGET
/// recipe from the checkout's own trees. The crate depends by path on
/// td-alloc, the Pulse client td-pulse and the shared UI toolkit td-ui;
/// td-ui mounts the compositor's font reader, its pinned Unifont face and
/// its Wayland wire codec by relative `#[path]` and embeds the licence
/// notices under `td-compositor/assets`, so that tree is staged beside
/// them. Its lock lists only itself and those three, so the closure is std
/// and the vendor set is empty; the binary is linked fully static. The
/// image copies the complete output, debug companion included, and links
/// `/bin/td-mixer` to it; the launcher's SOUND entry starts it through
/// td-authd's request `30`. `td-mixer-test` is its realized-output check.
pub fn recipe() -> Recipe {
    Recipe::rust("td-mixer", "0.1.0")
        .local_source("td-mixer")
        .local_source_trees(&["td-alloc", "td-pulse", "td-ui", "td-compositor"])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-mixer")
        .cargo_lock("td-mixer/Cargo.lock")
        .static_link()
        .bins(&["td-mixer"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_checkout_package_has_the_complete_toolkit_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-mixer-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-mixer"));
        assert_eq!(
            r.local_source_trees,
            Some(
                ["td-alloc", "td-pulse", "td-ui", "td-compositor"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-mixer"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-mixer/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-mixer".into()]));
        assert!(crate::source_pins::by_key("td-mixer-source").is_none());
    }
}
