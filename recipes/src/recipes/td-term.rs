use crate::types::Recipe;

/// td-term, the terminal (td-term/DESIGN.md), built as a TARGET recipe from
/// the checkout's own trees. The crate depends on the shared UI toolkit
/// `td-ui` by path, which carries the terminal model, renderer, keyboard
/// encoder, terminfo compiler and PTY mechanism, and td-ui mounts the
/// compositor's font reader, its pinned Unifont face, its Wayland wire codec
/// and its record filter by relative `#[path]` and embeds the licence notices
/// under `td-compositor/assets`, so both sibling trees are staged beside
/// td-term: the td-photo shape. Its lock lists only itself and td-ui, so the
/// closure is std and the vendor set is empty; the binary is linked fully
/// static, as every td-owned program in the system tree is. The compiled
/// terminfo entry is data this binary writes, so `td-term-terminfo` produces
/// it from this output; `td-term-test` is the realized-output check.
pub fn recipe() -> Recipe {
    Recipe::rust("td-term", "0.1.0")
        .local_source("td-term")
        .local_source_trees(&["td-ui", "td-compositor"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-term")
        .cargo_lock("td-term/Cargo.lock")
        .static_link()
        .bins(&["td-term"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ladder::TD_TERM_RUNTIME_MARKER;

    #[test]
    fn static_checkout_package_has_the_complete_toolkit_source_closure() {
        let r = recipe();
        assert_eq!(r.source_input.as_deref(), Some("td-term-source"));
        assert_eq!(r.local_source.as_deref(), Some("td-term"));
        assert_eq!(
            r.local_source_trees,
            Some(vec!["td-ui".into(), "td-compositor".into()])
        );
        assert_eq!(r.cargo_subdir.as_deref(), Some("td-term"));
        assert_eq!(r.cargo_lock.as_deref(), Some("td-term/Cargo.lock"));
        assert_eq!(r.static_link, Some(true));
        assert_eq!(r.bins, Some(vec!["td-term".into()]));
        assert!(crate::source_pins::by_key("td-term-source").is_none());
    }

    /// The terminal's readiness marker is the boot oracle's first-client
    /// proof, and the ladder's copy and the one td-term prints live in crates
    /// that cannot import each other, so the literal is pinned here.
    #[test]
    fn the_readiness_marker_is_the_one_the_boot_check_reads() {
        let ready = include_str!("../../../td-term/src/ready.rs");
        assert!(ready.contains(&format!(
            "pub const MARKER: &str = \"{TD_TERM_RUNTIME_MARKER}\";"
        )));
    }

    /// td-term's child command is an absolute path into a DIFFERENT staged
    /// package plus a flag that package must parse. Neither crate compiles
    /// against the other, so nothing but this would notice `--stdin` being
    /// renamed on one side: the terminal would build, ship, and fail at the
    /// first spawn.
    #[test]
    fn the_terminals_session_wrapper_matches_the_staged_td_init() {
        const SESSION: &str = include_str!("../../../td-term/src/session.rs");
        const CTTYHACK: &str = include_str!("../../../td-init/src/cttyhack.rs");
        assert!(SESSION.contains(r#"pub const CTTYHACK: &str = "/bin/cttyhack";"#));
        assert!(SESSION.contains(r#"pub const CTTYHACK_STDIN: &str = "--stdin";"#));
        assert!(CTTYHACK.contains(r#"const STDIN_FLAG: &str = "--stdin";"#));
        // And that the applet still advertises it, so `cttyhack` alone tells an
        // operator the mode exists.
        assert!(CTTYHACK.contains("usage: cttyhack [--stdin] PROG [ARG...]"));
        // `/bin/cttyhack` is td-init's own symlink name in the image roster.
        const INIT_MAIN: &str = include_str!("../../../td-init/src/main.rs");
        assert!(INIT_MAIN.contains(r#"("cttyhack", cttyhack::run)"#));
    }
}
