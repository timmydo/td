#![allow(clippy::unwrap_used)]

use super::*;

const TABLE: &str = "td-principals-v1\nsession\t1000\t993\t991\t989\nsession\t1001\t992\t990\t988\napplication\t1000\tfirefox\t65536\napplication\t1000\tmail\t65538\napplication\t1001\tmail\t65537\n";

const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\ntester:x:1000:1000:Tester:/home/tester:/bin/sh\nother:x:1001:1001:Other:/home/other:/bin/sh\n";
const GROUP: &str = "root:x:0:\ntester:x:1000:\nother:x:1001:\nwheel:x:10:tester\n";
const SHADOW: &str = "root::0:0:99999:7:::\ntester::0:0:99999:7:::\nother::0:0:99999:7:::\n";

#[test]
fn primary_table_rename_preserves_credentials_reservations_and_other_fields() {
    let registry = Registry::parse(TABLE).unwrap();
    let passwd = format!(
        "{}tdc1000:x:993:993:Compositor:/run/tdc1000:/bin/false\n\
        tda65536:x:65536:65536:Browser:/var/lib/td/applications/65536:/bin/false\n",
        PASSWD.replace(":Tester:", ":tester comment:")
    );
    let group = format!("{GROUP}tdc1000:x:993:\ntda65536:x:65536:\nteam:x:2000:tester,other\n");
    let shadow = format!(
        "{}tdc1000:!td-service:0:0:99999:7:::\n\
        tda65536:!td-service:0:0:99999:7:::\n",
        SHADOW.replace("tester::", "tester:opaque-tester-hash:")
    );
    let renamed = rename_primary_tables(&registry, &passwd, &group, &shadow, "alice").unwrap();
    assert_eq!(
        renamed.passwd,
        passwd.replace(
            "tester:x:1000:1000:tester comment:/home/tester:",
            "alice:x:1000:1000:tester comment:/home/alice:"
        )
    );
    assert_eq!(renamed.group, group.replace("tester", "alice"));
    assert_eq!(renamed.shadow, shadow.replace("\ntester:", "\nalice:"));
    assert_eq!(
        registry.application_accounts(&renamed.passwd).unwrap(),
        registry.application_accounts(&passwd).unwrap()
    );
    let again = rename_primary_tables(
        &registry,
        &renamed.passwd,
        &renamed.group,
        &renamed.shadow,
        "alice",
    )
    .unwrap();
    assert_eq!(again.passwd, renamed.passwd);
    assert_eq!(again.group, renamed.group);
    assert_eq!(again.shadow, renamed.shadow);
    for name in ["root", "other", "wheel", "tdc1000", "tda99999", "../alice"] {
        assert!(rename_primary_tables(&registry, &passwd, &group, &shadow, name).is_err());
    }
    let canonical = rename_primary_tables(
        &registry,
        &PASSWD.replace("/home/tester", "/var/home/tester"),
        GROUP,
        SHADOW,
        "tester",
    )
    .unwrap();
    assert_eq!(canonical.passwd, PASSWD);
}

#[test]
fn primary_table_rename_preserves_near_matches_and_refuses_growth_past_the_bound() {
    let registry = Registry::parse(TABLE).unwrap();
    let passwd = format!("{PASSWD}tester2:x:2000:2000:Other:/home/tester2:/bin/sh\natester:x:2001:2001:Other:/home/atester:/bin/sh\n");
    let group = format!("{GROUP}team:x:2000:tester2,tester,atester\n");
    let shadow = format!("{SHADOW}tester2::0:0:99999:7:::\natester::0:0:99999:7:::\n");
    let renamed = rename_primary_tables(&registry, &passwd, &group, &shadow, "alice").unwrap();
    assert!(renamed
        .group
        .ends_with("team:x:2000:tester2,alice,atester\n"));
    assert!(renamed.passwd.contains("\ntester2:x:2000:"));
    assert!(renamed.passwd.contains("\natester:x:2001:"));
    let bounded = SHADOW.replace(
        "other::",
        &format!("other:{}:", "x".repeat(MAX_BYTES - SHADOW.len())),
    );
    assert_eq!(bounded.len(), MAX_BYTES);
    registry
        .check_primary_name(PASSWD, GROUP, &bounded, "alexandria")
        .unwrap();
    let error = rename_primary_tables(&registry, PASSWD, GROUP, &bounded, "alexandria")
        .err()
        .unwrap();
    assert!(error.contains("exceeds its bound"), "{error}");
}

struct AccountStageFixture(PathBuf);

impl AccountStageFixture {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("td-primary-stage-{}-{stamp}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("etc"))
            .unwrap();
        for (name, contents, mode) in [
            (TABLE_NAME, TABLE, 0o444),
            ("passwd", PASSWD, 0o644),
            ("group", GROUP, 0o644),
            ("shadow", SHADOW, 0o600),
        ] {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(root.join("etc").join(name))
                .unwrap();
            file.write_all(contents.as_bytes()).unwrap();
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .unwrap();
        }
        Self(root)
    }
}

impl Drop for AccountStageFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn primary_staging_creates_consistent_private_output_without_changing_the_source() {
    let fixture = AccountStageFixture::new();
    let output = fixture.0.join("prepared");
    stage_primary_name(&fixture.0, "alice", &output).unwrap();
    check_primary_name(&output, "alice").unwrap();
    check_deployment(&output).unwrap();
    assert_eq!(std::fs::metadata(&output).unwrap().mode() & 0o7777, 0o700);
    assert_eq!(std::fs::metadata(output.join("etc")).unwrap().mode() & 0o7777, 0o755);
    for (name, original, mode) in [
        (TABLE_NAME, TABLE, 0o444),
        ("passwd", PASSWD, 0o644),
        ("group", GROUP, 0o644),
        ("shadow", SHADOW, 0o600),
    ] {
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("etc").join(name)).unwrap(),
            original
        );
        let path = output.join("etc").join(name);
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.mode() & 0o7777, mode);
        assert_eq!(metadata.uid(), std::fs::metadata(&fixture.0).unwrap().uid());
    }
    assert_eq!(
        std::fs::read_to_string(output.join("etc/td-principals.tsv")).unwrap(),
        TABLE
    );
    assert!(!output.join("var").exists());
    let before = std::fs::read(output.join("etc/passwd")).unwrap();
    assert!(stage_primary_name(&fixture.0, "bob", &output).is_err());
    assert_eq!(std::fs::read(output.join("etc/passwd")).unwrap(), before);
    let alias = fixture.0.join("alias");
    std::os::unix::fs::symlink(&output, &alias).unwrap();
    assert!(stage_primary_name(&fixture.0, "bob", &alias).is_err());
    assert_eq!(std::fs::read(output.join("etc/passwd")).unwrap(), before);
}

#[test]
fn primary_staging_refuses_bad_identity_or_writable_parent_before_creating_output() {
    let fixture = AccountStageFixture::new();
    let output = fixture.0.join("prepared");
    for name in ["root", "other", "tdc1000", "invalid:name"] {
        assert!(stage_primary_name(&fixture.0, name, &output).is_err());
        assert!(!output.exists());
    }
    std::fs::write(
        fixture.0.join("etc/group"),
        GROUP.replace("10:tester", "10:tester,alice"),
    )
    .unwrap();
    assert!(stage_primary_name(&fixture.0, "alice", &output).is_err());
    assert!(!output.exists());
    std::fs::write(fixture.0.join("etc/group"), GROUP).unwrap();
    let weak = fixture.0.join("weak");
    std::fs::create_dir(&weak).unwrap();
    std::fs::set_permissions(&weak, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(stage_primary_name(&fixture.0, "alice", &weak.join("prepared")).is_err());
    assert!(!weak.join("prepared").exists());
    let linked = fixture.0.join("linked");
    std::os::unix::fs::symlink(&fixture.0, &linked).unwrap();
    assert!(stage_primary_name(&fixture.0, "alice", &linked.join("prepared")).is_err());
    assert!(!output.exists());
}

#[test]
fn primary_names_preserve_numeric_reservations_and_reject_identity_collisions() {
    let registry = Registry::parse(TABLE).unwrap();
    for name in ["tester", "alice", "a-b_2", "tda", "tdcarol", "tda99x", &"a".repeat(32)] {
        registry.check_primary_name(PASSWD, GROUP, SHADOW, name).unwrap();
    }
    for name in ["root", "other", "wheel", "tdc1000", "tdb1001", "tdp1000", "tda65536",
        "tdc1002", "tda99999", "tdb0", "tdp0001",
        "", "Alice", "-alice", "1alice", "a.b", "../root", "alice\n", "álîce", &"a".repeat(33)] {
        assert!(registry.check_primary_name(PASSWD, GROUP, SHADOW, name).is_err(), "accepted {name:?}");
    }
    // The preflight also accepts a deployment already using a different name.
    registry.check_primary_name(
        &PASSWD.replace("tester", "alice"),
        &GROUP.replace("tester", "alice"),
        &SHADOW.replace("tester", "alice"), "alice").unwrap();
}

#[test]
fn primary_name_check_refuses_orphan_authority_and_inconsistent_tables() {
    let registry = Registry::parse(TABLE).unwrap();
    for (passwd, group, shadow) in [
        (PASSWD.to_owned(), GROUP.replace("10:tester", "10:tester,alice"), SHADOW.to_owned()),
        (PASSWD.to_owned(), GROUP.replace("10:tester", "10:tester,missing"), SHADOW.to_owned()),
        (PASSWD.to_owned(), GROUP.replace("10:tester", "10:tester,tester"), SHADOW.to_owned()),
        (PASSWD.to_owned(), GROUP.replace("10:tester", "10:tester,"), SHADOW.to_owned()),
        (PASSWD.to_owned(), GROUP.replace("tester:x:1000:", "alias:x:1000:"), SHADOW.to_owned()),
        (PASSWD.to_owned(), GROUP.replace("tester:x:1000:\n", ""), SHADOW.to_owned()),
        (PASSWD.to_owned(), GROUP.replace("tester:x:1000:", "tester:x:01000:"), SHADOW.to_owned()),
        (PASSWD.to_owned(), GROUP.to_owned(), SHADOW.replace("tester::0:0:99999:7:::\n", "")),
        (PASSWD.to_owned(), GROUP.to_owned(), format!("{SHADOW}alice:secret-hash:0:0:99999:7:::\n")),
        (PASSWD.replace("/home/tester", "/home/tester/../root"), GROUP.to_owned(), SHADOW.to_owned()),
        (PASSWD.replace("1000:1000", "1000:0"), GROUP.to_owned(), SHADOW.to_owned()),
    ] {
        let error = registry.check_primary_name(&passwd, &group, &shadow, "alice").unwrap_err();
        assert!(!error.contains("secret-hash"));
    }
}

#[test]
fn active_application_accounts_bind_reserved_uids_to_private_homes() {
    let registry = Registry::parse(TABLE).unwrap();
    assert!(registry.application_accounts(PASSWD).unwrap().is_empty());
    let row = "tda65536:x:65536:65536:Browser:/var/lib/td/applications/65536:/bin/false\n";
    let passwd = format!("{PASSWD}{row}");
    let group = format!("{GROUP}tda65536:x:65536:\n");
    let shadow = format!("{SHADOW}tda65536:!td-service:0:0:99999:7:::\n");
    registry.verify_accounts(&passwd, &group, &shadow).unwrap();
    let active = registry.application_accounts(&passwd).unwrap();
    assert_eq!(active, vec![Application { owner: 1000, name: "firefox".into(), uid: 65536 }]);
    for bad in [
        passwd.replace("applications/65536", "applications/65537"),
        passwd.replace("/var/lib/td/applications/65536", "/home/tester"),
        passwd.replace("applications/65536", "applications/65536/../65536"),
        passwd.replace("applications/65536", "applications//65536"),
        passwd.replace("applications/65536", "applications/./65536"),
        passwd.replace("65536:65536", "65536:1000"),
        passwd.replace("tda65536", "tester-app"),
        format!("{passwd}{row}"),
    ] {
        assert!(registry.verify_accounts(&bad, &group, &shadow).is_err());
    }
}

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
    check_primary_name(&root, "alice").unwrap();
    assert!(check_primary_name(&root, "root").is_err());
    for (name, contents) in [("passwd", PASSWD), ("group", GROUP), ("shadow", SHADOW), ("td-principals.tsv", TABLE)] {
        assert_eq!(fs::read_to_string(etc.join(name)).unwrap(), contents);
    }
    assert!(!root.join("var").exists());
    fs::rename(etc.join("passwd"), etc.join("other-passwd")).unwrap();
    std::os::unix::fs::symlink("other-passwd", etc.join("passwd")).unwrap();
    assert!(check_deployment(&root).is_err());
    assert!(check_primary_name(&root, "alice").is_err());
    fs::remove_file(etc.join("passwd")).unwrap();
    fs::hard_link(etc.join("other-passwd"), etc.join("passwd")).unwrap();
    assert!(check_deployment(&root).is_err());
    assert!(check_primary_name(&root, "alice").is_err());
    fs::remove_file(etc.join("other-passwd")).unwrap();
    check_deployment(&root).unwrap();
    check_primary_name(&root, "alice").unwrap();
    fs::set_permissions(etc.join("group"), Permissions::from_mode(0o600)).unwrap();
    assert!(check_primary_name(&root, "alice").is_err());
    fs::set_permissions(etc.join("group"), Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(etc.join("shadow"), Permissions::from_mode(0o644)).unwrap();
    assert!(check_deployment(&root).is_err());
    assert!(check_primary_name(&root, "alice").is_err());
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
