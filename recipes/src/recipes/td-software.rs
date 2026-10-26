use crate::types::Recipe;

/// td-software, the application manager (td-software/DESIGN.md), built as
/// a TARGET recipe from the checkout's own trees. The crate depends by
/// path on td-civil, td-fs and the shared UI toolkit `td-ui`, and compiles
/// td-app's status record from `engine/src/flathub_app.rs` by `#[path]`;
/// td-ui mounts the compositor's font reader, its pinned Unifont face and
/// its Wayland wire codec by relative `#[path]` and embeds the licence
/// notices under `td-compositor/assets`, so those trees and `engine` are
/// staged beside it. Its lock lists only itself, td-civil, td-fs and
/// td-ui, so the closure is std and the vendor set is empty; the binary is
/// linked fully static. The image copies the complete output, debug
/// companion included, and links `/bin/td-software` to it; the card starts
/// it through td-authd's request `22`. `td-software-test` is its
/// realized-output check.
pub fn recipe() -> Recipe {
    Recipe::rust("td-software", "0.1.0")
        .local_source("td-software")
        .local_source_trees(&["td-civil", "td-fs", "td-ui", "td-compositor", "engine"])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-software")
        .cargo_lock("td-software/Cargo.lock")
        .static_link()
        .bins(&["td-software"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_checkout_package_has_the_complete_toolkit_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-software-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-software"));
        assert_eq!(
            r.local_source_trees,
            Some(
                ["td-civil", "td-fs", "td-ui", "td-compositor", "engine"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-software"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-software/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-software".into()]));
        assert!(crate::source_pins::by_key("td-software-source").is_none());
    }
}
