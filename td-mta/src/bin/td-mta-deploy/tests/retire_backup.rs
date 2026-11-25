#![allow(clippy::unwrap_used)]
use super::*;
use std::os::unix::fs::PermissionsExt;

const TOOL: &str = "/opt/td-mta/bin/td-mta-operator";

/// The `ExecStartEx` busctl reports for one command.
fn exec(argv: &[&str], flags: &[&str]) -> Json {
    let words = |list: &[&str]| list.iter().map(|w| format!("{w:?}")).collect::<Vec<_>>();
    td_json::parse(&format!(
        r#"{{"type":"a(sasasttttuii)","data":[["{}",[{}],[{}],0,0,0,0,0,0,0]]}}"#,
        argv.first().unwrap_or(&""),
        words(argv).join(","),
        words(flags).join(",")
    ))
    .unwrap()
}

#[test]
fn the_destination_is_the_one_the_status_job_reads() {
    let status = |rest: &[&str]| {
        let mut argv = vec![TOOL, "install-td-mta", "--backup-status"];
        argv.extend(rest);
        status_command(&exec(&argv, &["no-env-expand"]))
    };
    let (destination, age) = status(&[
        "--destination",
        "/var/lib/td-mta/backups/scheduled",
        "--maximum-age-seconds",
        "172800",
    ])
    .unwrap();
    assert_eq!(destination, Path::new("/var/lib/td-mta/backups/scheduled"));
    assert_eq!(age, 172_800);
    // The tool's own default age; options in either order.
    let (_, age) = status(&["--destination", "/b"]).unwrap();
    assert_eq!(age, 172_800);
    let (_, age) = status(&["--maximum-age-seconds", "60", "--destination", "/b"]).unwrap();
    assert_eq!(age, 60);
    for refused in [
        &[][..],
        &["--maximum-age-seconds", "60"],
        &["--destination"],
        &["--destination", "relative"],
        &["--destination", "/a b"],
        &["--destination", "/$HOME"],
        &["--destination", "/a", "--destination", "/b"],
        &["--destination", "/a", "--maximum-age-seconds", "x"],
        &["--destination", "/a", "--other", "1"],
        &["--destination", "/a/../b"],
        &["--destination", "/a/./b"],
        &["--destination", "/a", "--maximum-age-seconds", "0"],
        &["--destination", "/a", "--maximum-age-seconds", "31536001"],
    ] {
        assert!(status(refused).is_err(), "{refused:?}");
    }
    let good = [
        TOOL,
        "install-td-mta",
        "--backup-status",
        "--destination",
        "/b",
    ];
    assert!(status_command(&exec(&good, &[])).is_ok());
    assert!(status_command(&exec(&good, &["ignore-failure"])).is_err());
    let other = [TOOL, "install-td-mta", "--backup", "--destination", "/b"];
    assert!(status_command(&exec(&other, &[])).is_err());
    // One command, no more.
    let two = td_json::parse(&format!(
        r#"{{"data":[{0},{0}]}}"#,
        exec(&good, &[])
            .get("data")
            .and_then(Json::as_arr)
            .and_then(<[Json]>::first)
            .unwrap()
    ))
    .unwrap();
    assert!(status_command(&two).is_err());
    assert!(status_command(&td_json::parse(r#"{"data":[]}"#).unwrap()).is_err());
    // The backup job names the directory it writes once.
    let backup = |rest: &[&str]| {
        let mut argv = vec![TOOL, "install-td-mta", "--backup"];
        argv.extend(rest);
        backup_destination(&exec(&argv, &["no-env-expand"])).map(str::to_string)
    };
    let written = [
        "--service-file",
        "/s",
        "--service",
        "td-mta-backup",
        "--destination",
        "/var/lib/td-mta/backups/scheduled",
        "--retain",
        "7",
    ];
    assert_eq!(
        backup(&written).unwrap(),
        "/var/lib/td-mta/backups/scheduled"
    );
    assert!(backup(&["--service", "x"]).is_err());
    assert!(backup(&["--destination"]).is_err());
    assert!(backup(&["--destination", "/a", "--destination", "/a"]).is_err());
    assert!(backup_destination(&exec(&good, &[])).is_err());
}

#[test]
fn busctl_unsigned_replies_are_read_exactly() {
    assert_eq!(
        unsigned(r#"{"type":"t","data":1764040500000000}"#),
        Some(1_764_040_500_000_000)
    );
    assert_eq!(
        unsigned(r#"{"type":"t","data":18446744073709551615}"#),
        Some(u64::MAX)
    );
    for bad in [
        r#"{"type":"t","data":-1}"#,
        r#"{"type":"t","data":"1"}"#,
        r#"{"type":"t"}"#,
        "",
        "garbage",
    ] {
        assert_eq!(unsigned(bad), None, "{bad}");
    }
}

/// A private directory holding archives created at `stamps`, each with
/// the receipt the scheduled backup writes beside it.
fn destination(dir: &td_fs::TempDir, stamps: &[u64]) -> PathBuf {
    let root = dir.path().join("scheduled");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for (pid, stamp) in stamps.iter().enumerate() {
        let archive = root.join(format!("backup-{stamp}-{pid}.dump"));
        fs::write(&archive, "dump").unwrap();
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o600)).unwrap();
        let receipt = root.join(format!("backup-{stamp}-{pid}.json"));
        fs::write(
            &receipt,
            format!(
                r#"{{"schema":1,"archive":"{}","bytes":4,"created_unix_seconds":{stamp}}}"#,
                archive.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
    }
    root
}

#[test]
fn the_newest_archive_is_judged_as_the_status_job_judges_it() {
    let day = 24 * 3600;
    let now = 10 * day;
    let dir = td_fs::TempDir::new("retire-backup").unwrap();
    let root = destination(&dir, &[now - 3 * day, now - 3600, now - day]);
    let backup = scheduled(&root, 2 * day, now).unwrap();
    assert_eq!(backup.status, "ready");
    let (archive, created) = backup.newest.as_ref().unwrap();
    assert_eq!(*created, now - 3600);
    assert!(archive.ends_with(format!("backup-{}-1.dump", now - 3600)));
    let json = backup.json().unwrap();
    assert_eq!(json.get("status").and_then(Json::as_str), Some("ready"));
    assert_eq!(
        json.get("created_unix_seconds").and_then(Json::as_u64),
        Some(now - 3600)
    );
    // Too old for the status job's maximum age, or from the future.
    assert_eq!(scheduled(&root, 60, now).unwrap().status, "stale");
    assert_eq!(
        scheduled(&root, 2 * day, now - 7200).unwrap().status,
        "future"
    );
    // The last attempt failed.
    fs::write(root.join("last-error"), "").unwrap();
    fs::set_permissions(root.join("last-error"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        scheduled(&root, 2 * day, now).unwrap().status,
        "last-attempt-failed"
    );
    fs::remove_file(root.join("last-error")).unwrap();
    // A receipt that does not match its archive refuses.
    let receipt = root.join(format!("backup-{}-1.json", now - 3600));
    let good = fs::read_to_string(&receipt).unwrap();
    for bad in [
        good.replace(r#""bytes":4"#, r#""bytes":5"#),
        good.replace(r#""schema":1"#, r#""schema":2"#),
        good.replace("-1.dump", "-0.dump"),
        good.replace(r#","created_unix_seconds""#, r#","created""#),
    ] {
        fs::write(&receipt, bad).unwrap();
        assert!(scheduled(&root, 2 * day, now).is_err());
    }
    fs::write(&receipt, &good).unwrap();
    // So does anything the status job would not read.
    fs::set_permissions(&receipt, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(scheduled(&root, 2 * day, now).is_err());
    fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(scheduled(&root, 2 * day, now).is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(scheduled(&dir.path().join("absent"), 2 * day, now).is_err());
    // No archive at all.
    let empty = td_fs::TempDir::new("retire-backup-empty").unwrap();
    let none = destination(&empty, &[]);
    let missing = scheduled(&none, 2 * day, now).unwrap();
    assert_eq!(missing.status, "missing");
    assert!(missing.newest.is_none());
    // Names the status job does not count as archives are passed over.
    for name in [
        "backup-1-2-3.dump",
        "backup-x-1.dump",
        "backup-9.dump",
        "other",
    ] {
        fs::write(none.join(name), "x").unwrap();
        fs::set_permissions(none.join(name), fs::Permissions::from_mode(0o600)).unwrap();
    }
    assert_eq!(scheduled(&none, 2 * day, now).unwrap().status, "missing");
    // A subdirectory, an off-host copy's marker or a name that is not
    // UTF-8 is what the status job refuses to read.
    let refuses = |path: PathBuf, directory: bool| {
        if directory {
            fs::create_dir(&path).unwrap();
        } else {
            fs::write(&path, "").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(scheduled(&none, 2 * day, now).is_err(), "{path:?}");
        if directory {
            fs::remove_dir(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
    };
    refuses(none.join("sub"), true);
    refuses(none.join("offhost-receiver-v1"), false);
    use std::os::unix::ffi::OsStrExt;
    refuses(
        none.join(std::ffi::OsStr::from_bytes(b"backup-\xff.dump")),
        false,
    );
    // Equal stamps: the later name, as the status job sorts paths; and a
    // zero-byte archive refuses.
    let tie = td_fs::TempDir::new("retire-backup-tie").unwrap();
    let root = destination(&tie, &[]);
    for pid in ["10", "9"] {
        let archive = root.join(format!("backup-500-{pid}.dump"));
        fs::write(&archive, "dump").unwrap();
        fs::set_permissions(&archive, fs::Permissions::from_mode(0o600)).unwrap();
        let receipt = root.join(format!("backup-500-{pid}.json"));
        fs::write(
            &receipt,
            format!(
                r#"{{"schema":1,"archive":"{}","bytes":4,"created_unix_seconds":500}}"#,
                archive.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let chosen = scheduled(&root, 2 * day, 600).unwrap();
    assert!(chosen.newest.unwrap().0.ends_with("backup-500-9.dump"));
    fs::write(root.join("backup-500-9.dump"), "").unwrap();
    assert!(scheduled(&root, 2 * day, 600).is_err());
}

#[test]
fn early_retirement_needs_a_ready_backup_newer_than_its_material() {
    let backup = |status, created: Option<u64>| Backup {
        destination: PathBuf::from("/var/lib/td-mta/backups/scheduled"),
        newest: created.map(|c| (PathBuf::from(format!("/b/backup-{c}-1.dump")), c)),
        status,
    };
    let refusal = |r: R<()>| r.unwrap_err().to_string();
    // A newer ready backup passes, before the material is known too.
    guard(&backup("ready", Some(200)), Some(199)).unwrap();
    guard(&backup("ready", Some(200)), None).unwrap();
    // One no newer than the newest material refuses, naming its time.
    for newest in [200, 201] {
        let older = refusal(guard(&backup("ready", Some(200)), Some(newest)));
        assert!(older.starts_with(NO_BACKUP), "{older}");
        assert!(
            older.starts_with(&format!("{NO_BACKUP} {newest},")),
            "{older}"
        );
        assert!(older.contains("created at 200"), "{older}");
    }
    // A backup that is not ready refuses whatever the material.
    for (status, created) in [
        ("missing", None),
        ("stale", Some(300)),
        ("future", Some(300)),
        ("last-attempt-failed", Some(300)),
    ] {
        for newest in [None, Some(100)] {
            let why = refusal(guard(&backup(status, created), newest));
            assert!(why.starts_with(NO_BACKUP), "{why}");
            assert!(why.contains(&format!("is {status};")), "{why}");
            assert!(why.ends_with(RETRY), "{why}");
        }
    }
    // What a run removes: early, all of it once the backup is newer, or
    // nothing; windowed, what the window releases, reading no backup.
    let day = 24 * 3600;
    let material = [
        Material {
            path: PathBuf::from("/old"),
            since: 100,
        },
        Material {
            path: PathBuf::from("/new"),
            since: 100 + 6 * day,
        },
    ];
    let now = 100 + 7 * day;
    let newer = backup("ready", Some(100 + 6 * day + 1));
    let (past, next) = retiring(&material, now, true, Some(&newer)).unwrap();
    assert_eq!(past, [Path::new("/old"), Path::new("/new")]);
    assert_eq!(next, None);
    let older = backup("ready", Some(100 + 6 * day));
    let refused = refusal(retiring(&material, now, true, Some(&older)).map(|_| ()));
    assert!(refused.starts_with(NO_BACKUP), "{refused}");
    let stale = backup("stale", Some(100 + 6 * day + 1));
    assert!(retiring(&material, now, true, Some(&stale)).is_err());
    assert!(retiring(&material, now, true, None).is_err());
    for judged in [None, Some(&older), Some(&stale)] {
        let (past, next) = retiring(&material, now, false, judged).unwrap();
        assert_eq!(past, [Path::new("/old")]);
        assert_eq!(next, Some(100 + 13 * day));
    }
    // Nothing to remove needs only a ready backup.
    assert!(retiring(&[], now, true, Some(&newer)).unwrap().0.is_empty());
    // The next backup's end, when systemd says when it starts.
    assert_eq!(
        finished_by(1_700_000_000_000_000, 2_700_000_000),
        Some(1_700_002_700)
    );
    assert_eq!(finished_by(1_700_000_000_000_001, 0), Some(1_700_000_001));
    assert_eq!(finished_by(0, 2_700_000_000), None);
    assert_eq!(finished_by(1, u64::MAX), None);
}
