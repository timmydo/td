#![allow(clippy::unwrap_used)]

use super::*;

const TABLE: &str = "td-principals-v1\nsession\t1000\t993\t991\t989\nsession\t1001\t992\t990\t988\napplication\t1000\tfirefox\t65536\napplication\t1000\tmail\t65538\napplication\t1001\tmail\t65537\n";

const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\ntester:x:1000:1000:Tester:/home/tester:/bin/sh\nother:x:1001:1001:Other:/home/other:/bin/sh\n";
const GROUP: &str = "root:x:0:\ntester:x:1000:\nother:x:1001:\nwheel:x:10:tester\n";
const SHADOW: &str = "root::0:0:99999:7:::\ntester::0:0:99999:7:::\nother::0:0:99999:7:::\n";

#[test]
fn reservations_allow_only_unique_service_accounts_without_supplementary_authority() {
    let registry = Registry::parse(TABLE).unwrap();
    registry.verify_accounts(PASSWD, GROUP, SHADOW).unwrap();
    let passwd = format!("{PASSWD}tdc1000:x:993:993:Compositor:/run/tdc1000:/bin/false\n");
    let group = format!("{GROUP}tdc1000:x:993:\n");
    let shadow = format!("{SHADOW}tdc1000:!td-service:0:0:99999:7:::\n");
    registry.verify_accounts(&passwd, &group, &shadow).unwrap();
    assert!(registry.verify_accounts(&passwd, GROUP, &shadow).is_err());
    assert!(registry.verify_accounts(PASSWD, GROUP, &shadow).is_err());
    for altered in [
        passwd.replace("tdc1000", "alias"),
        passwd.replace("993:993", "993:1000"),
        passwd.replace("/bin/false", "/bin/sh"),
        format!("{passwd}alias:x:993:993:A:/run/a:/bin/sh\n"),
        format!("{passwd}alias:x:999:993:A:/run/a:/bin/sh\n"),
    ] {
        assert!(registry.verify_accounts(&altered, &group, &shadow).is_err());
    }
    for altered in [
        group.replace("tdc1000", "alias"),
        group.replace("993:", "993:tester"),
        group.replace("10:tester", "10:tester,tdc1000"),
        format!("{group}alias:x:993:\n"),
    ] {
        assert!(registry
            .verify_accounts(&passwd, &altered, &shadow)
            .is_err());
    }
    for altered in [
        shadow.replace("!td-service", ""),
        shadow.replace("!td-service", "!"),
        SHADOW.to_string(),
        format!("{shadow}tdc1000::0:0:99999:7:::\n"),
    ] {
        assert!(registry.verify_accounts(&passwd, &group, &altered).is_err());
    }
}

#[test]
fn account_validation_covers_retired_reservations_and_missing_humans() {
    let retired = Registry::parse(TABLE).unwrap();
    let current = Registry::parse("td-principals-v1\nsession\t1000\t993\t991\t989\n").unwrap();
    let retained = retired.enroll(&current).unwrap();
    let reused = format!("{PASSWD}daemon:x:65537:65537:New:/run/new:/bin/false\n");
    assert!(retained.verify_accounts(&reused, GROUP, SHADOW).is_err());
    assert!(retained
        .verify_accounts("root:x:0:0:root:/root:/bin/sh\n", GROUP, SHADOW)
        .is_err());
}

#[test]
fn removed_human_accounts_may_disappear_without_freeing_their_reservations() {
    let old = Registry::parse(TABLE).unwrap();
    let active = Registry::parse("td-principals-v1\nsession\t1000\t993\t991\t989\n").unwrap();
    let passwd = "root:x:0:0:root:/root:/bin/sh\ntester:x:1000:1000:Tester:/home/tester:/bin/sh\n";
    old.verify_retained_accounts(&active, passwd, GROUP, SHADOW)
        .unwrap();
    assert!(old
        .verify_retained_accounts(&old, passwd, GROUP, SHADOW)
        .is_err());
}

#[test]
fn staged_configuration_check_is_read_only_and_refuses_substituted_files() {
    use std::fs::{self, DirBuilder, OpenOptions, Permissions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let root = std::env::temp_dir().join(format!("td-principal-check-{}", std::process::id()));
    DirBuilder::new().mode(0o700).create(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let etc = root.join("etc");
    DirBuilder::new().mode(0o700).create(&etc).unwrap();
    for (name, contents, mode) in [
        ("td-principals.tsv", TABLE, 0o444),
        ("passwd", PASSWD, 0o644),
        ("group", GROUP, 0o644),
        ("shadow", SHADOW, 0o600),
    ] {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(etc.join(name))
            .unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.set_permissions(Permissions::from_mode(mode)).unwrap();
    }
    check_deployment(&root).unwrap();
    assert!(!root.join("var").exists());
    fs::rename(etc.join("passwd"), etc.join("other-passwd")).unwrap();
    std::os::unix::fs::symlink("other-passwd", etc.join("passwd")).unwrap();
    assert!(check_deployment(&root).is_err());
    fs::remove_file(etc.join("passwd")).unwrap();
    fs::hard_link(etc.join("other-passwd"), etc.join("passwd")).unwrap();
    assert!(check_deployment(&root).is_err());
    fs::remove_file(etc.join("other-passwd")).unwrap();
    check_deployment(&root).unwrap();
    fs::set_permissions(etc.join("shadow"), Permissions::from_mode(0o644)).unwrap();
    assert!(check_deployment(&root).is_err());
}

#[test]
fn launch_session_binds_the_human_name_uid_gid_and_compositor() {
    let registry =
        super::Registry::parse("td-principals-v1\nsession\t1000\t993\t992\t991\n").unwrap();
    let passwd = "root:x:0:0:root:/root:/bin/sh\ntester:x:1000:1000:Test:/home/tester:/bin/sh\n";
    assert!(registry.launch_session("tester", 1000, 993, passwd).is_ok());
    for (user, owner, compositor) in [
        ("root", 1000, 993),
        ("missing", 1000, 993),
        ("tester", 1001, 993),
        ("tester", 1000, 992),
    ] {
        assert!(registry
            .launch_session(user, owner, compositor, passwd)
            .is_err());
    }
    for table in [
        passwd.replace("1000:1000", "1000:1001"),
        passwd.replace("1000:1000", "1001:1001"),
        format!("{passwd}tester:x:1000:1000:Test:/home/tester:/bin/sh\n"),
    ] {
        assert!(registry
            .launch_session("tester", 1000, 993, &table)
            .is_err());
    }
}

#[test]
fn the_full_account_check_rejects_a_second_name_for_the_human_uid() {
    let registry = Registry::parse("td-principals-v1\nsession\t1000\t993\t992\t991\n").unwrap();
    let passwd = "root:x:0:0:root:/root:/bin/sh\ntester:x:1000:1000:Test:/home/tester:/bin/sh\n";
    let group = "root:x:0:\ntester:x:1000:\n";
    let shadow = "root::1:0:99999:7:::\ntester::1:0:99999:7:::\n";
    assert!(registry.verify_accounts(passwd, group, shadow).is_ok());
    assert!(registry
        .verify_accounts(
            &format!("{passwd}alias:x:1000:1000:Alias:/home/alias:/bin/sh\n"),
            group,
            &format!("{shadow}alias::1:0:99999:7:::\n")
        )
        .is_err());
}

#[test]
fn malformed_launch_names_have_bounded_escaped_diagnostics() {
    let registry = Registry::parse("td-principals-v1\nsession\t1000\t993\t992\t991\n").unwrap();
    let table = format!("{}:bad\n", "\u{1b}".repeat(1000));
    let error = registry
        .launch_session("tester", 1000, 993, &table)
        .unwrap_err();
    assert!(!error.contains('\u{1b}'));
    assert!(error.len() < 300);
    assert!(error.contains("invalid launch account record"));
}
