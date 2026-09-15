use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-taskmgr", "0.1.0")
        .local_source("td-taskmgr")
        .local_source_trees(&["td-ui", "td-compositor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-taskmgr")
        .cargo_lock("td-taskmgr/Cargo.lock")
        .static_link()
        .bins(&["td-taskmgr"])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn static_checkout_package_has_the_complete_toolkit_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-taskmgr-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-taskmgr"));
        assert_eq!(
            r.local_source_trees,
            Some(vec!["td-ui".into(), "td-compositor".into()])
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-taskmgr"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-taskmgr/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-taskmgr".into()]));
        assert!(crate::source_pins::by_key("td-taskmgr-source").is_none());
    }
}
