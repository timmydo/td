use crate::types::Recipe;

/// td-net, the control-plane network multicall, built as a TARGET recipe from
/// the checkout's own trees (APPLICATIONS.md §W.8): the first control-plane
/// program rebuilt on the target toolchain, as AGENTS.md foresees, and the
/// target's one network client (APPLICATIONS.md §B.6): among others the
/// `td-fetchd` applet the jailed applications reach over a socket, Git's HTTP(S)
/// helpers and `td-curl`. `net/` is the crate; `engine/` is its path
/// dependency. The engine also shares td-civil calendar conversion and
/// td-fs helpers, td-primitives is its TLS and signature cryptography, and
/// td-tls-provider puts that behind rustls; all are staged beside it and the
/// crate's manifest names the main tree. The lock
/// is the crate's own committed one,
/// and the closure is the tier's reviewed vendored set; nothing else enters.
/// Static, as every td-owned program on the image, so the system tree's
/// `/bin/td-fetchd` link needs no loader.
pub fn recipe() -> Recipe {
    Recipe::rust("td-net", "0.1.0")
        .local_source("net")
        .local_source_trees(&[
            "engine",
            "td-fs",
            "td-civil",
            "td-primitives",
            "td-tls-provider",
        ])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
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
            Some(vec![
                "engine".to_string(),
                "td-fs".to_string(),
                "td-civil".to_string(),
                "td-primitives".to_string(),
                "td-tls-provider".to_string()
            ])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("net"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("net/Cargo.lock"));
        assert_eq!(recipe.bins, Some(vec!["td-net".to_string()]));
        assert_eq!(recipe.static_link, Some(true));
        // No C at all: ring's lock entry is inert, and any object it or anything
        // else compiled would fail the build against this declaration.
        assert_eq!(recipe.c_objects, None);
        // No fetch pin: the bytes are the committed trees, pinned by the
        // compiled seed-digest table.
        assert!(crate::source_pins::by_key("td-net-source").is_none());
    }
}
