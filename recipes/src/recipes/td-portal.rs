use crate::types::Recipe;

#[cfg(test)]
use crate::ladder::TD_JAIL_FIXTURE_DOWNLOAD_TARGET;
#[cfg(test)]
const SYSTEM_X86_64_RS: &str = include_str!("system-x86-64.rs");
#[cfg(test)]
const MAIN_RS: &str = include_str!("../../../td-portal/src/main.rs");

/// td-portal, the supervised desktop portal service, built as a TARGET recipe
/// from the checkout's own trees. Its `main.rs` reaches the broker codec, the
/// token-protected secret store, and the compositor's wire and keyboard
/// modules by relative `#[path]`, and the engine's sha256 through td-secret;
/// the file chooser depends on the shared UI toolkit `td-ui` by path, which
/// itself mounts the compositor's font and wire. Those sibling trees are
/// staged beside td-portal so cargo compiles exactly what the crate names.
/// td-portal's one dependency is the roster sibling `td-ui`, so its lock lists
/// only itself and td-ui; the binary is linked fully static, as every td-owned
/// program on the image, so the system tree's `/bin/td-portal` needs no loader.
///
/// The former hand-rolled rustc recipe vendored those modules file by file
/// with `include_str!`; this stages the sibling trees whole, the same shape
/// td-net uses. 7(c) added `td-ui` to the roster and moved the file chooser's
/// render onto its raster and chrome bands, retiring td-portal's own font
/// mount and second rasterizer. The shipped binary's static-shape assertion
/// and target-side selftest, which the hand-rolled recipe ran inline, moved to
/// the td-portal-test companion, the same split td-ui-test makes for the
/// compositor.
pub fn recipe() -> Recipe {
    Recipe::rust("td-portal", "0.1.0")
        .local_source("td-portal")
        .local_source_trees(&["td-secret", "td-busd", "td-compositor", "engine", "td-ui"])
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir("td-portal")
        .cargo_lock("td-portal/Cargo.lock")
        .static_link()
        .bins(&["td-portal"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ladder::{
        TD_PORTAL_REQUEST_RUNTIME_MARKER, TD_PORTAL_RUNTIME_MARKER,
        TD_PORTAL_UNAVAILABLE_RUNTIME_MARKER,
    };

    #[test]
    fn td_portal_is_built_from_the_checkout_with_its_siblings_staged() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("td-portal-source"));
        assert_eq!(recipe.local_source.as_deref(), Some("td-portal"));
        assert_eq!(
            recipe.local_source_trees,
            Some(vec![
                "td-secret".to_string(),
                "td-busd".to_string(),
                "td-compositor".to_string(),
                "engine".to_string(),
                "td-ui".to_string(),
            ])
        );
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("td-portal"));
        assert_eq!(recipe.cargo_lock.as_deref(), Some("td-portal/Cargo.lock"));
        assert_eq!(recipe.bins, Some(vec!["td-portal".to_string()]));
        assert_eq!(recipe.static_link, Some(true));
        // No fetch pin: the bytes are the committed trees, pinned by the
        // compiled seed-digest table.
        assert!(crate::source_pins::by_key("td-portal-source").is_none());
    }

    #[test]
    fn the_shipped_binary_prints_the_runtime_evidence_the_qemu_scanner_greps() {
        assert!(
            MAIN_RS.contains(&format!(
                "pub const READY_MARKER: &str = \"{TD_PORTAL_RUNTIME_MARKER}\";"
            )),
            "the target probe and QEMU scanner must share one exact evidence line"
        );
        assert!(MAIN_RS.contains(&format!(
            "pub const REQUEST_READY_MARKER: &str = \"{TD_PORTAL_REQUEST_RUNTIME_MARKER}\";"
        )));
        assert!(MAIN_RS.contains(&format!(
            "pub const UNAVAILABLE_READY_MARKER: &str =\n    \
             \"{TD_PORTAL_UNAVAILABLE_RUNTIME_MARKER}\";"
        )));
        assert!(MAIN_RS.contains("println!(\"{READY_MARKER}\");"));
        assert!(MAIN_RS.contains("println!(\"{REQUEST_READY_MARKER}\");"));
        assert!(MAIN_RS.contains("println!(\"{UNAVAILABLE_READY_MARKER}\");"));
    }

    #[test]
    fn shared_download_view_is_outside_private_reserved_mount_trees() {
        let view = MAIN_RS
            .split_once("const FIREFOX_HOST_DOWNLOADS: &str = \"")
            .unwrap().1.split('"').next().unwrap();
        let view = std::path::Path::new(view);
        for reserved in crate::permissions::RESERVED_FILESYSTEM_TREES {
            let reserved = std::path::Path::new(reserved);
            assert!(!view.starts_with(reserved) && !reserved.starts_with(view),
                "shared Downloads view {} would reserve the original grant through {}",
                view.display(), reserved.display());
        }
    }

    #[test]
    fn portal_and_firefox_share_the_exact_download_grant_pair() {
        assert_eq!(TD_JAIL_FIXTURE_DOWNLOAD_TARGET, "/home/td/Downloads");
        assert!(MAIN_RS
            .contains("const FIREFOX_HOST_DOWNLOADS: &str = \"/var/td-portal-files/1000/Downloads\";"));
        assert!(MAIN_RS.contains("const FIREFOX_GUEST_DOWNLOADS: &str = \"/home/td/Downloads\";"));
        let grant = include_str!("../../../td-authd/src/portal_files.rs");
        assert!(grant.contains("const VIEW: &str = \"/var/td-portal-files/1000/Downloads\";"));
        for step in [
            "let root = directory(Path::new(\"/\"), 0, true)?;",
            "let var = child(&root, \"var\", 0, true)?;",
            "let home = child(&var, \"home\", 0, true)?;",
            "let account = crate::primary_account::load().map_err(|error| error.to_string())?;",
            "let human = child(&home, account.name(), HUMAN, true)?;",
            "let source = child(&human, \"Downloads\", HUMAN, false)?;",
        ] { assert!(grant.contains(step), "{step}"); }
        assert!(SYSTEM_X86_64_RS
            .contains("const FIREFOX_DOWNLOAD_SOURCE: &str = \"/var/home/tester/Downloads\";"));
        assert!(SYSTEM_X86_64_RS.contains(
            r#"user_pref(\\\"browser.download.dir\\\", \\\"/home/td/Downloads\\\");"#
        ));
    }
}
