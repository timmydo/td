use crate::types::Recipe;

/// td-review, the integrator's branch review and landing window, built for
/// the image as the other toolkit consumers are: a static Cargo build of its
/// own tree with td-ui and td-compositor, whose font and wire sources td-ui
/// mounts, staged beside it. It runs the image's `/bin/git`.
pub fn recipe() -> Recipe {
    Recipe::rust("td-review", "0.1.0")
        .local_source("td-review")
        .local_source_trees(&["td-ui", "td-compositor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-review")
        .cargo_lock("td-review/Cargo.lock")
        .static_link()
        .bins(&["td-review"])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn static_checkout_package_has_the_complete_toolkit_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-review-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-review"));
        assert_eq!(
            r.local_source_trees,
            Some(vec!["td-ui".into(), "td-compositor".into()])
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-review"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-review/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-review".into()]));
        assert!(crate::source_pins::by_key("td-review-source").is_none());
    }
}
