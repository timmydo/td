use crate::types::Recipe;

/// td-net, the control-plane network multicall, built as a TARGET recipe from
/// the checkout's own trees (APPLICATIONS.md §W.8): the first control-plane
/// program rebuilt on the target toolchain, as AGENTS.md foresees, and the
/// target's one network client, for the `td-fetchd` applet the jailed
/// applications reach over a socket. `net/` is the crate; `engine/` is its
/// path dependency and `td-boot/` the crate whose deployment-protocol sources
/// it includes by relative path, so both are staged beside it and the crate's
/// manifest names the main tree. The lock is the crate's own committed one,
/// and the closure is the tier's reviewed vendored set; nothing else enters.
/// Static, as every td-owned program on the image, so the system tree's
/// `/bin/td-fetchd` link needs no loader.
pub fn recipe() -> Recipe {
    Recipe::rust("td-net", "0.1.0")
        .local_source("net")
        .local_source_trees(&["engine", "td-boot"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("net")
        .cargo_lock("net/Cargo.lock")
        .static_link()
        .bins(&["td-net"])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn td_net_is_built_from_the_checkout_with_its_siblings_staged() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("td-net-source"));
        assert_eq!(recipe.local_source.as_deref(), Some("net"));
        assert_eq!(
            recipe.local_source_trees,
            Some(vec!["engine".to_string(), "td-boot".to_string()])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("net"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("net/Cargo.lock"));
        assert_eq!(recipe.bins, Some(vec!["td-net".to_string()]));
        assert_eq!(recipe.static_link, Some(true));
        // No fetch pin: the bytes are the committed trees, pinned by the
        // compiled seed-digest table.
        assert!(crate::source_pins::by_key("td-net-source").is_none());
    }
}
