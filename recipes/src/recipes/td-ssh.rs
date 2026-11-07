use crate::types::Recipe;

/// td's SSH client, server and key tool (td-ssh/DESIGN.md), built as a TARGET
/// recipe from the checkout's own trees. The crate's cryptography is
/// td-primitives and its base64 td-encoding, both by path; its server mounts
/// td-login's account tables and status reader by relative `#[path]`, so td-login
/// is staged beside them, with td-fs and td-source-scan, which its lock names for
/// its tests, td-source-scan being td-primitives' dev-dependency too. The lock lists
/// only td crates, so the closure is std and the vendor set is empty; the
/// binaries are linked fully static, as each server stage re-executes its own
/// file with nothing else reachable from its jail. `td-ssh-test` is its
/// realized-output check.
pub fn recipe() -> Recipe {
    Recipe::rust("td-ssh", "0.1.0")
        .local_source("td-ssh")
        .local_source_trees(&[
            "td-primitives",
            "td-encoding",
            "td-login",
            "td-fs",
            "td-source-scan",
        ])
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .cargo_subdir("td-ssh")
        .cargo_lock("td-ssh/Cargo.lock")
        .static_link()
        .bins(&["td-ssh", "td-sshd", "td-ssh-keygen"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_checkout_package_has_the_complete_library_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-ssh-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-ssh"));
        assert_eq!(
            r.local_source_trees,
            Some(
                [
                    "td-primitives",
                    "td-encoding",
                    "td-login",
                    "td-fs",
                    "td-source-scan"
                ]
                .map(String::from)
                .to_vec()
            )
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-ssh"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-ssh/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(
            r.bins,
            Some(vec![
                "td-ssh".into(),
                "td-sshd".into(),
                "td-ssh-keygen".into()
            ])
        );
        assert!(crate::source_pins::by_key("td-ssh-source").is_none());
    }
}
