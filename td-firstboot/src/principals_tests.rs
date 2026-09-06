#![allow(clippy::unwrap_used)]

use super::*;

const TABLE: &str = "td-principals-v1\nsession\t1000\t993\t991\t989\nsession\t1001\t992\t990\t988\napplication\t1000\tfirefox\t65536\napplication\t1000\tmail\t65538\napplication\t1001\tmail\t65537\n";

#[test]
fn application_identity_is_scoped_to_its_human_and_exact_name() {
    let registry = Registry::parse(TABLE).unwrap();
    assert_eq!(registry.session(1000).unwrap().compositor, 993);
    assert_eq!(registry.session(1000).unwrap().broker, 991);
    assert_eq!(registry.session(1000).unwrap().portal, 989);
    assert_eq!(registry.application(1000, "mail").unwrap().uid, 65538);
    assert_eq!(registry.application(1001, "mail").unwrap().uid, 65537);
    assert!(registry.application(1001, "firefox").is_none());
    assert!(registry.application(1000, "Mail").is_none());
    assert!(registry.session(1002).is_none());
    assert_eq!(registry.application_for_uid(65537).unwrap().owner, 1001);
    assert!(registry.application_for_uid(1000).is_none());
    assert!(registry.application_for_uid(993).is_none());
}

#[test]
fn adding_an_earlier_name_does_not_renumber_existing_assignments() {
    let before = Registry::parse(TABLE).unwrap();
    let after = Registry::parse(&TABLE.replace(
        "application\t1000\tfirefox",
        "application\t1000\talpha\t65539\napplication\t1000\tfirefox",
    ))
    .unwrap();
    for (owner, name) in [(1000, "firefox"), (1000, "mail"), (1001, "mail")] {
        assert_eq!(
            before.application(owner, name),
            after.application(owner, name)
        );
    }
}

#[test]
fn duplicate_or_ambiguous_bindings_are_refused() {
    for altered in [
        TABLE.replace(
            "session\t1001\t992\t990\t988",
            "session\t1001\t993\t990\t988",
        ),
        TABLE.replace(
            "session\t1001\t992\t990\t988",
            "session\t1000\t992\t990\t988",
        ),
        TABLE.replace("mail\t65538", "mail\t65536"),
        TABLE.replace("\t991\t", "\t993\t"),
        TABLE.replace("\t989\n", "\t990\n"),
        TABLE.replace("application\t1000\tmail", "application\t1000\tfirefox"),
        TABLE.replace("application\t1001\tmail", "application\t1002\tmail"),
    ] {
        assert!(Registry::parse(&altered).is_err(), "{altered}");
    }
}

#[test]
fn ordering_is_numeric_then_name_and_sessions_cannot_follow_applications() {
    for altered in [
        TABLE.replace(
            "session\t1000\t993\t991\t989\nsession\t1001\t992\t990\t988",
            "session\t1001\t992\t990\t988\nsession\t1000\t993\t991\t989",
        ),
        TABLE.replace("application\t1000\tfirefox", "application\t1000\tzebra"),
        format!("{TABLE}session\t1002\t987\t986\t985\n"),
        TABLE.replace("application\t1001\tmail", "application\t1000\taardvark"),
    ] {
        assert!(Registry::parse(&altered).is_err(), "{altered}");
    }
}

#[test]
fn canonical_ids_exclude_root_overflow_and_cross_role_aliases() {
    for value in [
        "0",
        "999",
        "65534",
        "65535",
        "65536",
        "+1000",
        "01000",
        " 1000",
        "-1",
        "4294967296",
    ] {
        assert!(
            Registry::parse(&TABLE.replace("1000", value)).is_err(),
            "owner {value}"
        );
    }
    for value in ["0", "1000", "65534", "65536", "+993", "0993"] {
        assert!(
            Registry::parse(&TABLE.replace("993", value)).is_err(),
            "compositor {value}"
        );
    }
    for value in [
        "0",
        "993",
        "1000",
        "65534",
        "65535",
        "2147483648",
        "+65536",
        "065536",
    ] {
        assert!(
            Registry::parse(&TABLE.replace("65536", value)).is_err(),
            "app {value}"
        );
    }
}

#[test]
fn malformed_names_framing_and_versions_are_refused() {
    for value in [
        "",
        ".mail",
        "-mail",
        "Mail",
        "mail/name",
        "mail name",
        "máil",
        "mail\tother",
        "mail\0",
    ] {
        assert!(
            Registry::parse(&TABLE.replace("firefox", value)).is_err(),
            "name {value:?}"
        );
    }
    for altered in [
        String::new(),
        MAGIC.to_string(),
        TABLE.trim_end().to_string(),
        TABLE.replace('\n', "\r\n"),
        TABLE.replacen("v1", "v2", 1),
        format!("{TABLE}\n"),
        TABLE.replace("993", "993\textra"),
        TABLE.replace("65536", "65536\textra"),
        TABLE.replace("firefox", &"a".repeat(65)),
    ] {
        assert!(Registry::parse(&altered).is_err(), "{altered:?}");
    }
}

#[test]
fn aggregate_row_and_byte_bounds_are_enforced() {
    let mut text = format!("{MAGIC}session\t1000\t993\t991\t989\n");
    for index in 0..255 {
        text.push_str(&format!(
            "application\t1000\tapp{index:03}\t{}\n",
            65536 + index
        ));
    }
    assert!(Registry::parse(&text).is_ok());
    text.push_str("application\t1000\tapp255\t65791\n");
    assert!(Registry::parse(&text).is_err());
    assert!(Registry::parse(&format!("{TABLE}{}\n", "a".repeat(MAX_BYTES))).is_err());
}

#[test]
fn enrollment_preserves_retired_assignments_and_refuses_reuse() {
    let prior = Registry::parse(TABLE).unwrap();
    let retired = Registry::parse(&TABLE.replace("application\t1000\tmail\t65538\n", "")).unwrap();
    assert_eq!(prior.enroll(&retired).unwrap(), prior);
    let recycled = Registry::parse(&TABLE.replace(
        "application\t1000\tmail\t65538",
        "application\t1000\tnews\t65538",
    ))
    .unwrap();
    assert!(prior.enroll(&recycled).is_err());
    let renumbered = Registry::parse(&TABLE.replace("mail\t65538", "mail\t65539")).unwrap();
    assert!(prior.enroll(&renumbered).is_err());
    let new_compositor = Registry::parse(&TABLE.replace("993", "987")).unwrap();
    assert!(prior.enroll(&new_compositor).is_err());
}

#[test]
fn enrollment_is_idempotent_and_serializes_in_canonical_order() {
    let prior = Registry::parse(TABLE).unwrap();
    assert_eq!(prior.encode(), TABLE);
    assert_eq!(prior.enroll(&prior).unwrap(), prior);
    let expanded = Registry::parse(&TABLE.replace(
        "application\t1000\tfirefox",
        "application\t1000\talpha\t65539\napplication\t1000\tfirefox",
    ))
    .unwrap();
    let combined = prior.enroll(&expanded).unwrap();
    assert_eq!(combined, expanded);
    assert_eq!(combined.enroll(&prior).unwrap(), combined);
    assert_eq!(Registry::parse(&combined.encode()).unwrap(), combined);
}

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
