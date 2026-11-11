#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;

#[test]
fn mount_lookup_is_exact_and_rejects_ambiguous_stacks() {
    let line = format!("42 1 0:1 / {VIEW} ro,nosuid,nodev,noexec - btrfs /dev/vda rw\n");
    assert_eq!(
        mount_options_from(&line).unwrap().as_deref(),
        Some("ro,nosuid,nodev,noexec")
    );
    assert!(mount_options_from(&format!("{line}{line}")).is_err());
    assert_eq!(
        mount_options_from(&line.replace(VIEW, &format!("{VIEW}/nested"))).unwrap(),
        None
    );
    assert_eq!(
        mount_options_from(&line.replace(VIEW, &format!("{VIEW}-other"))).unwrap(),
        None
    );
    assert!(mount_options_from("malformed\n").is_err());
    assert!(mount_options_from(&format!("42 1 0:1 / {VIEW} - btrfs /dev/vda rw\n")).is_err());
}

#[test]
fn directory_checks_reject_redirects_wrong_owners_and_other_writers() {
    let root = std::env::temp_dir().join(format!(
        "td-portal-grant-directory-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let owner = fs::metadata(&root).unwrap().uid();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    // A test identity's primary GID need not equal its UID.
    let group = fs::metadata(&root).unwrap().gid();
    assert_eq!(directory(&root, owner, true).is_ok(), group == owner);
    assert!(directory(&root, owner.wrapping_add(1), true).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(directory(&root, owner, true).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let link = root.with_extension("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    assert!(directory(&link, owner, false).is_err());
    fs::remove_file(link).unwrap();
    fs::remove_dir(root).unwrap();
}

/// The portal's Downloads view follows Firefox's grant row alone: another
/// account's Downloads, another of Firefox's directories, or no table row
/// leaves FileChooser with nothing to serve.
#[test]
fn the_portal_downloads_view_needs_firefoxs_downloads_row() {
    use crate::workload_grants::Table;
    let table = |rows: &str| Table::parse(&format!("td-grants-v1\n{rows}")).unwrap();
    assert!(downloads_granted(&Table::seeded()));
    assert!(downloads_granted(&table(
        "application\tfirefox\tdirectory\tro\tDownloads\n"
    )));
    for revoked in [
        "",
        "application\tfirefox\tdirectory\trw\tDocuments\n",
        "application\tfirefox\tdirectory\trw\tDownloads/inbox\n",
        "application\tmail\tdirectory\trw\tDownloads\n",
        "sandbox\tfirefox\tdirectory\trw\tDownloads\n",
        "workspace\tfirefox\tdirectory\trw\tDownloads\n",
    ] {
        assert!(!downloads_granted(&table(revoked)), "{revoked:?}");
    }
}
