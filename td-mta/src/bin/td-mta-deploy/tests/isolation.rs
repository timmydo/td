//! Acceptance fixtures for the exact production identities and cutover files.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::collections::BTreeSet;
use std::os::unix::{fs::symlink, process::CommandExt};
use std::process::Stdio;
use td_fs::TempDir;
use td_json::Json;

#[test]
fn service_units_and_sudoers_bind_only_their_named_role() {
    let files = files::production().unwrap();
    let names: BTreeSet<_> = ROLES.iter().map(|r| role_user(r.name).unwrap()).collect();
    let connections: BTreeSet<_> = ROLES.iter().map(|r| connection(r.name).unwrap()).collect();
    assert_eq!((names.len(), connections.len()), (5, 5));
    assert_eq!(role("acme").unwrap().unit, "td-mta-acme-production.service");
    assert!(role("operator").is_err());
    assert_eq!(
        backup_files().unwrap(),
        files.iter().map(|f| f.path.as_path()).collect::<Vec<_>>()
    );
    for role in ROLES {
        let dir = format!("{}.d", role.unit);
        let unit = &files
            .iter()
            .find(|f| f.path.parent().unwrap().ends_with(&dir))
            .unwrap()
            .text;
        assert!(unit.contains(&format!("User={}\nGroup={}\n", role.user, role.user)));
        assert!(unit.contains("NoNewPrivileges=yes"));
        assert_eq!(
            unit.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE"),
            matches!(role.name, "frontend" | "receive" | "acme")
        );
        assert!(unit.contains(&format!("BindPaths={BASE}/{}:{MAIL}", role.name)));
        assert!(unit.contains("TemporaryFileSystem=/var/lib/td-mta:ro,mode=0755"));
        assert!(!unit.contains("ExecStart="));
    }
    let rules = &files.iter().find(|f| f.mode == 0o440).unwrap().text;
    let expected: BTreeSet<_> = ROLES
        .iter()
        .filter(|r| r.name != "acme")
        .flat_map(|r| {
            ["status --json --require-ready", "performance-status"].map(move |op| {
                format!(
                    "{LEGACY} ALL=({}) NOPASSWD: NOSETENV: {BIN} {op} --runtime {}",
                    r.user, r.runtime
                )
            })
        })
        .collect();
    assert_eq!(rules.lines().count(), 8);
    assert_eq!(
        rules.lines().map(str::to_owned).collect::<BTreeSet<_>>(),
        expected
    );
    if let Ok(mut visudo) = Command::new("visudo")
        .args(["-cf", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
    {
        visudo
            .stdin
            .take()
            .unwrap()
            .write_all(rules.as_bytes())
            .unwrap();
        assert!(visudo.wait().unwrap().success());
    }
}

fn legacy_checks() -> Json {
    let mut rows = vec![r#"{"name":"backup-status","argv":["other"]}"#.to_string()];
    for r in ROLES.iter().filter(|r| r.name != "acme") {
        for (prefix, cmd) in [
            ("mail-", r#""status","--json","--require-ready""#),
            ("performance-", r#""performance-status""#),
        ] {
            rows.push(format!(
                r#"{{"name":"{prefix}{}","argv":["{BIN}",{cmd},"--runtime","{}"]}}"#,
                r.name, r.legacy_runtime
            ));
        }
    }
    td_json::parse(&format!(
        r#"{{"checks":[{}],"state":"preserved"}}"#,
        rows.join(",")
    ))
    .unwrap()
}

#[test]
fn monitor_keeps_other_settings_and_rejects_ambiguous_or_changed_commands() {
    let mut config = legacy_checks();
    scoped::notification_config(&mut config).unwrap();
    assert_eq!(config.get("state").unwrap().as_str(), Some("preserved"));
    let Json::Arr(checks) = config.get("checks").unwrap() else {
        panic!()
    };
    assert_eq!(
        checks[0],
        td_json::parse(r#"{"name":"backup-status","argv":["other"]}"#).unwrap()
    );
    for c in checks.iter().skip(1) {
        let (prefix, name) = c
            .get("name")
            .unwrap()
            .as_str()
            .unwrap()
            .split_once('-')
            .unwrap();
        let role = role(name).unwrap();
        let Json::Arr(argv) = c.get("argv").unwrap() else {
            panic!()
        };
        let actual = argv.iter().map(|v| v.as_str().unwrap()).collect::<Vec<_>>();
        let mut want = vec!["/usr/bin/sudo", "-n", "-u", role.user, "--", BIN];
        want.extend(if prefix == "performance" {
            vec!["performance-status"]
        } else {
            vec!["status", "--json", "--require-ready"]
        });
        want.extend(["--runtime", role.runtime]);
        assert_eq!(actual, want);
    }
    assert!(scoped::notification_config(&mut config).is_err());
    for missing in [true, false] {
        let mut config = legacy_checks();
        let Json::Arr(checks) = config.get_mut("checks").unwrap() else {
            panic!()
        };
        if missing {
            checks.pop();
        } else {
            checks.push(checks.last().unwrap().clone());
        }
        assert!(scoped::notification_config(&mut config).is_err());
    }
}

#[test]
fn refresh_publishes_changes_reuses_current_config_and_refuses_symlink() {
    let tmp = TempDir::new("notification-refresh").unwrap();
    let root = tmp.path();
    let uid = fs::metadata(root).unwrap().uid();
    let source = root.join("config.json");
    let isolated = root.join("isolated.json");
    let mut config = legacy_checks();
    fs::write(&source, config.to_canonical()).unwrap();
    scoped::notifications(root, uid).unwrap();
    let initial = fs::metadata(&isolated).unwrap();
    assert_eq!(initial.mode() & 0o7777, 0o600);
    scoped::notifications(root, uid).unwrap();
    assert_eq!(fs::metadata(&isolated).unwrap().ino(), initial.ino());
    crate::common::set(
        &mut config,
        "recipient",
        Json::Str("new@example.test".into()),
    )
    .unwrap();
    fs::write(&source, config.to_canonical()).unwrap();
    scoped::notifications(root, uid).unwrap();
    assert_ne!(fs::metadata(&isolated).unwrap().ino(), initial.ino());
    assert_eq!(
        td_json::parse(&fs::read_to_string(&isolated).unwrap())
            .unwrap()
            .get("recipient"),
        config.get("recipient")
    );
    fs::remove_file(&isolated).unwrap();
    symlink(&source, &isolated).unwrap();
    assert!(scoped::notifications(root, uid).is_err());
    assert_eq!(fs::read_to_string(source).unwrap(), config.to_canonical());
}

#[test]
fn clock_transfer_carries_or_clears_evidence_and_refuses_linked_ancestors() {
    let tmp = TempDir::new("isolation-clocks").unwrap();
    let root = File::open(tmp.path()).unwrap();
    let uid = root.metadata().unwrap().uid();
    let shared = Path::new("shared");
    let isolated = Path::new("isolated");
    for (relative, generation) in [(shared, 1), (isolated, 2)] {
        make_directory(&tmp.path().join(relative), 0o700).unwrap();
        scoped::write_clock(&root, relative, uid, &[generation; 96]).unwrap();
    }
    let transfer = |src: &Path, dst: &Path| {
        let current = scoped::read_clock(&root, src, uid)?;
        scoped::write_clock(
            &root,
            dst,
            uid,
            current.as_ref().map_or(&[], |r| r.as_slice()),
        )
    };
    transfer(shared, isolated).unwrap();
    let dest = tmp.path().join(isolated).join("queue-clock");
    assert_eq!(fs::read(&dest).unwrap(), [1; 96]);
    assert_eq!(fs::metadata(&dest).unwrap().mode() & 0o7777, 0o600);
    scoped::write_clock(&root, isolated, uid, &[3; 96]).unwrap();
    transfer(isolated, shared).unwrap();
    assert_eq!(
        scoped::read_clock(&root, shared, uid).unwrap(),
        Some([3; 96])
    );
    fs::remove_file(&dest).unwrap();
    transfer(isolated, shared).unwrap();
    assert!(scoped::read_clock(&root, shared, uid).unwrap().is_none());
    let sentinel = tmp.path().join("sentinel");
    fs::write(&sentinel, [4; 96]).unwrap();
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
    for hard in [true, false] {
        if hard {
            fs::hard_link(&sentinel, &dest).unwrap();
        } else {
            symlink(&sentinel, &dest).unwrap();
        }
        let from = tmp.path().join(shared).join("queue-clock");
        symlink(&sentinel, &from).unwrap();
        transfer(isolated, shared).unwrap();
        assert!(fs::symlink_metadata(&from).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), [4; 96]);
        fs::remove_file(&dest).unwrap();
    }
    let real = tmp.path().join("real-runtime");
    fs::rename(tmp.path().join(shared), &real).unwrap();
    symlink(&real, tmp.path().join(shared)).unwrap();
    fs::write(real.join("queue-clock"), b"untouched").unwrap();
    assert!(transfer(isolated, shared).is_err());
    assert_eq!(fs::read(real.join("queue-clock")).unwrap(), b"untouched");
}

#[test]
#[ignore = "requires root-owned fixture in /run"]
fn unit_install_and_rollback_recover_partial_install_and_preserve_operator_files() {
    let tmp = TempDir::new_in(Path::new("/run"), "isolation-settings").unwrap();
    let list = files::settings(tmp.path(), &tmp.path().join("sudoers")).unwrap();
    let host = Host::default();
    let readiness = crate::readiness::settings(tmp.path()).unwrap();
    // Adopt an existing installation with just the three original jobs.
    files::install(&host, &readiness[..3]).unwrap();
    let existing = fs::metadata(&readiness[0].path).unwrap().ino();
    let mut added = Vec::new();
    files::install_tracking(&host, &readiness, |file| {
        added.push(file);
        Ok(())
    })
    .unwrap();
    assert_eq!(added.len(), 2);
    files::rollback(added).unwrap();
    assert!(readiness[..3]
        .iter()
        .all(|file| files::present(file).unwrap()));
    assert!(readiness[3..]
        .iter()
        .all(|file| !files::present(file).unwrap()));
    files::install(&host, &readiness).unwrap();
    files::install(&host, &readiness).unwrap();
    assert_eq!(existing, fs::metadata(&readiness[0].path).unwrap().ino());
    assert!(files::installed(&readiness).unwrap());
    files::install(&host, &list[..1]).unwrap();
    assert!(files::installed(&list)
        .unwrap_err()
        .to_string()
        .contains("incomplete"));
    let install = || files::install(&host, &list);
    install().unwrap();
    install().unwrap();
    assert!(files::installed(&list).unwrap());
    let unit = &list[0];
    // An interruption immediately after publication can leave the staging
    // directory in place. The published inode must already have one link.
    let staged_tree = TempDir::new_in(unit.path.parent().unwrap(), ".td-mta-identity").unwrap();
    let staged = staged_tree.path().join("setting");
    fs::rename(&unit.path, &staged).unwrap();
    files::publish(&host, &staged, &unit.path).unwrap();
    assert_eq!(fs::metadata(&unit.path).unwrap().nlink(), 1);
    assert!(files::installed(&list).unwrap());
    // A conflicting entry that appears after verification must survive.
    fs::write(&staged, "replacement").unwrap();
    assert!(files::publish(&host, &staged, &unit.path).is_err());
    assert_eq!(fs::read_to_string(&unit.path).unwrap(), unit.text);
    let foreign = unit.path.with_file_name("10-operator.conf");
    fs::write(&foreign, b"[Service]\nPrivateTmp=yes\n").unwrap();
    fs::write(&unit.path, "drift").unwrap();
    assert!(install().is_err());
    assert!(files::rollback(&list).is_err());
    fs::write(&unit.path, &unit.text).unwrap();
    files::rollback(&list).unwrap();
    files::rollback(&list).unwrap();
    assert!(!files::installed(&list).unwrap());
    assert!(files::installed(&readiness).unwrap());
    symlink(&foreign, &unit.path).unwrap();
    assert!(install().is_err());
    assert_eq!(fs::read(&foreign).unwrap(), b"[Service]\nPrivateTmp=yes\n");
}

#[test]
#[ignore = "dispatched with a real service uid by private_files_across_real_unix_uids"]
fn scoped_worker() {
    let Ok(raw) = std::env::var("ISOLATION_FIXTURE") else {
        return;
    };
    let root = Path::new(&raw);
    let name = std::env::var("ISOLATION_ROLE").unwrap();
    let uid = fs::metadata("/proc/self").unwrap().uid();
    assert_ne!(uid, 0);
    let arg = std::env::var("ISOLATION_ACTION").unwrap();
    if arg == "legacy-dac" {
        for r in ROLES {
            assert!(fs::read(
                root.join("services")
                    .join(r.name)
                    .join("runtime-connection")
            )
            .is_err());
        }
        return;
    }
    let r = role(&name).unwrap();
    if arg == "dac" {
        scoped::check_dac(
            &root.join("services"),
            r,
            Account { uid, gid: uid },
            &[root.join("legacy/owner-connection").to_str().unwrap()],
        )
        .unwrap();
        assert_eq!(
            fs::read(
                root.join("services")
                    .join(r.name)
                    .join("runtime-connection")
            )
            .unwrap(),
            r.name.as_bytes()
        );
        return;
    }
    let rel = if uid == 61500 {
        format!("legacy/{name}/runtime")
    } else {
        format!("services/{name}/{name}/runtime")
    };
    let root = File::open(root).unwrap();
    match arg.as_str() {
        "write" => scoped::write_clock(&root, Path::new(&rel), uid, &[42; 96]).unwrap(),
        "read" => assert_eq!(
            scoped::read_clock(&root, Path::new(&rel), uid).unwrap(),
            Some([42; 96])
        ),
        "invalid" => assert_eq!(
            scoped::read_clock(&root, Path::new(&rel), uid).unwrap(),
            None
        ),
        _ => panic!(),
    }
}

#[test]
#[ignore = "requires real independent Unix identities in a root test guest"]
fn private_files_across_real_unix_uids() {
    let tmp = TempDir::new_in(Path::new("/run"), "isolation-real-uids").unwrap();
    let root = tmp.path();
    fs::set_permissions(root.parent().unwrap(), fs::Permissions::from_mode(0o711)).unwrap();
    fs::set_permissions(root, fs::Permissions::from_mode(0o755)).unwrap();
    let base = root.join("services");
    make_directory(&base, 0o711).unwrap();
    let legacy = root.join("legacy");
    make_directory(&legacy, 0o700).unwrap();
    let secret = legacy.join("owner-connection");
    fs::write(&secret, "owner fixture").unwrap();
    for p in [&legacy, &secret] {
        chown(p, Some(61500), Some(61500)).unwrap();
    }
    let worker = |name: &str, uid: u32, action: &str| {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "--ignored", "isolation::tests::scoped_worker"])
            .env("ISOLATION_FIXTURE", root)
            .env("ISOLATION_ROLE", name)
            .env("ISOLATION_ACTION", action)
            .uid(uid)
            .gid(uid)
            .status()
            .unwrap();
        assert!(status.success(), "{name}/{uid}/{action} failed");
    };
    for (i, r) in ROLES.iter().enumerate() {
        let uid = 61501 + i as u32;
        let account = Account { uid, gid: uid };
        for _ in 0..2 {
            prepare(&base, r, account, r.name.as_bytes(), b"ca fixture").unwrap();
        }
    }
    worker("", 61500, "legacy-dac");
    for (i, r) in ROLES.iter().enumerate() {
        let uid = 61501 + i as u32;
        worker(r.name, uid, "dac");
        if !matches!(r.name, "relay" | "migadu") {
            continue;
        }
        let profile = legacy.join(r.name);
        let runtime = profile.join("runtime");
        for p in [&profile, &runtime] {
            make_directory(p, 0o700).unwrap();
            chown(p, Some(61500), Some(61500)).unwrap();
        }
        for id in [61500, uid] {
            worker(r.name, id, "write");
            worker(r.name, id, "read");
        }
        chown(
            base.join(r.name).join(r.name).join("runtime/queue-clock"),
            Some(0),
            Some(0),
        )
        .unwrap();
        worker(r.name, uid, "invalid");
    }
    let r = role("receive").unwrap();
    let uri = base.join(r.name).join("runtime-connection");
    fs::remove_file(&uri).unwrap();
    symlink(&secret, &uri).unwrap();
    prepare(
        &base,
        r,
        Account {
            uid: 61502,
            gid: 61502,
        },
        b"replacement",
        b"ca",
    )
    .unwrap();
    assert_eq!(fs::read(secret).unwrap(), b"owner fixture");
}
